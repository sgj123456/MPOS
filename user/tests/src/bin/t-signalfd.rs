//! t-signalfd - signals delivered as a file descriptor.
//!
//! Pinned here, per signalfd(2): `fd` of -1 makes a new descriptor, the create
//! flags are the `SFD_*` pair, the record is the 128-byte
//! `struct signalfd_siginfo` and a shorter read buffer is `EINVAL`, an empty
//! nonblocking read is `EAGAIN`, a child's exit arrives as `SIGCHLD`/`CLD_EXITED`
//! naming the child, the read consumes the signal so the pid is reported once,
//! and the read does **not** collect the child - `wait(2)` still has to, which
//! is the split the call exists for.
//!
//! And the contract beyond child exits, which is what makes the call useful to
//! anything but a `wait` loop: a signal that is **blocked by the reading task
//! and named in the mask** is reported through the descriptor, with the record
//! its sender left - a `sigqueue`'s `si_code` and payload included. A signal
//! in the mask that is *not* blocked is left alone (it runs its disposition),
//! and a signal outside the mask is never touched. That is the behaviour a
//! compositor depends on: weston's libwayland blocks `SIGTERM` and learns from
//! this descriptor that it has been asked to quit, so a kernel that reported
//! only `SIGCHLD` here left `kill <weston>` with nothing to notice.
//!
//! # The record, and the two spellings
//!
//! The no-std version read the record through hand-written byte offsets -
//! `ssi_pid` at byte 12, which the file's own check named against the
//! `siginfo_t` offset 16 that `waitid` writes. The migrated program reads the
//! machine's own `libc::signalfd_siginfo` instead, with the record's *length*
//! pinned against the ABI's 128 as its own claim, which is what `t-waitid` did
//! for the other record; the two records' disagreement is still what the
//! child's-pid claim says, because it is what a reader of either needs to know.
//!
//! The door is `libc::signalfd`, and it is one spelling rather than two: this
//! libc binds no `signalfd4` at all (measured), and its `signalfd` *is* the
//! modern syscall - the crate's own `libc.a` shows it passing `SYS_signalfd4`
//! (74 here, 289 on x86_64) with the size word 8, the same shape on both
//! machines. So the flag-wordless form this program pins at the end reaches the
//! *same* handler as the flagged one, where the no-std version's "plain
//! spelling" reached the legacy `signalfd` syscall (`nr_aarch64.rs`:
//! `SIGNALFD = 64024; // no arm64 number`). The mask travels as the libc's own
//! `sigset_t` built with `sigaddset`, whose *first* word is the wire's mask -
//! which is why the size handed to the kernel is 8 and the record's other 120
//! bytes never leave this side.
//!
//! # The fixture moved, and the reason is a divergence
//!
//! The no-std version never blocked `SIGCHLD`, and on this kernel it did not
//! have to: a child's exit is reported whatever the caller's mask says, so the
//! record was readable either way. Linux discards a generated signal that is
//! *unblocked* and whose disposition is the ignoring default, and `SIGCHLD`'s
//! default action is exactly that: measured on this host, the same pair answers
//! `-1/EAGAIN` unblocked and the 128-byte record blocked. So the block is part
//! of this program's fixture now - which is also what the file's own contract
//! for the *other* half already said, that a signal is reported here when it is
//! blocked by the reading task and named in the mask.
//!
//! With the block in place a second divergence shows, and the drain below is
//! shaped around it (`docs/uapi/` §5.2 carries both): this kernel keeps a
//! child's exit in one table and queues a generated `SIGCHLD` in another when
//! the caller has it blocked, so one death is handed over as **two** records -
//! the queued copy first, whose `ssi_code` is `SI_USER`, and then the exit
//! record itself, `CLD_EXITED` with the child's pid. Linux's queue is the one
//! table, so its single record for the death is `CLD_EXITED`. The count is
//! therefore the claim that differs between the machines, and it says so with
//! the note it prints; the claims about the *exit* record are made against the
//! last record the descriptor hands over, which is the exit on both.
//!
//! The rest of the program is the named calls: the reads are
//! `rustix::io::read` (a plain `read`, with the `EINVAL` for a short buffer and
//! the `EAGAIN` for an empty one as its answers), the block and the unblock are
//! `libc::sigprocmask`, the payload is `libc::sigqueue`, the child leaves
//! through the raw `SYS_exit_group` (a forked std child's inherited stdout must
//! not be flushed), the reaping is `rustix::process::waitpid`, and the
//! close-on-exec read-back is `rustix`'s `fcntl_getfd`/`FdFlags` - the pair
//! `t-fcntl` pins.

