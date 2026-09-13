//! t_munmap - one test for munmap, the strace's goodbye before exit_group:
//! Rust std frees its 4096-byte scratch page.
//!
//! The test mmaps a page and frees it, pins the 0 answer, then frees the
//! same range again: Linux lets munmap run over an aligned range that maps
//! nothing and still answers 0. Whether the range is actually unreachable
//! afterwards needs fault delivery and is deliberately not probed.
//!
//! Added pins: an unaligned address and a zero length answer EINVAL, an
//! unmapped aligned range is a hole answered 0, and unmapping the middle
//! page of a three-page fixed mapping leaves the pages either side with
//! their contents.

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

    // The refusal set the man page fixes: alignment and a positive length.
    lin::check_eq(
        &mut failed,
        b"an unaligned address answers EINVAL",
        lin::munmap(0x5000_0123, 4096),
        lin::EINVAL,
    );
    lin::check_eq(
        &mut failed,
        b"a zero length answers EINVAL",
        lin::munmap(0x5000_A000, 0),
        lin::EINVAL,
    );

    // An aligned range nothing is mapped in is a hole, not an error.
    lin::check_eq(
        &mut failed,
        b"an unmapped aligned range is a hole, not an error",
        lin::munmap(0x5000_A000, 4096),
        0,
    );

    // Unmapping the middle page of a three-page mapping leaves the pages
    // either side mapped and their contents intact.
    let three = lin::mmap(
        0x5000_B000,
        12288,
        lin::PROT_READ | lin::PROT_WRITE,
        lin::MAP_PRIVATE | lin::MAP_ANONYMOUS | lin::MAP_FIXED,
        -1,
        0,
    );
    lin::check_eq(
        &mut failed,
        b"the three-page range maps fixed",
        three,
        0x5000_B000,
    );
    const P1: u64 = 0x5151_5151_5151_5151;
    const P3: u64 = 0x5353_5353_5353_5353;
    unsafe {
        (0x5000_B000 as *mut u64).write_volatile(P1);
        ((0x5000_B000 + 8192) as *mut u64).write_volatile(P3);
    }
    lin::check_eq(
        &mut failed,
        b"unmapping the middle page succeeds",
        lin::munmap(0x5000_B000 + 4096, 4096),
        0,
    );
    let back1 = unsafe { (0x5000_B000 as *const u64).read_volatile() };
    let back3 = unsafe { ((0x5000_B000 + 8192) as *const u64).read_volatile() };
    lin::check_true(
        &mut failed,
        b"the pages either side keep their contents",
        back1 == P1 && back3 == P3,
    );

    lin::done(failed)
}

#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    lin::die(b"t_munmap: panicked\n")
}
