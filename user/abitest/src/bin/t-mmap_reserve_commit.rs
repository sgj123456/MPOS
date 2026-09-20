//! t-mmap_reserve_commit - a region reserved with `PROT_NONE` and then
//! committed with `mprotect` is writable.
//!
//! That is the shape every allocator and every thread stack uses: map the
//! address space first with nothing in it, then hand out (or `mprotect`) the
//! part that is about to be used. It is also the one shape that catches a
//! paging-structure bug nothing else here can reach, because it is the only one
//! where the *tables* and the *leaf* are created at different times.
//!
//! The U/S bit is not the leaf's to decide. The CPU ANDs it along the walk, so
//! a single upper level with U=0 makes its whole subtree a supervisor page
//! however the leaf reads - and a table is made once and then shared with
//! everything that lands under it. The `x86_64` crate's `map_to` derives the
//! upper levels' flags *from the leaf*, so a `PROT_NONE` mapping (which mpos
//! answers with `MapFlags::READ` and no `USER`, correctly) used to leave
//! PRESENT-only PML4E/PDPTE/PDE behind. `mprotect` then fixed the leaf and
//! nothing else, and the first write to the committed page was a page fault on
//! a page whose own PTE read P=1 W=1 U=1.
//!
//! That is exactly how `fish` died: `pthread_create` mmaps a stack, and
//! musl's `__copy_tls` stores the TCB header into it before anything else uses
//! it. The fault reported `rip` inside the loader, `0x7efffffffb20` - the page
//! below the top of the thread stack - and repeated forever once fish's own
//! SIGSEGV handler was in the way.

#![no_std]
#![no_main]

#[path = "../../lib/lin.rs"]
mod lin;

use core::panic::PanicInfo;

/// Large enough to need tables of its own rather than sharing a neighbour's,
/// and small enough that walking it costs nothing.
const LEN: usize = 0x4_0000;
const PAGE: usize = 0x1000;
/// Not zero: a page that reads back as zero is what an untouched page - or one
/// that got a frame of somebody else's leftovers - would also look like, and
/// the failure has to be unambiguous.
const BYTE: u8 = 0x5a;

#[unsafe(no_mangle)]
extern "C" fn mpos_user_main(_argc: usize, _argv: *const *const u8) -> ! {
    lin::puts(b"t-mmap_reserve_commit\n");
    let mut failed = false;

    let reserved = lin::mmap(
        0,
        LEN,
        lin::PROT_NONE,
        lin::MAP_PRIVATE | lin::MAP_ANONYMOUS,
        -1,
        0,
    );
    lin::check_true(
        &mut failed,
        b"mmap(PROT_NONE) reserves a region",
        reserved > 0,
    );
    if reserved <= 0 {
        lin::done(failed);
    }
    let base = reserved as usize;
    lin::check_eq(
        &mut failed,
        b"the reservation is page-aligned",
        (base % PAGE) as isize,
        0,
    );

    let rc = lin::mprotect(base, LEN, lin::PROT_READ | lin::PROT_WRITE);
    lin::check_eq(
        &mut failed,
        b"mprotect commits the reservation to read-write",
        rc,
        0,
    );

    // Every page, not just the first. The bug left the tables above the leaf
    // supervisor-only, which a write to the *first* page of a fresh span can
    // miss - the tables a mapping needs are created by whichever mapping gets
    // there first, and that need not be the page under test.
    let mut offset = 0;
    while offset < LEN {
        unsafe { core::ptr::write_volatile((base + offset) as *mut u8, BYTE) };
        offset += PAGE;
    }

    let mut read_back = true;
    let mut offset = 0;
    while offset < LEN {
        if unsafe { core::ptr::read_volatile((base + offset) as *const u8) } != BYTE {
            read_back = false;
        }
        offset += PAGE;
    }
    lin::check_true(
        &mut failed,
        b"every page of the committed region takes a write and reads it back",
        read_back,
    );

    // The same walk once more, now over memory the page tables have definitely
    // seen: a stale entry that the first pass papered over would show here.
    let mut offset = 0;
    while offset < LEN {
        unsafe { core::ptr::write_volatile((base + offset) as *mut u8, BYTE ^ 0xff) };
        offset += PAGE;
    }
    let mut second_pass = true;
    let mut offset = 0;
    while offset < LEN {
        if unsafe { core::ptr::read_volatile((base + offset) as *const u8) } != BYTE ^ 0xff {
            second_pass = false;
        }
        offset += PAGE;
    }
    lin::check_true(
        &mut failed,
        b"a second pass over the same pages still writes",
        second_pass,
    );

    let released = lin::munmap(base, LEN);
    lin::check_eq(&mut failed, b"munmap releases the reservation", released, 0);

    lin::done(failed)
}

#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    lin::die(b"t-mmap_reserve_commit: panicked\n")
}
