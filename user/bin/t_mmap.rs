//! t_mmap - one test for mmap, of which the musl_hello strace shows three
//! anonymous mappings on the way up:
//!
//! - a 12288-byte RW region with MAP_STACK, the alt stack Rust std reserves
//! - a 4096-byte PROT_NONE page at a MAP_FIXED address, its guard
//! - a plain 4096-byte RW page, malloc's overflow past brk
//!
//! The test asks for the same three shapes and pins the contract they
//! carry: kernel-picked addresses come back page-aligned and non-overlap-
//! ping, MAP_FIXED comes back exactly where it was asked, anonymous memory
//! arrives zeroed, and the writable regions take writes. Whether PROT_NONE
//! actually faults on access needs fault delivery and is deliberately not
//! probed.

#![no_std]
#![no_main]

#[path = "../lib/lin.rs"]
mod lin;

use core::panic::PanicInfo;

/// A fixed address far from the image base (0x400000), the loader's brk
/// region, and the kernel's mmap bump (0x80000000).
const GUARD: usize = 0x5000_0000;
const PATTERN: u64 = 0x1234_5678_9abc_def0;

#[unsafe(no_mangle)]
extern "C" fn mpos_user_main(_argc: usize, _argv: *const *const u8) -> ! {
    lin::puts(b"t_mmap\n");
    let mut failed = false;

    // The MAP_STACK region the alt stack would live in.
    let stack = lin::mmap(
        0,
        12288,
        lin::PROT_READ | lin::PROT_WRITE,
        lin::MAP_PRIVATE | lin::MAP_ANONYMOUS | lin::MAP_STACK,
        -1,
        0,
    );
    lin::check_true(
        &mut failed,
        b"the 12288-byte MAP_STACK mapping succeeds",
        stack != lin::MAP_FAILED && stack > 0,
    );
    lin::check_true(
        &mut failed,
        b"the kernel picks a page-aligned address",
        stack > 0 && stack as usize % 4096 == 0,
    );

    let first = unsafe { (stack as *const u64).read_volatile() };
    let third = unsafe { ((stack as usize + 8192) as *const u64).read_volatile() };
    lin::check_true(
        &mut failed,
        b"anonymous memory arrives zeroed",
        first == 0 && third == 0,
    );

    unsafe { (stack as *mut u64).write_volatile(PATTERN) };
    let back = unsafe { (stack as *const u64).read_volatile() };
    lin::check_true(
        &mut failed,
        b"the region takes writes",
        back == PATTERN,
    );

    // The guard page, at the fixed address.
    let guard = lin::mmap(
        GUARD,
        4096,
        lin::PROT_NONE,
        lin::MAP_PRIVATE | lin::MAP_ANONYMOUS | lin::MAP_FIXED,
        -1,
        0,
    );
    lin::check_eq(
        &mut failed,
        b"MAP_FIXED maps exactly the address asked",
        guard,
        GUARD as isize,
    );

    // The plain page: fresh address, no overlap with the first mapping.
    let page = lin::mmap(
        0,
        4096,
        lin::PROT_READ | lin::PROT_WRITE,
        lin::MAP_PRIVATE | lin::MAP_ANONYMOUS,
        -1,
        0,
    );
    lin::check_true(
        &mut failed,
        b"a plain 4096-byte mapping succeeds",
        page != lin::MAP_FAILED && page > 0 && page as usize % 4096 == 0,
    );
    lin::check_true(
        &mut failed,
        b"it does not overlap the earlier mapping",
        !(page >= stack && page < stack + 12288),
    );

    lin::done(failed)
}

#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    lin::die(b"t_mmap: panicked\n")
}