use mpos_user_tests::Report;
use rustix::fd::{AsRawFd, OwnedFd};
use rustix::io::{Errno, FdFlags, fcntl_getfd, read};
use rustix::process::{Pid, WaitOptions, WaitStatus, waitpid};
use std::mem::size_of;
use std::os::fd::FromRawFd;

/// The wire `sigset_t` as the libc spells it, built through the libc's own
/// `sigaddset` so the bit arithmetic stays the machine's business.
fn set_of(signals: &[i32]) -> libc::sigset_t {
    let mut set: libc::sigset_t = unsafe { std::mem::zeroed() };
    for &sig in signals {
        // SAFETY: `set` is a `sigset_t` this process owns and `sig` is one of
        // the numbers above.
        unsafe { libc::sigaddset(&mut set, sig) };
    }
    set
}

/// `signalfd(fd=-1, mask, flags)`: -1 means "make a new descriptor", which is
/// the one form this program uses.
fn signalfd_new(mask: &libc::sigset_t, flags: i32) -> Option<OwnedFd> {
    // SAFETY: a set this process owns, and -1 for the descriptor word.
    let raw = unsafe { libc::signalfd(-1, mask, flags) };
    if raw < 0 {
        None
    } else {
        // SAFETY: the call just handed this process a fresh descriptor, and
        // `raw` is the only copy of the number.
        Some(unsafe { OwnedFd::from_raw_fd(raw) })
    }
}

/// `sigprocmask(how, set)`, answering whether it succeeded.
fn mask_change(how: libc::c_int, set: &libc::sigset_t) -> bool {
    // SAFETY: a set this process owns, and no old set is wanted.
    unsafe { libc::sigprocmask(how, set, std::ptr::null_mut()) == 0 }
}

/// Sleep out a whole number of milliseconds; only the bounded retry that waits
/// for a forked child to be scheduled calls it.
fn sleep_ms(ms: i64) {
    let req = libc::timespec {
        tv_sec: ms / 1000,
        tv_nsec: (ms % 1000) * 1_000_000,
    };
    // SAFETY: a `timespec` this call owns, and the second pointer is null.
    unsafe { libc::nanosleep(&req, std::ptr::null_mut()) };
}

/// Leave through `exit_group`: the code is the claim, and the child's inherited
/// copy of the parent's stdout buffer must not be flushed.
fn leave(code: i32) -> ! {
    // SAFETY: the door does not return; `_exit` is the maintained fallback.
    unsafe {
        libc::syscall(libc::SYS_exit_group, code as libc::c_long);
        libc::_exit(code)
    }
}

/// The record a `signalfd` read returns, read out of the 128 bytes the kernel
/// wrote. The type is the machine's own and its length is pinned by its own
/// claim above.
fn record_of(buf: &[u8; 128]) -> libc::signalfd_siginfo {
    // SAFETY: `buf` holds the record the kernel just wrote, and
    // `signalfd_siginfo` is that record's type on this machine.
    unsafe { std::ptr::read_unaligned(buf.as_ptr().cast::<libc::signalfd_siginfo>()) }
}

