//! Pipes: a byte channel with the semantics of `|`.
//!
//! A pipe is two [`PipeEnd`]s over one shared buffer. It is an anonymous inode:
//! nothing names it, and the only way to reach it is through the fd the creator
//! was handed, which is what makes it a channel rather than a file.
//!
//! The non-blocking core - [`PipeEnd::read_once`] and [`PipeEnd::write_once`] -
//! is plain code over a [`Ring`] and runs under `cargo test` on the host. The
//! blocking `Inode` impl wraps it in [`crate::sched::block_until`], which is a
//! kernel-only facility; on the host those paths panic loudly rather than spin.
//!
//! EOF and `EPIPE` fall out of the end counters. The last writer dropping makes
//! reads see end-of-stream (`Ok(0)`); the last reader dropping makes writes see
//! [`VfsError::BrokenPipe`]. Both are discovered by whoever is blocked on the
//! other end, which is why every end registers its waiters: a drop does not
//! know which task to wake, but the pipe does.

use alloc::collections::VecDeque;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::cell::RefCell;
use core::sync::atomic::{AtomicBool, Ordering};

use critical_section::Mutex;

use super::{Ino, Inode, Metadata, NodeKind, Ready, VfsError, VfsResult};
use crate::sched::policy::TaskId;

/// Writes up to this many bytes never split across a blocking point, which is
/// what lets two processes use a pipe as a record channel. POSIX's minimum.
pub const PIPE_BUF: usize = 4096;

/// How much a pipe buffers before writers block. Sixteen pages is Linux's
/// default pipe capacity (`pipe-max-size`), so a pipeline holds exactly as much
/// in flight here as it would there. (A writer that needs a whole record of
/// room parks until one frees - see [`PipeEnd::wait_writable`] - so the buffer
/// being an exact multiple of [`PIPE_BUF`] is not the stall it once was.)
const CAPACITY: usize = 16 * PIPE_BUF;

/// The bytes in flight through one pipe, as a ring over a `Vec`.
///
/// The live bytes are `bytes[start..]`: the storage is taken lazily, so a pipe
/// nobody writes to costs nothing, and it is reclaimed as the reader drains -
/// when the tail catches the head both go back to zero, so the *next* write
/// starts from an empty buffer rather than from a high-water mark.
///
/// The shape it replaces was a `VecDeque<u8>` moved one byte per operation:
/// `pop_front` in a loop for a read, `extend(iter().copied())` for a write. A
/// `PIPE_BUF` round trip therefore cost 8192 deque operations where two slice
/// copies do, and `t-pipecost` measures the difference: 4096-byte units went
/// from 259 MiB/s to over a gigabyte.
///
/// The one thing that is not a slice copy is the move on a wrap. The buffer is
/// only ever written at the tail, so a `Vec` that keeps growing while reads
/// nibble at the front is exactly the wrong shape; the dead prefix is dropped
/// with one `drain` when the arrival would otherwise pass [`CAPACITY`]. That is
/// what makes this a ring rather than a log, and it costs one memmove per
/// `CAPACITY` bytes moved - the same amortized argument a real ring makes.
#[derive(Debug)]
struct Ring {
    bytes: Vec<u8>,
    start: usize,
}

impl Ring {
    const fn new() -> Ring {
        Ring {
            bytes: Vec::new(),
            start: 0,
        }
    }

    /// Bytes waiting to be read.
    fn len(&self) -> usize {
        self.bytes.len() - self.start
    }

    fn is_empty(&self) -> bool {
        self.bytes.len() == self.start
    }

    /// Forget everything, keeping the allocation. A closed FIFO's buffer is
    /// discarded when its last end goes, so the next open starts empty - see
    /// [`PipeState::discard_buffer`].
    fn clear(&mut self) {
        self.bytes.clear();
        self.start = 0;
    }

    /// Take up to `want` bytes into `buf`, answering how many were taken.
    ///
    /// The caller bounds `want` by [`Ring::len`] and by the next handover
    /// boundary; this bounds it again rather than trusting either, because a
    /// slice copy that ran past the data would read whatever the `Vec` last
    /// held there.
    fn take(&mut self, buf: &mut [u8], want: usize) -> usize {
        let n = want.min(buf.len()).min(self.len());
        buf[..n].copy_from_slice(&self.bytes[self.start..self.start + n]);
        self.start += n;
        if self.start == self.bytes.len() {
            // Fully drained. Keeping the allocation and resetting the cursor
            // is what makes the steady state allocation-free.
            self.bytes.clear();
            self.start = 0;
        }
        n
    }

    /// Copy up to `want` bytes from the front without taking them, answering
    /// how many were copied.
    ///
    /// This is the ring's half of `MSG_PEEK`: the caller looks at what a read
    /// would have handed it and the cursor does not move. The handover
    /// boundary is deliberately not applied here - a peek copies what the
    /// caller asked to see, across send boundaries, while the descriptors a
    /// send carried stay queued for the read that consumes their bytes
    /// (Linux's `MSG_PEEK` reads across skbs and attaches the first one's
    /// control data; measured in `.workbuddy/probe_unix_peek.c`).
    fn peek(&self, buf: &mut [u8], want: usize) -> usize {
        let n = want.min(buf.len()).min(self.len());
        buf[..n].copy_from_slice(&self.bytes[self.start..self.start + n]);
        n
    }

    /// Append the first `want` bytes of `buf`. The caller has checked that the
    /// pipe has room for them.
    fn put(&mut self, buf: &[u8], want: usize) {
        let n = want.min(buf.len());
        debug_assert!(
            n <= CAPACITY - self.len(),
            "a pipe write of {n} past a {CAPACITY}-byte buffer holding {}",
            self.len()
        );
        if self.bytes.len() + n > CAPACITY {
            // Room for the arrival only exists at the front, behind the bytes
            // already read. One move, and only on the pass that needed it.
            self.bytes.drain(..self.start);
            self.start = 0;
        }
        self.bytes.extend_from_slice(&buf[..n]);
    }
}

struct PipeState {
    data: Ring,
    /// SCM_RIGHTS side channel, one entry per send that carried descriptors.
    /// It lives here - beside the bytes it travels with - because the two
    /// crossed halves of a [`DuplexEnd`] share exactly this state, so a record
    /// pushed on the write side is the record the far reader pops, in order.
    /// Each entry is the set of open files to hand over; dropping one without a
    /// reader closes them, which is Linux's rule for a descriptor never taken.
    scm: VecDeque<Scm>,
    /// Bytes ever written into `data`, and ever taken out of it. A stream has no
    /// message boundaries of its own, so these two counters are what gives a
    /// handover its place: the number a send finished at is the offset its
    /// descriptors belong to.
    written: u64,
    consumed: u64,
    readers: u32,
    writers: u32,
    /// Tasks blocked waiting for data or EOF. A dead task's stale entry is
    /// harmless: waking a reused id can only cause a spurious wakeup, and the
    /// woken task re-checks its own condition before doing anything.
    #[cfg_attr(not(target_os = "none"), allow(dead_code))]
    waiting_readers: Vec<TaskId>,
    #[cfg_attr(not(target_os = "none"), allow(dead_code))]
    waiting_writers: Vec<TaskId>,
    /// Tasks parked in a FIFO's **open-time rendezvous** - an `open(O_RDONLY)`
    /// with no writer, or an `open(O_WRONLY)` with no reader, on a name whose
    /// channel is nothing yet. Kept apart from the two above because the
    /// condition they wait on is the *count*, not the buffer: an arriving peer
    /// wakes them, and the one that arrived re-checks its own count. A plain
    /// pipe never fills this list.
    #[cfg_attr(not(target_os = "none"), allow(dead_code))]
    waiting_open: Vec<TaskId>,
}

impl PipeState {
    /// Forget the buffer, as closing the last end of a FIFO does: the bytes a
    /// gone pipeline left behind do not belong to whoever opens the name next.
    /// Linux discards a FIFO's buffer when both its read and write counts reach
    /// zero (measured, `.workbuddy/probe_fifo.c`), and the two offsets go with
    /// it so a queued SCM_RIGHTS record cannot survive either.
    fn discard_buffer(&mut self) {
        self.data.clear();
        self.scm.clear();
        self.written = 0;
        self.consumed = 0;
    }
}

