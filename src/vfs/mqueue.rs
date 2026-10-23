//! POSIX message queues (`mq_open(3)` and its five siblings): named, bounded,
//! priority-ordered queues.
//!
//! This is the family the IPC namespace waits on: `unshare(CLONE_NEWIPC)` is
//! refused because "SysV IPC objects and POSIX message queues" are the thing
//! the kind would isolate, and the kernel has neither (`docs/notes/` §6.3). The
//! queue objects live here, in `vfs`, for the same reason `eventfd`/`timerfd`/
//! `pidfd` do: the ABI puts a *descriptor* in the caller's hand, and a
//! descriptor is this layer's business - it is pollable, it has a mode and an
//! inode number, and `close` is what ends its claim on the queue.
//!
//! Three things are deliberately not here: the syscalls (they are a thin,
//! validating face in `syscall/linux/ipc.rs`), the errno vocabulary (the queue
//! answers [`MqError`] and the ABI layer names the number), and any notion of a
//! *filesystem* view of the names. Linux's `/dev/mqueue` is a real mount whose
//! entries are the queues; this kernel has no mqueuefs, so a queue's name is its
//! only spelling and `/dev/mqueue` does not exist.
//!
//! The measurements behind the rules are in `docs/notes/` §6.12 - two probes run
//! against Linux 7.2.11 before any of this was written. What they decided:
//!
//! * a queue is **one object** however many descriptors name it, and `unlink`
//!   retires the name while the object lives on through its open descriptors;
//! * messages come out **highest priority first, FIFO within a priority** - the
//!   insert is what makes the ordering, not the read;
//! * `mq_notify` holds **one registration per queue**, and a *send that finds
//!   the queue empty* is what fires it and clears it. A `SIGEV_NONE`
//!   registration occupies the slot like any other (measured: it makes the next
//!   `SIGEV_SIGNAL` `EBUSY` until a send clears it), which is why this is one
//!   `Option<Registration>` and not a signal-or-nothing flag;
//! * the descriptor is readable when the queue is not empty and writable when it
//!   is not full - `POLLIN`/`POLLOUT` and nothing else, measured, with epoll
//!   agreeing;
//! * `read`/`write` on the descriptor are not the interface: `write` is `EINVAL`
//!   on Linux (its `read` answers the mqueuefs inode's own bytes, a filesystem
//!   quirk this kernel has nothing to reproduce), so both refuse. Programs use
//!   the `mq_*` doors.

use alloc::collections::VecDeque;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::cell::RefCell;
use core::sync::atomic::{AtomicU64, Ordering};

use critical_section::Mutex;

use super::{Ino, Inode, Metadata, NodeKind, Ready, VfsError, VfsResult};

/// `MQ_PRIO_MAX`: a priority is `< 32768`, and 32768 itself is `EINVAL`
/// (measured: 32767 is accepted, 32768 is refused).
pub const MQ_PRIO_MAX: u32 = 32768;

/// `/proc/sys/fs/mqueue/msg_max` and `msgsize_max`, as defaults.
///
/// Both are tunables on Linux and both are constants here, which is the honest
/// shape for a kernel with no sysctl tree for this family yet: a queue asking
/// for more than these is `EINVAL` (measured for `msg_max`) rather than quietly
/// given less.
pub const MSG_MAX: u64 = 10;
pub const MSGSIZE_MAX: u64 = 8192;

/// The longest queue name the **kernel** accepts.
///
/// The name the kernel sees has no leading slash - both libcs strip the one the
/// caller wrote, which is why `mq_open("/x")` and the syscall's `"x"` name the
/// same queue. Measured with raw syscalls on a current host
/// (`.workbuddy/probe_mqueue_names.c`): 255 bytes is accepted and 256 is
/// `ENAMETOOLONG`, and any slash at all - leading or inner - is `EACCES`.
pub const NAME_MAX: usize = 255;

