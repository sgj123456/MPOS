//! `signalfd(2)`: signals delivered as a file descriptor.
//!
//! A program that would rather read its signals than run a handler blocks them
//! and reads them here, which lets one loop serve both its files and its
//! children. `nlplug-findfs` does exactly that for `SIGCHLD`: its poll loop
//! learns from this descriptor that a helper it started has exited, and then
//! reaps it. A compositor does it for its exit signals: weston's libwayland
//! creates one descriptor per signal with `wl_event_loop_add_signal`, blocks
//! the signal, and learns from the read that it is time to shut down - so a
//! descriptor that reported only child exits left `kill <weston>` with nothing
//! to notice, and a compositor that could not be asked to leave politely.
//!
//! What a read reports is therefore the Linux contract: a signal that is both
//! **blocked by the reading task and named in this descriptor's mask**. A
//! signal in the mask that is *not* blocked is left alone — it runs its
//! disposition normally, which is what "must be blocked for reliable delivery
//! through this descriptor" means in `signalfd(2)` — and a signal outside the
//! mask is never touched here.
//!
//! The child-exit record is the one this kernel produces without the signal
//! machinery: a child's death is tracked as a pending *exit*, not as a queued
//! `SIGCHLD` (see `sched::read_sigchld`), because `wait(2)` is what collects
//! it and a shell that ignores `SIGCHLD` must not be handed one. The record is
//! produced here, unchanged, when the mask names `SIGCHLD`.

use alloc::sync::Arc;

use super::{Ino, Inode, Metadata, NodeKind, Ready, VfsError, VfsResult};
use crate::sched::signal;

/// The `struct signalfd_siginfo` a read produces: 128 bytes, of which the
/// first words carry the signal number, its code, the pid it came from, and —
/// for a `SI_TIMER` expiry — the overrun count and the `sigval` word, so a
/// caller that waits here for a timer sees what a handler would have seen.
const SIGINFO_BYTES: usize = 128;
const SIGCHLD: i32 = 17;
const CLD_EXITED: i32 = 1;
/// Where the `signalfd_siginfo` puts the signal number, the code, the pid and
/// the `sigval`: Linux's own offsets, which are *not* `siginfo_t`'s (the pid
/// is at 12 here and 16 there).
const SSI_SIGNO: usize = 0;
const SSI_ERRNO: usize = 4;
const SSI_CODE: usize = 8;
const SSI_PID: usize = 12;
const SSI_UID: usize = 16;
const SSI_PTR: usize = 48;

pub struct SignalFd {
    ino: Ino,
    /// The signals this descriptor reports, in the kernel's own mask
    /// convention (bit *n* names signal *n*), converted from the wire's
    /// `sigset_t` at creation so the comparison with the pending set is
    /// against one language.
    mask: signal::Pending,
    /// `SFD_NONBLOCK`: an empty read answers `EAGAIN` instead of parking.
    /// nlplug-findfs opens its descriptor nonblocking and reads it in a
    /// drain loop - a blocking read there would park with the queue empty
    /// and nothing left to wake it for a while.
    nonblock: core::sync::atomic::AtomicBool,
}

impl SignalFd {
    pub fn new(ino: Ino, mask: signal::Pending, nonblock: bool) -> Arc<SignalFd> {
        Arc::new(SignalFd {
            ino,
            mask,
            nonblock: core::sync::atomic::AtomicBool::new(nonblock),
        })
    }

    /// The child whose exit is waiting to be reported, if any. Nothing is
    /// collected here: `wait(2)` is what collects, and this only looks.
    #[cfg(target_os = "none")]
    fn pending_child(&self) -> Option<usize> {
        if self.mask & signal::bit(SIGCHLD as u8) == 0 {
            return None;
        }
        crate::sched::child_exited_unreaped()
    }

    #[cfg(not(target_os = "none"))]
    fn pending_child(&self) -> Option<usize> {
        // A host build has no scheduler and no children; the interesting
        // behaviour is the kernel's, and the shape is what is tested here.
        None
    }

    /// Whether anything this descriptor watches is waiting to be read.
    #[cfg(target_os = "none")]
    fn pending_any(&self) -> bool {
        crate::sched::current_pending() & crate::sched::current_blocked() & self.mask != 0
            || self.pending_child().is_some()
    }

    #[cfg(not(target_os = "none"))]
    fn pending_any(&self) -> bool {
        false
    }

    /// Take the next signal this descriptor is meant to report: a blocked
    /// pending signal in the mask first — it carries the `siginfo_t` the
    /// sender left — and the child-exit record after, which exists outside
    /// the pending set and so cannot be "taken" from it.
    #[cfg(target_os = "none")]
    fn take_signal(&self) -> Option<SignalRecord> {
        // Only signals the caller has *blocked* travel through here; a signal
        // in the mask that is not blocked runs its disposition instead, which
        // is what `signalfd(2)` requires of a caller and what keeps this
        // descriptor from stealing a signal that was meant for a handler.
        let blocked = self.mask & crate::sched::current_blocked();
        if let Some((sig, info)) = crate::sched::take_signal_in_set(blocked) {
            return Some(SignalRecord::From(sig, info));
        }
        Self::take_sigchld().map(SignalRecord::ChildExit)
    }