/// Which directions one end of a channel is open in.
///
/// A plain pipe hands out two ends, one each way, so it only ever uses `Read`
/// and `Write`. A FIFO's `open(O_RDWR)` is the case that needs `Both`: one
/// description that is a reader *and* a writer of the same buffer, so the
/// counts it takes and gives back are two, and it is the only end whose
/// `shut_read` and `shut_write` are both real.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Role {
    Read,
    Write,
    Both,
}

impl Role {
    fn reads(self) -> bool {
        matches!(self, Role::Read | Role::Both)
    }

    fn writes(self) -> bool {
        matches!(self, Role::Write | Role::Both)
    }
}

/// One SCM_RIGHTS handover, tagged with the stream position the send that
/// carried it ended at.
///
/// Linux keeps a stream's control messages per skb and stops a `recvmsg` at the
/// skb that has one, so the descriptors arrive with the bytes they travelled
/// with. Without the offset a reader that swallowed two sends in one `read`
/// would be handed one chain and lose the other's place, which is the difference
/// between passing two sockets and passing whichever one the sender happened to
/// finish with.
struct Scm {
    end: u64,
    files: Vec<Arc<super::File>>,
}

/// One end of a pipe, as an inode. Reads and writes ignore their offset: a
/// pipe is a stream, and the cursor an fd keeps is meaningless for it.
pub struct PipeEnd {
    state: Arc<Mutex<RefCell<PipeState>>>,
    /// Which directions this end is open in - what its `read`/`write` will do,
    /// which count it took on the way in, and which count its drop gives back.
    role: Role,
    /// Set when this end has left the count, by a half-close or by its own
    /// drop - whichever came first. Both have to be idempotent, since a
    /// `shutdown` can be asked for twice and the drop always follows it. Two
    /// bits rather than one, because a `Role::Both` end (a FIFO opened
    /// `O_RDWR`) leaves two counts and each may be half-closed on its own.
    closed_read: AtomicBool,
    closed_write: AtomicBool,
    /// `O_NONBLOCK`, as `pipe2(2)` is born with it and `fcntl(F_SETFL)` sets
    /// it: an operation that would wait answers `EAGAIN` instead. It lives
    /// here rather than in the descriptor, because the wait is here - testing
    /// it beside the state it would wait on is what makes the answer
    /// race-free, where a descriptor checked beforehand could have the data
    /// it saw taken by another reader before its own turn comes.
    nonblocking: AtomicBool,
    /// The node kind `stat` reports for a description of this end. `File` for an
    /// anonymous pipe; `Fifo` for one opened through a FIFO's name, whose
    /// description Linux reports as `S_IFIFO` like the name itself.
    kind: NodeKind,
    /// The inode a FIFO's end reports, so `fstat` of an opened FIFO names the
    /// node the name does. Zero for an anonymous pipe, which has none.
    ino: Ino,
}

/// POSIX's floor for record writes, held at compile time, and a buffer that is
/// a whole number of records.
const _: () = {
    assert!(PIPE_BUF >= 512);
    assert!(CAPACITY.is_multiple_of(PIPE_BUF));
};

/// A pipe: the read end and the write end of one shared buffer.
pub fn pair() -> (Arc<PipeEnd>, Arc<PipeEnd>) {
    let state = Arc::new(Mutex::new(RefCell::new(PipeState {
        data: Ring::new(),
        scm: VecDeque::new(),
        written: 0,
        consumed: 0,
        readers: 1,
        writers: 1,
        waiting_readers: Vec::new(),
        waiting_writers: Vec::new(),
        waiting_open: Vec::new(),
    })));
    let end = |role: Role| {
        Arc::new(PipeEnd {
            state: state.clone(),
            role,
            closed_read: AtomicBool::new(false),
            closed_write: AtomicBool::new(false),
            nonblocking: AtomicBool::new(false),
            kind: NodeKind::File,
            ino: 0,
        })
    };
    (end(Role::Read), end(Role::Write))
}

/// The channel behind a **FIFO**: one buffer that *opens* may join, rather than
/// a `pair` born with both ends already in hand.
///
/// The difference that makes it a channel and not a pipe is the rendezvous at
/// `open(2)`. A pipe's two ends exist the moment `pipe(2)` returns; a FIFO is a
/// *name* whose ends are opened one at a time, by processes that may never have
/// met, and Linux parks an `open` until the other side shows up. The counts that
/// make that possible are the same two the pipe already keeps - `readers` and
/// `writers` - which is why an end minted here is a plain [`PipeEnd`]: it takes
/// its count on the way in, gives it back on drop, and its reads and writes see
/// the same EOF and `EPIPE` a pipe's do.
pub struct Channel {
    state: Arc<Mutex<RefCell<PipeState>>>,
    kind: NodeKind,
    ino: Ino,
}

impl Channel {
    /// A FIFO nobody has open: both counts start at zero, so the first `open`
    /// is the one that finds no peer and may park.
    pub fn new(kind: NodeKind, ino: Ino) -> Channel {
        Channel {
            state: Arc::new(Mutex::new(RefCell::new(PipeState {
                data: Ring::new(),
                scm: VecDeque::new(),
                written: 0,
                consumed: 0,
                readers: 0,
                writers: 0,
                waiting_readers: Vec::new(),
                waiting_writers: Vec::new(),
                waiting_open: Vec::new(),
            }))),
            kind,
            ino,
        }
    }

    /// Open one end of the FIFO, running Linux's rendezvous.
    ///
    /// Measured on a current host through `syscall(2)` (`.workbuddy/probe_fifo.c`,
    /// Linux 7.2.9), and the whole shape is in `docs/uapi/` §5.2:
    /// `open(O_RDONLY)` with no writer and `open(O_WRONLY)` with no reader both
    /// **park** until a peer opens the other end; `open(O_RDWR)` does not park
    /// (it is its own peer); `O_RDONLY|O_NONBLOCK` returns at once; and
    /// `O_WRONLY|O_NONBLOCK` with no reader is **`ENXIO`**. A reader parked
    /// here counts as a reader, so the writer that arrives finds `readers > 0`
    /// and returns at once - which is how the arriving side releases the parked
    /// one rather than deadlocking behind it.
    pub fn open(&self, role: Role, nonblocking: bool) -> VfsResult<Arc<PipeEnd>> {
        // Take the count(s) first: a parked reader has to count as a reader for
        // the peer that arrives to see it, which is Linux's rule and the reason
        // the two sides do not deadlock.
        critical_section::with(|cs| {
            let mut st = self.state.borrow(cs).borrow_mut();
            if role.reads() {
                st.readers += 1;
            }
            if role.writes() {
                st.writers += 1;
            }
        });
        // A peer may already be here (the count went up), so tell anyone parked
        // to re-check; the arriving side is released by the same news.
        self.wake_open();

        match self.rendezvous(role, nonblocking) {
            Rendezvous::Ready => {}
            outcome => {
                // Give the count back and answer. `Refused` is `O_WRONLY|
                // O_NONBLOCK` with no reader (`ENXIO`); `Interrupted` is a
                // signal that ended the park, which Linux answers `EINTR` - and
                // the count has to go back or the FIFO would remember a peer
                // that never arrived. That give-back is what the rendezvous's
                // own wait is chosen for: it runs for a *fatal* signal too, so
                // this line is reached before the task dies rather than skipped
                // by a death inside the park (`Channel::rendezvous`).
                critical_section::with(|cs| {
                    let mut st = self.state.borrow(cs).borrow_mut();
                    if role.reads() {
                        st.readers -= 1;
                    }
                    if role.writes() {
                        st.writers -= 1;
                    }
                });
                return Err(match outcome {
                    Rendezvous::Interrupted => VfsError::Interrupted,
                    _ => VfsError::NoDevice,
                });
            }
        }

        Ok(Arc::new(PipeEnd {
            state: self.state.clone(),
            role,
            closed_read: AtomicBool::new(false),
            closed_write: AtomicBool::new(false),
            nonblocking: AtomicBool::new(nonblocking),
            kind: self.kind,
            ino: self.ino,
        }))
    }

