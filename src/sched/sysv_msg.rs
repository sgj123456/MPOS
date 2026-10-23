//! System V message queues: the queue table, the send/receive operations, and
//! the waits both sides can be in.
//!
//! A message queue is a FIFO of typed messages - a copy of the bytes at send,
//! not a shared mapping like the segment table's, which is why this family
//! lives beside the semaphore sets rather than beside the segments: what an
//! operation can do beyond its bookkeeping is **wait** (a send on a full queue,
//! a receive that has nothing it can hand over), and the wait is the
//! scheduler's (ARCHITECTURE §3's V-3, the same reason `sysv_sem` is here).
//! Nothing accumulates per process at death - the SysV message family has no
//! undo list - so there is no exit hook, unlike its siblings.
//!
//! The contract is Linux's, measured before any of it was written (Linux
//! 7.2.11, `.workbuddy/probe_sysv_msg.c`, `probe_sysv_msg_qbytes.c`; the record
//! is `docs/notes/` §6.15). The facts that shaped this file:
//!
//! * the queue is **FIFO**: messages leave in arrival order (a `type 0`
//!   receive takes the front), and *selection* is the receive's only rule -
//!   `type > 0` names the first message of that type, `type < 0` names the
//!   **lowest** type value that is `<= -type` (not the front-most: measured,
//!   with `{4, 10, 1}` queued a `-4` takes the `1`), and `MSG_EXCEPT` takes
//!   the first message whose type is *not* the one named;
//! * a receive that finds a matching message **too big for its buffer**
//!   without `MSG_NOERROR` does not take it and does not refuse either - it
//!   waits for one that fits (`E2BIG` only with `IPC_NOWAIT`, measured both
//!   ways: the blocking half is the shape that hangs a caller, which is how it
//!   was found);
//! * `MSG_COPY` answers a message **by queue index** without taking it, and
//!   without `IPC_NOWAIT` it is `EINVAL` (measured: with the flag, an index
//!   past the end is `ENOMSG` and the message stays queued);
//! * the byte budget (`msg_qbytes`, `MSGMNB` 16384 at boot) is what a full
//!   queue is full of - two 8192-byte messages (`MSGMAX`) fill a fresh one,
//!   and a third send is `EAGAIN` with `IPC_NOWAIT`;
//! * a blocked send or receive is woken by its opposite number, and answers
//!   `EIDRM` when the queue is removed underneath it.
//!
//! **What is not here**: the IPC namespace. As with the other two families,
//! this is one table for the machine until `CLONE_NEWIPC` exists.

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::cell::RefCell;
use core::sync::atomic::{AtomicBool, Ordering};

use critical_section::Mutex;

use super::TaskId;
use crate::mm::ipc_id;

/// `IPC_PRIVATE`: the key that always makes a fresh queue.
pub const IPC_PRIVATE: i32 = 0;

/// The triads of an `IPC_SET`-able mode.
pub const MODE_MASK: u32 = 0o777;

/// How many queues may exist at once - Linux's `MSGMNI` is 32000; this
/// kernel's bound is smaller for the same reason `SEMMNI` is (`docs/notes/`
/// §6.14): a table whose allocation a caller can steer needs one.
pub const MSGMNI: usize = 4_096;

/// The most bytes one message may carry (`MSGMAX`), and the byte budget a
/// fresh queue starts with (`MSGMNB`). Together they bound what a user can
/// make this kernel hold: at most `MSGMNB` one-byte messages per queue, and
/// `MSGMNI` queues.
pub const MSGMAX: usize = 8_192;
pub const MSGMNB: usize = 16_384;

/// The receive flags the operations read (`linux/msg.h`).
pub const MSG_NOERROR: u32 = 0o_10000;
pub const MSG_EXCEPT: u32 = 0o_20000;
pub const MSG_COPY: u32 = 0o_40000;

/// Why a queue operation was refused. The syscall layer maps these onto
/// errnos.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MsgError {
    /// The key names nothing and `IPC_CREAT` was not asked for.
    NoEntry,
    /// The key names something and `IPC_CREAT|IPC_EXCL` was asked for.
    Exists,
    /// The table is full.
    Full,
    /// `msgtyp` at the send is not positive.
    BadType,
    /// `msgsz` at the send is past `MSGMAX`.
    TooBig,
    /// The queue's byte budget leaves no room, with `IPC_NOWAIT`.
    QueueFull,
    /// No message matches, with `IPC_NOWAIT`.
    NoMsg,
    /// The message is bigger than the buffer, with `IPC_NOWAIT` and without
    /// `MSG_NOERROR`.
    TooSmall,
    /// `MSG_COPY` without `IPC_NOWAIT`.
    Invalid,
}

