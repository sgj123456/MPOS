//! t-waitid - a child's fate, described as a `siginfo_t`.
//!
//! Pinned here, per waitid(2): `idtype` is `P_ALL` or `P_PID` and nothing else
//! (`EINVAL`), `WEXITED` alone reports an exit as `SIGCHLD`/`CLD_EXITED` with
//! the child's pid in `si_pid` and its code in `si_status`, `WNOHANG` answers 0
//! with the info zeroed when nothing is ready, options without `WEXITED` are a
//! zero-filling no-op that answers 0, and `P_ALL` collects whichever child is
//! ready.
//!
//! Three divergences this kernel has are named here rather than hidden, each
//! with the host's answer measured beside it (`docs/UAPI.md` §5.2 carries them):
//!
//! * **`id` is ignored.** `P_PID` does not name the child waited for; the kernel
//!   collects any ready child. Every check below is ordered so only the child
//!   under test is ever ready, which is what the man page's `P_PID` would have
//!   given anyway.
//! * **Two idtypes, not four.** `P_ALL` and `P_PID` are served; the other two
//!   Linux has (`P_PGID` 2, `P_PIDFD` 3) are answered `EINVAL`. Linux *serves*
//!   `P_PGID`: measured on a current host, `idtype = 2` with `id = 0` is
//!   accepted and the childless caller is answered `ECHILD` (errno 10).
//! * **A call that asks for no event answers 0.** Linux clears the record and
//!   then refuses the call: measured, `waitid(P_ALL, 0, info, 0)` answers `-1`
//!   with `EINVAL` (errno 22). The *zeroing* is the half both machines agree on.
//!
//! A wait with no child at all is `ECHILD`, and that is pinned here: a second
//! wait for a child already collected must answer it rather than park. The
//! probe runs in a child with a deadline of its own, so a kernel that regressed
//! to parking shows up as a failed check rather than a hung suite - the deadline
//! is what a `waitid` asleep inside the probe cannot blow past.
//!
//! The `EFAULT` for a null or short `infop` cannot be reached from these
//! wrappers - the record handed over is never null and never short - so it is
//! not pinned here.
//!
//! # The migration
//!
//! The call is the raw `SYS_waitid` (247 here, 95 on the second machine, both
//! taken from libc's per-machine table), because the record the kernel writes
//! *is* the claim and `rustix::process::waitid` owns its buffer: the 0xFF seed
//! that proves a `WEXITED`-less call zeroes the info, and the fields read back
//! out of it, have nowhere to go through a wrapper that hands over its own.
//!
//! What the raw door must not carry is this repository's memory of the record's
//! *layout*, which is what the no-std version did - `si_pid` at byte 16 and
//! `si_status` at 24, read through a helper that took a raw offset. The
//! migrated program reads the machine's own `libc::siginfo_t` instead:
//! `si_signo`/`si_code` are fields, `si_pid()`/`si_status()` are libc's
//! accessors into the union, and the record's size is pinned against the ABI's
//! own `SI_MAX_SIZE` (128) rather than against a number written here. Measured
//! before the change was written, by dumping a real record from this host's
//! kernel through musl: `si_signo` 17 at byte 0, `si_code` 1 at byte 8, the
//! child's pid at 16, its uid at 20, the exit code at 24 - the asm-generic
//! `{signo, errno, code, union}` with the union padded to its 8-byte alignment,
//! which is the shape of *both* machines here (x86_64 includes asm-generic's
//! header; arm64 is asm-generic). The offsets the no-std version wrote down were
//! therefore right on both machines and are now unnecessary on both.
//!
//! The polling that watches the ECHILD probe and the child that sleeps are
//! `rustix`'s `waitpid` (`wait4` with a null `rusage`, and `Ok(None)` for
//! `WNOHANG` finding nobody ready) and `libc::nanosleep`; every child leaves
//! through the raw `SYS_exit_group`, because a forked std child holds a copy of
//! the parent's buffered stdout and only a non-flushing exit leaves it behind.