    /// Wait until this open's peer condition holds. [`Rendezvous::Ready`] means
    /// the open may return; `Refused` is the non-blocking writer with no reader
    /// (`ENXIO`); `Interrupted` is a signal (`EINTR`).
    ///
    /// The signal is *not* consumed here - the syscall gate delivers it on the
    /// way out - which is why `EINTR` is a return value rather than a death. The
    /// wait is [`block_until_interruptible_rolling_back`] rather than the plain
    /// interruptible one, and that choice is this open's whole contract: it takes
    /// a count *before* it parks, so a death inside the park would skip the
    /// give-back and leave the FIFO remembering a peer that never arrived. The
    /// rolling-back wait ends for a **fatal** signal too and leaves it pending,
    /// so the give-back runs and the death happens at the syscall gate - Linux's
    /// own order, where `fifo_open`'s `err_rd:` label is reached because
    /// `wait_event_interruptible` answers `-ERESTARTSYS` for `SIGKILL` as well.
    /// Measured on a current host before it was built
    /// (`.workbuddy/probe_fifo_park.c`, Linux 7.2.9): a reader parked in
    /// `open(O_RDONLY)` and killed inside the park leaves `open(O_WRONLY|
    /// O_NONBLOCK)` answering `ENXIO` (errno 6) rather than a success off a
    /// reader that is gone, and a parked *writer*'s death leaves a reader
    /// reading `0` (end of stream) rather than the `EAGAIN` a live writer owes.
    ///
    /// [`block_until_interruptible_rolling_back`]:
    ///     crate::sched::block_until_interruptible_rolling_back
    fn rendezvous(&self, role: Role, nonblocking: bool) -> Rendezvous {
        // A reader needs a writer; a writer needs a reader; `Both` is its own
        // peer, so it needs nothing.
        let ready = || {
            critical_section::with(|cs| {
                let st = self.state.borrow(cs).borrow();
                (!role.reads() || st.writers > 0) && (!role.writes() || st.readers > 0)
            })
        };
        if ready() {
            return Rendezvous::Ready;
        }
        if nonblocking {
            // A non-blocking reader with no writer succeeds (its reads will see
            // end-of-stream); a non-blocking writer with no reader is `ENXIO`.
            return if role.reads() {
                Rendezvous::Ready
            } else {
                Rendezvous::Refused
            };
        }
        #[cfg(target_os = "none")]
        {
            let id = crate::sched::current();
            critical_section::with(|cs| {
                let mut st = self.state.borrow(cs).borrow_mut();
                if !st.waiting_open.contains(&id) {
                    st.waiting_open.push(id);
                }
            });
            let end = crate::sched::block_until_interruptible_rolling_back(ready);
            critical_section::with(|cs| {
                self.state
                    .borrow(cs)
                    .borrow_mut()
                    .waiting_open
                    .retain(|&w| w != id);
            });
            match end {
                crate::sched::WaitEnd::Interrupted => Rendezvous::Interrupted,
                _ => Rendezvous::Ready,
            }
        }
        #[cfg(not(target_os = "none"))]
        {
            panic!("a FIFO's open-time rendezvous needs the kernel scheduler");
        }
    }

    /// Wake every task parked in the rendezvous: a count changed, so each one's
    /// `ready` may have gone true.
    fn wake_open(&self) {
        #[cfg(target_os = "none")]
        {
            let ids =
                critical_section::with(|cs| self.state.borrow(cs).borrow().waiting_open.clone());
            for id in ids {
                crate::sched::wake(id);
            }
        }
    }
}

/// How a FIFO's open-time rendezvous ended.
#[derive(Clone, Copy)]
enum Rendezvous {
    /// The peer condition holds (or a non-blocking reader with no writer).
    Ready,
    /// A non-blocking writer with no reader: `ENXIO`.
    Refused,
    /// A catchable signal ended the park: `EINTR`.
    Interrupted,
}

impl PipeEnd {
    /// Bytes waiting to be read.
    pub fn buffered(&self) -> usize {
        critical_section::with(|cs| self.state.borrow(cs).borrow().data.len())
    }

    /// Queue one SCM_RIGHTS handover beside the bytes just written. Any end of
    /// the shared buffer can push; the peer's read half is the one that pops.
    pub fn push_scm(&self, fds: Vec<Arc<super::File>>) {
        critical_section::with(|cs| {
            let mut st = self.state.borrow(cs).borrow_mut();
            let end = st.written;
            st.scm.push_back(Scm { end, files: fds });
        });
    }

    /// The oldest handover whose bytes this side has now consumed. A queue that
    /// has one but is not yet due returns `None` rather than the next one down,
    /// which is what keeps the order honest.
    pub fn take_scm(&self) -> Option<Vec<Arc<super::File>>> {
        critical_section::with(|cs| {
            let mut st = self.state.borrow(cs).borrow_mut();
            if st.scm.front().is_some_and(|s| st.consumed >= s.end) {
                return st.scm.pop_front().map(|s| s.files);
            }
            None
        })
    }

    /// The oldest handover, cloned rather than taken - what a `MSG_PEEK`
    /// receive reports.
    ///
    /// Linux installs the descriptors a peeked send carried and leaves the
    /// record queued, so the same handover is delivered again to the read that
    /// consumes its bytes (measured: a peeked `SCM_RIGHTS` fd and the fd the
    /// following read returned were both live). Cloning the `Arc`s is what
    /// makes the peek's copies share the queued files' descriptions, which is
    /// Linux's own `get_file` shape rather than a fresh open.
    pub fn peek_scm(&self) -> Option<Vec<Arc<super::File>>> {
        critical_section::with(|cs| {
            self.state
                .borrow(cs)
                .borrow()
                .scm
                .front()
                .map(|s| s.files.clone())
        })
    }

    /// Whether [`take_scm`] would answer with something, without taking it.
    ///
    /// The read side needs this to tell two waits apart. A handover whose send
    /// carried *no* bytes ends where the stream already is, so it is due the
    /// moment it is queued and there will never be a byte to arrive with it -
    /// and a read that parked for that byte would park forever with the record
    /// already in hand. Asking "is one due?" is what lets the read answer with
    /// the record instead, while a handover that is *not* yet due still waits
    /// for its own bytes, in order.
    ///
    /// [`take_scm`]: PipeEnd::take_scm
    pub fn scm_due(&self) -> bool {
        critical_section::with(|cs| {
            let st = self.state.borrow(cs).borrow();
            st.scm.front().is_some_and(|s| st.consumed >= s.end)
        })
    }

    /// Set or clear `O_NONBLOCK` on this end. `pipe2` sets it on birth,
    /// `fcntl(F_SETFL)` on demand, and both write the same bit: the pipe's
    /// answer to "would you wait?" is the one thing it changes.
    pub fn set_nonblocking(&self, on: bool) {
        self.nonblocking.store(on, Ordering::Relaxed);
    }
    /// One non-blocking read. `Some(n)` with `n > 0` is data, `Some(0)` is
    /// end-of-stream, `None` is "nothing to read yet".
    pub fn read_once(&self, buf: &mut [u8]) -> VfsResult<Option<usize>> {
        Ok(critical_section::with(|cs| {
            let mut st = self.state.borrow(cs).borrow_mut();
            if st.data.is_empty() {
                if st.writers == 0 { Some(0) } else { None }
            } else {
                let mut n = buf.len().min(st.data.len());
                // Stop the read at the byte the next handover's send ended on,
                // so those descriptors come back with their own data and the
                // following read picks up the next send's.
                if let Some(first) = st.scm.front() {
                    let boundary = first.end.saturating_sub(st.consumed) as usize;
                    if boundary > 0 {
                        n = n.min(boundary);
                    }
                }
                let n = st.data.take(buf, n);
                st.consumed += n as u64;
                Some(n)
            }
        }))
    }

    /// One non-blocking peek: the bytes a read would hand back, copied into
    /// `buf` with the cursor left where it was. The three answers are
    /// [`read_once`]'s - `Some(n)` data, `Some(0)` end-of-stream, `None` "not
    /// yet" - because a caller waiting for something to look at waits on the
    /// same condition a reader does.
    ///
    /// [`read_once`]: PipeEnd::read_once
    pub fn peek_once(&self, buf: &mut [u8]) -> VfsResult<Option<usize>> {
        Ok(critical_section::with(|cs| {
            let st = self.state.borrow(cs).borrow();
            if st.data.is_empty() {
                if st.writers == 0 { Some(0) } else { None }
            } else {
                Some(st.data.peek(buf, buf.len()))
            }
        }))
    }

