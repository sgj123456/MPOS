//! t_exit_group - one test for exit_group, how musl terminates: the strace
//! closes with `exit_group(0)`.
//!
//! An exit code cannot be checked from inside the dying process, so the
//! test announces its plan and leaves with a distinctive 42 - the same
//! code the kernel's milestone check already expects of /bin/hello. The
//! harness is expected to run
//!
//!     t_exit_group; echo $?
//!
//! and see 42. mpos maps the Linux number onto the same path EXIT takes,
//! so this also pins that the code survives the scheduler to the parent's
//! wait.

#![no_std]
#![no_main]

#[path = "../lib/lin.rs"]
mod lin;

use core::panic::PanicInfo;

/// The exit code the harness must observe.
const CODE: i32 = 42;

#[unsafe(no_mangle)]
extern "C" fn mpos_user_main(_argc: usize, _argv: *const *const u8) -> ! {
    lin::puts(b"t_exit_group: the code below is only observable from outside;\n");
    lin::puts(b"  run `t_exit_group; echo $?` and expect 42\n");
    lin::exit_group(CODE)
}

#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    lin::die(b"t_exit_group: panicked\n")
}
