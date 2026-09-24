//! t-execve - one test for execve, the first syscall in a musl binary's strace.
//!
//! `execve` replaces the calling process: the old image is gone, the new one
//! starts at its entry point, and **the call never returns**. That makes the
//! success case impossible to observe from inside the process that calls it, so
//! the test splits in two.
//!
//! The failures are checked in-process, because those *do* return: a path that
//! names nothing is ENOENT, and a real file that is not an ELF image is ENOEXEC
//! rather than something the loader tries to execute.
//!
//! The success is checked through a child. The child calls `execve` and the
//! parent reaps it; if the call had returned in the child, the child would exit
//! with 127 instead, and the parent's status check would catch it.

#![no_std]
#![no_main]

#[path = "../../lib/lin.rs"]
mod lin;

use core::panic::PanicInfo;

/// The target the child replaces itself with: it exits 0 without a word.
static PROG: [u8; 10] = *b"/bin/true\0";
static ARG0: [u8; 5] = *b"true\0";
static mut ARGV: [*const u8; 2] = [ARG0.as_ptr(), core::ptr::null()];
static mut ENVP: [*const u8; 1] = [core::ptr::null()];

/// A path that names nothing, and a real file that is not an ELF image.
static MISSING: [u8; 9] = *b"/no/such\0";
static MOTD: [u8; 10] = *b"/etc/motd\0";

/// What the child leaves with if `execve` came back, which can only mean it
/// failed. Distinct from any code `/bin/true` would produce.
const NO_EXEC: i32 = 127;

#[unsafe(no_mangle)]
extern "C" fn mpos_user_main(_argc: usize, _argv: *const *const u8) -> ! {
    lin::puts(b"t-execve\n");
    let mut failed = false;

    // A missing path: ENOENT, the errno the man page pins for a file that is not
    // there.
    let r = lin::execve(
        MISSING.as_ptr(),
        &raw const ARGV as *const *const u8,
        &raw const ENVP as *const *const u8,
    );
    lin::check_eq(
        &mut failed,
        b"execve on a missing path is ENOENT",
        r,
        lin::ENOENT,
    );

    // A file that exists but is not executable: Linux's permission check runs
    // before any binary-format probe, so `execve` answers EACCES (root's
    // CAP_DAC_OVERRIDE does not cover execute on a file with no x bit). `/etc/
    // motd` is plain data - not a program and not runnable - so this is EACCES
    // rather than ENOEXEC; the readable-but-unrunnable refusal is the point.
    let r = lin::execve(
        MOTD.as_ptr(),
        &raw const ARGV as *const *const u8,
        &raw const ENVP as *const *const u8,
    );
    lin::check_eq(
        &mut failed,
        b"execve on a non-executable file is EACCES",
        r,
        lin::EACCES,
    );

    // The success case, seen from the parent: the child becomes /bin/true and is
    // reaped with 0. A child that had returned from execve would have left 127.
    let pid = lin::fork();
    if pid < 0 {
        lin::check_true(&mut failed, b"fork for the execve child succeeds", false);
        lin::done(failed);
    }
    if pid == 0 {
        let _ = lin::execve(
            PROG.as_ptr(),
            &raw const ARGV as *const *const u8,
            &raw const ENVP as *const *const u8,
        );
        // Reached only when the exec did not happen.
        lin::exit_group(NO_EXEC);
    }

    let mut status: i32 = 0;
    let rc = lin::wait4(pid as usize, &mut status, 0);
    lin::check_eq(
        &mut failed,
        b"the exec'd child is reaped by its pid",
        rc,
        pid,
    );
    lin::check_eq(
        &mut failed,
        b"execve replaced the child: /bin/true ran and exited 0",
        lin::wexitstatus(status) as isize,
        0,
    );

    lin::done(failed)
}

#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    lin::die(b"t-execve: panicked\n")
}