    /// One non-blocking write. `Some(n)` with `n > 0` is bytes stored, `None`
    /// is "no room yet". A write of at most [`PIPE_BUF`] bytes is all-or-
    /// nothing: it waits rather than landing in pieces. `Err` is `EPIPE`.
    pub fn write_once(&self, buf: &[u8]) -> VfsResult<Option<usize>> {
        critical_section::with(|cs| {
            let mut st = self.state.borrow(cs).borrow_mut();
            if st.readers == 0 {
                return Err(VfsError::BrokenPipe);
            }
            let free = CAPACITY - st.data.len();
            let go = if buf.len() <= PIPE_BUF {
                free >= buf.len()
            } else {
                free > 0
            };
            if !go {
                return Ok(None);
            }
            let n = buf.len().min(free);
            st.data.put(buf, n);
            st.written += n as u64;
            Ok(Some(n))
        })
    }

    /// Register the running task, block until the read side could move, then
    /// unregister. Registration lives outside `block_until` because a drop -
    /// which is the event most likely to end the wait - knows the pipe's
    /// waiters, not the scheduler's sleepers.
    ///
    /// The wait is the interruptible one, so a catchable signal aimed at the
    /// reader ends it with `VfsError::Interrupted` and nothing consumed. That
    /// is what a shell's own read of a pipe needs: its child's death or its own
    /// `SIGALRM` has to be able to bring it back to look at its loop again,
    /// rather than being slept through until the far end writes.
    #[cfg(target_os = "none")]
    fn wait_readable(&self) -> VfsResult<()> {
        let id = crate::sched::current();
        critical_section::with(|cs| {
            let mut st = self.state.borrow(cs).borrow_mut();
            if !st.waiting_readers.contains(&id) {
                st.waiting_readers.push(id);
            }
        });
        let end = crate::sched::block_until_interruptible(|| self.readable_or_ended());
        critical_section::with(|cs| {
            self.state
                .borrow(cs)
                .borrow_mut()
                .waiting_readers
                .retain(|&w| w != id);
        });
        match end {
            crate::sched::WaitEnd::Interrupted => Err(VfsError::Interrupted),
            _ => Ok(()),
        }
    }

    #[cfg(target_os = "none")]
    fn wait_writable(&self, need: usize) -> VfsResult<()> {
        let id = crate::sched::current();
        critical_section::with(|cs| {
            let mut st = self.state.borrow(cs).borrow_mut();
            if !st.waiting_writers.contains(&id) {
                st.waiting_writers.push(id);
            }
        });
        let end = crate::sched::block_until_interruptible(|| self.writable_or_broken(need));
        critical_section::with(|cs| {
            self.state
                .borrow(cs)
                .borrow_mut()
                .waiting_writers
                .retain(|&w| w != id);
        });
        match end {
            crate::sched::WaitEnd::Interrupted => Err(VfsError::Interrupted),
            _ => Ok(()),
        }
    }

    #[cfg(not(target_os = "none"))]
    fn wait_readable(&self) -> VfsResult<()> {
        panic!("blocking pipe reads need the kernel scheduler; test read_once instead");
    }

    #[cfg(not(target_os = "none"))]
    fn wait_writable(&self, _need: usize) -> VfsResult<()> {
        panic!("blocking pipe writes need the kernel scheduler; test write_once instead");
    }

    #[cfg_attr(not(target_os = "none"), allow(dead_code))]
    fn readable_or_ended(&self) -> bool {
        critical_section::with(|cs| {
            let st = self.state.borrow(cs).borrow();
            !st.data.is_empty() || st.writers == 0
        })
    }

    /// Whether the writer asking for `need` bytes can move: the far end is gone
    /// (so the next write is `EPIPE`, and the wait must end to report it), or
    /// there is room for the whole request. The second half has to match what
    /// [`write_once`](Self::write_once) actually admits - a record-sized write
    /// is all-or-nothing and wants `need` free, not merely one free byte - or a
    /// writer with a partial line waiting for room would be woken, find the
    /// write still refused, and spin instead of sleeping.
    #[cfg_attr(not(target_os = "none"), allow(dead_code))]
    fn writable_or_broken(&self, need: usize) -> bool {
        critical_section::with(|cs| {
            let st = self.state.borrow(cs).borrow();
            st.readers == 0 || CAPACITY - st.data.len() >= need
        })
    }

    /// Half-close: this end stops being a writer without releasing the buffer,
    /// so the far end's reads drain what is already there and then see
    /// end-of-stream. That is what a half-close is for - telling the peer that
    /// no more is coming, while leaving the other direction alone.
    ///
    /// Idempotent: the count may only drop once, and the [`Drop`] that follows
    /// must not drop it again.
    pub fn shut_write(&self) {
        if self.role.writes() && !self.closed_write.swap(true, Ordering::Relaxed) {
            critical_section::with(|cs| {
                let mut st = self.state.borrow(cs).borrow_mut();
                st.writers -= 1;
                if st.readers == 0 && st.writers == 0 {
                    st.discard_buffer();
                }
            });
            self.wake_all();
        }
    }

    /// Half-close the other direction: this end's reads report end-of-stream,
    /// and the peer's writes fail the way a write to a closed pipe does.
    pub fn shut_read(&self) {
        if self.role.reads() && !self.closed_read.swap(true, Ordering::Relaxed) {
            critical_section::with(|cs| {
                let mut st = self.state.borrow(cs).borrow_mut();
                st.readers -= 1;
                if st.readers == 0 && st.writers == 0 {
                    st.discard_buffer();
                }
            });
            self.wake_all();
        }
    }

    /// Wake the waiters on the other side. `reading` is the side that just
    /// made progress: a reader freed room (wake writers), a writer added data
    /// (wake readers). A no-op on the host, where nothing ever sleeps.
    fn wake_waiters(&self, reading: bool) {
        #[cfg(target_os = "none")]
        {
            let ids = critical_section::with(|cs| {
                let st = self.state.borrow(cs).borrow();
                if reading {
                    st.waiting_writers.clone()
                } else {
                    st.waiting_readers.clone()
                }
            });
            for id in ids {
                crate::sched::wake(id);
            }
        }
        #[cfg(not(target_os = "none"))]
        let _ = reading;
    }

    fn wake_all(&self) {
        self.wake_waiters(true);
        self.wake_waiters(false);
        // A count changed, so anyone parked in a FIFO's open-time rendezvous has
        // to re-check whether their peer is here now.
        #[cfg(target_os = "none")]
        {
            let ids =
                critical_section::with(|cs| self.state.borrow(cs).borrow().waiting_open.clone());
            for id in ids {
                crate::sched::wake(id);
            }
        }
    }
}

impl Drop for PipeEnd {
    fn drop(&mut self) {
        let mut changed = false;
        critical_section::with(|cs| {
            let mut st = self.state.borrow(cs).borrow_mut();
            if self.role.reads() && !self.closed_read.swap(true, Ordering::Relaxed) {
                st.readers -= 1;
                changed = true;
            }
            if self.role.writes() && !self.closed_write.swap(true, Ordering::Relaxed) {
                st.writers -= 1;
                changed = true;
            }
            // The last end of a FIFO going takes the buffer with it, so the next
            // open of the name starts empty rather than reading a dead
            // pipeline's leftovers. A pipe's ends never both reach zero here -
            // its two counts fall together only when the whole pipe is gone -
            // so the rule is a no-op for one.
            if st.readers == 0 && st.writers == 0 {
                st.discard_buffer();
            }
        });
        if changed {
            // Counts changed, so EOF/EPIPE may have become true for the other
            // end - and an open parked in the rendezvous may now have a peer.
            self.wake_all();
        }
    }
}

impl Inode for PipeEnd {
    fn as_pipe(&self) -> Option<&PipeEnd> {
        Some(self)
    }

