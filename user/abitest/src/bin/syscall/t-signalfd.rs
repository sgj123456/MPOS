//! t-signalfd - signals delivered as a file descriptor.
//!
//! Pinned here, per signalfd(2)/signalfd4(2): `fd` of -1 makes a new
//! descriptor, the create flags are the `SFD_*` pair, the record is the
//! 128-byte `struct signalfd_siginfo` and a shorter read buffer is `EINVAL`,
//! an empty nonblocking read is `EAGAIN`, a child's exit arrives as
//! `SIGCHLD`/`CLD_EXITED` naming the child, the read consumes the signal so
//! the pid is reported once, and the read does **not** collect the child -
//! `wait(2)` still has to, which is the split the call exists for. The plain
//! `signalfd` spelling, with no flag word, makes a descriptor too.
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
//! The mask is the wire's `sigset_t`, as a libc spells it: bit *n* holds
//! signal *n+1*, so `SIGCHLD` (17) is bit 16 - the same convention
//! `t-rt-sigprocmask` pins.
//!
//! The record is Linux's `signalfd_siginfo`, whose `ssi_pid` sits at byte 12
//! (the offset the harness's `SignalFdInfo::pid()` encodes) - *not* the
//! `siginfo_t` offset 16 that `waitid` writes, which is why the check names
//! the byte.

#![no_std]
#![no_main]

#[path = "../../../lib/lin.rs"]
mod lin;

use core::panic::PanicInfo;

/// `SIGCHLD`'s bit in the wire mask: signal *n* is bit *n-1*.
const SIGCHLD_BIT: usize = (lin::SIGCHLD as usize) - 1;

/// Sleep out a whole number of milliseconds, via the harness's nanosleep.
fn sleep_ms(ms: i64) {
    let req = lin::TimeSpec {
        tv_sec: ms / 1000,
        tv_nsec: (ms % 1000) * 1_000_000,
    };
    let mut rem = lin::TimeSpec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    let _ = lin::nanosleep(&req, &mut rem);
}

