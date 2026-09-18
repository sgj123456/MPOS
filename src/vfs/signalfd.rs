//! `signalfd(2)`: signals delivered as a file descriptor.
//!
//! A program that would rather read its signals than run a handler blocks them
//! and reads them here, which lets one loop serve both its files and its
//! children. `nlplug-findfs` does exactly that for `SIGCHLD`: its poll loop
//! learns from this descriptor that a helper it started has exited, and then
//! reaps it.
//!
//! What this descriptor reports is child exits, and only those. That is not a
//! stand-in for the full call - it is the one signal this kernel has that must
//! be waited for rather than handled in place, and the mask the caller passes
//! is honoured in the only way it can be here: signals outside it are not
//! reported.

use alloc::sync::Arc;

use super::{Ino, Inode, Metadata, NodeKind, Ready, VfsError, VfsResult};

/// The `struct signalfd_siginfo` a read produces: 128 bytes, of which the
/// first words carry the signal number, its code and the pid it came from.
const SIGINFO_BYTES: usize = 128;
const SIGCHLD: i32 = 17;
const CLD_EXITED: i32 = 1;

pub struct SignalFd {
    ino: Ino,
    /// `SFD_NONBLOCK`: an empty read answers `EAGAIN` instead of parking.
    /// nlplug-findfs opens its descriptor nonblocking and reads it in a
    /// drain loop - a blocking read there would park with the queue empty
    /// and nothing left to wake it for a while.
    nonblock: core::sync::atomic::AtomicBool,
}

impl SignalFd {
    pub fn new(ino: Ino, nonblock: bool) -> Arc<SignalFd> {
        Arc::new(SignalFd {
            ino,
            nonblock: core::sync::atomic::AtomicBool::new(nonblock),
        })
    }

    /// The child whose exit is waiting to be reported, if any. Nothing is
    /// collected here: `wait(2)` is what collects, and this only looks.
    #[cfg(target_os = "none")]
    fn pending_child(&self) -> Option<usize> {
        crate::sched::child_exited_unreaped()
    }

    #[cfg(not(target_os = "none"))]
    fn pending_child(&self) -> Option<usize> {
        // A host build has no scheduler and no children; the interesting
        // behaviour is the kernel's, and the shape is what is tested here.
        None
    }
}

impl Inode for SignalFd {
    fn metadata(&self) -> Metadata {
        Metadata {
            ino: 0x6000_0000 + self.ino as u64,
            kind: NodeKind::CharDev,
            size: 0,
        }
    }

    fn read_at(&self, _offset: u64, buf: &mut [u8]) -> VfsResult<usize> {
        if buf.len() < SIGINFO_BYTES {
            return Err(VfsError::Invalid);
        }
        loop {
            // Reading a signal descriptor *consumes* the signal: the same
            // pid is never reported twice, which is what lets a caller's
            // drain loop (`while read > 0`) terminate. The exit code stays
            // on file - `wait(2)` is still what collects the child.
            if let Some(pid) = Self::take_sigchld() {
                buf[..SIGINFO_BYTES].fill(0);
                // ssi_signo, ssi_errno, ssi_code, ssi_pid - the first four
                // words, which is as much of the struct a caller reads to
                // decide what happened.
                buf[0..4].copy_from_slice(&SIGCHLD.to_ne_bytes());
                buf[8..12].copy_from_slice(&CLD_EXITED.to_ne_bytes());
                buf[12..16].copy_from_slice(&(pid as u32).to_ne_bytes());
                return Ok(SIGINFO_BYTES);
            }
            if self.nonblock.load(core::sync::atomic::Ordering::Relaxed) {
                return Err(VfsError::Again);
            }
            self.wait_for_child();
        }
    }

    fn poll_ready(&self) -> Ready {
        Ready {
            read: self.pending_child().is_some(),
            // There is nothing to write to a signal descriptor; a caller that
            // polls for it never gets an answer, which is what Linux does.
            write: false,
        }
    }
}

impl SignalFd {
    #[cfg(target_os = "none")]
    fn wait_for_child(&self) {
        crate::sched::block_until(|| self.pending_child().is_some());
    }

    #[cfg(not(target_os = "none"))]
    fn wait_for_child(&self) {
        panic!("blocking on a signalfd needs the kernel scheduler");
    }

    /// Take the next unreported `SIGCHLD`, or `None` when there is none.
    ///
    /// Paired like [`Self::wait_for_child`] and for the same reason: the
    /// pending-signal queue lives in the scheduler's task table, which only
    /// exists on a real target. The host build of this module still has to
    /// compile - the VFS is tested there - so the call is routed through a stub
    /// rather than made directly from `read_at`.
    #[cfg(target_os = "none")]
    fn take_sigchld() -> Option<usize> {
        crate::sched::read_sigchld()
    }

    #[cfg(not(target_os = "none"))]
    fn take_sigchld() -> Option<usize> {
        None
    }
}