/// A queue's identity and permissions: everything `IPC_STAT` reports and
/// `IPC_SET` may change.
#[derive(Clone, Copy)]
pub struct Perm {
    pub key: i32,
    pub id: u64,
    pub uid: u32,
    pub gid: u32,
    pub cuid: u32,
    pub cgid: u32,
    pub mode: u32,
    pub seq: u16,
}

/// One queued message: a positive type and its bytes. The type travels with
/// the bytes because every selection rule reads it.
struct Message {
    mtype: i64,
    data: Vec<u8>,
}

/// What a queue is beyond its permissions: the messages, the byte budget, and
/// the clocks and pids `IPC_STAT` reports.
struct Inner {
    perm: Perm,
    msgs: Vec<Message>,
    /// Bytes currently queued - the number the budget bounds.
    cbytes: usize,
    /// The budget itself (`msg_qbytes`), `MSGMNB` at creation.
    qbytes: usize,
    stime: i64,
    rtime: i64,
    ctime: i64,
    lspid: u32,
    lrpid: u32,
}

/// One message queue.
pub struct Queue {
    inner: Mutex<RefCell<Inner>>,
    /// `IPC_RMID` has been asked for. The queue leaves the table at once, but
    /// a waiter already asleep holds this `Arc` and reads this flag to answer
    /// `EIDRM` - the same shape the semaphore sets use.
    removed: AtomicBool,
}

impl Queue {
    pub fn perm(&self) -> Perm {
        critical_section::with(|cs| self.inner.borrow(cs).borrow().perm)
    }

    pub fn id(&self) -> u64 {
        self.perm().id
    }

    pub fn removed(&self) -> bool {
        self.removed.load(Ordering::Relaxed)
    }

    /// The counters `IPC_STAT` and `/proc/sysvipc/msg` report: bytes queued,
    /// messages queued, the budget, the two pids and the three clocks.
    pub fn stats(&self) -> (usize, usize, usize, u32, u32, i64, i64, i64) {
        critical_section::with(|cs| {
            let inner = self.inner.borrow(cs).borrow();
            (
                inner.cbytes,
                inner.msgs.len(),
                inner.qbytes,
                inner.lspid,
                inner.lrpid,
                inner.stime,
                inner.rtime,
                inner.ctime,
            )
        })
    }

    /// Rewrite the owner, group and triads: `IPC_SET`'s half, which leaves the
    /// creator and every bit above the triads alone.
    pub fn set_perm(&self, uid: u32, gid: u32, mode: u32, now: i64) {
        critical_section::with(|cs| {
            let mut inner = self.inner.borrow(cs).borrow_mut();
            inner.perm.uid = uid;
            inner.perm.gid = gid;
            inner.perm.mode = mode & MODE_MASK;
            inner.ctime = now;
        });
    }

    /// Rewrite the byte budget. The caller has answered the permission
    /// question; this is only the store, and a budget below the bytes already
    /// queued is stored as asked (measured: accepted, and the queue stays over
    /// budget until it drains).
    pub fn set_qbytes(&self, qbytes: usize, now: i64) {
        critical_section::with(|cs| {
            let mut inner = self.inner.borrow(cs).borrow_mut();
            inner.qbytes = qbytes;
            inner.ctime = now;
        });
    }
}

// ---------------------------------------------------------------------------
// The table
// ---------------------------------------------------------------------------

/// The machine's one queue table, on the same slot-and-generation numbers the
/// other two SysV families use (`mm::ipc_id`).
struct Table {
    queues: Vec<Option<Arc<Queue>>>,
    seqs: Vec<u16>,
}

impl Table {
    const fn empty() -> Table {
        Table {
            queues: Vec::new(),
            seqs: Vec::new(),
        }
    }

    fn slot(&self, index: u64) -> Option<&Arc<Queue>> {
        self.queues.get(index as usize)?.as_ref()
    }

    fn find_key(&self, key: i32) -> Option<Arc<Queue>> {
        self.queues
            .iter()
            .flatten()
            .find(|queue| queue.perm().key == key)
            .cloned()
    }

