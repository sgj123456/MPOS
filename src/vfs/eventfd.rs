//! `eventfd(2)`: a counter a process can wait on.
//!
//! It exists so that one thread can be woken by another without a pipe and
//! without polling: a write adds to the counter, a read takes it away, and a
//! `poll(2)` on the descriptor says whether there is anything to take. That is
//! all it does, and it is enough - `nlplug-findfs`'s worker thread tells its
//! main loop it has finished by writing one to an eventfd.

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::cell::RefCell;
use core::sync::atomic::{AtomicBool, Ordering};

use critical_section::Mutex;

use super::{Ino, Inode, Metadata, NodeKind, Ready, VfsError, VfsResult};

/// One eventfd.
pub struct EventFd {
    ino: Ino,
    counter: Mutex<RefCell<u64>>,
    /// Who is asleep waiting for the counter to move. A blocked task sleeps
    /// until someone wakes it - the deadline is "never" - so a writer has to
    /// name the readers it made a difference to.
    waiters: Mutex<RefCell<Vec<crate::sched::TaskId>>>,
    /// `EFD_NONBLOCK`, as the fd's `F_SETFL` keeps it in sync: a read of an
    /// empty counter answers `EAGAIN` instead of sleeping. Without this a
    /// caller that drains the change-signaller after a poll *timeout* - fish's
    /// fd-monitor loop does exactly that, 256 msec at a time - sleeps here
    /// forever while holding the very mutex its main thread is waiting for.
    nonblocking: AtomicBool,
}

impl EventFd {
    pub fn new(ino: Ino, initial: u64, nonblocking: bool) -> Arc<EventFd> {
        Arc::new(EventFd {
            ino,
            counter: Mutex::new(RefCell::new(initial)),
            waiters: Mutex::new(RefCell::new(Vec::new())),
            nonblocking: AtomicBool::new(nonblocking),
        })
    }

    pub fn set_nonblocking(&self, on: bool) {
        self.nonblocking.store(on, Ordering::Relaxed);
    }

    fn value(&self) -> u64 {
        critical_section::with(|cs| *self.counter.borrow(cs).borrow())
    }
}

impl Inode for EventFd {
    fn as_eventfd(&self) -> Option<&EventFd> {
        Some(self)
    }

    /// An event counter has no position: `lseek(2)` on one is `ESPIPE`.
    fn seekable(&self) -> bool {
        false
    }

    fn metadata(&self) -> Metadata {
        Metadata {
            // Event descriptors are anonymous; the number only has to be one
            // nothing else claims.
            ino: 0x5000_0000 + self.ino,
            kind: NodeKind::CharDev,
            size: 0,
        }
    }

    /// A read takes the counter and leaves zero behind - the semaphore shape,
    /// which is the only mode this kernel offers (no `EFD_SEMAPHORE`).
    fn read_at(&self, _offset: u64, buf: &mut [u8]) -> VfsResult<usize> {
        if buf.len() < 8 {
            return Err(VfsError::Invalid);
        }
        loop {
            let taken = critical_section::with(|cs| {
                let mut counter = self.counter.borrow(cs).borrow_mut();
                if *counter == 0 {
                    None
                } else {
                    let value = *counter;
                    *counter = 0;
                    Some(value)
                }
            });
            if let Some(value) = taken {
                buf[..8].copy_from_slice(&value.to_ne_bytes());
                return Ok(8);
            }
            if self.nonblocking.load(Ordering::Relaxed) {
                return Err(VfsError::Again);
            }
            // The wait a signal may end: a `SIGALRM` or a child's death is
            // what brings an event-loop caller back to look at its own state,
            // and sleeping through it would leave it parked on a counter
            // nobody has raised.
            self.wait_nonzero()?;
        }
    }