use mpos_user_tests::Report;
use rustix::process::{Pid, WaitOptions, waitpid};
use std::mem::size_of;

/// `waitid(idtype, id, infop, options, rusage)` through the raw door, with the
/// caller's own record. Returns the answer, or the negative errno the no-std
/// harness reported.
fn waitid(idtype: u32, id: i32, info: &mut libc::siginfo_t, options: i32) -> isize {
    let r = unsafe {
        libc::syscall(
            libc::SYS_waitid,
            idtype as libc::c_long,
            id as libc::c_long,
            info as *mut libc::siginfo_t as libc::c_long,
            options as libc::c_long,
            0 as libc::c_long,
        )
    };
    if r < 0 {
        -(std::io::Error::last_os_error().raw_os_error().unwrap_or(0) as isize)
    } else {
        r as isize
    }
}

/// A record seeded with 0xFF, so a field the kernel zeroes is a field the caller
/// can see cleared.
fn poisoned() -> libc::siginfo_t {
    let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
    unsafe {
        std::ptr::write_bytes(
            (&raw mut info).cast::<u8>(),
            0xFF,
            size_of::<libc::siginfo_t>(),
        );
    }
    info
}

/// Sleep out a whole number of milliseconds through the raw door, so a forked
/// child never touches the allocator.
fn sleep_ms(ms: i64) {
    let req = libc::timespec {
        tv_sec: ms / 1000,
        tv_nsec: (ms % 1000) * 1_000_000,
    };
    unsafe {
        libc::nanosleep(&req, core::ptr::null_mut());
    }
}

/// Leave through `exit_group`: the code is the claim, and the child's inherited
/// copy of the parent's stdout buffer must not be flushed.
fn leave(code: i32) -> ! {
    unsafe {
        libc::syscall(libc::SYS_exit_group, code as libc::c_long);
        // The door does not return; `_exit` is the maintained fallback.
        libc::_exit(code)
    }
}