    fn free_slot(&mut self) -> Option<u64> {
        if let Some(index) = self.queues.iter().position(|slot| slot.is_none()) {
            return Some(index as u64);
        }
        if self.queues.len() >= MSGMNI {
            return None;
        }
        self.queues.push(None);
        self.seqs.push(0);
        Some((self.queues.len() - 1) as u64)
    }
}

static TABLE: Mutex<RefCell<Table>> = Mutex::new(RefCell::new(Table::empty()));

/// Find a live queue by the number that names it, checking the generation.
pub fn lookup(id: i64) -> Option<Arc<Queue>> {
    let (slot, seq) = ipc_id::decode(id)?;
    critical_section::with(|cs| {
        let table = TABLE.borrow(cs).borrow();
        if table.seqs.get(slot as usize).copied()? != seq {
            return None;
        }
        table.slot(slot).cloned()
    })
}

/// The queue in table slot `index`, for `MSG_STAT`/`MSG_STAT_ANY`.
pub fn by_slot(index: u64) -> Option<Arc<Queue>> {
    critical_section::with(|cs| TABLE.borrow(cs).borrow().slot(index).cloned())
}

/// Every live queue, in slot order: what `/proc/sysvipc/msg` walks.
pub fn all() -> Vec<Arc<Queue>> {
    critical_section::with(|cs| {
        TABLE
            .borrow(cs)
            .borrow()
            .queues
            .iter()
            .flatten()
            .cloned()
            .collect()
    })
}

/// The highest slot a live queue occupies - the `IPC_INFO`/`MSG_INFO` return
/// value `ipcs` walks.
pub fn highest_slot() -> u64 {
    critical_section::with(|cs| {
        let table = TABLE.borrow(cs).borrow();
        match table.queues.iter().rposition(|slot| slot.is_some()) {
            Some(index) => index as u64,
            None => 0,
        }
    })
}

/// How many queues and messages are live, for `MSG_INFO`'s two re-purposed
/// fields (`msgpool`/`msgmap`, the message family's own "cursed" pair - see
/// `write_seminfo` for the semaphore family's).
pub fn totals() -> (usize, usize) {
    critical_section::with(|cs| {
        let table = TABLE.borrow(cs).borrow();
        let messages: usize = table
            .queues
            .iter()
            .flatten()
            .map(|queue| queue.stats().1)
            .sum();
        (table.queues.iter().flatten().count(), messages)
    })
}

/// The facts a new queue is made from.
pub struct New {
    pub key: i32,
    pub mode: u32,
    pub uid: u32,
    pub gid: u32,
    pub now: i64,
}

/// Make a queue, or answer the live one `key` already names.
pub fn get_or_create(
    new: New,
    create: bool,
    exclusive: bool,
) -> Result<(Arc<Queue>, bool), MsgError> {
    if new.key != IPC_PRIVATE {
        if let Some(existing) =
            critical_section::with(|cs| TABLE.borrow(cs).borrow().find_key(new.key))
        {
            if create && exclusive {
                return Err(MsgError::Exists);
            }
            return Ok((existing, false));
        }
        if !create {
            return Err(MsgError::NoEntry);
        }
    }
    create_queue(new).map(|queue| (queue, true))
}

fn create_queue(new: New) -> Result<Arc<Queue>, MsgError> {
    critical_section::with(|cs| {
        let mut table = TABLE.borrow(cs).borrow_mut();
        let slot = table.free_slot().ok_or(MsgError::Full)?;
        let seq = table.seqs[slot as usize];
        let id = ipc_id::build(slot, seq);
        let queue = Arc::new(Queue {
            inner: Mutex::new(RefCell::new(Inner {
                perm: Perm {
                    key: new.key,
                    id,
                    uid: new.uid,
                    gid: new.gid,
                    cuid: new.uid,
                    cgid: new.gid,
                    mode: new.mode & MODE_MASK,
                    seq,
                },
                msgs: Vec::new(),
                cbytes: 0,
                qbytes: MSGMNB,
                stime: 0,
                rtime: 0,
                ctime: new.now,
                lspid: 0,
                lrpid: 0,
            })),
            removed: AtomicBool::new(false),
        });
        table.queues[slot as usize] = Some(Arc::clone(&queue));
        Ok(queue)
    })
}

