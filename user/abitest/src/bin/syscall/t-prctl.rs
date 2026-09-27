//! t-prctl - the process-control call's option table.
//!
//! Pinned here, per prctl(2): `PR_GET_NAME` fills a 16-byte buffer and answers
//! 0, `PR_SET_NAME` is accepted, `PR_GET_SECCOMP` answers 0 while the process
//! is unconfined, entering the BPF filter mode with no program is refused
//! with `EFAULT` before anything is installed, and an option the handler does
//! not name is accepted as a no-op.
//!
//! Where mpos diverges: this kernel remembers no per-task name. `PR_GET_NAME`
//! therefore answers sixteen zero bytes rather than the name an earlier
//! `PR_SET_NAME` stored, and `PR_SET_NAME` is a request the handler does not
//! even name - it falls through the same `_ => 0` arm as every other option
//! outside the table. The re-read below pins that "accepted, not remembered"
//! pair explicitly. (`PR_GET_NAME` insists on a real 16-byte buffer and
//! answers `EFAULT` otherwise, which this wrapper cannot reach.)
//!
//! The strict-mode latch is deliberately *not* tested here: once
//! `PR_SET_SECCOMP` with `SECCOMP_MODE_STRICT` is granted, the process dies on
//! its next non-allowlisted syscall, so the whole test would end in that
//! death. That destructive path belongs to t-posix, which proves it in a
//! child; this file only pins the seccomp answers that leave the caller alive.

#![no_std]
#![no_main]

#[path = "../../../lib/lin.rs"]
mod lin;

use core::panic::PanicInfo;

/// `SECCOMP_MODE_FILTER`: the BPF filter mode. The program pointer below is
/// null on purpose - the install path reads the program *before* it decides
/// anything, which is what makes this call `EFAULT` and not a success that
/// confined nobody. (A well-formed program installs; the filters themselves
/// are argued by `seccomp.rs`'s host tests.)
const SECCOMP_MODE_FILTER: usize = 2;

#[unsafe(no_mangle)]
extern "C" fn mpos_user_main(_argc: usize, _argv: *const *const u8) -> ! {
    lin::puts(b"t-prctl\n");
    let mut failed = false;

    // PR_GET_NAME: a 16-byte buffer comes back all zero, because mpos
    // remembers no name to report. The 0xAA seed proves the write.
    let mut name = [0xAAu8; 16];
    lin::check_eq(
        &mut failed,
        b"PR_GET_NAME succeeds",
        lin::prctl(lin::PR_GET_NAME, name.as_mut_ptr() as usize, 0, 0, 0),
        0,
    );
    lin::check_true(
        &mut failed,
        b"PR_GET_NAME answers 16 zero bytes (mpos remembers no name)",
        name.iter().all(|&b| b == 0),
    );

    // PR_SET_NAME: accepted, and deliberately not remembered.
    let requested = b"t-prctl\0";
    lin::check_eq(
        &mut failed,
        b"PR_SET_NAME accepts a NUL-terminated name",
        lin::prctl(lin::PR_SET_NAME, requested.as_ptr() as usize, 0, 0, 0),
        0,
    );
    let mut name = [0xAAu8; 16];
    let _ = lin::prctl(lin::PR_GET_NAME, name.as_mut_ptr() as usize, 0, 0, 0);
    lin::check_true(
        &mut failed,
        b"the name was not remembered: PR_GET_NAME is still zeroed",
        name.iter().all(|&b| b == 0),
    );

    // PR_GET_SECCOMP: a fresh process is unconfined.
    lin::check_eq(
        &mut failed,
        b"PR_GET_SECCOMP answers 0 while unconfined",
        lin::prctl(lin::PR_GET_SECCOMP, 0, 0, 0, 0),
        0,
    );

    // PR_SET_SECCOMP: the filter mode refuses a request that carries no
    // program. Strict is left to t-posix, because latching it is fatal to
    // this process.
    lin::check_eq(
        &mut failed,
        b"PR_SET_SECCOMP with the filter mode and no program answers EFAULT",
        lin::prctl(lin::PR_SET_SECCOMP, SECCOMP_MODE_FILTER, 0, 0, 0),
        lin::EFAULT,
    );

    // An option the handler does not name is accepted and dropped.
    lin::check_eq(
        &mut failed,
        b"an unknown option answers 0, accepted as a no-op",
        lin::prctl(0x7fff, 0, 0, 0, 0),
        0,
    );

    lin::done(failed)
}

#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    lin::die(b"t-prctl: panicked\n")
}