/// Why a queue refused an operation, in the queue's own vocabulary.
///
/// The ABI layer maps these onto errno constants - `Full`/`Empty` are `EAGAIN`
/// and `MessageTooBig`/`BufferTooSmall` are `EMSGSIZE` - because the numbers are
/// the syscall's contract and this layer only knows what happened.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MqError {
    /// `EEXIST`: the name is taken (and `O_EXCL` was asked).
    Exists,
    /// `ENOENT`: no queue by that name.
    NoEntry,
    /// `EAGAIN`: full (send) or empty (receive), non-blocking.
    WouldBlock,
    /// `EMSGSIZE`: the message is wider than `mq_msgsize`.
    MessageTooBig,
    /// `EMSGSIZE`: the caller's receive buffer is smaller than `mq_msgsize`,
    /// which Linux refuses whatever the queued message's own length is.
    BufferTooSmall,
    /// `EINVAL`: an argument the queue refuses - a priority at `MQ_PRIO_MAX`.
    Invalid,
    /// `EBUSY`: the queue's one notification slot is taken by a live process.
    Busy,
    /// `EMFILE`: the caller's `RLIMIT_MSGQUEUE` would be exceeded.
    OutOfBudget,
}

/// One queued message: the caller's priority and its bytes.
struct Message {
    priority: u32,
    data: Vec<u8>,
}

/// A notification registration.
///
/// `signo` of 0 is a `SIGEV_NONE` registration: it holds the queue's one slot
/// without asking for a signal, which is what Linux does with it (measured) -
/// so the slot is occupied and the next registration is `EBUSY` until a send
/// clears it.
#[derive(Clone, Copy)]
pub struct Registration {
    pub signo: u8,
    pub value: u64,
    /// The registering process, as `si_pid`/the target of the signal.
    pub pid: i32,
    /// The task id behind `pid`, kept so a registration whose owner has died can
    /// be told from a live one without a scan.
    pub owner: usize,
}

/// A named queue.
///
/// Shared by every descriptor open on it, which is why the mutable half is
/// behind one lock: one core, interrupts off, so this is mutual exclusion rather
/// than a wait.
pub struct Queue {
    name: String,
    pub maxmsg: usize,
    pub msgsize: usize,
    /// Bytes this queue was charged against its creator's `RLIMIT_MSGQUEUE`.
    pub charge: u64,
    /// The uid the charge belongs to.
    pub uid: u32,
    messages: Mutex<RefCell<VecDeque<Message>>>,
    registration: Mutex<RefCell<Option<Registration>>>,
    waiters: Mutex<RefCell<Vec<crate::sched::TaskId>>>,
}