    fn write_at(&self, _offset: u64, buf: &[u8]) -> VfsResult<usize> {
        if buf.len() < 8 {
            return Err(VfsError::Invalid);
        }
        let mut bytes = [0u8; 8];
        bytes.copy_from_slice(&buf[..8]);
        let value = u64::from_ne_bytes(bytes);
        critical_section::with(|cs| {
            let mut counter = self.counter.borrow(cs).borrow_mut();
            // Saturating, as Linux's counter is: adding past the maximum stops
            // rather than wrapping to a number that means nothing.
            *counter = counter.saturating_add(value);
        });
        // Whoever is waiting on the counter looks again. On the host there is
        // no scheduler to wake and nothing asleep, so the list is empty and
        // the loop is not there at all.
        #[cfg(target_os = "none")]
        {
            let waiters = critical_section::with(|cs| self.waiters.borrow(cs).borrow().clone());
            for id in waiters {
                crate::sched::wake(id);
            }
        }
        Ok(8)
    }

    fn poll_ready(&self) -> Ready {
        Ready {
            read: self.value() != 0,
            write: true,
        }
    }
}

impl EventFd {
    /// Block until the counter is non-zero. Kernel-only: a host build has no
    /// scheduler to block on, and its tests read the ready path.
    ///
    /// Interruptible, like every other blocking wait a syscall parks in: a
    /// catchable signal ends it with `EINTR` and the counter untouched, which
    /// is the shape `select(2)`'s man page gives an eventfd for exactly this
    /// reason.
    #[cfg(target_os = "none")]
    fn wait_nonzero(&self) -> VfsResult<()> {
        let me = crate::sched::current();
        critical_section::with(|cs| {
            let mut waiters = self.waiters.borrow(cs).borrow_mut();
            if !waiters.contains(&me) {
                waiters.push(me);
            }
        });
        let end = crate::sched::block_until_interruptible(|| self.value() != 0);
        critical_section::with(|cs| {
            self.waiters.borrow(cs).borrow_mut().retain(|&id| id != me);
        });
        match end {
            crate::sched::WaitEnd::Interrupted => Err(VfsError::Interrupted),
            _ => Ok(()),
        }
    }

    #[cfg(not(target_os = "none"))]
    fn wait_nonzero(&self) -> VfsResult<()> {
        panic!("blocking on an eventfd needs the kernel scheduler");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_write_adds_and_a_read_takes_everything() {
        let fd = EventFd::new(1, 0, false);
        assert!(!fd.poll_ready().read, "nothing written yet");
        let mut buf = [0u8; 8];
        assert_eq!(fd.write_at(0, &3u64.to_ne_bytes()).unwrap(), 8);
        assert!(fd.poll_ready().read);
        assert_eq!(fd.read_at(0, &mut buf).unwrap(), 8);
        assert_eq!(u64::from_ne_bytes(buf), 3);
        assert!(!fd.poll_ready().read, "the counter was taken");
    }

    #[test]
    fn a_nonblocking_eventfd_answers_eagain_on_an_empty_counter() {
        // The shape fish's fd-monitor loop depends on: poll says not-readable,
        // the caller drains the signaller anyway, and the read has to come
        // back rather than sleep - the caller holds a lock its main thread
        // needs, and a sleeping drain holds that lock for ever.
        let fd = EventFd::new(3, 0, true);
        let mut buf = [0u8; 8];
        assert!(matches!(fd.read_at(0, &mut buf), Err(VfsError::Again)));
        fd.write_at(0, &1u64.to_ne_bytes()).unwrap();
        assert_eq!(fd.read_at(0, &mut buf).unwrap(), 8);
        assert!(matches!(fd.read_at(0, &mut buf), Err(VfsError::Again)));
    }

    #[test]
    fn an_eventfd_starts_at_its_initial_value_and_saturates() {
        let fd = EventFd::new(2, 7, false);
        let mut buf = [0u8; 8];
        fd.read_at(0, &mut buf).unwrap();
        assert_eq!(u64::from_ne_bytes(buf), 7);

        fd.write_at(0, &u64::MAX.to_ne_bytes()).unwrap();
        fd.write_at(0, &u64::MAX.to_ne_bytes()).unwrap();
        fd.read_at(0, &mut buf).unwrap();
        assert_eq!(u64::from_ne_bytes(buf), u64::MAX, "no wrap");
    }
}