    /// `FIONREAD` on a pipe reports its queued bytes, and it is the *same*
    /// buffer both ends count: a write end answers what its peer could read,
    /// measured on the host (`.workbuddy/probe_fionread.c`, Linux 7.2.9).
    fn fionread(&self) -> Option<VfsResult<usize>> {
        Some(Ok(self.buffered()))
    }

    /// A pipe has no position: `lseek(2)` on one is `ESPIPE`, which is how a
    /// reader learns it cannot push back the bytes it over-read.
    fn seekable(&self) -> bool {
        false
    }

    /// `pipe_poll`'s rule: a read is ready while bytes are *waiting*, and the
    /// end of the stream is not a read condition at all - it is the hang-up
    /// below. Keeping the two apart is what lets a waiter end on either, and
    /// one waiter in particular has to: udev's `spawn_wait` reads its child's
    /// pipe with epoll and takes only `EPOLLHUP` as "the program is done", so
    /// an end-of-stream reported as `EPOLLIN` alone leaves it reading 0 in a
    /// loop until its event timeout expires.
    fn poll_ready(&self) -> Ready {
        critical_section::with(|cs| {
            let st = self.state.borrow(cs).borrow();
            Ready {
                read: !st.data.is_empty(),
                // Room for something, or a write that is about to fail: with no
                // readers left, a write answers EPIPE rather than waiting.
                write: st.readers == 0 || st.data.len() < CAPACITY,
            }
        })
    }

    /// The stream's writing side is gone: every reader's next read drains what
    /// is buffered and then answers 0. Read off the same counter
    /// [`Self::poll_ready`] reads, so the two answers cannot disagree.
    fn poll_hup(&self) -> bool {
        critical_section::with(|cs| self.state.borrow(cs).borrow().writers == 0)
    }

    /// The stream's reading side is gone: a write answers `EPIPE`. That is
    /// `EPOLLERR` on a pipe, and the pair with [`Self::poll_hup`] completes
    /// `pipe_poll`'s two conditions.
    fn poll_err(&self) -> bool {
        critical_section::with(|cs| self.state.borrow(cs).borrow().readers == 0)
    }

    fn poll_subscribe(&self, id: TaskId) {
        critical_section::with(|cs| {
            let mut st = self.state.borrow(cs).borrow_mut();
            if self.role.writes() && !st.waiting_writers.contains(&id) {
                st.waiting_writers.push(id);
            }
            if self.role.reads() && !st.waiting_readers.contains(&id) {
                st.waiting_readers.push(id);
            }
        });
    }

    fn poll_unsubscribe(&self, id: TaskId) {
        critical_section::with(|cs| {
            let mut st = self.state.borrow(cs).borrow_mut();
            st.waiting_writers.retain(|&w| w != id);
            st.waiting_readers.retain(|&w| w != id);
        });
    }

    fn metadata(&self) -> Metadata {
        Metadata {
            ino: self.ino,
            kind: self.kind,
            size: self.buffered() as u64,
        }
    }

    fn read_at(&self, _offset: u64, buf: &mut [u8]) -> VfsResult<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        loop {
            match self.read_once(buf)? {
                Some(n) => {
                    if n > 0 {
                        // Room freed for a blocked writer.
                        self.wake_waiters(true);
                    }
                    return Ok(n);
                }
                None => {
                    // Nothing to take, and a writer still alive - which an
                    // O_NONBLOCK reader asked not to wait for. EAGAIN, not a
                    // park: a self-pipe's reader only reads when something
                    // was signalled, and parking it on the first empty read
                    // is how a program with an event loop freezes.
                    if self.nonblocking.load(Ordering::Relaxed) {
                        return Err(VfsError::Again);
                    }
                    // Everything below the flag is a wait, and this one is a
                    // wait a catchable signal may end - with nothing consumed,
                    // which is what `EINTR` promises on this door.
                    self.wait_readable()?;
                }
            }
        }
    }

    fn write_at(&self, _offset: u64, buf: &[u8]) -> VfsResult<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        let mut done = 0;
        while done < buf.len() {
            match self.write_once(&buf[done..])? {
                Some(n) => {
                    done += n;
                    // Data arrived for a blocked reader.
                    self.wake_waiters(false);
                }
                None => {
                    // No room - which an O_NONBLOCK writer asked to hear as
                    // EAGAIN. Bytes already landed are reported first: a
                    // nonblocking write answers with what it managed and
                    // leaves the rest to the caller, which is Linux's
                    // spelling of the same rule.
                    if self.nonblocking.load(Ordering::Relaxed) {
                        if done > 0 {
                            return Ok(done);
                        }
                        return Err(VfsError::Again);
                    }
                    // Park until there is room for what `write_once` would
                    // actually accept: a record-sized write wants the whole
                    // record free (it is all-or-nothing), while a longer one
                    // only wants a single byte to make progress in pieces.
                    //
                    // A signal may end this wait, and the answer is then the
                    // bytes that already landed rather than `EINTR`: a partial
                    // write is a legal answer on this door, and it is the only
                    // one that cannot duplicate them if the caller retries what
                    // it was told did not land.
                    let rest = buf.len() - done;
                    let need = if rest <= PIPE_BUF { rest } else { 1 };
                    if let Err(e) = self.wait_writable(need) {
                        if done > 0 {
                            return Ok(done);
                        }
                        return Err(e);
                    }
                }
            }
        }
        Ok(done)
    }
}

/// One end of a `socketpair`: a connected, bidirectional byte stream.
///
/// A Unix-domain socket and a pipe are the same object seen from two angles -
/// an anonymous inode that moves bytes and reports end-of-stream when the far
/// end closes - so this is built from [`pair`] rather than beside it: two
/// buffers, one per direction, cut in half and crossed so each end reads what
/// the other writes. What a pipe cannot express is a descriptor that is *both*
/// ends at once, which is exactly what this adds.
///
/// It is not a socket in the network sense: there is no address, no
/// connection, and nothing in `net`'s registry knows it exists. What it does
/// carry is the credentials of whoever made it, because `getsockopt` on a
/// `socketpair` end answers them on Linux and the callers of a pair - a
/// compositor and the children it launches - ask exactly that question.
pub struct DuplexEnd {
    read: Arc<PipeEnd>,
    write: Arc<PipeEnd>,
    /// The credentials of the process that made the pair - which is what each
    /// end's peer is, since the far half came into being in that process. This
    /// is `getsockopt(SO_PEERCRED)`'s answer, and the reason a pair carries
    /// credentials at all: a compositor hands one half to a child it launched
    /// and asks the other half who its client is.
    peer: (u32, u32, u32),
    /// `SO_PASSCRED`: whether a read should attach the peer's credentials.
    passcred: AtomicBool,
}

impl DuplexEnd {
    /// The two ends of one connected pair, made by a process whose
    /// credentials are `creator` - `(pid, uid, gid)`, or all zero when the
    /// caller has no process behind the pair.
    pub fn pair(creator: (u32, u32, u32)) -> (Arc<DuplexEnd>, Arc<DuplexEnd>) {
        // Two one-way links. `a_write` feeds `b_read`, and `b_write` feeds
        // `a_read`, so each end's read half is the other's write half.
        let (a_read, b_write) = pair();
        let (b_read, a_write) = pair();
        let end = |read: Arc<PipeEnd>, write: Arc<PipeEnd>| {
            Arc::new(DuplexEnd {
                read,
                write,
                peer: creator,
                passcred: AtomicBool::new(false),
            })
        };
        let a = end(a_read, a_write);
        let b = end(b_read, b_write);
        (a, b)
    }

    /// `SO_PEERCRED`: the pid, uid and gid of the process that created the
    /// pair, which Linux reports as the peer's credentials on both ends.
    pub fn peer_cred(&self) -> (u32, u32, u32) {
        self.peer
    }

    /// `SO_PASSCRED` as read.
    pub fn passcred(&self) -> bool {
        self.passcred.load(Ordering::Relaxed)
    }

    /// `SO_PASSCRED` as set.
    pub fn set_passcred(&self, on: bool) {
        self.passcred.store(on, Ordering::Relaxed);
    }

    /// Half-close the writing direction. The far end's next read drains what
    /// is buffered and then reports end-of-stream, which is the whole point of
    /// a half-close: the peer is told no more is coming without the reader's
    /// own direction going anywhere.
    pub fn shut_write(&self) {
        self.write.shut_write();
    }

