//! t-getrandom - one test for getrandom, which the strace shows twice at
//! startup (16 bytes and 8 bytes, GRND_NONBLOCK): Rust std seeds its
//! hasher with it.
//!
//! Pinned here: a blocking fill of n bytes answers exactly n, the
//! GRND_NONBLOCK variant answers n too (mpos always has entropy to spare),
//! and a zero-length request answers 0 without touching the buffer.
//! Randomness itself cannot be asserted - only the counts. A page-sized
//! 4096-byte fill answers its exact length, and two differently sized
//! fills never answer negative.

#![no_std]
#![no_main]

#[path = "../../lib/lin.rs"]
mod lin;

use core::panic::PanicInfo;

const GRND_NONBLOCK: usize = 1;
static mut A: [u8; 16] = [0; 16];
static mut B: [u8; 16] = [0; 16];
static mut C: [u8; 1] = [0; 1];
static mut BIG: [u8; 4096] = [0; 4096];
static mut D: [u8; 7] = [0; 7];
static mut E: [u8; 100] = [0; 100];

#[unsafe(no_mangle)]
extern "C" fn mpos_user_main(_argc: usize, _argv: *const *const u8) -> ! {
    lin::puts(b"t-getrandom\n");
    let mut failed = false;

    let a = unsafe { core::slice::from_raw_parts_mut(&raw mut A as *mut u8, 16) };
    let n = lin::getrandom(a, 0);
    lin::check_eq(&mut failed, b"a 16-byte fill answers 16", n, 16);

    let b = unsafe { core::slice::from_raw_parts_mut(&raw mut B as *mut u8, 16) };
    let n = lin::getrandom(b, GRND_NONBLOCK);
    lin::check_eq(&mut failed, b"the non-blocking fill answers 16 too", n, 16);

    let c = unsafe { core::slice::from_raw_parts_mut(&raw mut C as *mut u8, 1) };
    let n = lin::getrandom(c, 0);
    lin::check_eq(&mut failed, b"a 1-byte fill answers 1", n, 1);

    let empty: &mut [u8] = &mut [];
    let n = lin::getrandom(empty, 0);
    lin::check_eq(&mut failed, b"a zero-length request answers 0", n, 0);

    // A whole page of entropy in one call: the exact count comes back.
    let big = unsafe { core::slice::from_raw_parts_mut(&raw mut BIG as *mut u8, 4096) };
    let n = lin::getrandom(big, 0);
    lin::check_eq(&mut failed, b"a 4096-byte fill answers 4096", n, 4096);

    // Different sizes, same story: never a negative answer.
    let d = unsafe { core::slice::from_raw_parts_mut(&raw mut D as *mut u8, 7) };
    let n = lin::getrandom(d, 0);
    lin::check_true(&mut failed, b"a 7-byte fill never answers negative", n >= 0);
    let e = unsafe { core::slice::from_raw_parts_mut(&raw mut E as *mut u8, 100) };
    let n = lin::getrandom(e, 0);
    lin::check_true(
        &mut failed,
        b"a 100-byte fill never answers negative",
        n >= 0,
    );

    lin::done(failed)
}

#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    lin::die(b"t-getrandom: panicked\n")
}