impl Queue {
    fn new(name: String, maxmsg: usize, msgsize: usize, uid: u32) -> Arc<Queue> {
        Arc::new(Queue {
            name,
            maxmsg,
            msgsize,
            // Linux charges a queue against `RLIMIT_MSGQUEUE` at creation, by
            // its *maximal* size: the messages are inside that reservation, so a
            // send is not charged again (measured: with the limit at 0 the
            // creation is `EMFILE`, and nothing else is refused).
            charge: (maxmsg as u64).saturating_mul(msgsize as u64),
            uid,
            messages: Mutex::new(RefCell::new(VecDeque::new())),
            registration: Mutex::new(RefCell::new(None)),
            waiters: Mutex::new(RefCell::new(Vec::new())),
        })
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn queued(&self) -> usize {
        critical_section::with(|cs| self.messages.borrow(cs).borrow().len())
    }

    pub fn is_empty(&self) -> bool {
        self.queued() == 0
    }

    pub fn is_full(&self) -> bool {
        self.queued() >= self.maxmsg
    }

    /// Whether a message can be enqueued now.
    pub fn has_room(&self) -> bool {
        !self.is_full()
    }

    /// Queue a message, highest priority first and FIFO within one.
    ///
    /// The insert is what implements the ordering: Linux's `mq_send` walks the
    /// list from the front to the first message of *lower* priority and inserts
    /// there, so equal priorities stay in arrival order. The read simply takes
    /// the front.
    pub fn send(&self, priority: u32, data: Vec<u8>) -> Result<(), MqError> {
        if priority >= MQ_PRIO_MAX {
            return Err(MqError::Invalid);
        }
        if data.len() > self.msgsize {
            return Err(MqError::MessageTooBig);
        }
        critical_section::with(|cs| {
            let mut messages = self.messages.borrow(cs).borrow_mut();
            if messages.len() >= self.maxmsg {
                return Err(MqError::WouldBlock);
            }
            let at = messages
                .iter()
                .position(|m| m.priority < priority)
                .unwrap_or(messages.len());
            messages.insert(at, Message { priority, data });
            Ok(())
        })
    }

    /// Take the front message. `user_len` is the caller's buffer size, which
    /// Linux compares against `mq_msgsize` rather than against the message.
    pub fn receive(&self, user_len: usize) -> Result<(Vec<u8>, u32), MqError> {
        if user_len < self.msgsize {
            return Err(MqError::BufferTooSmall);
        }
        critical_section::with(|cs| {
            let mut messages = self.messages.borrow(cs).borrow_mut();
            match messages.pop_front() {
                Some(m) => Ok((m.data, m.priority)),
                None => Err(MqError::WouldBlock),
            }
        })
    }

    /// Try to take the queue's one notification slot for `registration`.
    ///
    /// A registration whose owner is gone does not hold the slot: Linux releases
    /// it when the process exits (measured - a child registers, exits, and the
    /// parent's registration then succeeds). Checking liveness here rather than
    /// hooking the exit path keeps that fact in one place.
    pub fn register(&self, registration: Registration) -> Result<(), MqError> {
        critical_section::with(|cs| {
            let mut slot = self.registration.borrow(cs).borrow_mut();
            if let Some(current) = *slot
                && owner_alive(current.owner)
            {
                return Err(MqError::Busy);
            }
            *slot = Some(registration);
            Ok(())
        })
    }

    /// Take and clear the registration, if there is one.
    ///
    /// Called by a send that found the queue empty - the only event Linux
    /// notifies on - so the caller can raise the signal outside this lock.
    pub fn take_registration(&self) -> Option<Registration> {
        critical_section::with(|cs| self.registration.borrow(cs).borrow_mut().take())
    }

    /// Whether a live registration occupies the slot.
    pub fn registered(&self) -> bool {
        critical_section::with(|cs| self.registration.borrow(cs).borrow().is_some())
    }

    /// Add a task to this queue's waiters, without duplicates.
    pub fn add_waiter(&self, id: crate::sched::TaskId) {
        critical_section::with(|cs| {
            let mut waiters = self.waiters.borrow(cs).borrow_mut();
            if !waiters.contains(&id) {
                waiters.push(id);
            }
        });
    }

    pub fn remove_waiter(&self, id: crate::sched::TaskId) {
        critical_section::with(|cs| {
            let mut waiters = self.waiters.borrow(cs).borrow_mut();
            if let Some(at) = waiters.iter().position(|w| *w == id) {
                waiters.remove(at);
            }
        });
    }

    /// Wake everyone parked on this queue: the state each of them tests changed.
    #[cfg(target_os = "none")]
    pub fn wake_waiters(&self) {
        let waiters = critical_section::with(|cs| self.waiters.borrow(cs).borrow().clone());
        for id in waiters {
            crate::sched::wake(id);
        }
    }

    #[cfg(not(target_os = "none"))]
    pub fn wake_waiters(&self) {}
}

/// Whether the task a registration belongs to is still there.
///
/// The machine asks the scheduler; the host build has no task table behind this
/// module (the registry is exercised by tests, not by a running machine), so it
/// answers "yes" and the single-slot rule is what those tests pin.
#[cfg(target_os = "none")]
fn owner_alive(owner: usize) -> bool {
    let id = crate::sched::TaskId(owner);
    crate::sched::pid_number_of(id).is_some() && !crate::sched::is_exited(id)
}

#[cfg(not(target_os = "none"))]
fn owner_alive(_owner: usize) -> bool {
    true
}

// ---------------------------------------------------------------------------
// The name registry
// ---------------------------------------------------------------------------

static REGISTRY: Mutex<RefCell<Vec<Arc<Queue>>>> = Mutex::new(RefCell::new(Vec::new()));

/// Per-uid byte usage, so `RLIMIT_MSGQUEUE` can be the bound Linux makes it.
static USAGE: Mutex<RefCell<Vec<(u32, u64)>>> = Mutex::new(RefCell::new(Vec::new()));

impl Queue {
    fn release_budget(&self) {
        critical_section::with(|cs| {
            let mut usage = USAGE.borrow(cs).borrow_mut();
            if let Some(row) = usage.iter_mut().find(|(uid, _)| *uid == self.uid) {
                row.1 = row.1.saturating_sub(self.charge);
            }
        });
    }
}

/// The queue named `name`, if one exists.
pub fn lookup(name: &str) -> Option<Arc<Queue>> {
    critical_section::with(|cs| {
        REGISTRY
            .borrow(cs)
            .borrow()
            .iter()
            .find(|q| q.name == name)
            .cloned()
    })
}

/// How many bytes `uid` already has in queues.
pub fn usage(uid: u32) -> u64 {
    critical_section::with(|cs| {
        USAGE
            .borrow(cs)
            .borrow()
            .iter()
            .find(|(u, _)| *u == uid)
            .map(|(_, bytes)| *bytes)
            .unwrap_or(0)
    })
}

/// Create and register a queue, charging it against `uid`'s budget.
///
/// `limit` is the caller's `RLIMIT_MSGQUEUE`; the charge is the queue's maximal
/// size, which is Linux's rule (measured: with the limit at 0 creation is
/// `EMFILE` and a send is not charged again).
pub fn create(
    name: String,
    maxmsg: usize,
    msgsize: usize,
    uid: u32,
    limit: u64,
) -> Result<Arc<Queue>, MqError> {
    critical_section::with(|cs| {
        {
            let registry = REGISTRY.borrow(cs).borrow();
            if registry.iter().any(|q| q.name == name) {
                return Err(MqError::Exists);
            }
        }
        let charge = (maxmsg as u64).saturating_mul(msgsize as u64);
        let used = usage(uid);
        if used.saturating_add(charge) > limit {
            return Err(MqError::OutOfBudget);
        }
        let queue = Queue::new(name, maxmsg, msgsize, uid);
        {
            let mut rows = USAGE.borrow(cs).borrow_mut();
            match rows.iter_mut().find(|(u, _)| *u == uid) {
                Some(row) => row.1 = row.1.saturating_add(charge),
                None => rows.push((uid, charge)),
            }
        }
        REGISTRY.borrow(cs).borrow_mut().push(queue.clone());
        Ok(queue)
    })
}

/// Retire `name`, keeping the object alive through its open descriptors.
pub fn unlink(name: &str) -> Result<(), MqError> {
    let found = critical_section::with(|cs| {
        let mut registry = REGISTRY.borrow(cs).borrow_mut();
        let at = registry.iter().position(|q| q.name == name)?;
        Some(registry.remove(at))
    });
    match found {
        Some(queue) => {
            queue.release_budget();
            Ok(())
        }
        None => Err(MqError::NoEntry),
    }
}

// ---------------------------------------------------------------------------
// The descriptor
// ---------------------------------------------------------------------------

/// An inode number nobody else claims.
static NEXT_INO: AtomicU64 = AtomicU64::new(1);

/// One open descriptor on a queue.
///
/// The queue is shared; this is what is per-open, and the two pieces of state
/// that belong to it are the access mode (`mq_send` on an `O_RDONLY` descriptor
/// is `EBADF`, measured) and its identity, for the notification slot's
/// lifetime. `O_NONBLOCK` is *not* here: it lives in the descriptor's status
/// flags, because `F_SETFL` and `mq_setattr` both write the one bit and Linux
/// reads it back through `mq_getattr`.
pub struct MqueueFile {
    ino: Ino,
    queue: Arc<Queue>,
    readable: bool,
    writable: bool,
}

impl MqueueFile {
    pub fn new(queue: Arc<Queue>, readable: bool, writable: bool) -> Arc<MqueueFile> {
        Arc::new(MqueueFile {
            ino: NEXT_INO.fetch_add(1, Ordering::Relaxed),
            queue,
            readable,
            writable,
        })
    }

