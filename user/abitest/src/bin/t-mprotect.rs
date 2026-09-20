//! t-mprotect - one test for mprotect, which a musl strace shows
//! turning the page below the alt stack into a PROT_NONE guard:
//! `mprotect(0x..., 4096, PROT_NONE) = 0`.
//!
//! The test mmaps a RW page, walks it through PROT_READ, PROT_NONE, and
//! back to PROT_READ|PROT_WRITE, and pins only what the strace shows:
//! page-aligned requests succeed. Whether PROT_NONE actually faults on
//! access needs fault delivery and is deliberately not probed; the write
//! after restoring RW proves the region survived the round trip.
//!
//! Added pins: an unaligned address answers EINVAL, a zero length answers 0,
//! a range larger than the mapping answers 0 (unmapped pages are skipped),
//! and the round trip extends to both pages of the mapping - guard them,
//! restore RW, and write and read each page.

#![no_std]
#![no_main]

#[path = "../../lib/lin.rs"]
mod lin;

use core::panic::PanicInfo;

const PATTERN: u64 = 0x0b5e_1ba11_b0de_0001;

#[unsafe(no_mangle)]
extern "C" fn mpos_user_main(_argc: usize, _argv: *const *const u8) -> ! {
    lin::puts(b"t-mprotect\n");
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

    // The refusal set: only the address's alignment is validated.
    lin::check_eq(
        &mut failed,
        b"an unaligned address answers EINVAL",
        lin::mprotect(0x5000_0123, 4096, lin::PROT_READ),
        lin::EINVAL,
    );
    lin::check_eq(
        &mut failed,
        b"a zero length answers 0",
        lin::mprotect(p as usize, 0, lin::PROT_READ),
        0,
    );
    lin::check_eq(
        &mut failed,
        b"a range larger than the mapping answers 0",
        lin::mprotect(p as usize, 16384, lin::PROT_READ | lin::PROT_WRITE),
        0,
    );

    // Round trip on both pages: guard them, restore RW, and prove each page
    // still takes writes and kept what it held.
    const P2: u64 = 0x0b5e_1ba11_b0de_0002;
    lin::check_eq(
        &mut failed,
        b"the guard-page PROT_NONE over both pages succeeds",
        lin::mprotect(p as usize, 8192, lin::PROT_NONE),
        0,
    );
    lin::check_eq(
        &mut failed,
        b"restoring PROT_READ|PROT_WRITE over both pages succeeds",
        lin::mprotect(p as usize, 8192, lin::PROT_READ | lin::PROT_WRITE),
        0,
    );
    unsafe { ((p as usize + 4096) as *mut u64).write_volatile(P2) };
    let back1 = unsafe { (p as *const u64).read_volatile() };
    let back2 = unsafe { ((p as usize + 4096) as *const u64).read_volatile() };
    lin::check_true(
        &mut failed,
        b"both pages take writes and keep their contents after the round trip",
        back1 == PATTERN && back2 == P2,
    );

    lin::done(failed)
}

#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    lin::die(b"t-mprotect: panicked\n")
}