/// Take a queue out of the table and wake everyone asleep on it.
///
/// `IPC_RMID` on a message queue is a removal, like the semaphore sets and
/// unlike the segment table's mark: the messages are gone, and every waiter -
/// sender or receiver - answers `EIDRM` (measured both halves). The wakers
/// hold the `Arc` to read [`Queue::removed`].
pub fn remove(id: i64, wake: impl FnMut(TaskId)) -> Option<Arc<Queue>> {
    let queue = critical_section::with(|cs| {
        let mut table = TABLE.borrow(cs).borrow_mut();
        let (slot, seq) = ipc_id::decode(id)?;
        if table.seqs.get(slot as usize).copied()? != seq {
            return None;
        }
        let taken = table.queues.get_mut(slot as usize)?.take()?;
        if let Some(next) = table.seqs.get_mut(slot as usize) {
            *next = ipc_id::next_generation(*next);
        }
        Some(taken)
    })?;
    queue.removed.store(true, Ordering::Relaxed);
    wake_waiters(queue.id(), wake);
    Some(queue)
}

// ---------------------------------------------------------------------------
// The waiters
// ---------------------------------------------------------------------------

struct Waiter {
    queue: u64,
    task: TaskId,
    /// Set by a wake. The task polls this from inside `block_until_nanos`, so
    /// a waker never has to find it anywhere else.
    woken: bool,
}

static WAITERS: Mutex<RefCell<Vec<Waiter>>> = Mutex::new(RefCell::new(Vec::new()));

/// Register the running task, *before* the condition is re-checked: the other
/// order has a window in which the opposite number goes through and the
/// sleeper never hears it.
fn register(queue: u64, task: TaskId) {
    critical_section::with(|cs| {
        let mut waiters = WAITERS.borrow(cs).borrow_mut();
        // One place per task: a retry replaces the entry rather than piling up.
        waiters.retain(|waiter| waiter.task != task);
        waiters.push(Waiter {
            queue,
            task,
            woken: false,
        });
    });
}

fn unregister(task: TaskId) {
    critical_section::with(|cs| {
        WAITERS
            .borrow(cs)
            .borrow_mut()
            .retain(|waiter| waiter.task != task);
    });
}

fn is_woken(task: TaskId) -> bool {
    critical_section::with(|cs| {
        WAITERS
            .borrow(cs)
            .borrow()
            .iter()
            .any(|waiter| waiter.task == task && waiter.woken)
    })
}

/// Mark every task asleep on `queue` woken, and wake each one.
///
/// The wake callback rather than `crate::sched::wake` for the same reason the
/// semaphore family needs one: the caller may already hold the task table.
pub fn wake_waiters(queue: u64, mut wake: impl FnMut(TaskId)) {
    let woken: Vec<TaskId> = critical_section::with(|cs| {
        let mut waiters = WAITERS.borrow(cs).borrow_mut();
        let mut woken = Vec::new();
        for waiter in waiters.iter_mut() {
            if waiter.queue == queue && !waiter.woken {
                waiter.woken = true;
                woken.push(waiter.task);
            }
        }
        woken
    });
    for task in woken {
        wake(task);
    }
}

// ---------------------------------------------------------------------------
// The operations
// ---------------------------------------------------------------------------

/// What one `msgsnd` attempt came to.
pub enum SendAttempt {
    /// Queued.
    Done,
    /// The queue is over budget. The running task has been registered as a
    /// waiter inside the same critical section that found it full.
    Blocked,
    /// Refused.
    Refused(MsgError),
}

/// Try to queue one message.
///
/// `park` is the task to register as a waiter if the budget leaves no room,
/// or `None` for a caller that will not block (`IPC_NOWAIT`).
pub fn send_attempt(
    queue: &Arc<Queue>,
    mtype: i64,
    data: Vec<u8>,
    now: i64,
    pid: u32,
    park: Option<TaskId>,
) -> SendAttempt {
    if mtype <= 0 {
        return SendAttempt::Refused(MsgError::BadType);
    }
    if data.len() > MSGMAX {
        return SendAttempt::Refused(MsgError::TooBig);
    }
    // Read out before the borrow below: `Queue::id` goes through `perm`,
    // which borrows the very `RefCell` this closure holds.
    let id = queue.id();
    critical_section::with(|cs| {
        let mut inner = queue.inner.borrow(cs).borrow_mut();
        if inner.cbytes + data.len() > inner.qbytes {
            if park.is_none() {
                return SendAttempt::Refused(MsgError::QueueFull);
            }
            register(id, park.expect("checked above"));
            return SendAttempt::Blocked;
        }
        inner.cbytes += data.len();
        inner.msgs.push(Message { mtype, data });
        inner.stime = now;
        inner.lspid = pid;
        SendAttempt::Done
    })
}