    /// Half-close the reading direction: this end's reads report
    /// end-of-stream from now on, and the peer's writes fail as they would to
    /// a closed pipe.
    pub fn shut_read(&self) {
        self.read.shut_read();
    }

    /// Queue an SCM_RIGHTS handover for the peer to pick up on its next read.
    /// It rides the writing direction, which is the buffer the peer reads.
    pub fn send_scm(&self, fds: Vec<Arc<super::File>>) {
        self.write.push_scm(fds);
    }

    /// Take the handover the peer queued for this end, if any.
    pub fn take_scm(&self) -> Option<Vec<Arc<super::File>>> {
        self.read.take_scm()
    }

    /// The handover a peek would report, cloned rather than taken. See
    /// [`PipeEnd::peek_scm`].
    pub fn peek_scm(&self) -> Option<Vec<Arc<super::File>>> {
        self.read.peek_scm()
    }

    /// Look at the bytes a read would hand back, without taking them. See
    /// [`PipeEnd::peek_once`].
    pub fn peek(&self, buf: &mut [u8]) -> VfsResult<Option<usize>> {
        self.read.peek_once(buf)
    }

    /// Whether a handover is waiting to be taken, without taking it. See
    /// [`PipeEnd::scm_due`].
    pub fn scm_due(&self) -> bool {
        self.read.scm_due()
    }
}

impl Inode for DuplexEnd {
    /// A socketpair is not a pipe and Linux does not answer it like one: the
    /// read half keeps `EPOLLIN` when the peer has gone - `unix_poll` sets
    /// `EPOLLIN` and `EPOLLHUP` together once the peer closed - where a pipe
    /// drops the read condition and reports the hang-up alone. Both readings
    /// are honest here (a read does answer 0 either way); the socket's is the
    /// tolerant one, and it is what the socketpair's own callers were written
    /// against, so the pipe's news is added *beside* it rather than replacing
    /// it.
    fn poll_ready(&self) -> Ready {
        Ready {
            read: self.read.poll_ready().read || self.read.poll_hup(),
            write: self.write.poll_ready().write,
        }
    }

    /// The peer is gone altogether: nobody is writing into this end and nobody
    /// is reading what it writes. A *half*-close is only the first of those,
    /// and Linux calls it something else again - `EPOLLRDHUP` with `EPOLLIN`,
    /// not a hang-up - so the two directions have to agree before this is true.
    fn poll_hup(&self) -> bool {
        self.read.poll_hup() && self.write.poll_err()
    }

    /// Never: a socket has error conditions (a pending `sk_err`), but a peer
    /// that has simply gone is the hang-up above, not one of them.
    fn poll_err(&self) -> bool {
        false
    }

    /// Neither half of a socketpair is seekable, for the same reason a pipe
    /// is not.
    fn seekable(&self) -> bool {
        false
    }

    fn poll_subscribe(&self, id: TaskId) {
        // Both halves, on purpose: a waiter wants to hear about data arriving
        // *and* about room appearing, and each half knows which of those its
        // own waiters are after.
        self.read.poll_subscribe(id);
        self.write.poll_subscribe(id);
    }

    fn poll_unsubscribe(&self, id: TaskId) {
        self.read.poll_unsubscribe(id);
        self.write.poll_unsubscribe(id);
    }

    fn as_duplex(&self) -> Option<&DuplexEnd> {
        Some(self)
    }

    /// `FIONREAD` on a `SOCK_STREAM` socketpair end reports the bytes queued in
    /// its *receive* half - what the peer has written and this end has not read.
    /// Measured on the host (`probe_fionread2.c`: 3 + 4 written answers 7, the
    /// whole stream rather than the first write). The writer's own end has an
    /// empty queue and answers 0.
    fn fionread(&self) -> Option<VfsResult<usize>> {
        Some(Ok(self.read.buffered()))
    }

    fn metadata(&self) -> Metadata {
        // A stream socket has no size, and nothing but the ends holds it. The
        // kind is `File` because the filesystem's vocabulary has no better
        // word for "anonymous byte channel" - the same answer pipes give.
        Metadata {
            ino: 0,
            kind: NodeKind::File,
            size: 0,
        }
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> VfsResult<usize> {
        self.read.read_at(offset, buf)
    }

    fn write_at(&self, offset: u64, buf: &[u8]) -> VfsResult<usize> {
        self.write.write_at(offset, buf)
    }
}

#[cfg(all(test, not(target_os = "none")))]
mod tests {
    use alloc::sync::Arc;
    use alloc::vec;

    use super::{CAPACITY, Channel, DuplexEnd, PIPE_BUF, Role, pair};
    use crate::vfs::{File, Inode, NodeKind, VfsError};

    #[test]
    fn a_socketpair_carries_bytes_both_ways() {
        let (a, b) = DuplexEnd::pair((0, 0, 0));
        let mut buf = [0u8; 8];

        assert_eq!(a.write_at(0, b"ping").unwrap(), 4);
        assert_eq!(b.read_at(0, &mut buf).unwrap(), 4);
        assert_eq!(&buf[..4], b"ping");

        // The other direction runs over the other buffer, so a reply cannot
        // be read back as an echo of what was just written.
        assert_eq!(b.write_at(0, b"pong").unwrap(), 4);
        assert_eq!(a.read_at(0, &mut buf).unwrap(), 4);
        assert_eq!(&buf[..4], b"pong");
    }

    #[test]
    fn an_scm_handover_rides_only_the_direction_it_was_sent_on() {
        let (a, b) = DuplexEnd::pair((0, 0, 0));
        // Two distinct open files, handed over in order on a's write side.
        let f1 = Arc::new(File::new(DuplexEnd::pair((0, 0, 0)).0));
        let f2 = Arc::new(File::new(DuplexEnd::pair((0, 0, 0)).0));
        let p1 = Arc::as_ptr(&f1);
        let p2 = Arc::as_ptr(&f2);

        // Before anything is sent, neither side owes the other a handover.
        assert!(a.take_scm().is_none());
        assert!(b.take_scm().is_none());

        a.send_scm(vec![f1, f2]);

        // The handover left on a's write direction, so it is not readable back
        // on a's own (inbound) side - only the peer pops what was sent to it.
        assert!(a.take_scm().is_none(), "a's send is not echoed to a");
        let got = b.take_scm().expect("the handover arrives on b");
        assert_eq!(got.len(), 2, "both descriptors ride one message");
        assert_eq!(Arc::as_ptr(&got[0]), p1, "in the order they were queued");
        assert_eq!(Arc::as_ptr(&got[1]), p2);
    }

    #[test]
    fn a_peek_shows_the_front_without_taking_it() {
        let (a, b) = DuplexEnd::pair((0, 0, 0));
        let mut buf = [0u8; 8];

        assert_eq!(a.write_at(0, b"abcdefghij").unwrap(), 10);
        // The look is repeatable, copies only what the caller asked for, and
        // leaves every byte where the read will find it.
        assert_eq!(b.peek(&mut buf[..4]).unwrap(), Some(4));
        assert_eq!(&buf[..4], b"abcd");
        assert_eq!(b.peek(&mut buf[..4]).unwrap(), Some(4));
        assert_eq!(&buf[..4], b"abcd");

        // A peek of the whole stream answers the whole stream and still takes
        // nothing.
        let mut all = [0u8; 16];
        assert_eq!(b.peek(&mut all).unwrap(), Some(10));
        assert_eq!(&all[..10], b"abcdefghij");
        assert_eq!(b.read_at(0, &mut buf).unwrap(), 8);
        assert_eq!(&buf[..8], b"abcdefgh");
        assert_eq!(b.read_at(0, &mut buf).unwrap(), 2);
        assert_eq!(&buf[..2], b"ij");

        // Empty with a writer: "nothing yet"; empty with no writer: EOF.
        assert_eq!(b.peek(&mut buf).unwrap(), None);
        drop(a);
        assert_eq!(b.peek(&mut buf).unwrap(), Some(0));
    }

