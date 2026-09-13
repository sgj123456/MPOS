//! t_madvise - one test for madvise, whose man page lets every advice it
//! knows answer 0 and has DONTNEED promise only that re-access refills the
//! pages. mpos's Linux surface is a documented stub that answers 0
//! unconditionally, so the pins are the returns - NORMAL, SEQUENTIAL and
//! DONTNEED on a live two-page range, and DONTNEED on an unmapped aligned
//! range - while the contents are deliberately not pinned: Linux zeroes a
//! DONTNEED page on re-access, the stub does nothing to it.

#![no_std]
#![no_main]

#[path = "../lib/lin.rs"]
mod lin;

use core::panic::PanicInfo;

#[unsafe(no_mangle)]
extern "C" fn mpos_user_main(_argc: usize, _argv: *const *const u8) -> ! {
    lin::puts(b"t_madvise\n");
    let mut failed = false;

    let p = lin::mmap(
        0x5000_2000,
        8192,
        lin::PROT_READ | lin::PROT_WRITE,
        lin::MAP_PRIVATE | lin::MAP_ANONYMOUS | lin::MAP_FIXED,
        -1,
        0,
    );
    lin::check_eq(
        &mut failed,
        b"the two-page range maps fixed",
        p,
        0x5000_2000,
    );

    lin::check_eq(
        &mut failed,
        b"MADV_NORMAL on the live range answers 0",
        lin::madvise(0x5000_2000, 8192, lin::MADV_NORMAL),
        0,
    );
    lin::check_eq(
        &mut failed,
        b"MADV_SEQUENTIAL on the live range answers 0",
        lin::madvise(0x5000_2000, 8192, lin::MADV_SEQUENTIAL),
        0,
    );
    lin::check_eq(
        &mut failed,
        b"MADV_DONTNEED on the live range answers 0",
        lin::madvise(0x5000_2000, 8192, lin::MADV_DONTNEED),
        0,
    );

    // The man page pins no contents for DONTNEED - re-access refills the
    // pages - and the stub touches nothing, so only the return is pinned,
    // here over a range nothing is mapped in.
    lin::check_eq(
        &mut failed,
        b"madvise over an unmapped aligned range still answers 0",
        lin::madvise(0x5000_4000, 4096, lin::MADV_DONTNEED),
        0,
    );

    lin::done(failed)
}

#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    lin::die(b"t_madvise: panicked\n")
}
