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
//! **A child's death is one of those signals and nothing else**, which is what
//! this descriptor used to get wrong. The exit queues a real `SIGCHLD` — with
//! Linux's own `CLD_EXITED`/`CLD_KILLED` record, the status and the child's uid
//! — when the parent wants the signal at all (a handler, or `SIGCHLD` blocked;
//! `.workbuddy/probe_signalfd_chld.c` measures all four dispositions), and the
//! read below hands *that* over like any other queued signal. What it must not
//! do is invent a second record out of the pending-exit table `wait(2)` works
//! from: a parent that never asked to be signalled — an unblocked `SIG_DFL`,
//! the default — gets no signal on Linux, so it gets none here either, while
//! the corpse is still collected by `wait(2)`. One death is one record, and the
//! one table is the pending queue.

use alloc::sync::Arc;

use super::{Ino, Inode, Metadata, NodeKind, Ready, VfsError, VfsResult};
use crate::sched::signal;

/// The `struct signalfd_siginfo` a read produces: 128 bytes, of which the
/// first words carry the signal number, its code, the pid it came from, and —
/// for a `SI_TIMER` expiry — the overrun count and the `sigval` word, so a
/// caller that waits here for a timer sees what a handler would have seen.
const SIGINFO_BYTES: usize = 128;
/// Where the `signalfd_siginfo` puts the signal number, the code, the pid, the
/// uid, the status and the `sigval`: Linux's own offsets, which are *not*
/// `siginfo_t`'s (the pid is at 12 here and 16 there).
///
/// `SSI_STATUS` is a field of its own here while `si_status` and `si_value`
/// share one word of `siginfo_t`'s union — so this record can carry both, and
/// does: a child's death fills the status and leaves the payload zero, a
/// `sigqueue` the reverse.
const SSI_SIGNO: usize = 0;
const SSI_ERRNO: usize = 4;
const SSI_CODE: usize = 8;
const SSI_PID: usize = 12;
const SSI_UID: usize = 16;
const SSI_STATUS: usize = 40;
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

    /// Whether anything this descriptor watches is waiting to be read: a
    /// signal that is pending *and* blocked by the caller *and* named in this
    /// mask. Nothing else — a child's death is a queued signal or it is not
    /// this descriptor's business.
    #[cfg(target_os = "none")]
    fn pending_any(&self) -> bool {
        crate::sched::current_pending() & crate::sched::current_blocked() & self.mask != 0
    }

    #[cfg(not(target_os = "none"))]
    fn pending_any(&self) -> bool {
        false
    }

    /// Take the next signal this descriptor is meant to report, with the
    /// record its sender left.
    #[cfg(target_os = "none")]
    fn take_signal(&self) -> Option<(u8, signal::SigInfo)> {
        // Only signals the caller has *blocked* travel through here; a signal
        // in the mask that is not blocked runs its disposition instead, which
        // is what `signalfd(2)` requires of a caller and what keeps this
        // descriptor from stealing a signal that was meant for a handler.
        let blocked = self.mask & crate::sched::current_blocked();
        crate::sched::take_signal_in_set(blocked)
    }

    #[cfg(not(target_os = "none"))]
    fn take_signal(&self) -> Option<(u8, signal::SigInfo)> {
        None
    }
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
            if let Some((sig, info)) = self.take_signal() {
                buf[..SIGINFO_BYTES].fill(0);
                buf[SSI_SIGNO..SSI_SIGNO + 4].copy_from_slice(&(sig as i32).to_ne_bytes());
                buf[SSI_ERRNO..SSI_ERRNO + 4].copy_from_slice(&info.errno.to_ne_bytes());
                buf[SSI_CODE..SSI_CODE + 4].copy_from_slice(&info.code.to_ne_bytes());
                buf[SSI_PID..SSI_PID + 4].copy_from_slice(&info.pid.to_ne_bytes());
                buf[SSI_UID..SSI_UID + 4].copy_from_slice(&info.uid.to_ne_bytes());
                // `ssi_status`: the child's exit code or the signal that ended
                // it, for the one sender that has one (`signal::SigInfo::child`
                // is where Linux's `CLD_EXITED`/`CLD_KILLED` pair is decided).
                // Zero for every other sender, which is Linux's answer too.
                buf[SSI_STATUS..SSI_STATUS + 4].copy_from_slice(&info.status.to_ne_bytes());
                // The `union sigval` sits where the ABI puts it, so a timer's
                // payload survives the trip through the wire.
                buf[SSI_PTR..SSI_PTR + 8].copy_from_slice(&info.value.to_ne_bytes());
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
}