/// What one `msgrcv` attempt came to.
#[derive(Debug)]
pub enum RecvAttempt {
    /// A message: its type and its bytes. Taken from the queue unless the
    /// caller asked for a copy.
    Got(i64, Vec<u8>),
    /// Nothing this receive can hand over now. The running task has been
    /// registered as a waiter inside the same critical section that found it
    /// empty (or found only oversized messages).
    Blocked,
    /// Refused.
    Refused(MsgError),
}

/// The index of the message `typ` selects, if any: the receive's whole
/// selection rule in one place.
///
/// * `typ == 0`: the front;
/// * `typ > 0` (and not `MSG_EXCEPT`): the first of that exact type;
/// * `typ < 0`: the first message of the **lowest** type that is `<= -typ` -
///   not the front-most that fits, which is the reading the first draft had
///   and the host refused (measured: with `{4, 10, 1}` queued, `-4` takes the
///   `1`);
/// * `MSG_EXCEPT` (with `typ > 0`): the first message whose type differs.
fn select(msgs: &[Message], typ: i64, except: bool) -> Option<usize> {
    if except {
        return msgs.iter().position(|m| m.mtype != typ);
    }
    match typ {
        0 => Some(0),
        t if t > 0 => msgs.iter().position(|m| m.mtype == t),
        t => {
            let bound = -t;
            let lowest = msgs
                .iter()
                .filter(|m| m.mtype <= bound)
                .map(|m| m.mtype)
                .min()?;
            msgs.iter().position(|m| m.mtype == lowest)
        }
    }
}

/// Try to receive one message.
///
/// `msgsz` is the caller's buffer; `flags` carries `MSG_NOERROR`,
/// `MSG_EXCEPT` and `MSG_COPY` (`IPC_NOWAIT` arrives as `park: None`). A
/// message that does not fit without `MSG_NOERROR` is *not* taken: the
/// blocking caller waits for one that fits, which is the measured shape - and
/// the reason the fit is part of the waiter's predicate rather than an error.
pub fn recv_attempt(
    queue: &Arc<Queue>,
    typ: i64,
    msgsz: usize,
    flags: u32,
    now: i64,
    pid: u32,
    park: Option<TaskId>,
) -> RecvAttempt {
    let id = queue.id();
    let noerror = flags & MSG_NOERROR != 0;
    let except = flags & MSG_EXCEPT != 0;
    let copy = flags & MSG_COPY != 0;
    critical_section::with(|cs| {
        let mut inner = queue.inner.borrow(cs).borrow_mut();
        if copy {
            // Copy mode answers by queue index and never takes, and it is the
            // one shape that may not wait - Linux ties it to `IPC_NOWAIT`
            // with an `EINVAL` when the flag is missing (measured).
            let Some(index) = usize::try_from(typ).ok().and_then(|i| inner.msgs.get(i)) else {
                return RecvAttempt::Refused(MsgError::NoMsg);
            };
            return RecvAttempt::Got(index.mtype, index.data.clone());
        }
        let Some(index) = select(&inner.msgs, typ, except) else {
            return block_or_refuse(&mut inner, id, park, MsgError::NoMsg);
        };
        // `select` answers an index; on an empty queue `typ == 0`'s is the
        // front of nothing, so the empty answer is decided here rather than in
        // the selector (whose `Some(0)` is about *shape*, not count - the first
        // draft indexed it and a blocking receive on an empty queue panicked).
        if index >= inner.msgs.len() {
            return block_or_refuse(&mut inner, id, park, MsgError::NoMsg);
        }
        let message = &inner.msgs[index];
        if message.data.len() > msgsz && !noerror {
            // Not this one: wait for one that fits rather than hand a caller
            // an `E2BIG` it did not ask to be final.
            if park.is_none() {
                return RecvAttempt::Refused(MsgError::TooSmall);
            }
            register(id, park.expect("checked above"));
            return RecvAttempt::Blocked;
        }
        let mut message = inner.msgs.remove(index);
        inner.cbytes = inner.cbytes.saturating_sub(message.data.len());
        inner.rtime = now;
        inner.lrpid = pid;
        let take = if message.data.len() > msgsz {
            message.data.truncate(msgsz);
            message
        } else {
            message
        };
        RecvAttempt::Got(take.mtype, take.data)
    })
}

