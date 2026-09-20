//! t-fork - one test for fork, the call every shell is built on.
//!
//! The man page pins three things at once: the child gets 0 back and the parent
//! gets the child's pid; both continue from the instruction after the call; and
//! the child gets a *copy* of the parent's memory, so a write on one side is
//! invisible on the other.
//!
//! The last one is what the test really exercises. A variable is set before the
//! fork and rewritten by the child; the parent must still see the old value.
//! That is copy-on-write doing its job - the page only splits when the child
//! writes it - and a kernel that handed both processes the same page would pass
//! everything else here while failing this.

#![no_std]
#![no_main]

#[path = "../../lib/lin.rs"]
mod lin;

use core::panic::PanicInfo;

/// The variable the child updates and the parent must not see change. A `static
/// mut` rather than a local so both processes address it the same way and the
/// comparison is unambiguous.
static mut SHARED: i32 = 7;

/// The code the child leaves with, distinct from anything the parent uses.
const CHILD_CODE: i32 = 42;

#[unsafe(no_mangle)]
extern "C" fn mpos_user_main(_argc: usize, _argv: *const *const u8) -> ! {
    lin::puts(b"t-fork\n");
    let mut failed = false;

    let pid = lin::fork();
    if pid < 0 {
        lin::check_true(&mut failed, b"fork succeeds", false);
        lin::done(failed);
    }

    if pid == 0 {
        // The child: write the shared variable, then leave with a code the
        // parent can recognise. Any print here would be reported against the
        // child's copy of `failed`, so the child stays quiet and lets the parent
        // do the judging.
        unsafe { SHARED = 99 };
        lin::exit_group(CHILD_CODE);
    }

    // The parent. `pid` is the child's pid, which is what fork promises.
    lin::check_true(&mut failed, b"fork returns the child's pid to the parent", pid > 0);

    let mut status: i32 = 0;
    let rc = lin::wait4(pid as usize, &mut status, 0);
    lin::check_eq(
        &mut failed,
        b"wait4 reaps the child and returns its pid",
        rc,
        pid,
    );
    lin::check_true(
        &mut failed,
        b"the child exited normally, not by signal",
        lin::wifexited(status),
    );
    lin::check_eq(
        &mut failed,
        b"the child's exit code is the one it passed to exit",
        lin::wexitstatus(status) as isize,
        CHILD_CODE as isize,
    );

    // The heart of it: the child's write stayed in the child.
    lin::check_eq(
        &mut failed,
        b"the child's write did not reach the parent (copy-on-write)",
        unsafe { SHARED } as isize,
        7,
    );

    lin::done(failed)
}

#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    lin::die(b"t-fork: panicked\n")
}
