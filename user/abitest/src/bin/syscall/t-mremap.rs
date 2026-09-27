//! t-mremap - one test for mremap, the resize-and-relocate call musl's
//! allocator leans on. The man page pins: a shrink answers the old address
//! with the tail's pages gone and the rest of the contents intact; a grow
//! where the pages above are free keeps the mapping in place with its
//! contents; MREMAP_MAYMOVE may relocate a mapping, carrying the same pages
//! to a fresh page-aligned address; MREMAP_FIXED|MREMAP_MAYMOVE moves to
//! exactly the address named; and a zero old_len, an unaligned old address,
//! or MREMAP_FIXED without MREMAP_MAYMOVE answer EINVAL. A same-size mremap
//! is a no-op on Linux too - mm/mremap.c answers old_len >= new_len from the
//! shrink path before MAYMOVE can matter - so the relocation path is
//! exercised by a grow the occupied neighborhood forces to move.

#![no_std]
#![no_main]

#[path = "../../../lib/lin.rs"]
mod lin;

use core::panic::PanicInfo;

const RW: usize = lin::PROT_READ | lin::PROT_WRITE;
const ANON_PRIVATE: usize = lin::MAP_PRIVATE | lin::MAP_ANONYMOUS;

#[unsafe(no_mangle)]
extern "C" fn mpos_user_main(_argc: usize, _argv: *const *const u8) -> ! {
    lin::puts(b"t-mremap\n");
    let mut failed = false;

    // (a) Shrink two pages to one: the tail is dropped, the head keeps its
    // contents.
    let p = lin::mmap(0x5000_C000, 8192, RW, ANON_PRIVATE | lin::MAP_FIXED, -1, 0);
    lin::check_eq(
        &mut failed,
        b"the two-page range maps fixed",
        p,
        0x5000_C000,
    );
    const P1: u64 = 0x5c00_5c00_5c00_0001;
    unsafe { (0x5000_C000 as *mut u64).write_volatile(P1) };
    lin::check_eq(
        &mut failed,
        b"shrinking answers the old address",
        lin::mremap(0x5000_C000, 8192, 4096, 0, 0),
        0x5000_C000,
    );
    let back = unsafe { (0x5000_C000 as *const u64).read_volatile() };
    lin::check_true(
        &mut failed,
        b"the head keeps its contents across the shrink",
        back == P1,
    );

    // (b) Grow one page to two where the pages above are free: the mapping
    // stays put and its contents survive.
    let p = lin::mmap(0x5000_D000, 4096, RW, ANON_PRIVATE | lin::MAP_FIXED, -1, 0);
    lin::check_eq(&mut failed, b"the page to grow maps fixed", p, 0x5000_D000);
    const P2: u64 = 0x5d00_5d00_5d00_0002;
    unsafe { (0x5000_D000 as *mut u64).write_volatile(P2) };
    lin::check_eq(
        &mut failed,
        b"growing in place answers the old address",
        lin::mremap(0x5000_D000, 4096, 8192, 0, 0),
        0x5000_D000,
    );
    let back = unsafe { (0x5000_D000 as *const u64).read_volatile() };
    lin::check_true(
        &mut failed,
        b"the contents survive the grow in place",
        back == P2,
    );

    // (c) A same-size mremap is a no-op: the old address comes back with the
    // contents intact, whatever MAYMOVE says - Linux answers old_len >=
    // new_len from the shrink path before any move can be considered.
    let p = lin::mmap(0x5000_E000, 4096, RW, ANON_PRIVATE | lin::MAP_FIXED, -1, 0);
    lin::check_eq(&mut failed, b"the page to move maps fixed", p, 0x5000_E000);
    const P3: u64 = 0x5e00_5e00_5e00_0003;
    unsafe { (0x5000_E000 as *mut u64).write_volatile(P3) };
    lin::check_eq(
        &mut failed,
        b"a same-size MREMAP_MAYMOVE answers the old address",
        lin::mremap(0x5000_E000, 4096, 4096, lin::MREMAP_MAYMOVE, 0),
        0x5000_E000,
    );
    let back = unsafe { (0x5000_E000 as *const u64).read_volatile() };
    lin::check_true(
        &mut failed,
        b"the contents survive the same-size call",
        back == P3,
    );

    // (c') The relocation path: grow by a page with the neighbor occupied,
    // so MAYMOVE has to find a fresh page-aligned home for the same pages.
    let blocker = lin::mmap(0x5000_F000, 4096, RW, ANON_PRIVATE | lin::MAP_FIXED, -1, 0);
    lin::check_eq(
        &mut failed,
        b"the neighbor blocking the grow maps fixed",
        blocker,
        0x5000_F000,
    );
    let moved = lin::mremap(0x5000_E000, 4096, 8192, lin::MREMAP_MAYMOVE, 0);
    lin::check_true(
        &mut failed,
        b"MREMAP_MAYMOVE relocates to a fresh page-aligned address",
        moved != lin::MAP_FAILED
            && moved > 0
            && moved as usize % 4096 == 0
            && moved != 0x5000_E000
            && !(moved >= 0x5000_E000 && moved < 0x5001_0000),
    );
    let back = unsafe { (moved as *const u64).read_volatile() };
    lin::check_true(
        &mut failed,
        b"the contents travel with the mapping",
        back == P3,
    );
    // Neither the moved mapping nor the blocker matters from here on.
    let _ = lin::munmap(moved as usize, 8192);
    let _ = lin::munmap(0x5000_F000, 4096);

    // (d) A zero old_len is refused.
    lin::check_eq(
        &mut failed,
        b"a zero old_len answers EINVAL",
        lin::mremap(0x5001_2000, 0, 4096, 0, 0),
        lin::EINVAL,
    );

    // (e) So is an unaligned old address.
    lin::check_eq(
        &mut failed,
        b"an unaligned old address answers EINVAL",
        lin::mremap(0x5000_E123, 4096, 4096, 0, 0),
        lin::EINVAL,
    );

    // (f) And MREMAP_FIXED without MREMAP_MAYMOVE.
    lin::check_eq(
        &mut failed,
        b"MREMAP_FIXED without MREMAP_MAYMOVE answers EINVAL",
        lin::mremap(0x5001_3000, 4096, 4096, lin::MREMAP_FIXED, 0x5001_4000),
        lin::EINVAL,
    );

    // (g) MREMAP_FIXED|MREMAP_MAYMOVE lands on exactly the address named,
    // contents and all, on a fresh pair of addresses.
    let p = lin::mmap(0x5001_0000, 4096, RW, ANON_PRIVATE | lin::MAP_FIXED, -1, 0);
    lin::check_eq(
        &mut failed,
        b"the fixed-move source maps fixed",
        p,
        0x5001_0000,
    );
    const P5: u64 = 0x5100_5100_5100_0005;
    unsafe { (0x5001_0000 as *mut u64).write_volatile(P5) };
    lin::check_eq(
        &mut failed,
        b"MREMAP_FIXED|MREMAP_MAYMOVE answers the address named",
        lin::mremap(
            0x5001_0000,
            4096,
            4096,
            lin::MREMAP_FIXED | lin::MREMAP_MAYMOVE,
            0x5001_1000,
        ),
        0x5001_1000,
    );
    let back = unsafe { (0x5001_1000 as *const u64).read_volatile() };
    lin::check_true(
        &mut failed,
        b"the contents arrive at the fixed destination",
        back == P5,
    );

    lin::done(failed)
}

#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    lin::die(b"t-mremap: panicked\n")
}
