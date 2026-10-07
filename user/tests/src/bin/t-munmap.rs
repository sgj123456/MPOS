//! t-munmap - one test for munmap, the strace's goodbye before exit_group:
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
//!
//! Same arena rule as `t-mmap`/`t-madvise`: the fixed three-page span lives
//! inside a PROT_NONE region this process reserved, so "the pages either
//! side keep their contents" cannot be broken by somebody else's mapping
//! landing in the hole.

use mpos_user_tests::Report;

const PAGE: usize = 4096;
const PROT_RW: libc::c_int = libc::PROT_READ | libc::PROT_WRITE;
const ANON_PRIVATE: libc::c_int = libc::MAP_PRIVATE | libc::MAP_ANONYMOUS;

static mut ARENA_BASE: *mut libc::c_void = core::ptr::null_mut();
const ARENA_LEN: usize = 2 * 1024 * 1024;

fn refused(ret: isize, want: i32) -> bool {
    ret == -1 && std::io::Error::last_os_error().raw_os_error() == Some(want)
}

fn main() {
    println!("t-munmap");
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

    let p = unsafe { libc::mmap(core::ptr::null_mut(), PAGE, PROT_RW, ANON_PRIVATE, -1, 0) };
    r.check(
        "the page to free succeeds",
        p != libc::MAP_FAILED && !p.is_null() && (p as usize).is_multiple_of(PAGE),
    );

    let first = unsafe { libc::munmap(p, PAGE) };
    r.check("unmapping a live page succeeds", first == 0);

    let again = unsafe { libc::munmap(p, PAGE) };
    r.check("unmapping the range again still succeeds", again == 0);

    // The refusal set the man page fixes: alignment and a positive length.
    let bad_align = unsafe { libc::munmap((base + 0x123) as *mut _, PAGE) };
    r.check(
        "an unaligned address answers EINVAL",
        refused(bad_align as isize, libc::EINVAL),
    );
    let zero_len = unsafe { libc::munmap((base + 0x4_0000) as *mut _, 0) };
    r.check(
        "a zero length answers EINVAL",
        refused(zero_len as isize, libc::EINVAL),
    );

    // An aligned range nothing is mapped in is a hole, not an error.
    let hole = unsafe { libc::munmap((base + 0x4_0000) as *mut _, PAGE) };
    r.check(
        "an unmapped aligned range is a hole, not an error",
        hole == 0,
    );

    // Unmapping the middle page of a three-page mapping leaves the pages
    // either side mapped and their contents intact.
    let three_at = base + 0x5_0000;
    let three = unsafe {
        libc::mmap(
            three_at as *mut _,
            12288,
            PROT_RW,
            ANON_PRIVATE | libc::MAP_FIXED,
            -1,
            0,
        )
    };
    r.check(
        "the three-page range maps fixed",
        three == three_at as *mut libc::c_void,
    );
    const P1: u64 = 0x5151_5151_5151_5151;
    const P3: u64 = 0x5353_5353_5353_5353;
    unsafe {
        (three_at as *mut u64).write_volatile(P1);
        ((three_at + 8192) as *mut u64).write_volatile(P3);
    }
    let mid = unsafe { libc::munmap((three_at + 4096) as *mut _, PAGE) };
    r.check("unmapping the middle page succeeds", mid == 0);
    let back1 = unsafe { (three_at as *const u64).read_volatile() };
    let back3 = unsafe { ((three_at + 8192) as *const u64).read_volatile() };
    r.check(
        "the pages either side keep their contents",
        back1 == P1 && back3 == P3,
    );

    let _ = unsafe { libc::munmap(arena, ARENA_LEN) };

    r.finish()
}
