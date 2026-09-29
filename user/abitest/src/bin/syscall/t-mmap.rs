//! t-mmap - one test for mmap, of which a musl strace shows three
//! anonymous mappings on the way up:
//!
//! - a 12288-byte RW region with MAP_STACK, the alt stack Rust std reserves
//! - a 4096-byte PROT_NONE page at a MAP_FIXED address, its guard
//! - a plain 4096-byte RW page, malloc's overflow past brk
//!
//! The test asks for the same three shapes and pins the contract they
//! carry: kernel-picked addresses come back page-aligned and non-overlap-
//! ping, MAP_FIXED comes back exactly where it was asked, anonymous memory
//! arrives zeroed, and the writable regions take writes. Whether PROT_NONE
//! actually faults on access needs fault delivery and is deliberately not
//! probed.
//!
//! The refusals the man page fixes are pinned too - a zero length, a
//! file-backed request over a bad fd, and a MAP_FIXED that is unaligned or
//! past the user limit - along with the full shape of the address argument: a
//! free hint is honoured exactly, a hint already in use is ignored, and
//! MAP_FIXED takes its range and arrives freshly zeroed. On this raw syscall
//! surface an errno comes back as the negated return, Linux's own convention;
//! MAP_FAILED is what a libc wrapper builds from it.

#![no_std]
#![no_main]

#[path = "../../../lib/lin.rs"]
mod lin;

use core::panic::PanicInfo;

/// A fixed address far from the image base (0x400000), the loader's brk
/// region, and the kernel's mmap bump (0x80000000).
const GUARD: usize = 0x5000_0000;
const PATTERN: u64 = 0x1234_5678_9abc_def0;
/// `MAP_SHARED`, which lin.rs does not name but the refusal test needs.
const MAP_SHARED: usize = 0x01;

