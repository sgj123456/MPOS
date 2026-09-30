//! t-madvise - one test for madvise, whose man page lets every advice it
//! knows answer 0 and gives DONTNEED a contract about the *pages*: after it,
//! later accesses "will result in either repopulating the memory contents from
//! ... the underlying mapped file ... or zero-fill-on-demand pages for
//! anonymous private mappings".
//!
//! That sentence is the whole test, and it is why this file changed when
//! anonymous demand paging landed. Before it, a dropped page had nothing to
//! repopulate it and `MADV_DONTNEED` was a validated no-op; the pins below
//! are the two halves of the promise.
//!
//! * A private anonymous page that is written, released with DONTNEED and read
//!   again reads as **zero** - and `mincore` shows it went away in between.
//!   A no-op kernel fails the zero and an eager one fails the residency.
//! * A page of a *shared* mapping does not vanish with the mapping: the object
//!   behind it still holds it, so the next access finds the same bytes. That is
//!   the documented shared case ("repopulating ... from the underlying mapped
//!   file"), and for the unnamed object a `MAP_SHARED|MAP_ANONYMOUS` mapping is
//!   made of, the file's contents *are* those pages.
//!
//! The other advisories stay the successful no-ops they are, and the range
//! validations are unchanged: a range that does not begin on a page boundary is
//! not one the kernel can locate.

#![no_std]
#![no_main]

#[path = "../../../lib/lin.rs"]
mod lin;

use core::panic::PanicInfo;

const PAGE: usize = 4096;
const RW: usize = lin::PROT_READ | lin::PROT_WRITE;
const ANON_PRIVATE: usize = lin::MAP_PRIVATE | lin::MAP_ANONYMOUS;
/// Not zero: a page that reads back zero is what an untouched page, a
/// released page and a page that got somebody else's leftovers all look like,
/// so the value written has to be distinguishable from all three.
const PATTERN: u64 = 0x1234_5678_9abc_def0;

static mut VEC: [u8; 1] = [0; 1];

#[unsafe(no_mangle)]
extern "C" fn mpos_user_main(_argc: usize, _argv: *const *const u8) -> ! {
    lin::puts(b"t-madvise\n");
    let mut failed = false;
    let vec = unsafe { &raw mut VEC as *mut u8 };

    let p = lin::mmap(0x5000_2000, 8192, RW, ANON_PRIVATE | lin::MAP_FIXED, -1, 0);
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
        b"MADV_DONTNEED on an untouched live range answers 0",
        lin::madvise(0x5000_2000, 8192, lin::MADV_DONTNEED),
        0,
    );

    // The man page pins no contents for DONTNEED over a range nothing is
    // mapped in, and the answer is still success.
    lin::check_eq(
        &mut failed,
        b"madvise over an unmapped aligned range still answers 0",
        lin::madvise(0x5000_4000, PAGE, lin::MADV_DONTNEED),
        0,
    );

    // --- the advice vocabulary is closed, not a blanket accept ---
    //
    // Linux answers `EINVAL` for an advice number it does not name; a kernel
    // that answers `0` for one is making a promise it was never asked to keep
    // (§4.2), and this is the shape that used to be here. `5` is one of the
    // retired `MADV_*` slots and `0xffff` is past the recognised block; both
    // are rejected, while a real hint is still accepted - so the gate is the
    // vocabulary, not a refusal of everything.
    lin::check_eq(
        &mut failed,
        b"a retired advice number (5) answers EINVAL, not a silent 0",
        lin::madvise(0x5000_2000, PAGE, 5),
        lin::EINVAL,
    );
    lin::check_eq(
        &mut failed,
        b"an advice past the block (0xffff) answers EINVAL",
        lin::madvise(0x5000_2000, PAGE, 0xffff),
        lin::EINVAL,
    );
    lin::check_eq(
        &mut failed,
        b"a recognised hint (MADV_WILLNEED = 3) is still accepted",
        lin::madvise(0x5000_2000, PAGE, 3),
        0,
    );

    // --- the private promise: a released page comes back zero ---
    let page = 0x5000_6000;
    lin::check_eq(
        &mut failed,
        b"the page to release maps fixed",
        lin::mmap(page, PAGE, RW, ANON_PRIVATE | lin::MAP_FIXED, -1, 0),
        page as isize,
    );
    unsafe { (page as *mut u64).write_volatile(PATTERN) };
    lin::check_eq(
        &mut failed,
        b"mincore on the touched page succeeds",
        lin::mincore(page, PAGE, vec),
        0,
    );
    lin::check_true(
        &mut failed,
        b"the touched page is resident before the advice",
        unsafe { VEC[0] == 1 },
    );

    lin::check_eq(
        &mut failed,
        b"MADV_DONTNEED on the touched page answers 0",
        lin::madvise(page, PAGE, lin::MADV_DONTNEED),
        0,
    );
    lin::check_eq(
        &mut failed,
        b"mincore after the advice succeeds",
        lin::mincore(page, PAGE, vec),
        0,
    );
    lin::check_true(
        &mut failed,
        b"the released page is no longer resident",
        unsafe { VEC[0] == 0 },
    );
    let back = unsafe { (page as *const u64).read_volatile() };
    lin::check_true(
        &mut failed,
        b"the page is zero-filled again on the next access",
        back == 0,
    );

    // --- the shared promise: the object still holds the page ---
    let s = lin::mmap(0, PAGE, RW, lin::MAP_SHARED | lin::MAP_ANONYMOUS, -1, 0);
    lin::check_true(&mut failed, b"a shared anonymous page maps", s > 0);
    if s > 0 {
        let s = s as usize;
        unsafe { (s as *mut u64).write_volatile(PATTERN) };
        lin::check_eq(
            &mut failed,
            b"MADV_DONTNEED on a shared page answers 0",
            lin::madvise(s, PAGE, lin::MADV_DONTNEED),
            0,
        );
        let back = unsafe { (s as *const u64).read_volatile() };
        lin::check_true(
            &mut failed,
            b"a shared page's contents survive the advice, as the man page says",
            back == PATTERN,
        );
    }

    lin::done(failed)
}

#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    lin::die(b"t-madvise: panicked\n")
}
