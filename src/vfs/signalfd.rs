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
/// `ssi_tid`: the `timer_create` handle a `SI_TIMER` expiry came from. It is
/// where `siginfo_t` keeps `si_timerid` - the `_timer` member's first word -
/// and it is **not** where `siginfo_t` keeps a sender's pid, which is why this
/// record has its own offsets: measured on Linux 7.3.4, an `SI_TIMER` record
/// read `ssi_tid` 1 for the second timer created while the same record through
/// a handler's `siginfo_t` read `si_timerid` 1 at raw word 4
/// (`build/scratch/sfdhost/{dump,all}.c`).
const SSI_TID: usize = 24;
/// `ssi_overrun`: how many expiries of that timer found the previous signal
/// still pending - the number `timer_getoverrun` reports, measured equal to it
/// in the same instant (`build/scratch/sfdhost/{dump,life2}.c`). Zero for every
/// other sender.
const SSI_OVERRUN: usize = 32;
const SSI_STATUS: usize = 40;
/// `ssi_int`: the **low word** of the `union sigval`, beside `ssi_ptr`'s eight
/// bytes at 48.
///
/// The two are one field seen twice, which is `signalfd_siginfo`'s shape and
/// not `siginfo_t`'s: Linux fills both for a `sigqueue` or a timer whose
/// payload is an `int` (measured: a `sigqueue` of `0xdeadbeef` read `ssi_int`
/// -559038737 and `ssi_ptr` 0x77441122deadbeef; a timer's 4242 read both
/// slots, `build/scratch/sfdhost/{fields,all}.c`). This kernel wrote only the
/// pointer word, so a caller that reads the `int` view of its payload was
/// handed a zero.
const SSI_INT: usize = 44;
const SSI_PTR: usize = 48;
/// `ssi_utime`/`ssi_stime`: the child's CPU in `USER_HZ` ticks, the `_sigchld`
/// half of the record. Unlike `siginfo_t`, `signalfd_siginfo` is a flat struct
/// with no union, so these sit at their own offsets and are written for every
/// sender (zero for the ones that have no such field).
const SSI_UTIME: usize = 56;
const SSI_STIME: usize = 64;

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

    /// The signal descriptor this node is, if it is one - the shape
    /// [`Inode::as_eventfd`] has, for the same reason: `/proc/<pid>/fdinfo`'s
    /// `mnt_id` line names the *filesystem* a descriptor is on, and a
    /// `signalfd` lives on `anon_inodefs` like the four kinds beside it
    /// (`build/scratch/fdinfo/rel.c`: all five answer `mnt_id: 19`).
    fn as_signalfd(&self) -> Option<&SignalFd> {
        Some(self)
    }

    fn metadata(&self) -> Metadata {
        Metadata {
            ino: 0x6000_0000 + self.ino,
            kind: NodeKind::Anon { mode: 0o600 },
            size: 0,
        }
    }

    /// Linux's name for the descriptor in `/proc/<pid>/fd` (measured,
    /// `.workbuddy/probe_anon_fd_stat.c`).
    fn fd_name(&self) -> Option<alloc::string::String> {
        Some(alloc::string::String::from("anon_inode:[signalfd]"))
    }

    /// The `signalfd` block Linux's `show_fdinfo` adds: the signal set this
    /// descriptor reports, as sixteen hex digits.
    ///
    /// Measured on Linux 7.2.14 (`build/scratch/fdinfo/pertype.c`): a `signalfd`
    /// for `{SIGUSR1, SIGTERM}` (`{10, 15}`) answers
    /// `sigmask:\t0000000000004200`, one for `{1}` answers
    /// `sigmask:\t0000000000000001`, and one for `{1, 31}` answers
    /// `sigmask:\t0000000040000001`. So the field is the wire `sigset_t` -
    /// signal *n* in bit *n-1*, 64 bits, sixteen lowercase hex digits,
    /// zero-padded - not the kernel's internal numbering.
    ///
    /// That layout is already this kernel's: [`super::signal::Pending`] keeps a
    /// signal at bit `n-1` too, and its `from_wire`/`to_wire` are the identity
    /// for exactly that reason, so no shift belongs here. The three measured
    /// masks above are the check: `bit(10) | bit(15)` is `0x4200`.
    fn fdinfo_lines(&self) -> Option<alloc::string::String> {
        Some(alloc::format!("sigmask:\t{:016x}\n", self.mask))
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
                write_record(&mut buf[..SIGINFO_BYTES], sig, &info);
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

/// Render one record into the wire's 128 bytes.
///
/// This is separate from [`SignalFd::read_at`] because it is the whole of the
/// ABI in one place and it can be a host test: the offsets belong to
/// `signalfd_siginfo`, whose layout is **not** `siginfo_t`'s (this struct is
/// flat, so `ssi_addr` sits at 72 rather than sharing offset 16 with the
/// sender's `si_pid`/`si_uid`), and a wrong offset is invisible to a caller
/// until it reads a zero where a number should be.
///
/// The three fields this kernel used to leave zero are the `_timer` member and
/// the payload's int view - written here from the record's own fields, which
/// the timer's expiry fills (`docs/notes/` §6.166, measured against Linux
/// 7.3.4 in `build/scratch/sfdhost/`).
fn write_record(buf: &mut [u8], sig: u8, info: &signal::SigInfo) {
    buf.fill(0);
    buf[SSI_SIGNO..SSI_SIGNO + 4].copy_from_slice(&(sig as i32).to_ne_bytes());
    buf[SSI_ERRNO..SSI_ERRNO + 4].copy_from_slice(&info.errno.to_ne_bytes());
    buf[SSI_CODE..SSI_CODE + 4].copy_from_slice(&info.code.to_ne_bytes());
    buf[SSI_PID..SSI_PID + 4].copy_from_slice(&info.pid.to_ne_bytes());
    buf[SSI_UID..SSI_UID + 4].copy_from_slice(&info.uid.to_ne_bytes());
    // `ssi_status`: the child's exit code or the signal that ended it, for the
    // one sender that has one (`signal::SigInfo::child` is where Linux's
    // `CLD_EXITED`/`CLD_KILLED` pair is decided). Zero for every other sender,
    // which is Linux's answer too.
    buf[SSI_STATUS..SSI_STATUS + 4].copy_from_slice(&info.status.to_ne_bytes());
    // The child's CPU, in the `_sigchld` member's own slots. Zero for every
    // other sender, whose record has no such field.
    buf[SSI_UTIME..SSI_UTIME + 8].copy_from_slice(&info.utime.to_ne_bytes());
    buf[SSI_STIME..SSI_STIME + 8].copy_from_slice(&info.stime.to_ne_bytes());
    // The `union sigval` sits where the ABI puts it, so a timer's payload
    // survives the trip through the wire - and the low word of it again in
    // `ssi_int`, because this struct is flat and Linux fills both views of the
    // one field.
    buf[SSI_PTR..SSI_PTR + 8].copy_from_slice(&info.value.to_ne_bytes());
    buf[SSI_INT..SSI_INT + 4].copy_from_slice(&(info.value as u32 as i32).to_ne_bytes());
    // `ssi_tid`/`ssi_overrun`: the `_timer` member, filled for a
    // `timer_create` expiry and zero for every other class (Linux's own answer
    // - a `setitimer` expiry carries neither, because it has no handle).
    buf[SSI_TID..SSI_TID + 4].copy_from_slice(&info.tid.to_ne_bytes());
    buf[SSI_OVERRUN..SSI_OVERRUN + 4].copy_from_slice(&info.overrun.to_ne_bytes());
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

#[cfg(all(test, not(target_os = "none")))]
mod tests {
    use super::*;

    /// The `sigmask` line is the wire `sigset_t`: sixteen zero-padded hex
    /// digits, signal *n* in bit *n-1*.
    ///
    /// Measured on Linux 7.2.14 (`build/scratch/fdinfo/pertype.c`):
    /// `{SIGUSR1, SIGTERM}` = `{10, 15}` read
    /// `sigmask:\t0000000000004200`; `{1}` read `...0001`; `{1, 31}` read
    /// `...0000000040000001`. The three are the check that no shift belongs
    /// here - `bit(10) | bit(15)` is `0x4200` in this kernel's own numbering.
    #[test]
    fn the_sigmask_line_is_the_wire_sigset() {
        let mut mask: signal::Pending = 0;
        signal::add(&mut mask, 10);
        signal::add(&mut mask, 15);
        let fd = SignalFd::new(1, mask, false);
        assert_eq!(fd.fdinfo_lines().unwrap(), "sigmask:\t0000000000004200\n");

        let mut one: signal::Pending = 0;
        signal::add(&mut one, 1);
        let fd = SignalFd::new(2, one, false);
        assert_eq!(fd.fdinfo_lines().unwrap(), "sigmask:\t0000000000000001\n");

        // Signal 31 lands in bit 30, which is the high half of the low word:
        // a renderer that printed bit `n` instead of `n-1` would answer
        // `80000000`.
        let mut high: signal::Pending = 0;
        signal::add(&mut high, 1);
        signal::add(&mut high, 31);
        let fd = SignalFd::new(3, high, false);
        assert_eq!(fd.fdinfo_lines().unwrap(), "sigmask:\t0000000040000001\n");
    }

    /// The `_timer` member and the payload's int view are written where
    /// `signalfd_siginfo` keeps them, and **not** where `siginfo_t` keeps a
    /// sender's pid/uid - this struct is flat, so the two disagree.
    ///
    /// Measured on Linux 7.3.4 (`build/scratch/sfdhost/dump.c`): an `SI_TIMER`
    /// record for the timer whose handle is 3 read raw words
    /// `[6]=3` (`ssi_tid`, offset 24), `[11]=16962` (`ssi_int`, offset 44) and
    /// `[12]=16962` (`ssi_ptr`, offset 48). This kernel wrote none of the first
    /// two, so a reader was handed zero where its timer handle and its payload
    /// belonged.
    #[test]
    fn the_timer_record_is_written_where_signalfd_keeps_it() {
        let info = signal::SigInfo::timer(signal::SIGRTMIN + 3, 3, 7, 0x4242);
        let mut buf = [0xa5u8; SIGINFO_BYTES];
        write_record(&mut buf, signal::SIGRTMIN + 3, &info);

        let word = |at: usize| u32::from_ne_bytes(buf[at..at + 4].try_into().unwrap());
        let dword = |at: usize| u64::from_ne_bytes(buf[at..at + 8].try_into().unwrap());

        assert_eq!(word(SSI_CODE), signal::SI_TIMER as u32);
        assert_eq!(
            word(SSI_TID),
            3,
            "ssi_tid is the timer_create handle, at offset 24"
        );
        assert_eq!(
            word(SSI_OVERRUN),
            7,
            "ssi_overrun is timer_getoverrun's number, at offset 32"
        );
        assert_eq!(
            word(SSI_INT),
            0x4242,
            "ssi_int is the payload's low word, at offset 44"
        );
        assert_eq!(
            dword(SSI_PTR),
            0x4242,
            "and the same field again as a pointer, at 48"
        );
        // The sender's slots are not the timer's: a record that wrote the
        // handle at 12 or 16 would be describing a pid and a uid.
        assert_eq!(word(SSI_PID), 0);
        assert_eq!(word(SSI_UID), 0);
        // Zero-fill first, so a stub that leaves a slot alone reddens here.
        // `ssi_addr_lsb` sits at 80 and is zero for every class Linux delivers
        // to a signalfd (a synchronous fault cannot be queued to one).
        assert_eq!(word(80), 0);
        assert_eq!(buf[120..128], [0u8; 8]);
    }

    /// A class without the `_timer` member leaves both of its slots zero, and
    /// a `sigqueue` payload still reads back through `ssi_int`.
    #[test]
    fn the_other_classes_leave_the_timer_slots_alone() {
        let mut buf = [0u8; SIGINFO_BYTES];
        write_record(
            &mut buf,
            signal::SIGALRM,
            &signal::SigInfo::kernel(signal::SIGALRM),
        );
        let word = |at: usize| u32::from_ne_bytes(buf[at..at + 4].try_into().unwrap());
        assert_eq!(word(SSI_CODE), signal::SI_KERNEL as u32);
        assert_eq!(word(SSI_TID), 0, "a setitimer expiry has no handle");
        assert_eq!(word(SSI_OVERRUN), 0);

        let mut buf = [0u8; SIGINFO_BYTES];
        let q = signal::SigInfo::queue(signal::SIGUSR1, 7, 0, 0xdead_beef);
        write_record(&mut buf, signal::SIGUSR1, &q);
        let word = |at: usize| u32::from_ne_bytes(buf[at..at + 4].try_into().unwrap());
        assert_eq!(word(SSI_CODE), signal::SI_QUEUE as u32);
        assert_eq!(
            word(SSI_INT) as i32,
            -559_038_737,
            "0xdeadbeef reads as the int view too"
        );
    }
}
