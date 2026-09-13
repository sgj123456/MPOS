//! t_wait4 - one test for wait4, the call every shell spends its life in.
//!
//! The man page pins: wait4(pid) returns the child's pid and encodes the
//! exit status in `*status`, ECHILD for a pid that is not a child, and
//! WNOHANG to reap without blocking. mpos keeps the shape but not the
//! encoding - a documented divergence: the RETURN VALUE is the child's
//! exit code itself, not the pid, and `*status` is left alone. This test
//! pins mpos's answer as-is and says so: wait4 of /bin/true answers 0,
//! wait4 of a pid that is nobody's child answers ECHILD, and WNOHANG on a
//! child still sleeping answers EAGAIN until a blocking call reaps it.

#![no_std]
#![no_main]

#[path = "../lib/lin.rs"]
mod lin;

use core::panic::PanicInfo;

static TRUE_PROG: [u8; 10] = *b"/bin/true\0";
static SLEEP_PROG: [u8; 11] = *b"/bin/sleep\0";
static TRUE_ARG0: [u8; 5] = *b"true\0";
static SLEEP_ARG0: [u8; 6] = *b"sleep\0";
static SLEEP_ARG1: [u8; 2] = *b"2\0";
static mut TRUE_ARGV: [*const u8; 2] = [TRUE_ARG0.as_ptr(), core::ptr::null()];
static mut SLEEP_ARGV: [*const u8; 3] = [
    SLEEP_ARG0.as_ptr(),
    SLEEP_ARG1.as_ptr(),
    core::ptr::null(),
];
static mut ENVP: [*const u8; 1] = [core::ptr::null()];

#[unsafe(no_mangle)]
extern "C" fn mpos_user_main(_argc: usize, _argv: *const *const u8) -> ! {
    lin::puts(b"t_wait4\n");
    let mut failed = false;
    let mut status: i32 = -1;

    let pid = lin::execve(
        TRUE_PROG.as_ptr(),
        &raw const TRUE_ARGV as *const *const u8,
        &raw const ENVP as *const *const u8,
    );
    lin::check_true(
        &mut failed,
        b"execve(/bin/true) starts the child and returns its task id",
        pid > 0,
    );

    // The documented mpos divergence: Linux would return the child's pid
    // here and encode the exit code in *status; mpos returns the exit code
    // itself - /bin/true exits 0 - and does not touch the status word.
    if pid > 0 {
        let code = lin::wait4(pid as usize, &mut status, 0);
        lin::check_eq(
            &mut failed,
            b"wait4 of the exited child answers its exit code, 0",
            code,
            0,
        );
    }

    // A pid that is no child of ours - here, one past the whole task
    // table - cannot be reaped: ECHILD, the man page's "no waitable child".
    let r = lin::wait4(9999, &mut status, 0);
    lin::check_eq(
        &mut failed,
        b"wait4 on a non-child pid is ECHILD",
        r,
        lin::ECHILD,
    );

    // WNOHANG: ask about a child that is still alive without blocking.
    let sleeper = lin::execve(
        SLEEP_PROG.as_ptr(),
        &raw const SLEEP_ARGV as *const *const u8,
        &raw const ENVP as *const *const u8,
    );
    lin::check_true(
        &mut failed,
        b"execve(/bin/sleep 2) starts the child and returns its task id",
        sleeper > 0,
    );
    if sleeper > 0 {
        let r = lin::wait4(sleeper as usize, &mut status, lin::WNOHANG);
        lin::check_eq(
            &mut failed,
            b"WNOHANG on the still-running child is EAGAIN",
            r,
            lin::EAGAIN,
        );

        // The blocking call waits out the child's full sleep and reaps it.
        let r = lin::wait4(sleeper as usize, &mut status, 0);
        lin::check_eq(
            &mut failed,
            b"the blocking wait4 reaps the sleeping child with exit code 0",
            r,
            0,
        );
    }

    // One more pid nobody could own, re-checked now that a child was reaped.
    let r = lin::wait4(99999, &mut status, 0);
    lin::check_eq(
        &mut failed,
        b"wait4 on pid 99999 is ECHILD too",
        r,
        lin::ECHILD,
    );

    lin::done(failed)
}

#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    lin::die(b"t_wait4: panicked\n")
}
