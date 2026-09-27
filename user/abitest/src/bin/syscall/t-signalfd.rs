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
//! Where mpos diverges: this kernel reports child exits and only child exits -
//! the mask's contents are taken as a validity check and nothing else, so a
//! caller that asked for other signals gets an empty queue. The record is
//! Linux's `signalfd_siginfo`, whose `ssi_pid` sits at byte 12 (the offset the
//! harness's `SignalFdInfo::pid()` encodes) - *not* the `siginfo_t` offset 16
//! that `waitid` writes, which is why the check names the byte.
//!
//! The mask here follows mpos's own convention, shared by `blocked`, the
//! pending set and `rt_sigpending`: bit *n* names signal *n*, not the libc's
//! bit *n-1*.

#![no_std]
#![no_main]

#[path = "../../../lib/lin.rs"]
mod lin;

use core::panic::PanicInfo;

/// `SIGCHLD`'s bit in the mask convention above.
const SIGCHLD_BIT: usize = 17;

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

    // The original spelling, with no flag word at all, makes a descriptor too.
    let plain = lin::signalfd(-1, &mask, 8);
    lin::check_true(
        &mut failed,
        b"the plain signalfd(fd=-1) makes a descriptor",
        plain >= 0,
    );

    let _ = lin::close(plain as i32);
    let _ = lin::close(fd);
    lin::done(failed)
}

#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    lin::die(b"t-signalfd: panicked\n")
}