#[unsafe(no_mangle)]
extern "C" fn mpos_user_main(_argc: usize, _argv: *const *const u8) -> ! {
    lin::puts(b"t-mmap\n");
    let mut failed = false;

    // The MAP_STACK region the alt stack would live in.
    let stack = lin::mmap(
        0,
        12288,
        lin::PROT_READ | lin::PROT_WRITE,
        lin::MAP_PRIVATE | lin::MAP_ANONYMOUS | lin::MAP_STACK,
        -1,
        0,
    );
    lin::check_true(
        &mut failed,
        b"the 12288-byte MAP_STACK mapping succeeds",
        stack != lin::MAP_FAILED && stack > 0,
    );
    lin::check_true(
        &mut failed,
        b"the kernel picks a page-aligned address",
        stack > 0 && stack as usize % 4096 == 0,
    );

    let first = unsafe { (stack as *const u64).read_volatile() };
    let third = unsafe { ((stack as usize + 8192) as *const u64).read_volatile() };
    lin::check_true(
        &mut failed,
        b"anonymous memory arrives zeroed",
        first == 0 && third == 0,
    );

    unsafe { (stack as *mut u64).write_volatile(PATTERN) };
    let back = unsafe { (stack as *const u64).read_volatile() };
    lin::check_true(&mut failed, b"the region takes writes", back == PATTERN);

    // The guard page, at the fixed address.
    let guard = lin::mmap(
        GUARD,
        4096,
        lin::PROT_NONE,
        lin::MAP_PRIVATE | lin::MAP_ANONYMOUS | lin::MAP_FIXED,
        -1,
        0,
    );
    lin::check_eq(
        &mut failed,
        b"MAP_FIXED maps exactly the address asked",
        guard,
        GUARD as isize,
    );

    // The plain page: fresh address, no overlap with the first mapping.
    let page = lin::mmap(
        0,
        4096,
        lin::PROT_READ | lin::PROT_WRITE,
        lin::MAP_PRIVATE | lin::MAP_ANONYMOUS,
        -1,
        0,
    );
    lin::check_true(
        &mut failed,
        b"a plain 4096-byte mapping succeeds",
        page != lin::MAP_FAILED && page > 0 && page as usize % 4096 == 0,
    );
    lin::check_true(
        &mut failed,
        b"it does not overlap the earlier mapping",
        !(page >= stack && page < stack + 12288),
    );

    // The refusals the man page fixes. The errno arrives as the negated
    // return, Linux's raw-syscall convention.
    let r = lin::mmap(
        0,
        0,
        lin::PROT_READ | lin::PROT_WRITE,
        lin::MAP_PRIVATE | lin::MAP_ANONYMOUS,
        -1,
        0,
    );
    lin::check_eq(&mut failed, b"a zero length answers EINVAL", r, lin::EINVAL);

    let r = lin::mmap(
        0,
        4096,
        lin::PROT_READ | lin::PROT_WRITE,
        lin::MAP_PRIVATE,
        -1,
        0,
    );
    // The man page's ENODEV is what a *valid* descriptor on an unmappable
    // filesystem earns. An fd that names nothing - -1 here - is checked
    // first: real Linux answers EBADF, and this kernel follows that order.
    lin::check_eq(
        &mut failed,
        b"a file-backed request over a bad fd answers EBADF",
        r,
        lin::EBADF,
    );

    let r = lin::mmap(
        0,
        4096,
        lin::PROT_READ | lin::PROT_WRITE,
        MAP_SHARED | lin::MAP_ANONYMOUS,
        -1,
        0,
    );
    // Phase 5.1: a shared anonymous mapping is served as an *object* - an
    // unnamed memfd the mapping aliases - so it is a usable region and two
    // processes across a `fork` watch each other's writes through it. It used
    // to answer ENOSYS, and then to be a private range that merely happened to
    // be shared between threads.
    let mapped = (r as isize) > 0;
    lin::check_true(
        &mut failed,
        b"MAP_SHARED|MAP_ANONYMOUS is served as a mapping",
        mapped,
    );
    if mapped {
        let p = r as *mut u8;
        unsafe {
            p.write(0xa5);
            lin::check_true(
                &mut failed,
                b"the shared anonymous region takes and reads back a write",
                p.read() == 0xa5,
            );
        }
    }

    let r = lin::mmap(
        0x5000_0123,
        4096,
        lin::PROT_READ | lin::PROT_WRITE,
        lin::MAP_PRIVATE | lin::MAP_ANONYMOUS | lin::MAP_FIXED,
        -1,
        0,
    );
    lin::check_eq(
        &mut failed,
        b"MAP_FIXED to an unaligned address answers EINVAL",
        r,
        lin::EINVAL,
    );

    let r = lin::mmap(
        0x8000_0000_0000_0000,
        4096,
        lin::PROT_READ | lin::PROT_WRITE,
        lin::MAP_PRIVATE | lin::MAP_ANONYMOUS | lin::MAP_FIXED,
        -1,
        0,
    );
    lin::check_eq(
        &mut failed,
        b"MAP_FIXED past the user limit answers EINVAL",
        r,
        lin::EINVAL,
    );

    // A hint with nothing mapped in the way is honoured exactly.
    let hinted = lin::mmap(
        0x5000_4000,
        4096,
        lin::PROT_READ | lin::PROT_WRITE,
        lin::MAP_PRIVATE | lin::MAP_ANONYMOUS,
        -1,
        0,
    );
    lin::check_eq(
        &mut failed,
        b"a free hint is honoured exactly",
        hinted,
        0x5000_4000,
    );

    // A hint that is already in use is ignored: the mapping goes wherever
    // there is room, which cannot be the hinted page.
    let owner = lin::mmap(
        0x5000_6000,
        4096,
        lin::PROT_READ | lin::PROT_WRITE,
        lin::MAP_PRIVATE | lin::MAP_ANONYMOUS | lin::MAP_FIXED,
        -1,
        0,
    );
    lin::check_eq(
        &mut failed,
        b"the busy hint's owner maps fixed",
        owner,
        0x5000_6000,
    );
    let elsewhere = lin::mmap(
        0x5000_6000,
        4096,
        lin::PROT_READ | lin::PROT_WRITE,
        lin::MAP_PRIVATE | lin::MAP_ANONYMOUS,
        -1,
        0,
    );
    lin::check_true(
        &mut failed,
        b"a busy hint is ignored for a page-aligned map elsewhere",
        elsewhere != lin::MAP_FAILED
            && elsewhere > 0
            && elsewhere as usize % 4096 == 0
            && !(elsewhere >= 0x5000_6000 && elsewhere < 0x5000_7000),
    );

    // MAP_FIXED takes the range: whatever was mapped there is dropped and
    // fresh zeroed pages go down in its place.
    let taken = lin::mmap(
        0x5000_8000,
        4096,
        lin::PROT_READ | lin::PROT_WRITE,
        lin::MAP_PRIVATE | lin::MAP_ANONYMOUS | lin::MAP_FIXED,
        -1,
        0,
    );
    lin::check_eq(
        &mut failed,
        b"the range MAP_FIXED will take maps fixed",
        taken,
        0x5000_8000,
    );
    unsafe { (0x5000_8000 as *mut u64).write_volatile(PATTERN) };
    let again = lin::mmap(
        0x5000_8000,
        4096,
        lin::PROT_READ | lin::PROT_WRITE,
        lin::MAP_PRIVATE | lin::MAP_ANONYMOUS | lin::MAP_FIXED,
        -1,
        0,
    );
    lin::check_eq(
        &mut failed,
        b"MAP_FIXED over a live mapping still answers the address",
        again,
        0x5000_8000,
    );
    let back = unsafe { (0x5000_8000 as *const u64).read_volatile() };
    lin::check_true(
        &mut failed,
        b"the taken range arrives freshly zeroed",
        back == 0,
    );

    // `MAP_FIXED_NOREPLACE` (Linux 4.17): the address is mandatory but an
    // occupied range is refused with `EEXIST`, not relocated and not replaced -
    // what separates it from a plain hint (ignored when in use) and from
    // `MAP_FIXED` (takes the range). And `MAP_HUGETLB` with no huge-page pool is
    // the honest `ENOMEM`, never a silent 4 KiB page that would let the caller
    // believe it had huge pages.
    {
        const MAP_FIXED_NOREPLACE: usize = 0x10_0000;
        const MAP_HUGETLB: usize = 0x4_0000;
        let nr_at = GUARD + 0x20_0000;
        let nr = lin::mmap(
            nr_at,
            4096,
            lin::PROT_READ | lin::PROT_WRITE,
            lin::MAP_PRIVATE | lin::MAP_ANONYMOUS | MAP_FIXED_NOREPLACE,
            -1,
            0,
        );
        lin::check_eq(
            &mut failed,
            b"MAP_FIXED_NOREPLACE on a free range answers the exact address",
            nr,
            nr_at as isize,
        );
        let nr2 = lin::mmap(
            nr_at,
            4096,
            lin::PROT_READ | lin::PROT_WRITE,
            lin::MAP_PRIVATE | lin::MAP_ANONYMOUS | MAP_FIXED_NOREPLACE,
            -1,
            0,
        );
        lin::check_eq(
            &mut failed,
            b"MAP_FIXED_NOREPLACE over a live range is EEXIST, not relocated",
            nr2,
            lin::EEXIST,
        );
        let conflict = lin::mmap(
            0,
            4096,
            lin::PROT_READ | lin::PROT_WRITE,
            lin::MAP_PRIVATE | lin::MAP_ANONYMOUS | lin::MAP_FIXED | MAP_FIXED_NOREPLACE,
            -1,
            0,
        );
        lin::check_eq(
            &mut failed,
            b"MAP_FIXED together with MAP_FIXED_NOREPLACE is EINVAL",
            conflict,
            lin::EINVAL,
        );
        let huge = lin::mmap(
            0,
            4096,
            lin::PROT_READ | lin::PROT_WRITE,
            lin::MAP_PRIVATE | lin::MAP_ANONYMOUS | MAP_HUGETLB,
            -1,
            0,
        );
        lin::check_eq(
            &mut failed,
            b"MAP_HUGETLB is refused with ENOMEM, not a silent 4 KiB page",
            huge,
            lin::ENOMEM,
        );
    }

    lin::done(failed)
}

#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    lin::die(b"t-mmap: panicked\n")
}
