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
//! # A child's death is a queued `SIGCHLD`, and the fixture blocks it
//!
//! The three divergences this program used to name and work around are closed
//! (`docs/uapi/` §5.1 item 29), and what replaced them is Linux's own shape: a
//! child's death *is* the `SIGCHLD` record - `CLD_EXITED` with the exit code,
//! `CLD_KILLED` with the signal that ended it - queued on the parent's pending
//! set, and this descriptor hands that one record over like any other. Two
//! consequences are claims of their own below, because each is the half a wrong
//! kernel passes the other half of:
//!
//!   * a death the parent never asked to be **signalled** for leaves nothing
//!     here. `SIGCHLD` is queued for a handler, or for a *blocked* signal; an
//!     unblocked `SIG_DFL` parent is told nothing at all (the corpse is still
//!     `wait(2)`'s), and a parent that ignores it gets nothing *and* has the
//!     child reaped for it. Measured on a current host, all four dispositions,
//!     with `.workbuddy/probe_signalfd_chld.c`: unblocked `-1`/`EAGAIN`,
//!     blocked one 128-byte record, `SIG_IGN` nothing and `ECHILD`;
//!   * one death is therefore **one** record, which is what the drain below
//!     used to be shaped around: this kernel kept the exit in a second table
//!     and queued a generated `SIGCHLD` beside it, so one death arrived twice.
//!
//! `si_utime`/`si_stime` are the one part of Linux's record this kernel leaves
//! zero (measured: 6 ticks for a child that burned 60 ms of CPU), and the
//! `CLD_DUMPED`/`WCOREDUMP` half of a signalled death is named rather than
//! answered - both in `docs/uapi/` §5.2.
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
use std::sync::atomic::{AtomicI32, Ordering};

/// What the `SIGCHLD` handler at the end of the program saw. `printf` is not
/// async-signal-safe, so the handler records and the program reads it back -
/// the shape `t-sa-restart`'s handler and `t-udpblock`'s alarm use.
static HANDLED: AtomicI32 = AtomicI32::new(0);
static HANDLED_CODE: AtomicI32 = AtomicI32::new(-1);
static HANDLED_STATUS: AtomicI32 = AtomicI32::new(-1);
static HANDLED_PID: AtomicI32 = AtomicI32::new(-1);

/// The `SIGCHLD` handler: the frame's `siginfo_t`, which is the *same* record
/// the descriptor hands over - the claim is that it carries the same two words,
/// including the one that shares its slot with every other sender's `sigval`.
extern "C" fn chld_handler(_sig: libc::c_int, info: *mut libc::siginfo_t, _ctx: *mut libc::c_void) {
    // SAFETY: the kernel hands a handler a live `siginfo_t` for its signal, and
    // this one only reads the three fields the claim is about.
    unsafe {
        if !info.is_null() {
            HANDLED_CODE.store((*info).si_code, Ordering::Relaxed);
            // `si_pid`/`si_status` are the libc's *accessors* on this target
            // (they read the union), where `si_code` is a plain field - the
            // same split `t-waitid` reads its record through.
            HANDLED_STATUS.store((*info).si_status(), Ordering::Relaxed);
            HANDLED_PID.store((*info).si_pid(), Ordering::Relaxed);
            HANDLED.store(1, Ordering::Relaxed);
        }
    }
}

/// Install [`chld_handler`] with `SA_SIGINFO`, the shape a handler that reads
/// the record's fields needs.
fn install_chld_handler() -> bool {
    let mut sa: libc::sigaction = unsafe { std::mem::zeroed() };
    sa.sa_sigaction = chld_handler
        as extern "C" fn(libc::c_int, *mut libc::siginfo_t, *mut libc::c_void)
        as usize;
    sa.sa_flags = libc::SA_SIGINFO;
    // SAFETY: a `sigaction` this process owns, and no old one is wanted.
    unsafe { libc::sigaction(libc::SIGCHLD, &sa, std::ptr::null_mut()) == 0 }
}

