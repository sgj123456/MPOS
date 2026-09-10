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

use critical_section::Mutex;

use super::{Inode, Metadata, NodeKind, VfsError, VfsResult};
use crate::sched::policy::TaskId;

/// Writes up to this many bytes never split across a blocking point, which is
/// what lets two processes use a pipe as a record channel. POSIX's minimum.
pub const PIPE_BUF: usize = 4096;

/// How much a pipe buffers before writers block. A multiple of [`PIPE_BUF`],
/// so a stream of record-sized writes never wedges against itself.
const CAPACITY: usize = 16 * PIPE_BUF;

struct PipeState {
    data: VecDeque<u8>,
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
        readers: 1,
        writers: 1,
        waiting_readers: Vec::new(),
        waiting_writers: Vec::new(),
    })));
    let read_end = Arc::new(PipeEnd {
        state: state.clone(),
        writing: false,
    });
    let write_end = Arc::new(PipeEnd {
        state,
        writing: true,
    });
    (read_end, write_end)
}

impl PipeEnd {
    /// Bytes waiting to be read.
    pub fn buffered(&self) -> usize {
        critical_section::with(|cs| self.state.borrow(cs).borrow().data.len())
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
    fn wait_writable(&self) {
        let id = crate::sched::current();
        critical_section::with(|cs| {
            let mut st = self.state.borrow(cs).borrow_mut();
            if !st.waiting_writers.contains(&id) {
                st.waiting_writers.push(id);
            }
        });
        crate::sched::block_until(|| self.writable_or_broken());
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
    fn wait_writable(&self) {
        panic!("blocking pipe writes need the kernel scheduler; test write_once instead");
    }

    #[cfg_attr(not(target_os = "none"), allow(dead_code))]
    fn readable_or_ended(&self) -> bool {
        critical_section::with(|cs| {
            let st = self.state.borrow(cs).borrow();
            !st.data.is_empty() || st.writers == 0
        })
    }

    #[cfg_attr(not(target_os = "none"), allow(dead_code))]
    fn writable_or_broken(&self) -> bool {
        critical_section::with(|cs| {
            let st = self.state.borrow(cs).borrow();
            st.readers == 0 || st.data.len() < CAPACITY
        })
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
                None => self.wait_readable(),
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
                None => self.wait_writable(),
            }
        }
        Ok(done)
    }
}

#[cfg(test)]
mod tests {
    use alloc::vec;

    use super::{CAPACITY, PIPE_BUF, pair};
    use crate::vfs::{Inode, VfsError};

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
    fn zero_sized_operations_do_nothing() {
        let (read_end, write_end) = pair();
        assert_eq!(read_end.read_at(0, &mut []).unwrap(), 0);
        assert_eq!(write_end.write_at(0, b"").unwrap(), 0);
    }
}
