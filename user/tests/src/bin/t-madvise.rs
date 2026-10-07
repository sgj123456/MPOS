//! t-madvise - one test for madvise, whose man page lets every advice it
//! knows answer 0 and gives DONTNEED a contract about the *pages*: after it,
//! later accesses "will result in either repopulating the memory contents
//! from ... the underlying mapped file ... or zero-fill-on-demand pages for
//! anonymous private mappings".
//!
//! That sentence is the whole test, and it is why this file changed when
//! anonymous demand paging landed. Before it, a dropped page had nothing to
//! repopulate it and `MADV_DONTNEED` was a validated no-op; the pins below
//! are the two halves of the promise.
//!
//! * A private anonymous page that is written, released with DONTNEED and
//!   read again reads as **zero** - and `mincore` shows it went away in
//!   between. A no-op kernel fails the zero and an eager one fails the
//!   residency.
//! * A page of a *shared* mapping does not vanish with the mapping: the
//!   object behind it still holds it, so the next access finds the same
//!   bytes. That is the documented shared case ("repopulating ... from the
//!   underlying mapped file"), and for the unnamed object a
//!   `MAP_SHARED|MAP_ANONYMOUS` mapping is made of, the file's contents
//!   *are* those pages.
//!
//! The other advisories stay the successful no-ops they are, and the range
//! validations are unchanged: a range that does not begin on a page boundary
//! is not one the kernel can locate.
//!
//! # The fixed addresses moved into a reserved arena
//!
//! Same rule as `t-mmap`: the no-std version mapped at `0x5000_*` because
//! nothing else could live there under a static image; under a dynamic musl
//! the test reserves its own PROT_NONE arena and runs every claim inside it.
//!
//! The advice vocabulary probes (a retired number 5, a past-the-block
//! 0xffff) go through `libc::madvise`, clause (c): rustix's `Advice` is a
//! plain enum whose vocabulary cannot name them - the same shape batch 8's
//! fadvise words were.

use mpos_user_tests::Report;

const PAGE: usize = 4096;
const PROT_RW: libc::c_int = libc::PROT_READ | libc::PROT_WRITE;
const ANON_PRIVATE: libc::c_int = libc::MAP_PRIVATE | libc::MAP_ANONYMOUS;
/// Not zero: a page that reads back zero is what an untouched page, a
/// released page and a page that got somebody else's leftovers all look
/// like, so the value written has to be distinguishable from all three.
const PATTERN: u64 = 0x1234_5678_9abc_def0;

static mut ARENA_BASE: *mut libc::c_void = core::ptr::null_mut();
const ARENA_LEN: usize = 2 * 1024 * 1024;

fn refused(ret: isize, want: i32) -> bool {
    ret == -1 && std::io::Error::last_os_error().raw_os_error() == Some(want)
}

/// Map inside our arena at a page offset.
fn mm(off: usize, len: usize, prot: libc::c_int, flags: libc::c_int) -> isize {
    let addr = if off == usize::MAX {
        core::ptr::null_mut()
    } else {
        unsafe { ARENA_BASE.cast::<u8>().add(off) as *mut libc::c_void }
    };
    unsafe { libc::mmap(addr, len, prot, flags, -1, 0) as isize }
}

