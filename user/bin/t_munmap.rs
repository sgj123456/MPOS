//! t_munmap - one test for munmap, the strace's goodbye before exit_group:
//! Rust std frees its 4096-byte scratch page.
//!
//! The test mmaps a page and frees it, pins the 0 answer, then frees the
//! same range again: Linux lets munmap run over an aligned range that maps
//! nothing and still answers 0. Whether the range is actually unreachable
//! afterwards needs fault delivery and is deliberately not probed.

#![no_std]
#![no_main]

#[path = "../lib/lin.rs"]
mod lin;

use core::panic::PanicInfo;

#[unsafe(no_mangle)]
extern "C" fn mpos_user_main(_argc: usize, _argv: *const *const u8) -> ! {
    lin::puts(b"t_munmap\n");
    let mut failed = false;

    let p = lin::mmap(
        0,
        4096,
        lin::PROT_READ | lin::PROT_WRITE,
        lin::MAP_PRIVATE | lin::MAP_ANONYMOUS,
        -1,
        0,
    );
    lin::check_true(
        &mut failed,
        b"the page to free succeeds",
        p != lin::MAP_FAILED && p > 0 && p as usize % 4096 == 0,
    );

    let r = lin::munmap(p as usize, 4096);
    lin::check_eq(&mut failed, b"unmapping a live page succeeds", r, 0);

    let r = lin::munmap(p as usize, 4096);
    lin::check_eq(
        &mut failed,
        b"unmapping the range again still succeeds",
        r,
        0,
    );

    lin::done(failed)
}

#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    lin::die(b"t_munmap: panicked\n")
}
