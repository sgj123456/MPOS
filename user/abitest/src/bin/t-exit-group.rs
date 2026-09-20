//! t-exit-group - one test for exit_group, how musl terminates: the strace
//! closes with `exit_group(0)`.
//!
//! The code a process leaves with cannot be read from inside the dying
//! process, so it is read from just outside it: the test forks, the child
//! calls `exit_group(42)`, and the parent reaps it and checks the status the
//! kernel handed back. That is what "the code survives the scheduler to the
//! parent's wait" means, and it is observable - a test that simply exited 42
//! would leave the harness holding the code, reading it as a failure of the
//! very test that produced it.

#![no_std]
#![no_main]

#[path = "../../lib/lin.rs"]
mod lin;

use core::panic::PanicInfo;

/// The exit code the parent must observe.
const CODE: i32 = 42;

#[unsafe(no_mangle)]
extern "C" fn mpos_user_main(_argc: usize, _argv: *const *const u8) -> ! {
    lin::puts(b"t-exit-group\n");
    let mut failed = false;

    let child = lin::fork();
    if child == 0 {
        // Nothing after this runs: the group ends here, with `CODE`.
        lin::exit_group(CODE);
    }
    lin::check_true(&mut failed, b"fork answered a child", child > 0);

    let mut status: i32 = 0;
    let reaped = lin::wait4(child as usize, &raw mut status, 0);
    lin::check_eq(
        &mut failed,
        b"the exit_group'ing child is reaped by its pid",
        reaped,
        child,
    );
    lin::check_true(
        &mut failed,
        b"the child left a status, not a signal",
        status & 0x7f == 0,
    );
    lin::check_eq(
        &mut failed,
        b"the code passed to exit_group is the one the parent waits for",
        ((status >> 8) & 0xff) as isize,
        CODE as isize,
    );

    lin::done(failed)
}

#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    lin::die(b"t-exit-group: panicked\n")
}