fn main() {
    println!("t-madvise");
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

    let mut vec = [0u8; 1];
    let base = arena as usize;

    let p = mm(0x1_0000, 8192, PROT_RW, ANON_PRIVATE | libc::MAP_FIXED);
    r.check(
        "the two-page range maps fixed",
        p == base as isize + 0x1_0000,
    );
    let span = p as usize;

    r.check(
        "MADV_NORMAL on the live range answers 0",
        unsafe { libc::madvise(span as *mut _, 8192, libc::MADV_NORMAL) } == 0,
    );
    r.check(
        "MADV_SEQUENTIAL on the live range answers 0",
        unsafe { libc::madvise(span as *mut _, 8192, libc::MADV_SEQUENTIAL) } == 0,
    );
    r.check(
        "MADV_DONTNEED on an untouched live range answers 0",
        unsafe { libc::madvise(span as *mut _, 8192, libc::MADV_DONTNEED) } == 0,
    );

    // The man page pins no contents for DONTNEED over a range nothing is
    // mapped in, and the answer is still success.
    r.check(
        "madvise over an unmapped aligned range still answers 0",
        unsafe { libc::madvise((base + 0x2_0000) as *mut _, PAGE, libc::MADV_DONTNEED) } == 0,
    );

    // --- the advice vocabulary is closed, not a blanket accept ---
    //
    // Linux answers `EINVAL` for an advice number it does not name; a kernel
    // that answers `0` for one is making a promise it was never asked to
    // keep (§4.2), and this is the shape that used to be here. `5` is one of
    // the retired `MADV_*` slots and `0xffff` is past the recognised block;
    // both are rejected, while a real hint is still accepted - so the gate
    // is the vocabulary, not a refusal of everything.
    // Advice 5 was MADV_REMOVE, which Linux still recognises (it refuses it
    // on an anonymous mapping with EINVAL anyway, but on a *file* mapping it
    // would answer 0); mpos dropped it from the vocabulary entirely. The
    // payload pins EINVAL for the retired slot.
    let ret5 = unsafe { libc::madvise(span as *mut _, PAGE, 5) };
    r.check(
        "a retired advice number (5) answers EINVAL, not a silent 0",
        refused(ret5 as isize, libc::EINVAL) || mpos_user_tests::host_run() && ret5 == 0,
    );
    r.check(
        "an advice past the block (0xffff) answers EINVAL",
        refused(
            unsafe { libc::madvise(span as *mut _, PAGE, 0xffff) } as isize,
            libc::EINVAL,
        ),
    );
    r.check(
        "a recognised hint (MADV_WILLNEED = 3) is still accepted",
        unsafe { libc::madvise(span as *mut _, PAGE, 3) } == 0,
    );

    // --- the private promise: a released page comes back zero ---
    let page = base + 0x3_0000;
    r.check(
        "the page to release maps fixed",
        mm(0x3_0000, PAGE, PROT_RW, ANON_PRIVATE | libc::MAP_FIXED) == page as isize,
    );
    unsafe { (page as *mut u64).write_volatile(PATTERN) };
    r.check(
        "mincore on the touched page succeeds",
        unsafe { libc::mincore(page as *mut _, PAGE, vec.as_mut_ptr()) } == 0,
    );
    r.check(
        "the touched page is resident before the advice",
        vec[0] == 1,
    );

    r.check(
        "MADV_DONTNEED on the touched page answers 0",
        unsafe { libc::madvise(page as *mut _, PAGE, libc::MADV_DONTNEED) } == 0,
    );
    vec[0] = 0;
    r.check(
        "mincore after the advice succeeds",
        unsafe { libc::mincore(page as *mut _, PAGE, vec.as_mut_ptr()) } == 0,
    );
    r.check("the released page is no longer resident", vec[0] == 0);
    let back = unsafe { (page as *const u64).read_volatile() };
    r.check(
        "the page is zero-filled again on the next access",
        back == 0,
    );

    // --- the shared promise: the object still holds the page ---
    let s = mm(
        usize::MAX,
        PAGE,
        PROT_RW,
        libc::MAP_SHARED | libc::MAP_ANONYMOUS,
    );
    r.check("a shared anonymous page maps", s > 0);
    if s > 0 {
        let sp = s as usize;
        unsafe { (sp as *mut u64).write_volatile(PATTERN) };
        r.check(
            "MADV_DONTNEED on a shared page answers 0",
            unsafe { libc::madvise(sp as *mut _, PAGE, libc::MADV_DONTNEED) } == 0,
        );
        let back = unsafe { (sp as *const u64).read_volatile() };
        r.check(
            "a shared page's contents survive the advice, as the man page says",
            back == PATTERN,
        );
    }

    let _ = unsafe { libc::munmap(arena, ARENA_LEN) };

    r.finish()
}
