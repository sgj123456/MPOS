//! t_mmap - one test for mmap, of which a musl strace shows three
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
//! The refusals the man page fixes are pinned too - a zero length, a file-backed
//! request, a shared anonymous mapping, and a MAP_FIXED that is unaligned or
//! past the user limit - along with the full shape of the address argument: a
//! free hint is honoured exactly, a hint already in use is ignored, and
//! MAP_FIXED takes its range and arrives freshly zeroed. On this raw syscall
//! surface an errno comes back as the negated return, Linux's own convention;
//! MAP_FAILED is what a libc wrapper builds from it.

#![no_std]
#![no_main]

#[path = "../lib/lin.rs"]
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
    lin::puts(b"t_mmap\n");
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
    lin::check_true(
        &mut failed,
        b"the region takes writes",
        back == PATTERN,
    );

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
    lin::check_eq(
        &mut failed,
        b"a file-backed request without MAP_ANONYMOUS answers ENODEV",
        r,
        lin::ENODEV,
    );

    let r = lin::mmap(
        0,
        4096,
        lin::PROT_READ | lin::PROT_WRITE,
        MAP_SHARED | lin::MAP_ANONYMOUS,
        -1,
        0,
    );
    lin::check_eq(
        &mut failed,
        b"MAP_SHARED|MAP_ANONYMOUS answers ENOSYS",
        r,
        lin::ENOSYS,
    );

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

    lin::done(failed)
}

#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    lin::die(b"t_mmap: panicked\n")
}
