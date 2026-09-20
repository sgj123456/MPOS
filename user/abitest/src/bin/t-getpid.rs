//! t-getpid - one test for the process-identity trio: getpid, gettid, and
//! getppid.
//!
//! The man page pins: getpid returns the caller's process id, always
//! positive; gettid returns the calling thread's id, which for the single
//! main thread of a process IS the process id; and getppid returns the
//! parent's id, 0 only for init. Pinned here: the id is positive, stable
//! across two calls - an id that moved between reads would break every
//! /proc lookup - the main thread's tid equals the pid, and the parent is
//! a real, different process: this test is spawned by the shell, so the
//! parent id is positive and not our own.

#![no_std]
#![no_main]

#[path = "../../lib/lin.rs"]
mod lin;

use core::panic::PanicInfo;

#[unsafe(no_mangle)]
extern "C" fn mpos_user_main(_argc: usize, _argv: *const *const u8) -> ! {
    lin::puts(b"t-getpid\n");
    let mut failed = false;

    let first = lin::getpid();
    lin::check_true(
        &mut failed,
        b"getpid answers a positive id",
        first > 0,
    );

    let second = lin::getpid();
    lin::check_eq(
        &mut failed,
        b"a second getpid answers the same id",
        second,
        first,
    );

    // The main thread is the process: its tid is the pid.
    let tid = lin::gettid();
    lin::check_eq(
        &mut failed,
        b"the main thread's gettid equals getpid",
        tid,
        first,
    );

    // The shell spawned this test, so the parent is real and not us.
    let parent = lin::getppid();
    lin::check_true(
        &mut failed,
        b"getppid answers a positive parent id",
        parent > 0,
    );
    lin::check_true(
        &mut failed,
        b"the parent is a different process than this one",
        parent != first,
    );

    lin::done(failed)
}

#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    lin::die(b"t-getpid: panicked\n")
}
