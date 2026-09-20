//! t-wait4 - one test for wait4, the call every shell spends its life in.
//!
//! The man page pins four things: `wait4(pid)` returns the child's **pid**,
//! encodes the exit in `*status` where `WEXITSTATUS` reads it, answers `ECHILD`
//! for a pid that is nobody's child, and answers `0` - not an error - for
//! `WNOHANG` while the child it was asked about is still running.
//!
//! The pid/status split is what the test is really about. A kernel that returned
//! the exit code itself would look fine in a shell that only prints `$?`, and
//! would break every libc that calls `WIFEXITED` on the status word.

#![no_std]
#![no_main]

#[path = "../../lib/lin.rs"]
mod lin;

use core::panic::PanicInfo;

static TRUE_PROG: [u8; 10] = *b"/bin/true\0";
static FALSE_PROG: [u8; 11] = *b"/bin/false\0";
static SLEEP_PROG: [u8; 11] = *b"/bin/sleep\0";
static TRUE_ARG0: [u8; 5] = *b"true\0";
static FALSE_ARG0: [u8; 6] = *b"false\0";
static SLEEP_ARG0: [u8; 6] = *b"sleep\0";
static SLEEP_ARG1: [u8; 2] = *b"2\0";
static mut TRUE_ARGV: [*const u8; 2] = [TRUE_ARG0.as_ptr(), core::ptr::null()];
static mut FALSE_ARGV: [*const u8; 2] = [FALSE_ARG0.as_ptr(), core::ptr::null()];
static mut SLEEP_ARGV: [*const u8; 3] = [
    SLEEP_ARG0.as_ptr(),
    SLEEP_ARG1.as_ptr(),
    core::ptr::null(),
];
static mut ENVP: [*const u8; 1] = [core::ptr::null()];

/// Start `prog` as a child and return the child's pid, or a negative errno.
fn start(prog: *const u8, argv: *const *const u8) -> isize {
    let pid = lin::fork();
    if pid != 0 {
        return pid;
    }
    let _ = lin::execve(prog, argv, &raw const ENVP as *const *const u8);
    // Reached only when the exec failed: give the parent a code it will notice.
    lin::exit_group(127);
}

#[unsafe(no_mangle)]
extern "C" fn mpos_user_main(_argc: usize, _argv: *const *const u8) -> ! {
    lin::puts(b"t-wait4\n");
    let mut failed = false;
    let mut status: i32 = -1;

    // A child that exits 0. The return value is its pid; the status word says
    // "exited normally with 0".
    let pid = start(TRUE_PROG.as_ptr(), &raw const TRUE_ARGV as *const *const u8);
    lin::check_true(&mut failed, b"a /bin/true child was started", pid > 0);
    if pid > 0 {
        let rc = lin::wait4(pid as usize, &mut status, 0);
        lin::check_eq(&mut failed, b"wait4 returns the child's pid", rc, pid);
        lin::check_true(
            &mut failed,
            b"WIFEXITED holds for a normal exit",
            lin::wifexited(status),
        );
        lin::check_eq(
            &mut failed,
            b"WEXITSTATUS reads back 0",
            lin::wexitstatus(status) as isize,
            0,
        );
    }

    // A pid that is nobody's child. One past the whole task table cannot be
    // anyone's, and the answer is ECHILD.
    let r = lin::wait4(9999, &mut status, 0);
    lin::check_eq(
        &mut failed,
        b"wait4 on a non-child pid is ECHILD",
        r,
        lin::ECHILD,
    );

    // A child that exits non-zero: the code survives the round trip through the
    // status word, which is the part a naive implementation gets wrong.
    let pid = start(FALSE_PROG.as_ptr(), &raw const FALSE_ARGV as *const *const u8);
    lin::check_true(&mut failed, b"a /bin/false child was started", pid > 0);
    if pid > 0 {
        let rc = lin::wait4(pid as usize, &mut status, 0);
        lin::check_eq(&mut failed, b"the failing child is reaped by pid", rc, pid);
        lin::check_eq(
            &mut failed,
            b"WEXITSTATUS reads back 1, the code /bin/false exits with",
            lin::wexitstatus(status) as isize,
            1,
        );
    }

    // WNOHANG while the child is still sleeping: 0 means "nothing yet", which is
    // not the same as an error.
    let sleeper = start(
        SLEEP_PROG.as_ptr(),
        &raw const SLEEP_ARGV as *const *const u8,
    );
    lin::check_true(&mut failed, b"a /bin/sleep 2 child was started", sleeper > 0);
    if sleeper > 0 {
        let r = lin::wait4(sleeper as usize, &mut status, lin::WNOHANG);
        lin::check_eq(
            &mut failed,
            b"WNOHANG on a running child answers 0, not an error",
            r,
            0,
        );

        // The blocking call waits out the sleep and collects it.
        let rc = lin::wait4(sleeper as usize, &mut status, 0);
        lin::check_eq(
            &mut failed,
            b"the blocking wait4 then collects the sleeping child",
            rc,
            sleeper,
        );
        lin::check_eq(
            &mut failed,
            b"the sleeping child exited 0",
            lin::wexitstatus(status) as isize,
            0,
        );
    }

    lin::done(failed)
}

#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    lin::die(b"t-wait4: panicked\n")
}