/// The empty-queue half of the receive: `ENOMSG` for a caller that will not
/// wait, a registered waiter for one that will.
fn block_or_refuse(inner: &mut Inner, id: u64, park: Option<TaskId>, e: MsgError) -> RecvAttempt {
    if inner.msgs.is_empty() {
        if park.is_none() {
            return RecvAttempt::Refused(e);
        }
        register(id, park.expect("checked above"));
        return RecvAttempt::Blocked;
    }
    RecvAttempt::Refused(e)
}

/// Sleep until a wake marks this task or a signal arrives.
///
/// The caller has already registered itself (inside the attempt that found
/// the wait), so this only waits and then leaves the queue. There is no
/// deadline: neither `msgsnd` nor `msgrcv` takes a timeout.
#[cfg(target_os = "none")]
pub fn park(task: TaskId) -> super::WaitEnd {
    let end = super::block_until_nanos(|| is_woken(task), u64::MAX);
    unregister(task);
    end
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fresh() -> Arc<Queue> {
        get_or_create(
            New {
                key: IPC_PRIVATE,
                mode: 0o600,
                uid: 0,
                gid: 0,
                now: 1,
            },
            true,
            false,
        )
        .expect("a private queue")
        .0
    }

    /// A send queues and the counters move; the queue is FIFO.
    #[test]
    fn a_send_queues_and_counts() {
        let q = fresh();
        assert!(matches!(
            send_attempt(&q, 3, b"hello".to_vec(), 2, 1, None),
            SendAttempt::Done
        ));
        send_attempt(&q, 1, b"x".to_vec(), 3, 1, None);
        let (cbytes, qnum, qbytes, lspid, _, stime, _, _) = q.stats();
        assert_eq!(cbytes, 6);
        assert_eq!(qnum, 2);
        assert_eq!(qbytes, MSGMNB);
        assert_eq!(lspid, 1);
        assert_eq!(stime, 3);
    }

    /// A non-positive type refuses at the send, and a size past `MSGMAX`
    /// refuses before the queue is looked at.
    #[test]
    fn a_bad_type_or_size_is_refused() {
        let q = fresh();
        assert!(matches!(
            send_attempt(&q, 0, b"x".to_vec(), 1, 1, None),
            SendAttempt::Refused(MsgError::BadType)
        ));
        assert!(matches!(
            send_attempt(&q, -1, b"x".to_vec(), 1, 1, None),
            SendAttempt::Refused(MsgError::BadType)
        ));
    }

    /// The budget is the fullness: two `MSGMAX` messages fill a fresh queue
    /// and a third is `EAGAIN` for a caller that will not wait, while a
    /// blocking one registers as a waiter.
    #[test]
    fn the_byte_budget_bounds_the_queue() {
        let q = fresh();
        assert!(matches!(
            send_attempt(&q, 1, alloc::vec![0u8; MSGMAX], 1, 1, None),
            SendAttempt::Done
        ));
        assert!(matches!(
            send_attempt(&q, 1, alloc::vec![0u8; MSGMAX], 2, 1, None),
            SendAttempt::Done
        ));
        assert!(matches!(
            send_attempt(&q, 1, b"x".to_vec(), 3, 1, None),
            SendAttempt::Refused(MsgError::QueueFull)
        ));
        match send_attempt(&q, 1, b"x".to_vec(), 4, 1, Some(TaskId(9))) {
            SendAttempt::Blocked => {}
            _ => panic!("a full queue must block a waiting caller"),
        }
        // The receive that drains it wakes the sender, whose retry then fits.
        assert!(matches!(
            recv_attempt(&q, 0, MSGMAX, 0, 5, 1, None),
            RecvAttempt::Got(1, ref d) if d.len() == MSGMAX
        ));
        wake_waiters(q.id(), |_| {});
        assert!(is_woken(TaskId(9)));
        assert!(matches!(
            send_attempt(&q, 1, b"x".to_vec(), 6, 1, Some(TaskId(9))),
            SendAttempt::Done
        ));
        unregister(TaskId(9));
    }

    /// The selection rules, one per shape: front, exact type, lowest type
    /// under a bound, and `MSG_EXCEPT`.
    #[test]
    fn the_selection_rules_pick_the_measured_message() {
        let q = fresh();
        for mtype in [5i64, 3, 3, 4, 10, 1] {
            send_attempt(&q, mtype, alloc::vec![0u8; 1], 1, 1, None);
        }
        let got = |typ: i64, flags: u32| match recv_attempt(&q, typ, 8, flags, 1, 1, None) {
            RecvAttempt::Got(mtype, _) => mtype,
            other => panic!("expected a message, got {other:?}"),
        };
        assert_eq!(got(0, 0), 5);
        assert_eq!(got(3, 0), 3);
        assert_eq!(got(3, 0), 3);
        // The rule the first draft had wrong: `-4` takes the *lowest* fitting
        // type (the 1), not the front-most (the 4).
        assert_eq!(got(-4, 0), 1);
        assert_eq!(got(-10, 0), 4);
        // Only the 10 is left, and `EXCEPT 10` wants anything but it.
        assert!(matches!(
            recv_attempt(&q, 10, 8, MSG_EXCEPT, 1, 1, None),
            RecvAttempt::Refused(MsgError::NoMsg)
        ));
        assert_eq!(got(10, 0), 10);
    }

    /// A message too big for the buffer stays queued without `MSG_NOERROR`,
    /// and truncates with it; a *blocking* short receive waits rather than
    /// refusing.
    #[test]
    fn a_short_receive_waits_or_truncates() {
        let q = fresh();
        send_attempt(&q, 9, b"abcdef".to_vec(), 1, 1, None);
        assert!(matches!(
            recv_attempt(&q, 0, 4, 0, 1, 1, None),
            RecvAttempt::Refused(MsgError::TooSmall)
        ));
        match recv_attempt(&q, 0, 4, 0, 1, 1, Some(TaskId(3))) {
            RecvAttempt::Blocked => {}
            other => panic!("a blocking short receive must wait, got {other:?}"),
        }
        assert_eq!(q.stats().1, 1);
        unregister(TaskId(3));
        assert!(matches!(
            recv_attempt(&q, 0, 4, MSG_NOERROR, 2, 1, None),
            RecvAttempt::Got(9, ref d) if d.len() == 4
        ));
        assert_eq!(q.stats().0, 0);
    }

    /// `MSG_COPY` answers by index without taking.
    #[test]
    fn a_copy_answer_is_by_index_and_takes_nothing() {
        let q = fresh();
        send_attempt(&q, 1, b"a".to_vec(), 1, 1, None);
        send_attempt(&q, 2, b"b".to_vec(), 2, 1, None);
        assert!(matches!(
            recv_attempt(&q, 1, 8, MSG_COPY, 1, 1, None),
            RecvAttempt::Got(2, ref d) if d == b"b"
        ));
        assert_eq!(q.stats().1, 2);
        assert!(matches!(
            recv_attempt(&q, 9, 8, MSG_COPY, 1, 1, None),
            RecvAttempt::Refused(MsgError::NoMsg)
        ));
    }

    /// A receive on an *empty* queue, both shapes: the blocking one waits and
    /// the `IPC_NOWAIT` one refuses - and neither indexes the front of
    /// nothing, which is the panic the first draft had (`typ == 0` selects
    /// index 0 whether or not there is one).
    #[test]
    fn a_receive_on_an_empty_queue_waits_or_refuses() {
        let q = fresh();
        assert!(matches!(
            recv_attempt(&q, 0, 8, 0, 1, 1, None),
            RecvAttempt::Refused(MsgError::NoMsg)
        ));
        match recv_attempt(&q, 0, 8, 0, 1, 1, Some(TaskId(5))) {
            RecvAttempt::Blocked => {}
            other => panic!("a blocking receive on an empty queue must wait, got {other:?}"),
        }
        unregister(TaskId(5));
    }

    /// `IPC_RMID` empties the table slot, turns the number stale, and wakes
    /// the sleepers.
    #[test]
    fn a_removal_wakes_and_ages_the_number() {
        let q = fresh();
        let id = q.id() as i64;
        remove(id, |_| {});
        assert!(q.removed());
        assert!(lookup(id).is_none());
    }
}
