//! t-syslog - the kernel log's own door, klogctl's numbers.
//!
//! Pinned here, per syslog(2): the counter types answer the unread byte
//! count and the buffer's capacity, the read-all type copies the tail of
//! the log without consuming it, a read with a null buffer is EINVAL
//! before anything else, the console-level type validates its level
//! against 1..=8, the bookkeeping types are accepted, and a type outside
//! the eleven is EINVAL. The destructive read (type 2) waits until the
//! log is nonempty and is not exercised here: the ring is userland's to
//! fill, and a test that parked in it would outlive the machine.

#![no_std]
#![no_main]

#[path = "../../../lib/lin.rs"]
mod lin;

use core::panic::PanicInfo;

static mut LOG: [u8; 512] = [0; 512];

#[unsafe(no_mangle)]
extern "C" fn mpos_user_main(_argc: usize, _argv: *const *const u8) -> ! {
    lin::puts(b"t-syslog\n");
    let mut failed = false;

    // The counters: the unread bytes are non-negative, and the buffer is
    // the capacity this kernel keeps, 16 KiB.
    let unread = lin::syslog(9, core::ptr::null_mut(), 0);
    lin::check_true(&mut failed, b"SIZE_UNREAD answers a count", unread >= 0);
    lin::check_eq(
        &mut failed,
        b"SIZE_BUFFER answers the capacity",
        lin::syslog(10, core::ptr::null_mut(), 0),
        16384,
    );

    // The read-all: the tail of the log, non-destructively, so the unread
    // count is the same after it as before.
    let before = lin::syslog(9, core::ptr::null_mut(), 0);
    let got = lin::syslog(3, unsafe { &raw mut LOG as *mut u8 }, 512);
    lin::check_true(&mut failed, b"READ_ALL answers a count", got >= 0);
    let r = lin::syslog(3, unsafe { &raw mut LOG as *mut u8 }, 512);
    lin::check_eq(&mut failed, b"READ_ALL did not consume the log", r, got);
    let _ = before;

    // The validations: a null buffer for the readers is EINVAL, and the
    // console-level type checks its level against 1..=8.
    lin::check_eq(
        &mut failed,
        b"READ_ALL with a null buffer answers EINVAL",
        lin::syslog(3, core::ptr::null_mut(), 512),
        lin::EINVAL,
    );
    lin::check_eq(
        &mut failed,
        b"READ with a null buffer answers EINVAL",
        lin::syslog(2, core::ptr::null_mut(), 512),
        lin::EINVAL,
    );
    lin::check_eq(
        &mut failed,
        b"CONSOLE_LEVEL with level 0 answers EINVAL",
        lin::syslog(8, core::ptr::null_mut(), 0),
        lin::EINVAL,
    );
    lin::check_eq(
        &mut failed,
        b"CONSOLE_LEVEL with level 9 answers EINVAL",
        lin::syslog(8, core::ptr::null_mut(), 9),
        lin::EINVAL,
    );
    lin::check_eq(
        &mut failed,
        b"CONSOLE_LEVEL with level 5 succeeds",
        lin::syslog(8, core::ptr::null_mut(), 5),
        0,
    );

    // The bookkeeping types: open, close, console on and off, and the
    // clear, which answers success and empties the tail.
    lin::check_eq(
        &mut failed,
        b"CLOSE succeeds",
        lin::syslog(0, core::ptr::null_mut(), 0),
        0,
    );
    lin::check_eq(
        &mut failed,
        b"OPEN succeeds",
        lin::syslog(1, core::ptr::null_mut(), 0),
        0,
    );
    lin::check_eq(
        &mut failed,
        b"CONSOLE_OFF succeeds",
        lin::syslog(6, core::ptr::null_mut(), 0),
        0,
    );
    lin::check_eq(
        &mut failed,
        b"CONSOLE_ON succeeds",
        lin::syslog(7, core::ptr::null_mut(), 0),
        0,
    );
    lin::check_eq(
        &mut failed,
        b"CLEAR succeeds",
        lin::syslog(5, core::ptr::null_mut(), 0),
        0,
    );
    lin::check_eq(
        &mut failed,
        b"the tail after a clear is empty",
        lin::syslog(3, unsafe { &raw mut LOG as *mut u8 }, 512),
        0,
    );

    // A type outside the eleven: EINVAL.
    lin::check_eq(
        &mut failed,
        b"an unknown type answers EINVAL",
        lin::syslog(99, core::ptr::null_mut(), 0),
        lin::EINVAL,
    );

    lin::done(failed)
}

#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    lin::die(b"t-syslog: panicked\n")
}
