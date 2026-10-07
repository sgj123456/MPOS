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
//!
//! # The fixed addresses moved into a reserved arena
//!
//! Same rule as `t-mmap`: every source, blocker and destination address now
//! lives inside a 2 MiB PROT_NONE span this process reserves first, so no
//! other mapping can occupy the neighbourhood the claims depend on. The
//! calls ride `libc::mremap` (rustix binds it nowhere - clause (a)); the
//! five-word shape including the raw flag word is what the ABI promises.

use mpos_user_tests::Report;

const PAGE: usize = 4096;
const PROT_RW: libc::c_int = libc::PROT_READ | libc::PROT_WRITE;
const ANON_PRIVATE: libc::c_int = libc::MAP_PRIVATE | libc::MAP_ANONYMOUS;

static mut ARENA_BASE: *mut libc::c_void = core::ptr::null_mut();
const ARENA_LEN: usize = 2 * 1024 * 1024;

fn refused(ret: isize, want: i32) -> bool {
    ret == -1 && std::io::Error::last_os_error().raw_os_error() == Some(want)
}

/// Map our own two-page range at an arena offset, fixed.
fn map_at(off: usize, len: usize) -> usize {
    let addr = unsafe { ARENA_BASE.cast::<u8>().add(off) as *mut libc::c_void };
    let p = unsafe { libc::mmap(addr, len, PROT_RW, ANON_PRIVATE | libc::MAP_FIXED, -1, 0) };
    if p == libc::MAP_FAILED {
        return 0;
    }
    p as usize
}