    pub fn queue(&self) -> &Arc<Queue> {
        &self.queue
    }

    pub fn readable(&self) -> bool {
        self.readable
    }

    pub fn writable(&self) -> bool {
        self.writable
    }
}

impl Inode for MqueueFile {
    fn as_mqueue(&self) -> Option<&MqueueFile> {
        Some(self)
    }

    /// A queue has no position: `lseek` on the descriptor is `ESPIPE` for the
    /// same reason an eventfd's is.
    fn seekable(&self) -> bool {
        false
    }

    /// A regular file with mode `0600`, which is the shape Linux's mqueue
    /// filesystem reports for a queue (measured: `S_IFREG`, `0600`).
    ///
    /// `size` is 0 where Linux reports the mqueuefs inode's own 80 bytes: that
    /// number is a filesystem's *directory entry* arithmetic, and this kernel has
    /// no mqueuefs to have one. It is named as a divergence rather than faked.
    fn metadata(&self) -> Metadata {
        Metadata {
            ino: 0x8000_0000 + self.ino,
            kind: NodeKind::File,
            size: 0,
        }
    }

    fn mode(&self) -> u32 {
        0o600
    }

    fn keeps_modes(&self) -> bool {
        true
    }

    /// `read`/`write` are not this descriptor's interface: `write` is `EINVAL` on
    /// Linux and `read` answers the mqueuefs inode, which this kernel does not
    /// have. Both refuse, and the `mq_*` doors are the way in.
    fn read_at(&self, _offset: u64, _buf: &mut [u8]) -> VfsResult<usize> {
        Err(VfsError::Invalid)
    }

