//! t_clone - one test for clone, the call `fork` and `pthread_create` are both
//! built on.
//!
//! `fork` is `clone` with a zero flag word, so the process-shaped half has to
//! behave exactly like `fork`: 0 in the child, the child's pid in the parent, and
//! a private copy of memory. What is new is the flag word, and the test spends
//! most of its checks there - the pieces of Linux's contract that a kernel can
//! plausibly get wrong:
//!
//! * an unknown flag bit is `EINVAL`, because a kernel that ignored what it did
//!   not understand would hand back a child unlike the one that was asked for;
//! * `CLONE_THREAD` implies `CLONE_SIGHAND`, which implies `CLONE_VM` - a thread
//!   shares its creator's handlers and memory, and a request that says otherwise
//!   is not a request this kernel can honour;
//! * a tid flag with nowhere to put the tid is `EINVAL`, not a silent no-op;
//! * `CLONE_CHILD_SETTID` really does park the tid where the child can read it,
//!   which is how a thread learns its own id before it does anything else;
//! * the spawn shape - `CLONE_VM|CLONE_VFORK`, which is what a libc's
//!   `posix_spawn` builds - starts a child that can exec;
//! * a real thread (`CLONE_THREAD`) is refused with `ENOSYS` rather than faked,
//!   because a shared address space is something this kernel cannot yet give.

#![no_std]
#![no_main]

#[path = "../lib/lin.rs"]
mod lin;

use core::panic::PanicInfo;

/// The variable the clone child updates and the caller must not see change.
static mut SHARED: i32 = 5;

/// The code the clone child leaves with.
const CHILD_CODE: i32 = 33;

/// What the spawn-shaped child execs: it exits 0 without a word.
static TRUE_PROG: [u8; 10] = *b"/bin/true\0";
static TRUE_ARG0: [u8; 5] = *b"true\0";
static mut TRUE_ARGV: [*const u8; 2] = [TRUE_ARG0.as_ptr(), core::ptr::null()];
static mut ENVP: [*const u8; 1] = [core::ptr::null()];

#[unsafe(no_mangle)]
extern "C" fn mpos_user_main(_argc: usize, _argv: *const *const u8) -> ! {
    lin::puts(b"t_clone\n");
    let mut failed = false;
    let null = core::ptr::null_mut::<i32>();

    // An unknown flag bit names something this kernel does not implement.
    let r = lin::clone(1 << 30, 0, null, null, 0);
    lin::check_eq(
        &mut failed,
        b"an unknown flag bit is EINVAL",
        r,
        lin::EINVAL,
    );

    // CLONE_THREAD without the two flags it depends on.
    let r = lin::clone(lin::nr::CLONE_THREAD, 0, null, null, 0);
    lin::check_eq(
        &mut failed,
        b"CLONE_THREAD without CLONE_SIGHAND|CLONE_VM is EINVAL",
        r,
        lin::EINVAL,
    );

    // CLONE_SIGHAND without CLONE_VM.
    let r = lin::clone(lin::nr::CLONE_SIGHAND, 0, null, null, 0);
    lin::check_eq(
        &mut failed,
        b"CLONE_SIGHAND without CLONE_VM is EINVAL",
        r,
        lin::EINVAL,
    );

    // A tid flag with a null destination.
    let r = lin::clone(lin::nr::CLONE_PARENT_SETTID, 0, null, null, 0);
    lin::check_eq(
        &mut failed,
        b"CLONE_PARENT_SETTID with a null pointer is EINVAL",
        r,
        lin::EINVAL,
    );

    // A process-shaped clone is fork: the child gets 0 and its own memory.
    let pid = lin::clone(0, 0, null, null, 0);
    if pid == 0 {
        unsafe { SHARED = 88 };
        lin::exit_group(CHILD_CODE);
    }
    lin::check_true(
        &mut failed,
        b"clone(0) returns the child's pid to the caller",
        pid > 0,
    );
    if pid > 0 {
        let mut status: i32 = 0;
        let rc = lin::wait4(pid as usize, &mut status, 0);
        lin::check_eq(&mut failed, b"the clone child is reaped by its pid", rc, pid);
        lin::check_eq(
            &mut failed,
            b"the clone child's exit code survives",
            lin::wexitstatus(status) as isize,
            CHILD_CODE as isize,
        );
        lin::check_eq(
            &mut failed,
            b"the clone child got its own copy of memory",
            unsafe { SHARED } as isize,
            5,
        );
    }

    // CLONE_CHILD_SETTID: the kernel writes the child's tid into the child's own
    // memory before the child runs, which is how a thread reads its own id.
    let mut slot: i32 = 0;
    let pid = lin::clone(
        lin::nr::CLONE_CHILD_SETTID,
        0,
        null,
        &mut slot as *mut i32,
        0,
    );
    if pid == 0 {
        // The child resumes with 0 in `rax`, so this branch runs with its copy
        // of `slot`, which the kernel should have filled in already.
        lin::exit_group(if slot > 0 { 0 } else { 1 });
    }
    lin::check_true(
        &mut failed,
        b"CLONE_CHILD_SETTID clone returns the child's pid",
        pid > 0,
    );
    if pid > 0 {
        let mut status: i32 = 0;
        let rc = lin::wait4(pid as usize, &mut status, 0);
        lin::check_eq(&mut failed, b"the CLONE_CHILD_SETTID child is reaped", rc, pid);
        lin::check_eq(
            &mut failed,
            b"the child saw its own tid in the slot it named",
            lin::wexitstatus(status) as isize,
            0,
        );
        lin::check_eq(
            &mut failed,
            b"the write stayed in the child's copy of memory",
            slot as isize,
            0,
        );
    }

    // The spawn shape: `CLONE_VM|CLONE_VFORK` with no `CLONE_THREAD` is exactly
    // what a libc's `posix_spawn` builds, and the child's whole job is to exec.
    // This has to work, so it is pinned here from the syscall side; `t_stdspawn`
    // pins the same thing from the std side.
    let pid = lin::clone(
        lin::nr::CLONE_VM | lin::nr::CLONE_VFORK,
        0,
        null,
        null,
        0,
    );
    if pid == 0 {
        let _ = lin::execve(
            TRUE_PROG.as_ptr(),
            &raw const TRUE_ARGV as *const *const u8,
            &raw const ENVP as *const *const u8,
        );
        // Reached only if the exec failed.
        lin::exit_group(127);
    }
    lin::check_true(
        &mut failed,
        b"the spawn shape CLONE_VM|CLONE_VFORK starts a child",
        pid > 0,
    );
    if pid > 0 {
        let mut status: i32 = 0;
        let rc = lin::wait4(pid as usize, &mut status, 0);
        lin::check_eq(&mut failed, b"the spawned child is reaped by its pid", rc, pid);
        lin::check_eq(
            &mut failed,
            b"the spawned child exec'd and exited 0",
            lin::wexitstatus(status) as isize,
            0,
        );
    }

    // A real thread shares its creator's memory and signal handlers. That needs
    // reference-counted page tables, and it is refused rather than faked: a child
    // that only looks like a thread breaks the first time two threads touch the
    // same word.
    let r = lin::clone(
        lin::nr::CLONE_VM | lin::nr::CLONE_THREAD | lin::nr::CLONE_SIGHAND | lin::nr::CLONE_FILES,
        0,
        null,
        null,
        0,
    );
    lin::check_eq(
        &mut failed,
        b"CLONE_THREAD is refused with ENOSYS, not faked",
        r,
        lin::ENOSYS,
    );

    lin::done(failed)
}

#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    lin::die(b"t_clone: panicked\n")
}