fn main() {
    println!("t-waitid");
    let mut r = Report::new();

    // The record's size is the ABI's, not this file's: `SI_MAX_SIZE` is 128 on
    // both machines, and the fields below are read out of a struct of it.
    r.check(
        "the siginfo record is the size the ABI names (128)",
        size_of::<libc::siginfo_t>() == 128,
    );

    // idtype is P_ALL, P_PID, P_PGID or P_PIDFD on Linux; this kernel knows the
    // first two and answers EINVAL for anything else. Linux *serves* the word
    // this program probes with: measured on a current host, `idtype = 2`
    // (`P_PGID`) with `id = 0` is accepted, and the childless caller is answered
    // `ECHILD` (errno 10) rather than `EINVAL`. The payload pins this kernel's
    // refusal and the host names the service it does not share, which is the
    // shape `docs/UAPI.md` §5.2 records (the `id`-ignoring half above is the
    // other one this program pins).
    let mut info = poisoned();
    let answered = waitid(2, 0, &mut info, libc::WEXITED);
    r.check(
        "an idtype outside this kernel's two answers EINVAL",
        answered
            == if mpos_user_tests::host_run() {
                -(libc::ECHILD as isize)
            } else {
                -(libc::EINVAL as isize)
            },
    );

    // Options with no event asked for: Linux clears the record *before* it
    // validates the options, and then refuses the call - measured on a current
    // host, `waitid(P_ALL, 0, info, 0)` answers `-1`/`EINVAL` (errno 22) with
    // every seeded byte of `info` cleared, which the "and zero the info" claim
    // below reads for itself. This kernel clears the record and answers 0. The
    // payload pins that answer and the host names Linux's refusal; the zeroing
    // is the half the two machines agree on.
    let mut info = poisoned();
    let answered = waitid(libc::P_ALL, 0, &mut info, 0);
    r.check(
        "a call asking for no event answers this kernel's 0",
        answered
            == if mpos_user_tests::host_run() {
                -(libc::EINVAL as isize)
            } else {
                0
            },
    );
    r.check(
        "and zero the info (si_pid cleared)",
        unsafe { info.si_pid() } == 0,
    );

    // A child that exits 3: the info record carries signo, code, pid, status.
    let kid = unsafe { libc::fork() };
    if kid == 0 {
        leave(3);
    }
    r.check("a child that exits 3 was forked", kid > 0);
    if kid > 0 {
        let mut info = poisoned();
        r.check(
            "waitid(P_PID, child, WEXITED) succeeds",
            waitid(libc::P_PID, kid, &mut info, libc::WEXITED) == 0,
        );
        r.check("si_signo is SIGCHLD", info.si_signo == libc::SIGCHLD);
        r.check("si_code is CLD_EXITED", info.si_code == libc::CLD_EXITED);
        r.check("si_pid is the child", unsafe { info.si_pid() } == kid);
        r.check(
            "si_status is the exit code 3",
            unsafe { info.si_status() } == 3,
        );

        // A second wait for a child already collected: no child is left, so
        // Linux answers ECHILD instead of blocking. The probe runs in a child
        // with a deadline of its own, so a kernel that parked here again would
        // show up as the probe never exiting rather than as a hung suite.
        let probe = unsafe { libc::fork() };
        if probe == 0 {
            let mut probe_info = poisoned();
            let answered = waitid(libc::P_PID, kid, &mut probe_info, libc::WEXITED);
            leave(if answered == -(libc::ECHILD as isize) {
                0
            } else {
                1
            });
        }
        r.check("the ECHILD probe was forked", probe > 0);
        if probe > 0 {
            let want = Pid::from_raw(probe);
            let mut status = None;
            let mut tries = 0;
            while status.is_none() && tries < 200 {
                status = waitpid(want, WaitOptions::NOHANG)
                    .ok()
                    .flatten()
                    .map(|(_, status)| status);
                if status.is_none() {
                    sleep_ms(1);
                }
                tries += 1;
            }
            r.check(
                "a second wait for a collected child answers ECHILD, not a park",
                status.is_some_and(|s| s.exited() && s.exit_status() == Some(0)),
            );
            if status.is_none() {
                // Still parked: end the probe so the rest of the suite runs.
                unsafe {
                    libc::kill(probe, libc::SIGKILL);
                }
                let _ = waitpid(want, WaitOptions::empty());
            }
        }
    }

    // WNOHANG while the only child is still running: 0 with the info cleared,
    // not an error and not a blocking wait.
    let sleeper = unsafe { libc::fork() };
    if sleeper == 0 {
        sleep_ms(1000);
        leave(0);
    }
    r.check("a sleeping child was forked", sleeper > 0);
    if sleeper > 0 {
        let mut info = poisoned();
        let answered = waitid(
            libc::P_PID,
            sleeper,
            &mut info,
            libc::WEXITED | libc::WNOHANG,
        );
        r.check(
            "WNOHANG with nothing ready answers 0, not an error",
            answered == 0,
        );
        r.check("and clears si_pid to 0", unsafe { info.si_pid() } == 0);

        // Collect it, so it cannot be mistaken for a later check's child.
        let mut info = poisoned();
        r.check(
            "the blocking wait then collects the sleeper",
            waitid(libc::P_PID, sleeper, &mut info, libc::WEXITED) == 0,
        );
    }

    // P_ALL takes whichever child is ready.
    let any = unsafe { libc::fork() };
    if any == 0 {
        leave(5);
    }
    r.check("a child that exits 5 was forked", any > 0);
    if any > 0 {
        let mut info = poisoned();
        r.check(
            "waitid(P_ALL) collects a child",
            waitid(libc::P_ALL, 0, &mut info, libc::WEXITED) == 0,
        );
        r.check(
            "P_ALL reports the child's pid",
            unsafe { info.si_pid() } == any,
        );
        r.check("P_ALL reports its code", unsafe { info.si_status() } == 5);
    }

    r.finish()
}
