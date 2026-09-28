//! t-mmap-demand - a mapping is an address space, and a page appears when it
//! is first touched.
//!
//! The property that separates demand paging from a kernel that fills a
//! mapping in at `mmap` time is not "does the call succeed" but "is anything
//! behind the addresses". `mincore` is the instrument: it answers from the page
//! tables, so a range that is mapped and whose pages have never been touched
//! reads as *not* resident. Where the eager kernel would have said every page
//! of a 64 MiB `mmap` was resident - because every page had a frame from the
//! moment the call returned - this one says none of them are.
//!
//! Three separate things are pinned, because they are three separate code
//! paths:
//!
//! * mapping a large region succeeds and backs nothing (the fault handler is
//!   what fills it, and it has not run);
//! * reading an untouched page gets zeros and *then* the page is resident (a
//!   ring-3 read fault populated it);
//! * a syscall that fills a caller's buffer works on a page that is not there
//!   yet (the kernel-copy half: `buffer_ok` must accept it, and the store that
//!   follows must be able to reach it);
//! * `MADV_DONTNEED` gives the pages back and the next touch zero-fills them,
//!   which is only a promise a kernel with demand paging can make.
//!
//! The last one is the interesting one. `getrandom` validates the buffer with
//! `buffer_ok` and then fills it through the caller's own page tables, in ring
//! 0 - so the page has to be resolvable from *both* directions, and a kernel
//! that only taught its page-fault handler about lazily-mapped pages would
//! answer `EFAULT` here.

#![no_std]
#![no_main]

#[path = "../../../lib/lin.rs"]
mod lin;

use core::panic::PanicInfo;

const PAGE: usize = 4096;
/// The region the plan names: large enough that filling it eagerly would be
/// 16384 frames of a 2 GiB guest, small enough to be an unremarkable `mmap`.
const ARENA_BYTES: usize = 64 * 1024 * 1024;
const ARENA_PAGES: usize = ARENA_BYTES / PAGE;
/// One further page, out at the end, for the untouched-read check.
const FAR_OFFSET: usize = ARENA_BYTES - PAGE;
/// A page in the middle, never touched, for the syscall-fill check.
const MID_OFFSET: usize = ARENA_BYTES / 2;
/// A second mapping, larger than the guest's memory. If the pages were real at
/// `mmap` time this would not come back at all; demand paging makes it a
/// statement about addresses, which is the whole of what §8.0 leans on.
const BIG_BYTES: usize = 1024 * 1024 * 1024;
const BIG_PAGES: usize = BIG_BYTES / PAGE;
/// Room for one residency byte per page of the larger mapping.
const VEC_BYTES: usize = BIG_PAGES;

/// A pattern no zero-filled page could hold, so "this page was written" and
/// "this page is fresh" cannot be confused.
const FILL: u8 = 0x5a;

