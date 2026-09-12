//! t_mprotect - one test for mprotect, which the musl_hello strace shows
//! turning the page below the alt stack into a PROT_NONE guard:
//! `mprotect(0x..., 4096, PROT_NONE) = 0`.
//!
//! The test mmaps a RW page, walks it through PROT_READ, PROT_NONE, and
//! back to PROT_READ|PROT_WRITE, and pins only what the strace shows:
//! page-aligned requests succeed. Whether PROT_NONE actually faults on
//! access needs fault delivery and is deliberately not probed; the write
//! after restoring RW proves the region survived the round trip.

#![no_std]
#![no_main]

#[path = "../lib/lin.rs"]
mod lin;

use core::panic::PanicInfo;

const PATTERN: u64 = 0x0b5e_1ba11_b0de_0001;

#[unsafe(no_mangle)]
extern "C" fn mpos_user_main(_argc: usize, _argv: *const *const u8) -> ! {
    lin::puts(b"t_mprotect\n");
    let mut failed = false;

    let p = lin::mmap(
        0,
        8192,
        lin::PROT_READ | lin::PROT_WRITE,
        lin::MAP_PRIVATE | lin::MAP_ANONYMOUS,
        -1,
        0,
    );
    lin::check_true(
        &mut failed,
        b"the RW page to protect succeeds",
        p != lin::MAP_FAILED && p > 0 && p as usize % 4096 == 0,
    );

    let r = lin::mprotect(p as usize, 8192, lin::PROT_READ);
    lin::check_eq(&mut failed, b"dropping to PROT_READ succeeds", r, 0);

    let r = lin::mprotect(p as usize, 8192, lin::PROT_NONE);
    lin::check_eq(&mut failed, b"the guard-page PROT_NONE succeeds", r, 0);

    let r = lin::mprotect(p as usize, 8192, lin::PROT_READ | lin::PROT_WRITE);
    lin::check_eq(&mut failed, b"restoring PROT_READ|PROT_WRITE succeeds", r, 0);

    unsafe { (p as *mut u64).write_volatile(PATTERN) };
    let back = unsafe { (p as *const u64).read_volatile() };
    lin::check_true(
        &mut failed,
        b"the region takes writes after the round trip",
        back == PATTERN,
    );

    lin::done(failed)
}

#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    lin::die(b"t_mprotect: panicked\n")
}
