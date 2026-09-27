//! t-waitid - a child's fate, described as a `siginfo_t`.
//!
//! Pinned here, per waitid(2): `idtype` is `P_ALL` or `P_PID` and nothing else
//! (`EINVAL`), `WEXITED` alone reports an exit as `SIGCHLD`/`CLD_EXITED` with
//! the child's pid in `si_pid` (byte 16) and its code in `si_status` (byte 24),
//! `WNOHANG` answers 0 with the info zeroed when nothing is ready, options
//! without `WEXITED` are a zero-filling no-op that answers 0, and `P_ALL`
//! collects whichever child is ready.
//!
//! Where mpos diverges: the `id` argument is ignored - `P_PID` does not name
//! the child waited for, it collects any ready child. Every check below is
//! ordered so only the child under test is ever ready, which is what the man
//! page's `P_PID` would have given anyway.
//!
//! A wait with no child at all is `ECHILD`, and that is pinned here: a second
//! wait for a child already collected must answer it rather than park. The
//! probe runs in a child with a deadline of its own, so a kernel that regressed
//! to parking shows up as a failed check rather than a hung suite - the deadline
//! is what a `waitid` asleep inside the probe cannot blow past.
//!
//! The `EFAULT` for a null or short `infop` cannot be reached from these
//! wrappers - `&mut [u8; 128]` is never null and never short - so it is not
//! pinned here.

#![no_std]
#![no_main]

#[path = "../../../lib/lin.rs"]
mod lin;

use core::panic::PanicInfo;

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
    lin::puts(b"t-waitid\n");
    let mut failed = false;

    // idtype is P_ALL or P_PID; anything else is EINVAL. Checked first, with
    // no child in the table, so nothing can block.
    let mut info = [0xFFu8; 128];
    lin::check_eq(
        &mut failed,
        b"an idtype that is neither P_ALL nor P_PID answers EINVAL",
        lin::waitid(2, 0, &mut info, lin::WEXITED),
        lin::EINVAL,
    );

    // Options with no WEXITED ask for nothing: the info is zeroed and the
    // answer is success, not a blank wait. The 0xFF seed proves the zeroing.
    let r = lin::waitid(lin::P_ALL, 0, &mut info, 0);
    lin::check_eq(&mut failed, b"options without WEXITED answer 0", r, 0);
    lin::check_eq(
        &mut failed,
        b"and zero the info (si_pid cleared)",
        lin::siginfo_i32(&info, 16) as isize,
        0,
    );

    // A child that exits 3: the info record carries signo, code, pid, status.
    let kid = lin::fork();
    if kid == 0 {
        lin::exit_group(3);
    }
    lin::check_true(&mut failed, b"a child that exits 3 was forked", kid > 0);
    if kid > 0 {
        let mut info = [0xFFu8; 128];
        lin::check_eq(
            &mut failed,
            b"waitid(P_PID, child, WEXITED) succeeds",
            lin::waitid(lin::P_PID, kid as usize, &mut info, lin::WEXITED),
            0,
        );
        lin::check_eq(
            &mut failed,
            b"si_signo is SIGCHLD",
            lin::siginfo_i32(&info, 0) as isize,
            lin::SIGCHLD as isize,
        );
        lin::check_eq(
            &mut failed,
            b"si_code is CLD_EXITED",
            lin::siginfo_i32(&info, 8) as isize,
            lin::CLD_EXITED as isize,
        );
        lin::check_eq(
            &mut failed,
            b"si_pid is the child",
            lin::siginfo_i32(&info, 16) as isize,
            kid,
        );
        lin::check_eq(
            &mut failed,
            b"si_status is the exit code 3",
            lin::siginfo_i32(&info, 24) as isize,
            3,
        );

        // A second wait for a child already collected: no child is left, so
        // Linux answers ECHILD instead of blocking. The probe runs in a child
        // with a deadline of its own, so a kernel that parked here again would
        // show up as the probe never exiting rather than as a hung suite.
        let probe = lin::fork();
        if probe == 0 {
            let mut probe_info = [0u8; 128];
            let r = lin::waitid(lin::P_PID, kid as usize, &mut probe_info, lin::WEXITED);
            lin::exit_group(if r == lin::ECHILD { 0 } else { 1 });
        }
        lin::check_true(&mut failed, b"the ECHILD probe was forked", probe > 0);
        if probe > 0 {
            let mut status = -1i32;
            let mut reaped = lin::wait4(probe as usize, &mut status, lin::WNOHANG);
            let mut tries = 0;
            while reaped == 0 && tries < 200 {
                sleep_ms(1);
                reaped = lin::wait4(probe as usize, &mut status, lin::WNOHANG);
                tries += 1;
            }
            lin::check_true(
                &mut failed,
                b"a second wait for a collected child answers ECHILD, not a park",
                reaped == probe && lin::wifexited(status) && lin::wexitstatus(status) == 0,
            );
            if reaped == 0 {
                // Still parked: end the probe so the rest of the suite runs.
                let _ = lin::kill(probe as i32, lin::SIGKILL);
                let _ = lin::wait4(probe as usize, &mut status, 0);
            }
        }
    }

    // WNOHANG while the only child is still running: 0 with si_pid cleared,
    // not an error and not a blocking wait.
    let sleeper = lin::fork();
    if sleeper == 0 {
        sleep_ms(1000);
        lin::exit_group(0);
    }
    lin::check_true(&mut failed, b"a sleeping child was forked", sleeper > 0);
    if sleeper > 0 {
        let mut info = [0xFFu8; 128];
        let r = lin::waitid(
            lin::P_PID,
            sleeper as usize,
            &mut info,
            lin::WEXITED | lin::WNOHANG,
        );
        lin::check_eq(
            &mut failed,
            b"WNOHANG with nothing ready answers 0, not an error",
            r,
            0,
        );
        lin::check_eq(
            &mut failed,
            b"and clears si_pid to 0",
            lin::siginfo_i32(&info, 16) as isize,
            0,
        );

        // Collect it, so it cannot be mistaken for a later check's child.
        let mut info = [0u8; 128];
        let r = lin::waitid(lin::P_PID, sleeper as usize, &mut info, lin::WEXITED);
        lin::check_eq(
            &mut failed,
            b"the blocking wait then collects the sleeper",
            r,
            0,
        );
    }

    // P_ALL takes whichever child is ready.
    let any = lin::fork();
    if any == 0 {
        lin::exit_group(5);
    }
    lin::check_true(&mut failed, b"a child that exits 5 was forked", any > 0);
    if any > 0 {
        let mut info = [0xFFu8; 128];
        let r = lin::waitid(lin::P_ALL, 0, &mut info, lin::WEXITED);
        lin::check_eq(&mut failed, b"waitid(P_ALL) collects a child", r, 0);
        lin::check_eq(
            &mut failed,
            b"P_ALL reports the child's pid",
            lin::siginfo_i32(&info, 16) as isize,
            any,
        );
        lin::check_eq(
            &mut failed,
            b"P_ALL reports its code",
            lin::siginfo_i32(&info, 24) as isize,
            5,
        );
    }

    lin::done(failed)
}

#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    lin::die(b"t-waitid: panicked\n")
}