#[unsafe(no_mangle)]
extern "C" fn mpos_user_main(_argc: usize, _argv: *const *const u8) -> ! {
    lin::puts(b"t-mmap-demand\n");
    let mut failed = false;

    // The residency buffer, itself a demand-paged mapping: `mincore` writes
    // into it, and the first call has to populate it through the same path the
    // test is about. That is not a complication, it is a second sample.
    let vec = lin::mmap(
        0,
        VEC_BYTES,
        lin::PROT_READ | lin::PROT_WRITE,
        lin::MAP_PRIVATE | lin::MAP_ANONYMOUS,
        -1,
        0,
    );
    lin::check_true(&mut failed, b"the residency buffer maps", vec > 0);
    if vec <= 0 {
        lin::done(failed);
    }
    let vec = vec as *mut u8;

    let arena = lin::mmap(
        0,
        ARENA_BYTES,
        lin::PROT_READ | lin::PROT_WRITE,
        lin::MAP_PRIVATE | lin::MAP_ANONYMOUS,
        -1,
        0,
    );
    lin::check_true(
        &mut failed,
        b"a 64 MiB anonymous mapping succeeds",
        arena > 0 && arena as usize % PAGE == 0,
    );
    if arena <= 0 {
        lin::done(failed);
    }
    let arena = arena as usize;

    // --- nothing is behind the mapping ---
    lin::check_eq(
        &mut failed,
        b"mincore over the fresh mapping answers 0",
        lin::mincore(arena, ARENA_BYTES, vec),
        0,
    );
    let mut resident = 0usize;
    for i in 0..ARENA_PAGES {
        if unsafe { vec.add(i).read() } != 0 {
            resident += 1;
        }
    }
    lin::check_eq(
        &mut failed,
        b"not one of the mapping's pages is resident yet",
        resident as isize,
        0,
    );

    // --- touching one page backs one page ---
    unsafe { (arena as *mut u8).write_volatile(FILL) };
    lin::check_eq(
        &mut failed,
        b"mincore after the first write answers 0",
        lin::mincore(arena, ARENA_BYTES, vec),
        0,
    );
    let first = unsafe { vec.read() };
    let second = unsafe { vec.add(1).read() };
    let last = unsafe { vec.add(ARENA_PAGES - 1).read() };
    lin::check_true(
        &mut failed,
        b"the written page is resident and its neighbours are not",
        first != 0 && second == 0 && last == 0,
    );

    // --- an untouched page reads as zeros, and the read backs it ---
    let far = unsafe { ((arena + FAR_OFFSET) as *const u8).read_volatile() };
    lin::check_eq(
        &mut failed,
        b"an untouched page reads as zero",
        far as isize,
        0,
    );
    unsafe { vec.add(ARENA_PAGES - 1).write(0) };
    lin::check_eq(
        &mut failed,
        b"mincore after the read answers 0",
        lin::mincore(arena, ARENA_BYTES, vec),
        0,
    );
    let far_resident = unsafe { vec.add(ARENA_PAGES - 1).read() };
    lin::check_true(
        &mut failed,
        b"the read faulted the far page in",
        far_resident != 0,
    );

    // --- a syscall that fills a caller's buffer ---
    //
    // The page has been touched by nothing in ring 3. `getrandom` validates the
    // range and then writes it from ring 0, so this only works if the kernel
    // can bring a page in for its own copy as well as for the CPU's.
    let mid = arena + MID_OFFSET;
    lin::check_eq(
        &mut failed,
        b"getrandom fills an untouched page",
        lin::getrandom(
            unsafe { core::slice::from_raw_parts_mut(mid as *mut u8, 8) },
            0,
        ),
        8,
    );
    unsafe { vec.add(MID_OFFSET / PAGE).write(0) };
    lin::check_eq(
        &mut failed,
        b"mincore after the syscall answers 0",
        lin::mincore(arena, ARENA_BYTES, vec),
        0,
    );
    let mid_resident = unsafe { vec.add(MID_OFFSET / PAGE).read() };
    lin::check_true(
        &mut failed,
        b"the syscall's buffer is resident afterwards",
        mid_resident != 0,
    );

    // The same page read back by hand still holds what the kernel put there -
    // the copy landed in the page the fault handler made, not in a scratch one
    // that the mapping never saw. Eight random bytes are zero with probability
    // 2^-64, so a zero here means the fill went somewhere else.
    let written = unsafe { core::ptr::read_volatile(mid as *const u64) };
    lin::check_true(
        &mut failed,
        b"the bytes the kernel filled are readable through the mapping",
        written != 0,
    );

    // --- the pages can be given back, and come again ---
    //
    // `MADV_DONTNEED` is answerable for the same reason the mapping is:
    // dropping a page leaves an address that the next fault refills. The page
    // that was written before the advice reads as *zero* afterwards - a fresh
    // page, not the old one with its bytes - which is the promise the man page
    // makes and the assertion a no-op fails.
    lin::check_eq(
        &mut failed,
        b"MADV_DONTNEED over the arena answers 0",
        lin::madvise(arena, ARENA_BYTES, lin::MADV_DONTNEED),
        0,
    );
    lin::check_eq(
        &mut failed,
        b"mincore after the advice answers 0",
        lin::mincore(arena, ARENA_BYTES, vec),
        0,
    );
    let mut still_resident = 0usize;
    for i in 0..ARENA_PAGES {
        if unsafe { vec.add(i).read() } != 0 {
            still_resident += 1;
        }
    }
    lin::check_eq(
        &mut failed,
        b"the advice released every page of the arena",
        still_resident as isize,
        0,
    );
    let released = unsafe { (arena as *const u8).read_volatile() };
    lin::check_eq(
        &mut failed,
        b"the written page is zero-filled again",
        released as isize,
        0,
    );

    lin::check_eq(
        &mut failed,
        b"the arena is given back",
        lin::munmap(arena, ARENA_BYTES),
        0,
    );

    // --- more address space than memory ---
    let big = lin::mmap(
        0,
        BIG_BYTES,
        lin::PROT_READ | lin::PROT_WRITE,
        lin::MAP_PRIVATE | lin::MAP_ANONYMOUS,
        -1,
        0,
    );
    lin::check_true(
        &mut failed,
        b"a 1 GiB mapping is address space, and succeeds",
        big > 0,
    );
    if big > 0 {
        lin::check_eq(
            &mut failed,
            b"mincore over the 1 GiB mapping answers 0",
            lin::mincore(big as usize, BIG_BYTES, vec),
            0,
        );
        let mut big_resident = 0usize;
        // Every page: one sample would pass on a kernel that allocated
        // everything except the page it was shown.
        for i in 0..BIG_PAGES {
            if unsafe { vec.add(i).read() } != 0 {
                big_resident += 1;
            }
        }
        lin::check_eq(
            &mut failed,
            b"not one page of the 1 GiB mapping is backed",
            big_resident as isize,
            0,
        );
        lin::check_eq(
            &mut failed,
            b"the 1 GiB mapping is given back",
            lin::munmap(big as usize, BIG_BYTES),
            0,
        );
    }

    lin::done(failed)
}

#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    lin::die(b"t-mmap-demand: panicked\n")
}
