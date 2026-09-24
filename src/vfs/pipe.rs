//! Pipes: a byte channel with the semantics of `|`.
//!
//! A pipe is two [`PipeEnd`]s over one shared buffer. It is an anonymous inode:
//! nothing names it, and the only way to reach it is through the fd the creator
//! was handed, which is what makes it a channel rather than a file.
//!
//! The non-blocking core - [`PipeEnd::read_once`] and [`PipeEnd::write_once`] -
//! is plain code over a `VecDeque` and runs under `cargo test` on the host. The
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

use super::{Inode, Metadata, NodeKind, Ready, VfsError, VfsResult};
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

struct PipeState {
    data: VecDeque<u8>,
    /// SCM_RIGHTS side channel, one entry per send that carried descriptors.
    /// It lives here - beside the bytes it travels with - because the two
    /// crossed halves of a [`DuplexEnd`] share exactly this state, so a record
    /// pushed on the write side is the record the far reader pops, in order.
    /// Each entry is the set of open files to hand over; dropping one without a
    /// reader closes them, which is Linux's rule for a descriptor never taken.
    scm: VecDeque<Vec<Arc<super::File>>>,
    readers: u32,
    writers: u32,
    /// Tasks blocked waiting for data or EOF. A dead task's stale entry is
    /// harmless: waking a reused id can only cause a spurious wakeup, and the
    /// woken task re-checks its own condition before doing anything.
    #[cfg_attr(not(target_os = "none"), allow(dead_code))]
    waiting_readers: Vec<TaskId>,
    #[cfg_attr(not(target_os = "none"), allow(dead_code))]
    waiting_writers: Vec<TaskId>,
}

/// One end of a pipe, as an inode. Reads and writes ignore their offset: a
/// pipe is a stream, and the cursor an fd keeps is meaningless for it.
pub struct PipeEnd {
    state: Arc<Mutex<RefCell<PipeState>>>,
    writing: bool,
    /// Set when this end has left the count, by a half-close or by its own
    /// drop - whichever came first. Both have to be idempotent, since a
    /// `shutdown` can be asked for twice and the drop always follows it.
    closed: AtomicBool,
    /// `O_NONBLOCK`, as `pipe2(2)` is born with it and `fcntl(F_SETFL)` sets
    /// it: an operation that would wait answers `EAGAIN` instead. It lives
    /// here rather than in the descriptor, because the wait is here - testing
    /// it beside the state it would wait on is what makes the answer
    /// race-free, where a descriptor checked beforehand could have the data
    /// it saw taken by another reader before its own turn comes.
    nonblocking: AtomicBool,
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
        data: VecDeque::new(),
        scm: VecDeque::new(),
        readers: 1,
        writers: 1,
        waiting_readers: Vec::new(),
        waiting_writers: Vec::new(),
    })));
    let read_end = Arc::new(PipeEnd {
        state: state.clone(),
        writing: false,
        closed: AtomicBool::new(false),
        nonblocking: AtomicBool::new(false),
    });
    let write_end = Arc::new(PipeEnd {
        state,
        writing: true,
        closed: AtomicBool::new(false),
        nonblocking: AtomicBool::new(false),
    });
    (read_end, write_end)
}

impl PipeEnd {
    /// Bytes waiting to be read.
    pub fn buffered(&self) -> usize {
        critical_section::with(|cs| self.state.borrow(cs).borrow().data.len())
    }

    /// Queue one SCM_RIGHTS handover beside the bytes just written. Any end of
    /// the shared buffer can push; the peer's read half is the one that pops.
    pub fn push_scm(&self, fds: Vec<Arc<super::File>>) {
        critical_section::with(|cs| self.state.borrow(cs).borrow_mut().scm.push_back(fds));
    }