    #[test]
    fn a_peeked_handover_is_delivered_again_to_the_read() {
        let (a, b) = DuplexEnd::pair((0, 0, 0));
        let f = Arc::new(File::new(DuplexEnd::pair((0, 0, 0)).0));
        let want = Arc::as_ptr(&f);
        a.write_at(0, b"x").unwrap();
        a.send_scm(vec![f]);

        // `MSG_PEEK` reports the queued record and the queue keeps it; Linux
        // installs a copy of each descriptor and the read installs another.
        let peeked = b.peek_scm().expect("the handover is visible to a peek");
        assert_eq!(peeked.len(), 1);
        assert_eq!(
            Arc::as_ptr(&peeked[0]),
            want,
            "the same file, not a new one"
        );
        assert!(
            !b.scm_due(),
            "the record becomes due when its bytes are consumed"
        );
        drop(peeked);

        let mut buf = [0u8; 1];
        assert_eq!(b.read_at(0, &mut buf).unwrap(), 1);
        let taken = b.take_scm().expect("the read still gets the record");
        assert_eq!(Arc::as_ptr(&taken[0]), want);
        assert!(b.take_scm().is_none(), "and only once");
    }

    #[test]
    fn half_closing_a_socketpair_leaves_the_other_direction_running() {
        let (a, b) = DuplexEnd::pair((0, 0, 0));
        a.write_at(0, b"last").unwrap();
        a.shut_write();

        // What was already in flight is still delivered, and only then does
        // the far end hear that nothing more is coming.
        let mut buf = [0u8; 8];
        assert_eq!(b.read_at(0, &mut buf).unwrap(), 4);
        assert_eq!(&buf[..4], b"last");
        assert_eq!(b.read_at(0, &mut buf).unwrap(), 0, "EOF");

        // b's own direction is untouched, which is the point of a half-close.
        assert_eq!(b.write_at(0, b"still here").unwrap(), 10);
        assert_eq!(a.read_at(0, &mut buf).unwrap(), 8); // buf is 8 bytes
    }

    #[test]
    fn shutting_down_twice_is_harmless() {
        let (a, _b) = DuplexEnd::pair((0, 0, 0));
        a.shut_write();
        // The second call must not take the end out of the writer count again,
        // and the drop that follows must not take it out a third time: an
        // underflow here would be reported as a panic on a `u32`.
        a.shut_write();
        drop(a);
    }

    #[test]
    fn a_shut_down_reader_makes_the_peer_write_fail() {
        let (a, b) = DuplexEnd::pair((0, 0, 0));
        a.shut_read();
        assert_eq!(b.write_at(0, b"nobody").err(), Some(VfsError::BrokenPipe));
    }

    #[test]
    fn dropping_one_end_ends_the_stream_the_way_a_pipe_does() {
        let (a, b) = DuplexEnd::pair((0, 0, 0));
        drop(b);
        let mut buf = [0u8; 4];
        assert_eq!(a.read_at(0, &mut buf).unwrap(), 0, "the far end is gone");
        assert_eq!(a.write_at(0, b"gone").err(), Some(VfsError::BrokenPipe));
    }

    #[test]
    fn a_pipe_carries_bytes_from_writer_to_reader() {
        let (read_end, write_end) = pair();

        assert_eq!(write_end.write_at(0, b"hello").unwrap(), 5);
        assert_eq!(read_end.metadata().size, 5);

        let mut buf = [0u8; 8];
        assert_eq!(read_end.read_at(0, &mut buf).unwrap(), 5);
        assert_eq!(&buf[..5], b"hello");
        // The writer is still alive, so an empty pipe is a wait, not an EOF.
        assert_eq!(read_end.read_once(&mut buf).unwrap(), None);
    }

    #[test]
    fn a_write_at_most_pipe_buf_never_splits() {
        let (read_end, write_end) = pair();

        // Fill until fewer than PIPE_BUF bytes of room are left.
        let big = vec![0u8; CAPACITY - PIPE_BUF + 1];
        assert_eq!(write_end.write_at(0, &big).unwrap(), big.len());

        // There is room, but not for a record this size: the write waits.
        assert_eq!(write_end.write_once(&vec![1u8; PIPE_BUF]).unwrap(), None);
        // A smaller record still lands in the same room.
        assert_eq!(write_end.write_once(&[2u8; 16]).unwrap(), Some(16));
        // And the reader sees both parts in order.
        let mut buf = vec![0u8; CAPACITY];
        assert_eq!(read_end.read_at(0, &mut buf).unwrap(), big.len() + 16);
        assert_eq!(&buf[big.len()..big.len() + 16], &[2u8; 16]);
    }

    #[test]
    fn a_write_bigger_than_the_pipe_makes_progress_in_pieces() {
        let (read_end, write_end) = pair();

        let big = vec![7u8; CAPACITY + 1];
        assert_eq!(write_end.write_once(&big).unwrap(), Some(CAPACITY));
        // Full, and the write is unrecord-sized: nothing more fits.
        assert_eq!(write_end.write_once(&[8u8; 4]).unwrap(), None);

        let mut buf = vec![0u8; CAPACITY];
        assert_eq!(read_end.read_at(0, &mut buf).unwrap(), CAPACITY);
        assert!(buf.iter().all(|&b| b == 7));
        // The freed room takes the rest, but only as much as fits.
        assert_eq!(write_end.write_once(&big[CAPACITY..]).unwrap(), Some(1));
    }

    #[test]
    fn dropping_the_writer_ends_the_stream() {
        let (read_end, write_end) = pair();
        write_end.write_at(0, b"last").unwrap();
        drop(write_end);

        // Buffered data survives the writer; EOF comes after it drains.
        let mut buf = [0u8; 8];
        assert_eq!(read_end.read_at(0, &mut buf).unwrap(), 4);
        assert_eq!(&buf[..4], b"last");
        assert_eq!(read_end.read_at(0, &mut buf).unwrap(), 0, "EOF");
        assert_eq!(read_end.read_at(0, &mut buf).unwrap(), 0, "EOF is sticky");
    }

    #[test]
    fn a_writer_parks_for_a_whole_record_not_mere_room() {
        // The regression: `write_once` admits a record-sized write only with a
        // full record free, so the condition a blocked writer sleeps on must
        // agree. If it woke on `any` room instead, a writer waiting on a
        // partial line would be woken, refused again, and spin at 100% CPU
        // rather than sleeping until the reader makes real room.
        let (read_end, write_end) = pair();

        // Fill all but 1000 bytes: there is room, but not a whole record's.
        write_end.write_at(0, &vec![7u8; CAPACITY - 1000]).unwrap();
        assert!(
            !write_end.writable_or_broken(PIPE_BUF),
            "a record must wait"
        );
        assert!(write_end.writable_or_broken(1), "one byte can go through");

        // Draining the pipe down frees a whole record's worth, so the parked
        // record write can finally proceed.
        let mut all = vec![0u8; CAPACITY];
        assert_eq!(read_end.read_at(0, &mut all).unwrap(), CAPACITY - 1000);
        assert!(write_end.writable_or_broken(PIPE_BUF), "a record fits now");

        // A vanished reader outranks the room question: the wait must end so the
        // writer learns about `EPIPE`, even with no space at all.
        let (read_end, write_end) = pair();
        write_end.write_at(0, &vec![7u8; CAPACITY]).unwrap();
        assert!(!write_end.writable_or_broken(1));
        drop(read_end);
        assert!(write_end.writable_or_broken(PIPE_BUF), "broken beats full");
    }

    #[test]
    fn writing_with_no_reader_is_broken_pipe() {
        let (read_end, write_end) = pair();
        drop(read_end);

        assert_eq!(
            write_end.write_at(0, b"nobody listening").err(),
            Some(VfsError::BrokenPipe)
        );
        // Draining first does not help: the reader is gone for good.
        let (read_end, write_end) = pair();
        write_end.write_at(0, b"hi").unwrap();
        drop(read_end);
        assert_eq!(write_end.write_once(b"!").err(), Some(VfsError::BrokenPipe));
    }