#[unsafe(no_mangle)]
extern "C" fn mpos_user_main(_argc: usize, _argv: *const *const u8) -> ! {
    lin::puts(b"t-signalfd\n");
    let mut failed = false;

    // The 8-byte mask the wrapper takes, with SIGCHLD set.
    let mut mask = [0u8; 8];
    mask[SIGCHLD_BIT / 8] |= 1u8 << (SIGCHLD_BIT % 8);

    // fd -1 means "make a new descriptor".
    let fd = lin::signalfd4(-1, &mask, 8, lin::SFD_NONBLOCK) as i32;
    lin::check_true(
        &mut failed,
        b"signalfd4(-1, mask, 8, SFD_NONBLOCK) hands back a descriptor",
        fd >= 0,
    );

    // The record is 128 bytes; a shorter buffer is EINVAL, and an empty
    // nonblocking read is EAGAIN rather than a park.
    let mut eight = [0u8; 8];
    lin::check_eq(
        &mut failed,
        b"an 8-byte read answers EINVAL (128 are required)",
        lin::read(fd, &mut eight),
        lin::EINVAL,
    );
    let mut buf = [0u8; 128];
    lin::check_eq(
        &mut failed,
        b"a read with no child exited answers EAGAIN",
        lin::read(fd, &mut buf),
        lin::EAGAIN,
    );

    // A child exits; its death becomes readable as one record. The read is
    // retried on a bounded loop because the child has to be scheduled first.
    let kid = lin::fork();
    if kid == 0 {
        lin::exit_group(7);
    }
    lin::check_true(&mut failed, b"a child was forked", kid > 0);
    if kid > 0 {
        let mut n = lin::EAGAIN;
        let mut tries = 0;
        while tries < 200 {
            n = lin::read(fd, &mut buf);
            if n == 128 {
                break;
            }
            sleep_ms(1);
            tries += 1;
        }
        lin::check_eq(
            &mut failed,
            b"the child's exit arrives as a 128-byte record",
            n,
            128,
        );
        let mut info = lin::SignalFdInfo::new();
        info.bytes.copy_from_slice(&buf);
        lin::check_eq(
            &mut failed,
            b"the record's signo is SIGCHLD",
            info.signo() as isize,
            lin::SIGCHLD as isize,
        );
        lin::check_eq(
            &mut failed,
            b"the record's code is CLD_EXITED",
            info.code() as isize,
            lin::CLD_EXITED as isize,
        );
        lin::check_eq(
            &mut failed,
            b"the record names the child (ssi_pid at byte 12, not siginfo_t's 16)",
            info.pid() as isize,
            kid,
        );
        lin::check_eq(
            &mut failed,
            b"the pid is reported once, so the next read is EAGAIN",
            lin::read(fd, &mut buf),
            lin::EAGAIN,
        );

        // The read does not collect the child: wait(2) still must.
        let mut status = -1i32;
        let rc = lin::wait4(kid as usize, &mut status, 0);
        lin::check_eq(
            &mut failed,
            b"wait4 still reaps the child the signalfd reported",
            rc,
            kid,
        );
        lin::check_eq(
            &mut failed,
            b"and the exit code is 7",
            lin::wexitstatus(status) as isize,
            7,
        );
    }

    // The blocked-signal half of the contract: a signal that is blocked *and*
    // named in the mask is reported here, with the record its sender left. This
    // is the shape weston depends on for `SIGTERM` - and the shape a kernel
    // that reported only child exits never produced.
    {
        // The wire's mask for SIGTERM alone: signal *n* is bit *n-1*.
        let sigterm_bit = (lin::SIGTERM as usize) - 1;
        let mut term_mask = [0u8; 8];
        term_mask[sigterm_bit / 8] |= 1u8 << (sigterm_bit % 8);
        let sfd = lin::signalfd4(-1, &term_mask, 8, lin::SFD_NONBLOCK) as i32;
        lin::check_true(
            &mut failed,
            b"a descriptor watching SIGTERM is made",
            sfd >= 0,
        );

        // Block it, then queue it to ourselves with a payload. A signal that
        // is *not* blocked must not arrive here (it would run its default
        // action), so blocking is what makes this read well-defined - which is
        // exactly what `signalfd(2)` requires of the caller.
        let blocked_set: u64 = 1u64 << sigterm_bit;
        let r = lin::rt_sigprocmask(lin::SIG_BLOCK, &blocked_set, core::ptr::null_mut());
        lin::check_eq(&mut failed, b"blocking SIGTERM succeeds", r, 0);

        let mut sent = lin::SigInfo::zero();
        sent.signo = lin::SIGTERM;
        sent.code = lin::SI_QUEUE;
        sent.value = 0x5150_5150_5150_5150;
        let r = lin::rt_sigqueueinfo(lin::getpid(), lin::SIGTERM, &sent);
        lin::check_eq(
            &mut failed,
            b"sigqueue of the blocked SIGTERM succeeds",
            r,
            0,
        );

        let n = lin::read(sfd, &mut buf);
        lin::check_eq(
            &mut failed,
            b"the blocked signal arrives through the descriptor",
            n,
            128,
        );
        if n == 128 {
            let mut info = lin::SignalFdInfo::new();
            info.bytes.copy_from_slice(&buf);
            lin::check_eq(
                &mut failed,
                b"the record's signo is SIGTERM",
                info.signo() as isize,
                lin::SIGTERM as isize,
            );
            lin::check_eq(
                &mut failed,
                b"the record's code is SI_QUEUE, the sender's own",
                info.code() as isize,
                lin::SI_QUEUE as isize,
            );
        }
        lin::check_eq(
            &mut failed,
            b"the signal was consumed by the read, so the next is EAGAIN",
            lin::read(sfd, &mut buf),
            lin::EAGAIN,
        );

        // Nothing is pending now, so unblocking runs no handler and the
        // default action never gets the chance to end this test.
        let r = lin::rt_sigprocmask(lin::SIG_UNBLOCK, &blocked_set, core::ptr::null_mut());
        lin::check_eq(
            &mut failed,
            b"unblocking the now-consumed SIGTERM succeeds",
            r,
            0,
        );
        let _ = lin::close(sfd);
    }

    // The original spelling, with no flag word at all, makes a descriptor too.
    let plain = lin::signalfd(-1, &mask, 8);
    lin::check_true(
        &mut failed,
        b"the plain signalfd(fd=-1) makes a descriptor",
        plain >= 0,
    );

    // The `signalfd4` flag word has exactly two bits - `SFD_NONBLOCK` and
    // `SFD_CLOEXEC`. An unmodelled bit is refused with `EINVAL` rather than
    // silently dropped, and `SFD_CLOEXEC` sets the descriptor's close-on-exec,
    // which the kernel used to lose (a compositor that installs a signalfd and
    // then execs a helper would leak it across).
    {
        let unknown = lin::signalfd4(-1, &mask, 8, 0x1);
        lin::check_eq(
            &mut failed,
            b"signalfd4 with an unknown flag answers EINVAL",
            unknown,
            lin::EINVAL,
        );
        let clo = lin::signalfd4(-1, &mask, 8, lin::SFD_CLOEXEC) as i32;
        lin::check_true(
            &mut failed,
            b"signalfd4(SFD_CLOEXEC) hands back a descriptor",
            clo >= 0,
        );
        if clo >= 0 {
            let f = lin::fcntl(clo, lin::F_GETFD, 0);
            lin::check_true(
                &mut failed,
                b"and the descriptor came back close-on-exec",
                f & (lin::FD_CLOEXEC as isize) != 0,
            );
            let _ = lin::close(clo);
        }
    }

    let _ = lin::close(plain as i32);
    let _ = lin::close(fd);
    lin::done(failed)
}

#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    lin::die(b"t-signalfd: panicked\n")
}