/// Restore `SIG_DFL` for `SIGCHLD`, so the handler cannot outlive its section.
fn restore_chld_default() -> bool {
    let mut sa: libc::sigaction = unsafe { std::mem::zeroed() };
    sa.sa_sigaction = libc::SIG_DFL;
    // SAFETY: as above.
    unsafe { libc::sigaction(libc::SIGCHLD, &sa, std::ptr::null_mut()) == 0 }
}

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

    // A child exits; its death becomes readable through the descriptor, as the
    // one 128-byte record it is. `SIGCHLD` is **blocked** first, and that is
    // Linux's own requirement rather than a fixture this kernel needs: a
    // generated signal is queued for a handler or for a blocked signal, and an
    // unblocked `SIG_DFL` parent is told nothing at all - the claim two
    // sections down, and the one a kernel that reports the exit whatever the
    // mask says fails.
    // The record carries the child's **real** uid, and the only way to tell that
    // from the effective one - or from the zero a field nobody filled carries -
    // is to make the two differ: root may move its own real id while keeping its
    // effective one, and every child forked from here on inherits exactly that.
    // `CAP_SETUID` is what the move needs; the payload has it and a hand host run
    // often does not (this sandbox's shell has an empty effective capability set
    // and is already that uid, so the call below would be a no-op success and the
    // claim would be about nothing), so the claims are made where they can be and
    // **named** where they cannot - `t-waitid`'s shape for the same field, and
    // deliberately not a claim the flag can pass on its own.
    const MOVED: u32 = 1000;
    let mut moved = false;
    if mpos_user_tests::host_run() {
        println!(
            "note: a child's real uid cannot be moved without CAP_SETUID, so the ssi_uid claims \
             below are not made on this host run"
        );
    } else {
        // `u32::MAX` is `setresuid`'s "-1": leave that id alone.
        moved = unsafe { libc::setresuid(MOVED, u32::MAX, u32::MAX) } == 0;
        r.check(
            "the program can move its own real uid (CAP_SETUID) and keep its effective one",
            moved,
        );
    }

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
        // A bounded wait for the record: the retries cover the child's own
        // scheduling, not a second source of records.
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
        let exit = record_of(&buf);
        r.check(
            "the record's signo is SIGCHLD",
            exit.ssi_signo == libc::SIGCHLD as u32,
        );
        r.check("its code is CLD_EXITED", exit.ssi_code == libc::CLD_EXITED);
        r.check(
            "and its ssi_status is the exit code (7)",
            exit.ssi_status == 7,
        );
        r.check(
            "it names the child (ssi_pid, not siginfo_t's si_pid)",
            exit.ssi_pid == kid as u32,
        );
        if moved {
            r.check(
                &format!("and carries the child's real uid, not its effective one ({MOVED})"),
                exit.ssi_uid == MOVED,
            );
        } else {
            println!(
                "note: the record's ssi_uid reads {} - not claimed on this run, where the real \
                 id could not be moved",
                exit.ssi_uid
            );
        }
        r.check(
            "one death is one record (the next read is EAGAIN)",
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

    // ---- a death the parent never asked to be signalled for ----------------
    //
    // With `SIGCHLD` unblocked and at its default disposition, Linux queues
    // nothing: the child is a zombie `wait(2)` collects, and a descriptor
    // watching `SIGCHLD` learns nothing from the death.
    //
    // The **read comes before the reap**, and that order is the claim: the
    // reap is what took the exit out of the second table this kernel used to
    // keep for it, so claiming afterwards would pass on either kernel. What
    // establishes the death instead is the clock - 50 ms is many ticks for a
    // child whose whole body is one `_exit` - with the reap right after as the
    // corroboration that it really did die.
    {
        r.check(
            "unblocking SIGCHLD succeeds",
            mask_change(libc::SIG_UNBLOCK, &child_mask),
        );
        // SAFETY: the child exits immediately and allocates nothing.
        let kid = unsafe { libc::fork() };
        if kid == 0 {
            leave(9);
        }
        r.check("a child was forked", kid > 0);
        if kid > 0 {
            sleep_ms(50);
            r.check(
                "its death was never queued for this descriptor (EAGAIN)",
                read(&fd, &mut buf) == Err(Errno::AGAIN),
            );
            let want = Pid::from_raw(kid);
            let code = match waitpid(want, WaitOptions::empty()) {
                Ok(Some((got, status))) if Some(got) == want => status.exit_status(),
                _ => None,
            };
            r.check(
                "and the unblocked child is still reaped, with its code (9)",
                code == Some(9),
            );
        }
    }

    // ---- a child killed by a signal: CLD_KILLED, with the signal -----------
    //
    // The other half of the record: `si_status` is the exit code for an exit
    // and the *signal* for a death, and `si_code` is what tells the two apart -
    // `CLD_KILLED` here. Measured on a current host: a child that takes
    // `SIGKILL` arrives as `code=2`, `status=9`.
    {
        r.check(
            "blocking SIGCHLD succeeds",
            mask_change(libc::SIG_BLOCK, &child_mask),
        );
        let Ok((rd, wr)) = rustix::pipe::pipe() else {
            r.check("a pipe the doomed child can park on", false);
            r.finish();
        };
        // SAFETY: the child parks in a `read` that can never return - the
        // parent holds the only write end and never writes - so the `SIGKILL`
        // below is what ends it, inside a syscall.
        let kid = unsafe { libc::fork() };
        if kid == 0 {
            drop(wr);
            let mut one = [0u8; 1];
            let _ = read(&rd, &mut one);
            leave(0); // unreachable: nothing is ever written
        }
        drop(rd);
        r.check("a child was forked", kid > 0);
        if kid > 0 {
            // A moment for it to reach the `read`, so the kill lands on a task
            // that is parked rather than racing its own start.
            sleep_ms(20);
            // SAFETY: a live child of this process, and a signal number.
            r.check(
                "SIGKILL to the child succeeds",
                unsafe { libc::kill(kid, libc::SIGKILL) } == 0,
            );
            let want = Pid::from_raw(kid);
            let how = match waitpid(want, WaitOptions::empty()) {
                Ok(Some((got, status))) if Some(got) == want => {
                    (status.signaled(), status.terminating_signal())
                }
                _ => (false, None),
            };
            r.check(
                "the child died of the signal rather than exiting",
                how == (true, Some(libc::SIGKILL)),
            );
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
            r.check("its death arrives through the descriptor", got == Ok(128));
            let killed = record_of(&buf);
            r.check(
                "as CLD_KILLED, with the signal in ssi_status",
                killed.ssi_code == libc::CLD_KILLED && killed.ssi_status == libc::SIGKILL,
            );
            r.check(
                "and the same child in ssi_pid",
                killed.ssi_pid == kid as u32,
            );
        }
        drop(wr);
    }

    // ---- the same record through a handler --------------------------------
    //
    // A queued `SIGCHLD` is also the signal a *handler* runs on, so the frame's
    // `siginfo_t` must carry the same two words - including the status, which
    // shares its slot with every other sender's `sigval`. A kernel that wrote
    // the payload there would read 0 where this claims the exit code.
    {
        HANDLED.store(0, Ordering::Relaxed);
        r.check(
            "installing a SIGCHLD handler succeeds",
            install_chld_handler(),
        );
        r.check(
            "unblocking SIGCHLD succeeds",
            mask_change(libc::SIG_UNBLOCK, &child_mask),
        );
        // SAFETY: the child exits immediately and allocates nothing.
        let kid = unsafe { libc::fork() };
        if kid == 0 {
            leave(5);
        }
        r.check("a child was forked", kid > 0);
        if kid > 0 {
            // A syscall is where a handler runs: the delivery path needs the
            // register frame only a syscall boundary has.
            sleep_ms(30);
            r.check(
                "the handler ran for the child's death",
                HANDLED.load(Ordering::Relaxed) == 1,
            );
            r.check(
                "and the frame's siginfo_t carries CLD_EXITED with the code",
                HANDLED_CODE.load(Ordering::Relaxed) == libc::CLD_EXITED
                    && HANDLED_STATUS.load(Ordering::Relaxed) == 5,
            );
            r.check(
                "naming the child",
                HANDLED_PID.load(Ordering::Relaxed) == kid,
            );
            let want = Pid::from_raw(kid);
            let _ = waitpid(want, WaitOptions::empty());
        }
        r.check(
            "putting SIGCHLD back to its default succeeds",
            restore_chld_default(),
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
