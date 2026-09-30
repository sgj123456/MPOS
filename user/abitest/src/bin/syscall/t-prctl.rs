//! t-prctl - the process-control call's option table.
//!
//! Pinned here, per prctl(2): the name pair round-trips - a `PR_SET_NAME`
//! stores the caller's bytes and `PR_GET_NAME` reads them back, NUL-terminated
//! within the sixteen-byte `TASK_COMM_LEN` slot - and a name that fills that
//! slot whole is *truncated and accepted*, not refused, as Linux truncates it;
//! `PR_GET_SECCOMP` answers 0 while the process is unconfined; entering the BPF
//! filter mode with no program is refused with `EFAULT` before anything is
//! installed; and an option the handler does not model is refused with
//! `EINVAL`, the way Linux's `prctl` default arm answers a number it does not
//! know.
//!
//! The name round-trip is the behaviour this program exists to pin: `PR_SET_NAME`
//! used to fall through the same `_ => 0` arm as every unmodelled option -
//! accepted and dropped - and `PR_GET_NAME` answered sixteen zeroes, so the
//! read-back below would have found no name. It now does, which is what makes
//! this the counterfactual for the change.
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

    // The name pair round-trips: what PR_SET_NAME stores is what PR_GET_NAME
    // hands back, NUL-terminated inside the slot. The 0xAA seed proves the
    // read-back is the kernel's write, not the buffer's initial state.
    let requested = b"renamed\0";
    lin::check_eq(
        &mut failed,
        b"PR_SET_NAME accepts a NUL-terminated name",
        lin::prctl(lin::PR_SET_NAME, requested.as_ptr() as usize, 0, 0, 0),
        0,
    );
    let mut name = [0xAAu8; 16];
    lin::check_eq(
        &mut failed,
        b"PR_GET_NAME succeeds",
        lin::prctl(lin::PR_GET_NAME, name.as_mut_ptr() as usize, 0, 0, 0),
        0,
    );
    lin::check_true(
        &mut failed,
        b"the name round-trips: PR_GET_NAME reads back what PR_SET_NAME stored",
        &name[..7] == b"renamed" && name[7] == 0,
    );

    // A name that fills the whole slot is truncated and accepted, not refused.
    // Measured on Linux: sixteen characters with no NUL inside `TASK_COMM_LEN`
    // answer `0`, and `PR_GET_NAME` then reads back fifteen. This kernel used to
    // answer `EFAULT` there - the wrong-shape answer in §4.2's sense, because a
    // program that named itself sixteen bytes long would take its
    // "the kernel would not name me" fallback on a kernel that did name it.
    let long = [b'x'; 16];
    lin::check_eq(
        &mut failed,
        b"PR_SET_NAME truncates a name that fills the slot, it does not refuse it",
        lin::prctl(lin::PR_SET_NAME, long.as_ptr() as usize, 0, 0, 0),
        0,
    );
    let mut name = [0xAAu8; 16];
    let _ = lin::prctl(lin::PR_GET_NAME, name.as_mut_ptr() as usize, 0, 0, 0);
    lin::check_true(
        &mut failed,
        b"fifteen characters survive the cut and the slot ends in NUL",
        name[..15] == [b'x'; 15] && name[15] == 0,
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

    // An option the handler does not model is refused, the way Linux's `prctl`
    // default arm answers a number it does not know - not the silent `0` this
    // kernel used to hand back for every unmodelled option.
    lin::check_eq(
        &mut failed,
        b"an unknown option is refused with EINVAL",
        lin::prctl(0x7fff, 0, 0, 0, 0),
        lin::EINVAL,
    );

    lin::done(failed)
}

#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    lin::die(b"t-prctl: panicked\n")
}