    /// Take the oldest pending handover, if any, from this (reading) side.
    pub fn take_scm(&self) -> Option<Vec<Arc<super::File>>> {
        critical_section::with(|cs| self.state.borrow(cs).borrow_mut().scm.pop_front())
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
                let n = buf.len().min(st.data.len());
                for byte in buf.iter_mut().take(n) {
                    *byte = st.data.pop_front().expect("length checked");
                }
                Some(n)
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
            st.data.extend(buf[..n].iter().copied());
            Ok(Some(n))
        })
    }

    /// Register the running task, block until the read side could move, then
    /// unregister. Registration lives outside `block_until` because a drop -
    /// which is the event most likely to end the wait - knows the pipe's
    /// waiters, not the scheduler's sleepers.
    #[cfg(target_os = "none")]
    fn wait_readable(&self) {
        let id = crate::sched::current();
        critical_section::with(|cs| {
            let mut st = self.state.borrow(cs).borrow_mut();
            if !st.waiting_readers.contains(&id) {
                st.waiting_readers.push(id);
            }
        });
        crate::sched::block_until(|| self.readable_or_ended());
        critical_section::with(|cs| {
            self.state
                .borrow(cs)
                .borrow_mut()
                .waiting_readers
                .retain(|&w| w != id);
        });
    }

    #[cfg(target_os = "none")]
    fn wait_writable(&self, need: usize) {
        let id = crate::sched::current();
        critical_section::with(|cs| {
            let mut st = self.state.borrow(cs).borrow_mut();
            if !st.waiting_writers.contains(&id) {
                st.waiting_writers.push(id);
            }
        });
        crate::sched::block_until(|| self.writable_or_broken(need));
        critical_section::with(|cs| {
            self.state
                .borrow(cs)
                .borrow_mut()
                .waiting_writers
                .retain(|&w| w != id);
        });
    }

    #[cfg(not(target_os = "none"))]
    fn wait_readable(&self) {
        panic!("blocking pipe reads need the kernel scheduler; test read_once instead");
    }

    #[cfg(not(target_os = "none"))]
    fn wait_writable(&self, _need: usize) {
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
        if self.writing && !self.closed.swap(true, Ordering::Relaxed) {
            critical_section::with(|cs| {
                self.state.borrow(cs).borrow_mut().writers -= 1;
            });
            self.wake_all();
        }
    }

    /// Half-close the other direction: this end's reads report end-of-stream,
    /// and the peer's writes fail the way a write to a closed pipe does.
    pub fn shut_read(&self) {
        if !self.writing && !self.closed.swap(true, Ordering::Relaxed) {
            critical_section::with(|cs| {
                self.state.borrow(cs).borrow_mut().readers -= 1;
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
    }
}

impl Drop for PipeEnd {
    fn drop(&mut self) {
        if self.closed.swap(true, Ordering::Relaxed) {
            // A half-close already took this end out of the count.
            return;
        }
        critical_section::with(|cs| {
            let mut st = self.state.borrow(cs).borrow_mut();
            if self.writing {
                st.writers -= 1;
            } else {
                st.readers -= 1;
            }
        });
        // Counts changed, so EOF/EPIPE may have become true for the other end.
        self.wake_all();
    }
}

impl Inode for PipeEnd {
    fn as_pipe(&self) -> Option<&PipeEnd> {
        Some(self)
    }

    /// A pipe has no position: `lseek(2)` on one is `ESPIPE`, which is how a
    /// reader learns it cannot push back the bytes it over-read.
    fn seekable(&self) -> bool {
        false
    }

    fn poll_ready(&self) -> Ready {
        critical_section::with(|cs| {
            let st = self.state.borrow(cs).borrow();
            Ready {
                // Data waiting, or the end of it: with no writers left, a read
                // answers 0 rather than waiting, and that is readable too.
                read: !st.data.is_empty() || st.writers == 0,
                // Room for something, or a write that is about to fail: with no
                // readers left, a write answers EPIPE rather than waiting.
                write: st.readers == 0 || st.data.len() < CAPACITY,
            }
        })
    }

    fn poll_subscribe(&self, id: TaskId) {
        critical_section::with(|cs| {
            let mut st = self.state.borrow(cs).borrow_mut();
            let queue = if self.writing {
                &mut st.waiting_writers
            } else {
                &mut st.waiting_readers
            };
            if !queue.contains(&id) {
                queue.push(id);
            }
        });
    }

    fn poll_unsubscribe(&self, id: TaskId) {
        critical_section::with(|cs| {
            let mut st = self.state.borrow(cs).borrow_mut();
            let queue = if self.writing {
                &mut st.waiting_writers
            } else {
                &mut st.waiting_readers
            };
            queue.retain(|&w| w != id);
        });
    }

    fn metadata(&self) -> Metadata {
        Metadata {
            ino: 0,
            kind: NodeKind::File,
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
                    self.wait_readable();
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
                    let rest = buf.len() - done;
                    let need = if rest <= PIPE_BUF { rest } else { 1 };
                    self.wait_writable(need);
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
/// connection, and nothing in `net` knows it exists. `socketpair(2)` never
/// makes any other kind, and programs that ask for one want a channel, not a
/// protocol.
pub struct DuplexEnd {
    read: Arc<PipeEnd>,
    write: Arc<PipeEnd>,
}

impl DuplexEnd {
    /// The two ends of one connected pair.
    pub fn pair() -> (Arc<DuplexEnd>, Arc<DuplexEnd>) {
        // Two one-way links. `a_write` feeds `b_read`, and `b_write` feeds
        // `a_read`, so each end's read half is the other's write half.
        let (a_read, b_write) = pair();
        let (b_read, a_write) = pair();
        let a = Arc::new(DuplexEnd {
            read: a_read,
            write: a_write,
        });
        let b = Arc::new(DuplexEnd {
            read: b_read,
            write: b_write,
        });
        (a, b)
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
}

impl Inode for DuplexEnd {
    fn poll_ready(&self) -> Ready {
        Ready {
            read: self.read.poll_ready().read,
            write: self.write.poll_ready().write,
        }
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

    use super::{CAPACITY, DuplexEnd, PIPE_BUF, pair};
    use crate::vfs::{File, Inode, VfsError};

    #[test]
    fn a_socketpair_carries_bytes_both_ways() {
        let (a, b) = DuplexEnd::pair();
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
        let (a, b) = DuplexEnd::pair();
        // Two distinct open files, handed over in order on a's write side.
        let f1 = Arc::new(File::new(DuplexEnd::pair().0));
        let f2 = Arc::new(File::new(DuplexEnd::pair().0));
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
    fn half_closing_a_socketpair_leaves_the_other_direction_running() {
        let (a, b) = DuplexEnd::pair();
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
        let (a, _b) = DuplexEnd::pair();
        a.shut_write();
        // The second call must not take the end out of the writer count again,
        // and the drop that follows must not take it out a third time: an
        // underflow here would be reported as a panic on a `u32`.
        a.shut_write();
        drop(a);
    }

    #[test]
    fn a_shut_down_reader_makes_the_peer_write_fail() {
        let (a, b) = DuplexEnd::pair();
        a.shut_read();
        assert_eq!(b.write_at(0, b"nobody").err(), Some(VfsError::BrokenPipe));
    }

    #[test]
    fn dropping_one_end_ends_the_stream_the_way_a_pipe_does() {
        let (a, b) = DuplexEnd::pair();
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
}
