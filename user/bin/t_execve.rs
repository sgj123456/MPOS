//! t_execve - one test for execve, the first syscall in a musl binary's strace.
//!
//! In the strace, execve is how the host starts the binary; a process does
//! not call it again at runtime. mpos maps the Linux number onto its
//! loader as a documented approximation: the image is loaded as a new task
//! that is a child of the caller, the new task's id comes back, and the
//! caller keeps running - a real execve would replace the caller and never
//! return. This test pins that contract: execve starts /bin/true and
//! returns a live task id, and WAIT4 observes the child exit with 0.
//!
//! Also pinned: the two refusals the loader makes - a path that names
//! nothing is ENOENT and a file that is not an ELF image is ENOEXEC - and
//! a null argv, which the kernel accepts by synthesizing argv[0] from the
//! path's last component, the courtesy every libc's getopt expects.

#![no_std]
#![no_main]

#[path = "../lib/lin.rs"]
mod lin;

use core::panic::PanicInfo;

/// NUL-terminated target, plus the argv/envp arrays musl would build.
static PROG: [u8; 10] = *b"/bin/true\0";
static mut ARGV: [*const u8; 2] = [PROG.as_ptr(), core::ptr::null()];
static mut ENVP: [*const u8; 1] = [core::ptr::null()];

/// A path that names nothing, and a real file that is not an ELF image.
static MISSING: [u8; 9] = *b"/no/such\0";
static MOTD: [u8; 10] = *b"/etc/motd\0";

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

    // A path that names nothing cannot be loaded: ENOENT, the errno the
    // man page pins for a missing file.
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

    // /etc/motd exists but is no ELF image: the loader refuses it with
    // ENOEXEC rather than executing garbage.
    let r = lin::execve(
        MOTD.as_ptr(),
        &raw const ARGV as *const *const u8,
        &raw const ENVP as *const *const u8,
    );
    lin::check_eq(
        &mut failed,
        b"execve on a non-ELF file is ENOEXEC",
        r,
        lin::ENOEXEC,
    );

    // A null argv is legal: the kernel synthesizes argv[0] from the path,
    // so the child's main still sees a program name.
    let pid = lin::execve(PROG.as_ptr(), core::ptr::null(), core::ptr::null());
    lin::check_true(
        &mut failed,
        b"execve with a null argv starts the image (argv[0] synthesized)",
        pid > 0,
    );
    if pid > 0 {
        let mut status: i32 = -1;
        let code = lin::wait4(pid as usize, &mut status, 0);
        lin::check_eq(
            &mut failed,
            b"wait4 sees the null-argv child exit with 0",
            code,
            0,
        );
    }

    lin::done(failed)
}

#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    lin::die(b"t_execve: panicked\n")
}
