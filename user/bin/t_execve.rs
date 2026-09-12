//! t_execve - one test for execve, the first syscall in the musl_hello
//! strace.
//!
//! In the strace, execve is how the host starts the binary; a process does
//! not call it again at runtime. mpos maps the Linux number onto its
//! loader as a documented approximation: the image is loaded as a new task
//! that is a child of the caller, the new task's id comes back, and the
//! caller keeps running - a real execve would replace the caller and never
//! return. This test pins that contract: execve starts /bin/true and
//! returns a live task id, and WAIT4 observes the child exit with 0.

#![no_std]
#![no_main]

#[path = "../lib/lin.rs"]
mod lin;

use core::panic::PanicInfo;

/// NUL-terminated target, plus the argv/envp arrays musl would build.
static PROG: [u8; 10] = *b"/bin/true\0";
static mut ARGV: [*const u8; 2] = [PROG.as_ptr(), core::ptr::null()];
static mut ENVP: [*const u8; 1] = [core::ptr::null()];

#[unsafe(no_mangle)]
extern "C" fn mpos_user_main(_argc: usize, _argv: *const *const u8) -> ! {
    lin::puts(b"t_execve\n");
    let mut failed = false;

    let pid = lin::execve(
        PROG.as_ptr(),
        &raw const ARGV as *const *const u8,
        &raw const ENVP as *const *const u8,
    );
    lin::check_true(
        &mut failed,
        b"execve(/bin/true) starts the image and returns its task id",
        pid > 0,
    );

    if pid > 0 {
        let mut status: i32 = -1;
        let code = lin::wait4(pid as usize, &mut status, 0);
        // mpos hands the child's exit code straight out of WAIT4 (Linux
        // would return the pid and encode the code in *status): /bin/true
        // exits 0 either way the check reads.
        lin::check_eq(&mut failed, b"wait4 sees the child exit with 0", code, 0);
    }

    lin::done(failed)
}

#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    lin::die(b"t_execve: panicked\n")
}