fn main() {
    println!("t-signalfd");
    let mut r = Report::new();

    // The record is the ABI's 128 bytes, not a number written here.
    r.check(
        "the signalfd record is the size the ABI names (128)",
        size_of::<libc::signalfd_siginfo>() == 128,
    );

    let child_mask = set_of(&[libc::SIGCHLD]);
    let Some(fd) = signalfd_new(&child_mask, libc::SFD_NONBLOCK) else {
        r.check(
            "signalfd(-1, mask, SFD_NONBLOCK) hands back a descriptor",
            false,
        );
        r.finish();
    };
    r.check(
        "signalfd(-1, mask, SFD_NONBLOCK) hands back a descriptor",
        true,
    );

    // The read's own validation: fewer than 128 bytes is EINVAL, and an empty
    // nonblocking descriptor is EAGAIN rather than a park.
    let mut eight = [0u8; 8];
    r.check(
        "an 8-byte read answers EINVAL (128 are required)",
        read(&fd, &mut eight) == Err(Errno::INVAL),
    );
    let mut buf = [0u8; 128];
    r.check(
        "a read with no child exited answers EAGAIN",
        read(&fd, &mut buf) == Err(Errno::AGAIN),
    );

    // A child exits; its death becomes readable through the descriptor.
    // `SIGCHLD` is *blocked* first, and that is this migration's one fixture
    // change: Linux discards a generated signal that is unblocked and whose
    // disposition is the ignoring default - which is `SIGCHLD`'s - so a
    // descriptor that is to see the exit needs the signal held. Measured on
    // this host with the pair a child's exit makes: unblocked the read answers
    // `-1/EAGAIN`, blocked it answers the 128-byte record.
    r.check(
        "blocking SIGCHLD before the fork succeeds",
        mask_change(libc::SIG_BLOCK, &child_mask),
    );
    // SAFETY: the child exits immediately and allocates nothing.
    let kid = unsafe { libc::fork() };
    if kid == 0 {
        leave(7);
    }
    r.check("a child was forked", kid > 0);
    if kid > 0 {
        // The drain, because the two machines disagree about how many records
        // one death makes (`docs/uapi/` §5.2): Linux reports it once, out of
        // the pending queue, with `ssi_code` `CLD_EXITED`; this kernel keeps
        // the exit in its own table *and* queues a generated `SIGCHLD` for a
        // caller that blocked it, so it hands over the queued copy (an
        // `SI_USER` record) and then the exit. Reading every record the
        // descriptor has is what lets the claims about the *exit* record be
        // made on both machines - it is the last one either way - and the
        // count, which is where they differ, is claimed below with a note.
        let mut got = Err(Errno::AGAIN);
        let mut tries = 0;
        while tries < 200 {
            got = read(&fd, &mut buf);
            if got == Ok(128) {
                break;
            }
            sleep_ms(1);
            tries += 1;
        }
        r.check(
            "the child's exit arrives as a 128-byte record",
            got == Ok(128),
        );
        let mut records = 0u32;
        let mut queued_code = 0;
        let mut exit = record_of(&buf);
        while records < 4 {
            if records == 1 {
                // The second pass: the record read first was the queued copy,
                // and the one in `buf` now is the exit.
                queued_code = exit.ssi_code;
            }
            records += 1;
            exit = record_of(&buf);
            if read(&fd, &mut buf) != Ok(128) {
                break;
            }
        }
        r.check(
            "the record's signo is SIGCHLD",
            exit.ssi_signo == libc::SIGCHLD as u32,
        );
        r.check(
            "the exit record's code is CLD_EXITED",
            exit.ssi_code == libc::CLD_EXITED,
        );
        r.check(
            "the exit record names the child (ssi_pid, not siginfo_t's si_pid)",
            exit.ssi_pid == kid as u32,
        );
        r.check(
            "a child's death is reported once",
            records == if mpos_user_tests::host_run() { 1 } else { 2 },
        );
        if records > 1 {
            println!(
                "note: {records} records for one death - the first carries `ssi_code` \
                 {queued_code}, this kernel's queued generated SIGCHLD, where Linux's \
                 one record for the death is `CLD_EXITED`"
            );
        }
        r.check(
            "and the descriptor is empty afterwards (the next read is EAGAIN)",
            read(&fd, &mut buf) == Err(Errno::AGAIN),
        );

        // The read does not collect the child: wait(2) still must. The pid
        // comparison is `t-wait4`'s: `Some(got) == want` is the "and it was
        // that child" half, and `None` covers every other answer.
        let want = Pid::from_raw(kid);
        let reaped = match waitpid(want, WaitOptions::empty()) {
            Ok(Some((got, status))) if Some(got) == want => Some(status),
            _ => None,
        };
        r.check(
            "wait4 still reaps the child the signalfd reported",
            reaped.is_some(),
        );
        r.check(
            "and the exit code is 7",
            reaped.and_then(WaitStatus::exit_status) == Some(7),
        );
    }

    // The blocked-signal half of the contract: a signal that is blocked *and*
    // named in the mask is reported here, with the record its sender left. This
    // is the shape weston depends on for `SIGTERM` - and the shape a kernel
    // that reported only child exits never produced.
    {
        let term_mask = set_of(&[libc::SIGTERM]);
        let Some(sfd) = signalfd_new(&term_mask, libc::SFD_NONBLOCK) else {
            r.check("a descriptor watching SIGTERM is made", false);
            r.finish();
        };
        r.check("a descriptor watching SIGTERM is made", true);

        // Block it, then queue it to ourselves with a payload. A signal that
        // is *not* blocked must not arrive here (it would run its default
        // action), so blocking is what makes this read well-defined - which is
        // exactly what signalfd(2) requires of the caller.
        r.check(
            "blocking SIGTERM succeeds",
            mask_change(libc::SIG_BLOCK, &term_mask),
        );
        r.check(
            "sigqueue of the blocked SIGTERM succeeds",
            queue_self(libc::SIGTERM, 0x5150_5150_5150_5150),
        );

        let n = read(&sfd, &mut buf);
        r.check(
            "the blocked signal arrives through the descriptor",
            n == Ok(128),
        );
        let info = record_of(&buf);
        r.check(
            "the record's signo is SIGTERM",
            n == Ok(128) && info.ssi_signo == libc::SIGTERM as u32,
        );
        r.check(
            "the record's code is SI_QUEUE, the sender's own",
            n == Ok(128) && info.ssi_code == libc::SI_QUEUE,
        );
        r.check(
            "the signal was consumed by the read, so the next is EAGAIN",
            read(&sfd, &mut buf) == Err(Errno::AGAIN),
        );

        // Nothing is pending now, so unblocking runs no handler and the
        // default action never gets the chance to end this test.
        r.check(
            "unblocking the now-consumed SIGTERM succeeds",
            mask_change(libc::SIG_UNBLOCK, &term_mask),
        );
    }

    // The flag-wordless form makes a descriptor too - the same handler as the
    // flagged one here (see the door note above), and the form a caller that
    // wants the descriptor's own blocking behaviour uses.
    let plain = signalfd_new(&child_mask, 0);
    r.check(
        "the plain signalfd(fd=-1) makes a descriptor",
        plain.is_some(),
    );

    // The flag word has exactly two bits - `SFD_NONBLOCK` and `SFD_CLOEXEC`.
    // An unmodelled bit is refused with `EINVAL` rather than silently dropped,
    // and `SFD_CLOEXEC` sets the descriptor's close-on-exec, which the kernel
    // used to lose (a compositor that installs a signalfd and then execs a
    // helper would leak it across).
    {
        // SAFETY: a set this process owns and -1 for the descriptor word.
        let refused = unsafe { libc::signalfd(-1, &child_mask, 0x1) };
        let err = std::io::Error::last_os_error().raw_os_error().unwrap_or(0);
        r.check(
            "signalfd with an unknown flag answers EINVAL",
            refused == -1 && err == libc::EINVAL,
        );
        if let Some(clo) = signalfd_new(&child_mask, libc::SFD_CLOEXEC) {
            r.check("signalfd(SFD_CLOEXEC) hands back a descriptor", true);
            r.check(
                "and the descriptor came back close-on-exec",
                fcntl_getfd(&clo).is_ok_and(|f| f.contains(FdFlags::CLOEXEC)),
            );
        } else {
            r.check("signalfd(SFD_CLOEXEC) hands back a descriptor", false);
        }
    }

    // Nothing is left to close: every descriptor above is an `OwnedFd`, and
    // the two that were only borrowed (`plain`) go the same way as this scope
    // ends. `plain`'s own number is deliberately unused here - `as_raw_fd` is
    // how the suite names a descriptor it wants nothing from.
    let _ = plain.as_ref().map(AsRawFd::as_raw_fd);

    r.finish()
}

/// `sigqueue` to this process, answering whether the libc reported success.
fn queue_self(sig: i32, value: u64) -> bool {
    let val = libc::sigval {
        sival_ptr: value as usize as *mut libc::c_void,
    };
    // SAFETY: signalling this process, whose delivery goes to the signalfd
    // watching `sig` because the signal is blocked.
    unsafe { libc::sigqueue(libc::getpid(), sig, val) == 0 }
}
