//! t-brk - one test for brk, which musl's allocator leans on before it ever
//! falls back to mmap. The strace shows both shapes: `brk(NULL)` to learn the
//! break, `brk(+0x2000)` to grow it by two pages.
//!
//! The contract pinned here: the fresh break is a positive page-aligned
//! address, growing returns the requested end, the grown region takes writes
//! on both of its pages, and shrinking returns the smaller break. (Linux
//! leaves the freed tail mapped or not as it pleases; only the return value
//! is pinned.)
//!
//! Added pins: re-growing after the shrink answers the request, a byte
//! written at the old break still reads back, and brk asked for the same
//! address twice answers it both times.
//!
//! # The heap moved under this test, and that changed the fixture
//!
//! The no-std version grew *the program break* from vaddr-zero. Under musl
//! the break is Rust's own heap, and writing through addresses the allocator
//! also owns would corrupt it - so the migrated version first **moves the
//! break out of the way**: `sbrk(0)` records where the heap ended, and every
//! brk claim runs against an address above it, in the gap between the heap and
//! the mmap region (see the fixture in `main`: the first draft asked for a
//! break above a mapping made at the top of the mmap region, which no kernel
//! can grant - a heap may not walk into the mmap region). The claims
//! themselves are unchanged: alignment, the returned end, writes taken, the
//! shrink answer.
//!
//! The raw call is clause (c) in its plainest form: `libc::brk` does not
//! exist on linux-musl (measured), only `sbrk`, whose increment shape cannot
//! ask for an absolute break - and the absolute address *is* what four of
//! the seven claims read. Numbers from libc's table (`SYS_brk`: 12 / 214),
//! which agree with this kernel's own tables at the same values.

use mpos_user_tests::Report;

fn brk(addr: usize) -> isize {
    unsafe { libc::syscall(libc::SYS_brk, addr) as isize }
}

fn main() {
    println!("t-brk");
    let mut r = Report::new();

    // Move the break clear of the heap: the request goes to the next 64 KiB
    // boundary above whatever `sbrk(0)` reports, which is the *gap* between
    // the program's own heap (low, where the loader put the image) and the
    // mmap region (whose base is the highest address this kernel hands out).
    //
    // The gap is the only place the request can live. A break that reached the
    // mmap region is refused - Linux refuses it too, because a heap that walked
    // into it would collide with every mapping the program makes - so asking
    // for one *above* a fixture mapped at the top of the mmap region (which is
    // where a fresh `mmap` lands, the region growing down from its base) is a
    // request no kernel could grant. That is what this fixture used to do, and
    // the refusal then read as a broken `brk` (`NOTES.md` §15.18).
    //
    // mpos moves the break to the request; glibc's brk refuses a jump farther
    // than its own heap can grow and answers the *current* break instead
    // (measured here), so on a hosted run the fallback answer is accepted and
    // named. The payload sweep pins that the request is granted.
    let heap_end = unsafe { libc::sbrk(0) } as usize;
    let start = (heap_end + 0x1_0000 + 0xFFFF) & !0xFFFF;
    let cur = brk(start);
    r.check(
        "brk reports a break at or past the request",
        cur >= start as isize || mpos_user_tests::host_run() && cur > 0,
    );
    r.check(
        "the break is page-aligned",
        cur > 0 && (cur as usize).is_multiple_of(4096),
    );

    // From here every claim is stated against the break the kernel
    // actually gave, which is what Linux's own contract says a caller must
    // re-read rather than assume.
    let want = cur + 8192;
    let end = brk(want as usize);
    r.check(
        "growing by two pages returns the requested break",
        end == want,
    );

    const A: u64 = 0x1111_1111_1111_1111;
    const B: u64 = 0x2222_2222_2222_2222;
    unsafe {
        (cur as *mut u64).write_volatile(A);
        ((cur as usize + 4096) as *mut u64).write_volatile(B);
    }
    let back_a = unsafe { (cur as *const u64).read_volatile() };
    let back_b = unsafe { ((cur as usize + 4096) as *const u64).read_volatile() };
    r.check(
        "the grown heap takes writes on both pages",
        back_a == A && back_b == B,
    );

    let end = brk((cur + 4096) as usize);
    r.check("shrinking returns the smaller break", end == cur + 4096);

    // Re-growing after the shrink answers the request: the pages are this
    // process's from here on, whatever happened to them in between.
    let end = brk(want as usize);
    r.check(
        "re-growing after the shrink returns the request",
        end == want,
    );

    // A byte at the old break - the address the first call reported -
    // writes and reads like any other.
    const BYTE: u8 = 0x5A;
    unsafe { (cur as *mut u8).write_volatile(BYTE) };
    let back = unsafe { (cur as *const u8).read_volatile() };
    r.check(
        "a byte at the old break still writes and reads",
        back == BYTE,
    );

    // Asking for the same break twice answers it both times.
    let same1 = brk(want as usize);
    let same2 = brk(want as usize);
    r.check(
        "brk at the same address answers it, twice",
        same1 >= want && same2 >= want,
    );

    r.finish()
}