fn main() {
    println!("t-mremap");
    let mut r = Report::new();

    let arena = unsafe {
        libc::mmap(
            core::ptr::null_mut(),
            ARENA_LEN,
            libc::PROT_NONE,
            ANON_PRIVATE,
            -1,
            0,
        )
    };
    if arena as isize == -1 || arena == libc::MAP_FAILED {
        r.check("the fixture arena maps", false);
        r.finish();
    }
    unsafe { ARENA_BASE = arena };
    r.check("the fixture arena maps", true);
    let base = arena as usize;

    // (a) Shrink two pages to one: the tail is dropped, the head keeps its
    // contents.
    let p = map_at(0x1_0000, 8192);
    r.check("the two-page range maps fixed", p == base + 0x1_0000);
    const P1: u64 = 0x5c00_5c00_5c00_0001;
    unsafe { (p as *mut u64).write_volatile(P1) };
    let shrunk = unsafe { libc::mremap(p as *mut _, 8192, PAGE, 0, 0) };
    r.check("shrinking answers the old address", shrunk as usize == p);
    let back = unsafe { (p as *const u64).read_volatile() };
    r.check("the head keeps its contents across the shrink", back == P1);

    // (b) Grow one page to two where the pages above are free: the mapping
    // stays put and its contents survive.
    let p = map_at(0x2_0000, PAGE);
    r.check("the page to grow maps fixed", p == base + 0x2_0000);
    const P2: u64 = 0x5d00_5d00_5d00_0002;
    unsafe { (p as *mut u64).write_volatile(P2) };
    // Linux grows only into pages that are already *mapped* above the range
    // (measured here: without MAYMOVE a grow over untouched space is
    // ENOMEM); mpos keeps the mapping in place when the neighbourhood is
    // free. The payload pins the in-place answer.
    let grew = unsafe { libc::mremap(p as *mut _, PAGE, 8192, 0, 0) };
    r.check(
        "growing in place answers the old address",
        grew as usize == p
            || std::env::var_os("MPOS_ON_HOST").is_some() && refused(grew as isize, libc::ENOMEM),
    );
    let back = unsafe { (p as *const u64).read_volatile() };
    r.check("the contents survive the grow in place", back == P2);

    // (c) A same-size mremap is a no-op: the old address comes back with the
    // contents intact, whatever MAYMOVE says - Linux answers old_len >=
    // new_len from the shrink path before any move can be considered.
    let p = map_at(0x3_0000, PAGE);
    r.check("the page to move maps fixed", p == base + 0x3_0000);
    const P3: u64 = 0x5e00_5e00_5e00_0003;
    unsafe { (p as *mut u64).write_volatile(P3) };
    let same = unsafe { libc::mremap(p as *mut _, PAGE, PAGE, libc::MREMAP_MAYMOVE, 0) };
    r.check(
        "a same-size MREMAP_MAYMOVE answers the old address",
        same as usize == p,
    );
    let back = unsafe { (p as *const u64).read_volatile() };
    r.check("the contents survive the same-size call", back == P3);

    // (c') The relocation path: grow by a page with the neighbor occupied,
    // so MAYMOVE has to find a fresh page-aligned home for the same pages.
    let blocker = map_at(0x4_0000, PAGE);
    r.check(
        "the neighbor blocking the grow maps fixed",
        blocker == base + 0x4_0000,
    );
    let moved = unsafe { libc::mremap(p as *mut _, PAGE, 8192, libc::MREMAP_MAYMOVE, 0) };
    r.check(
        "MREMAP_MAYMOVE relocates to a fresh page-aligned address",
        moved as isize != -1
            && moved != libc::MAP_FAILED
            && (moved as usize).is_multiple_of(PAGE)
            && moved as usize != p
            && !(moved as usize >= p && (moved as usize) < p + 8192),
    );
    let back = unsafe { (moved as *const u64).read_volatile() };
    r.check("the contents travel with the mapping", back == P3);
    // Neither the moved mapping nor the blocker matters from here on.
    let _ = unsafe { libc::munmap(moved, 8192) };
    let _ = unsafe { libc::munmap(blocker as *mut _, PAGE) };

    // (d) A zero old_len is refused.
    let d = unsafe { libc::mremap((base + 0x5_0000) as *mut _, 0, PAGE, 0, 0) };
    r.check(
        "a zero old_len answers EINVAL",
        refused(d as isize, libc::EINVAL),
    );

    // (e) So is an unaligned old address.
    let e = unsafe { libc::mremap((base + 0x3_123) as *mut _, PAGE, PAGE, 0, 0) };
    r.check(
        "an unaligned old address answers EINVAL",
        refused(e as isize, libc::EINVAL),
    );

    // (f) And MREMAP_FIXED without MREMAP_MAYMOVE.
    let f = unsafe {
        libc::mremap(
            (base + 0x6_0000) as *mut _,
            PAGE,
            PAGE,
            libc::MREMAP_FIXED,
            (base + 0x7_0000) as *mut libc::c_void,
        )
    };
    r.check(
        "MREMAP_FIXED without MREMAP_MAYMOVE answers EINVAL",
        refused(f as isize, libc::EINVAL),
    );

    // (g) MREMAP_FIXED|MREMAP_MAYMOVE lands on exactly the address named,
    // contents and all, on a fresh pair of addresses.
    let src = map_at(0x8_0000, PAGE);
    r.check("the fixed-move source maps fixed", src == base + 0x8_0000);
    const P5: u64 = 0x5100_5100_5100_0005;
    unsafe { (src as *mut u64).write_volatile(P5) };
    let dst_off = 0x9_0000;
    let dst = unsafe { ARENA_BASE.cast::<u8>().add(dst_off) } as *mut libc::c_void;
    let g = unsafe {
        libc::mremap(
            src as *mut _,
            PAGE,
            PAGE,
            libc::MREMAP_FIXED | libc::MREMAP_MAYMOVE,
            dst,
        )
    };
    r.check(
        "MREMAP_FIXED|MREMAP_MAYMOVE answers the address named",
        g == dst,
    );
    let back = unsafe { ((base + dst_off) as *const u64).read_volatile() };
    r.check("the contents arrive at the fixed destination", back == P5);

    let _ = unsafe { libc::munmap(arena, ARENA_LEN) };

    r.finish()
}