    #[cfg(not(target_os = "none"))]
    fn take_signal(&self) -> Option<SignalRecord> {
        None
    }
}

/// One record a read will render, kept as what it is rather than as bytes so
/// the two sources stay told apart at the point they are written.
enum SignalRecord {
    /// A queued signal, with the record its sender left.
    From(u8, signal::SigInfo),
    /// An unreaped child's exit, the record this kernel makes outside the
    /// signal machinery.
    ChildExit(usize),
}

impl Inode for SignalFd {
    /// A signal queue has no position: `lseek(2)` on one is `ESPIPE`.
    fn seekable(&self) -> bool {
        false
    }

    fn metadata(&self) -> Metadata {
        Metadata {
            ino: 0x6000_0000 + self.ino,
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
            // signal is never reported twice, which is what lets a caller's
            // drain loop (`while read > 0`) terminate.
            if let Some(record) = self.take_signal() {
                buf[..SIGINFO_BYTES].fill(0);
                match record {
                    SignalRecord::ChildExit(pid) => {
                        // ssi_signo, ssi_code, ssi_pid — what a caller reads
                        // to decide that a child is waiting to be reaped.
                        buf[SSI_SIGNO..SSI_SIGNO + 4].copy_from_slice(&SIGCHLD.to_ne_bytes());
                        buf[SSI_CODE..SSI_CODE + 4].copy_from_slice(&CLD_EXITED.to_ne_bytes());
                        // The reader's namespace's number for that child - the
                        // same translation `/proc/<pid>` and `wait4` do, so a
                        // `signalfd` reader inside a namespace is handed a
                        // number it can use.
                        let shown = crate::sched::pid_number_of(crate::sched::TaskId(pid))
                            .unwrap_or(pid as u32);
                        buf[SSI_PID..SSI_PID + 4].copy_from_slice(&shown.to_ne_bytes());
                    }
                    SignalRecord::From(sig, info) => {
                        buf[SSI_SIGNO..SSI_SIGNO + 4].copy_from_slice(&(sig as i32).to_ne_bytes());
                        buf[SSI_ERRNO..SSI_ERRNO + 4].copy_from_slice(&info.errno.to_ne_bytes());
                        buf[SSI_CODE..SSI_CODE + 4].copy_from_slice(&info.code.to_ne_bytes());
                        buf[SSI_PID..SSI_PID + 4].copy_from_slice(&info.pid.to_ne_bytes());
                        buf[SSI_UID..SSI_UID + 4].copy_from_slice(&info.uid.to_ne_bytes());
                        // The `union sigval` sits where the ABI puts it, so a
                        // timer's payload survives the trip through the wire.
                        buf[SSI_PTR..SSI_PTR + 8].copy_from_slice(&info.value.to_ne_bytes());
                    }
                }
                return Ok(SIGINFO_BYTES);
            }
            if self.nonblock.load(core::sync::atomic::Ordering::Relaxed) {
                return Err(VfsError::Again);
            }
            // A wait a catchable signal ends with `EINTR` - and the signal that
            // ends it is *not* consumed by the wait: it is delivered on the way
            // out of the syscall, and a signal the caller had blocked (which is
            // the only kind this descriptor can read) stays queued for the next
            // read. That is Linux's answer for a signalfd reader whose alarm
            // fires, and it is what keeps a loop that drains descriptors from
            // parking on this one for ever.
            self.wait_for_signal()?;
        }
    }

    fn poll_ready(&self) -> Ready {
        Ready {
            read: self.pending_any(),
            // There is nothing to write to a signal descriptor; a caller that
            // polls for it never gets an answer, which is what Linux does.
            write: false,
        }
    }
}

impl SignalFd {
    #[cfg(target_os = "none")]
    fn wait_for_signal(&self) -> VfsResult<()> {
        match crate::sched::block_until_interruptible(|| self.pending_any()) {
            crate::sched::WaitEnd::Interrupted => Err(VfsError::Interrupted),
            _ => Ok(()),
        }
    }

    #[cfg(not(target_os = "none"))]
    fn wait_for_signal(&self) -> VfsResult<()> {
        panic!("blocking on a signalfd needs the kernel scheduler");
    }

    /// Take the next unreported `SIGCHLD`, or `None` when there is none.
    ///
    /// Paired like [`Self::wait_for_signal`] and for the same reason: the
    /// pending-child bookkeeping lives in the scheduler's task table, which
    /// only exists on a real target. The host build of this module still has
    /// to compile - the VFS is tested there - so the call is routed through a
    /// stub rather than made directly from `read_at`.
    #[cfg(target_os = "none")]
    fn take_sigchld() -> Option<usize> {
        crate::sched::read_sigchld()
    }

    #[cfg(not(target_os = "none"))]
    fn take_sigchld() -> Option<usize> {
        None
    }
}