    #[test]
    fn a_nonblocking_end_answers_eagain_rather_than_waiting() {
        let (read_end, write_end) = pair();
        read_end.set_nonblocking(true);

        // Empty with a writer alive: EAGAIN, not a wait (which would panic
        // on the host - the wait needs a scheduler - and park the caller
        // forever on the machine).
        let mut buf = [0u8; 4];
        assert_eq!(read_end.read_at(0, &mut buf).err(), Some(VfsError::Again));

        // Buffered data still reads back: the flag changes what a *wait*
        // is, not what a read takes.
        assert_eq!(write_end.write_at(0, b"data").unwrap(), 4);
        assert_eq!(read_end.read_at(0, &mut buf).unwrap(), 4);
        assert_eq!(&buf[..4], b"data");

        // A full pipe answers a writer the same way: fill to capacity, and
        // one more write is EAGAIN rather than a park.
        write_end.set_nonblocking(true);
        let big = vec![0u8; CAPACITY];
        assert_eq!(write_end.write_at(0, &big).unwrap(), CAPACITY);
        assert_eq!(write_end.write_at(0, b"more").err(), Some(VfsError::Again));

        // Draining lets writes through again: the EAGAIN was about the
        // state, not the pipe's lifetime.
        let mut sink = vec![0u8; CAPACITY];
        assert_eq!(read_end.read_at(0, &mut sink).unwrap(), CAPACITY);
        assert_eq!(write_end.write_at(0, b"done").unwrap(), 4);

        // EOF outranks the flag: with the writer gone, the drain reads the
        // last bytes and then answers 0, exactly as a blocking one would.
        write_end.shut_write();
        assert_eq!(read_end.read_at(0, &mut buf).unwrap(), 4);
        assert_eq!(read_end.read_at(0, &mut buf).unwrap(), 0, "EOF");
    }

    #[test]
    fn zero_sized_operations_do_nothing() {
        let (read_end, write_end) = pair();
        assert_eq!(read_end.read_at(0, &mut []).unwrap(), 0);
        assert_eq!(write_end.write_at(0, b"").unwrap(), 0);
    }

    /// A FIFO's rendezvous, at the one place the host can drive it without
    /// blocking: the *non-blocking* doors. A non-blocking writer with no reader
    /// is refused (`ENXIO`) rather than parked, and a non-blocking reader with
    /// no writer opens (its reads will see end-of-stream) - both measured on a
    /// current host in `.workbuddy/probe_fifo.c`, and the shape the whole
    /// rendezvous turns on.
    #[test]
    fn a_fifo_channel_refuses_a_lonely_nonblocking_writer() {
        let ch = Channel::new(NodeKind::Fifo, 7);

        // No reader anywhere: a non-blocking writer hears ENXIO, and taking the
        // count is rolled back so the FIFO remembers no writer either.
        assert_eq!(ch.open(Role::Write, true).err(), Some(VfsError::NoDevice));
        // Which the next reader proves: it opens (its own non-blocking reader
        // path) and reads end-of-stream, not the EAGAIN a live writer would give.
        let rd = ch.open(Role::Read, true).unwrap();
        let mut buf = [0u8; 4];
        assert_eq!(rd.read_once(&mut buf).unwrap(), Some(0), "no writer at all");
    }

    /// The reader parks first (non-blocking, so it returns), and the writer that
    /// arrives finds it - the property that keeps the two sides from deadlocking.
    /// The writer's open is *blocking* and returns at once, because the parked
    /// reader already counts as a reader.
    #[test]
    fn a_fifo_channel_pairs_a_reader_and_a_writer() {
        let ch = Channel::new(NodeKind::Fifo, 9);
        let rd = ch.open(Role::Read, true).unwrap();
        // Blocking, and it must *return* - the reader is already counted.
        let wr = ch.open(Role::Write, false).unwrap();

        assert_eq!(wr.write_at(0, b"hey").unwrap(), 3);
        let mut buf = [0u8; 8];
        assert_eq!(rd.read_at(0, &mut buf).unwrap(), 3);
        assert_eq!(&buf[..3], b"hey");
    }

    /// `O_RDWR` is one description that is both ends, so it is its own peer and
    /// never waits - and its `metadata` reports the FIFO, not an anonymous pipe.
    #[test]
    fn a_fifo_open_readwrite_is_its_own_peer() {
        let ch = Channel::new(NodeKind::Fifo, 11);
        let both = ch.open(Role::Both, false).unwrap();

        assert_eq!(both.metadata().kind, NodeKind::Fifo);
        assert_eq!(both.metadata().ino, 11);
        assert!(!both.seekable(), "a FIFO is ESPIPE");
        // It writes to, and reads back from, the one buffer.
        assert_eq!(both.write_at(0, b"round").unwrap(), 5);
        let mut buf = [0u8; 8];
        assert_eq!(both.read_at(0, &mut buf).unwrap(), 5);
        assert_eq!(&buf[..5], b"round");
    }

    /// The buffer is discarded when both counts reach zero - the reader here
    /// closes while the writer still holds it (so nothing is discarded), and the
    /// writer's close takes both to zero and drops the two unread bytes.
    #[test]
    fn closing_a_fifos_last_two_ends_discards_the_buffer() {
        let ch = Channel::new(NodeKind::Fifo, 13);
        let rd = ch.open(Role::Read, true).unwrap();
        let wr = ch.open(Role::Write, true).unwrap();
        assert_eq!(wr.write_at(0, b"xy").unwrap(), 2);

        // Reader gone, writer alive: the bytes stay for whoever is left, which
        // is nobody - but the buffer is not discarded yet.
        drop(rd);
        assert_eq!(wr.metadata().size, 2, "not discarded while a writer lives");
        // The last end goes: both counts are zero, and the buffer with them.
        drop(wr);

        let fresh = ch.open(Role::Read, true).unwrap();
        assert_eq!(fresh.metadata().size, 0, "the next open starts empty");
        let mut buf = [0u8; 4];
        assert_eq!(fresh.read_once(&mut buf).unwrap(), Some(0));
    }

    /// `pipe_poll`'s two conditions, and the one that is easy to get wrong:
    /// the end of the stream is a *hang-up*, not another way of being
    /// readable. A waiter that ends on `EPOLLHUP` alone - udev's
    /// `spawn_wait`, whose epoll loop takes a read's 0 as "ask again" and
    /// the hang-up as "done" - can only finish while the two stay apart.
    #[test]
    fn end_of_stream_is_a_hang_up_and_not_a_read_condition() {
        let (read_end, write_end) = pair();

        // A writer still attached and nothing buffered: neither condition.
        assert!(!read_end.poll_ready().read);
        assert!(!read_end.poll_hup());
        assert!(!read_end.poll_err());

        // Bytes waiting: readable, and not hung up.
        write_end.write_at(0, b"x").unwrap();
        assert!(read_end.poll_ready().read);
        assert!(!read_end.poll_hup());

        // The writer goes, with the byte still unread: both, as Linux
        // reports both.
        drop(write_end);
        assert!(read_end.poll_ready().read);
        assert!(read_end.poll_hup());

        // Drained: the read condition is gone and the hang-up is what is
        // left. This is the state the udev waiter has to be able to see.
        let mut buf = [0u8; 1];
        assert_eq!(read_end.read_at(0, &mut buf).unwrap(), 1);
        assert!(!read_end.poll_ready().read);
        assert!(read_end.poll_hup());
        assert!(!read_end.poll_err(), "this end is the reader, and it lives");
    }

    /// The other half of the same rule, plus the socketpair's difference: a
    /// write end whose readers are gone reports the *error* condition, and a
    /// socketpair keeps its peer-closed news readable as well (`unix_poll`
    /// sets `EPOLLIN` and `EPOLLHUP` together, where a pipe does not).
    #[test]
    fn a_lost_reader_is_an_error_and_a_socketpair_stays_readable() {
        let (read_end, write_end) = pair();
        assert!(!write_end.poll_err());
        assert!(!write_end.poll_hup(), "the writer counts itself");
        drop(read_end);
        assert!(write_end.poll_err(), "nobody left to write to");
        assert!(
            write_end.poll_ready().write,
            "a write answers EPIPE, not a wait"
        );

        let (a, b) = DuplexEnd::pair((0, 0, 0));
        drop(b);
        assert!(a.poll_hup(), "the peer closed, both directions with it");
        assert!(
            !a.poll_err(),
            "and a socket calls that a hang-up, not an error"
        );
        assert!(
            a.poll_ready().read,
            "a unix socket answers end-of-stream as readable too"
        );
    }
}