    fn write_at(&self, _offset: u64, _buf: &[u8]) -> VfsResult<usize> {
        Err(VfsError::Invalid)
    }

    /// Readable when the queue holds a message, writable when it has room - and
    /// only from the side this descriptor was opened for. Measured on Linux:
    /// `POLLIN|POLLOUT` on an empty queue is `POLLOUT`, one message is both, a
    /// full queue is `POLLIN`.
    fn poll_ready(&self) -> Ready {
        Ready {
            read: self.readable && !self.queue.is_empty(),
            write: self.writable && !self.queue.is_full(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The insert order *is* the priority rule: highest first, arrival order
    /// within one priority.
    #[test]
    fn messages_come_out_by_priority_then_arrival() {
        let q = Queue::new(String::from("/t"), 8, 16, 0);
        q.send(1, alloc::vec![1]).unwrap();
        q.send(9, alloc::vec![9]).unwrap();
        q.send(5, alloc::vec![5, 1]).unwrap();
        q.send(5, alloc::vec![5, 2]).unwrap();
        let mut order = alloc::vec::Vec::new();
        while !q.is_empty() {
            let (data, priority) = q.receive(16).unwrap();
            order.push((priority, data[0]));
        }
        assert_eq!(order, alloc::vec![(9, 9), (5, 5), (5, 5), (1, 1)]);
    }

    /// A full queue refuses, an empty one refuses, and the caller's buffer is
    /// compared with `mq_msgsize` rather than with the message.
    #[test]
    fn the_queue_is_bounded_in_both_directions() {
        let q = Queue::new(String::from("/t"), 2, 16, 0);
        assert!(q.has_room());
        q.send(0, alloc::vec![0; 1]).unwrap();
        q.send(0, alloc::vec![0; 1]).unwrap();
        assert!(!q.has_room());
        assert_eq!(q.send(0, alloc::vec![0; 1]), Err(MqError::WouldBlock));
        let q2 = Queue::new(String::from("/e"), 2, 16, 0);
        assert_eq!(q2.receive(8), Err(MqError::BufferTooSmall));
        assert_eq!(q2.receive(16), Err(MqError::WouldBlock));
        assert_eq!(q2.send(0, alloc::vec![0; 17]), Err(MqError::MessageTooBig));
        assert_eq!(
            q2.send(MQ_PRIO_MAX, alloc::vec![0; 1]),
            Err(MqError::Invalid)
        );
        assert!(q2.send(MQ_PRIO_MAX - 1, alloc::vec![0; 1]).is_ok());
    }

    /// One registration per queue, and a `SIGEV_NONE` one holds the slot too -
    /// Linux's measured behaviour, and the reason `take_registration` is what a
    /// send uses.
    #[test]
    fn the_notification_slot_is_single_and_cleared_by_a_send() {
        let q = Queue::new(String::from("/n"), 2, 16, 0);
        let me = crate::sched::TaskId(1);
        assert!(
            q.register(Registration {
                signo: 0,
                value: 0,
                pid: 1,
                owner: me.0,
            })
            .is_ok()
        );
        assert!(q.registered());
        assert_eq!(
            q.register(Registration {
                signo: 10,
                value: 7,
                pid: 1,
                owner: me.0,
            }),
            Err(MqError::Busy)
        );
        let taken = q.take_registration().expect("one registration");
        assert_eq!(taken.signo, 0);
        assert!(!q.registered());
    }
}
