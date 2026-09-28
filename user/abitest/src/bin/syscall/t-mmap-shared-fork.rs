//! t-mmap-shared-fork - a `MAP_SHARED|MAP_ANONYMOUS` mapping is an *object*,
//! and a `fork` keeps both processes looking at it.
//!
//! This is PLAN §5.1's gap, closed. Before anonymous demand paging a shared
//! anonymous mapping was served as an ordinary anonymous range: two threads of
//! one process (`CLONE_VM`) saw each other's writes, because they share one
//! address space, but a `fork`ed child got a copy-on-write private range and the
//! first store on either side diverged. The mapping now sits over an unnamed
//! `Memfd` - the same frame store `memfd_create` hands out - and `fork` copies
//! the *object* rather than private-copying the pages, so both processes alias
//! the same frames.
//!
//! The assertion that separates the two is direction-sensitive, which is why
//! there are two of them:
//!
//! * a write in the **child**, read in the **parent** after `wait4`. This is the
//!   one a COW'd mapping fails: the child's store is answered by the fault
//!   handler with a private frame, and the parent goes on reading what it wrote.
//! * a write in the **parent** *after* the fork, read in the **child**. A pipe
//!   orders the two, because "after the fork" is the half of the contract a
//!   pre-fork copy would also satisfy.
//!
//! The `MADV_DONTNEED` step in the middle is the object proving itself: the
//! pages are dropped for this mapping and the next access finds the *object's*
//! page, with the bytes the other process put there.

#![no_std]
#![no_main]

#[path = "../../../lib/lin.rs"]
mod lin;

use core::panic::PanicInfo;

const PAGE: usize = 4096;
const RW: usize = lin::PROT_READ | lin::PROT_WRITE;

/// The bytes the child leaves behind, at an offset the parent never writes.
const CHILD_AT: usize = 64;
const CHILD: &[u8] = b"CHILD";
/// And the ones the parent leaves for a child that is already running.
const LATER_AT: usize = 128;
const LATER: &[u8] = b"LATER";

#[unsafe(no_mangle)]
extern "C" fn mpos_user_main(_argc: usize, _argv: *const *const u8) -> ! {
    lin::puts(b"t-mmap-shared-fork\n");
    let mut failed = false;

    let s = lin::mmap(0, PAGE, RW, lin::MAP_SHARED | lin::MAP_ANONYMOUS, -1, 0);
    lin::check_true(
        &mut failed,
        b"the shared anonymous mapping lands",
        s > 0 && s as usize % PAGE == 0,
    );
    if s <= 0 {
        lin::done(failed);
    }
    let base = s as *mut u8;
    unsafe { core::ptr::copy_nonoverlapping(b"PARENT".as_ptr(), base, 6) };

    // --- direction one: the child writes, the parent looks ---
    let pid = lin::fork();
    if pid == 0 {
        // Read what the parent wrote, then leave a marker the parent can only
        // see if the frames are the same memory. Reading the parent's bytes is
        // not the discriminating check - a copy has those too - so it is
        // reported as its own exit code and the marker is what the parent
        // asserts on.
        let inherited = unsafe { core::slice::from_raw_parts(base, 6) == b"PARENT" };
        unsafe { core::ptr::copy_nonoverlapping(CHILD.as_ptr(), base.add(CHILD_AT), CHILD.len()) };
        lin::exit_group(if inherited { 0 } else { 1 });
    }
    lin::check_true(&mut failed, b"the writing child started", pid > 0);
    let mut status = 0i32;
    lin::check_eq(
        &mut failed,
        b"the writing child is reaped",
        lin::wait4(pid as usize, &mut status, 0),
        pid,
    );
    lin::check_true(
        &mut failed,
        b"the child exited normally",
        status & 0x7f == 0,
    );
    lin::check_eq(
        &mut failed,
        b"the child read the parent's bytes through the mapping",
        ((status >> 8) & 0xff) as isize,
        0,
    );
    let seen = unsafe { core::slice::from_raw_parts(base.add(CHILD_AT), CHILD.len()) == CHILD };
    lin::check_true(
        &mut failed,
        b"the child's write is visible in the parent: the frames are the same memory",
        seen,
    );

    // --- the pages come from the object, not from a copy ---
    lin::check_eq(
        &mut failed,
        b"MADV_DONTNEED on the shared mapping answers 0",
        lin::madvise(s as usize, PAGE, lin::MADV_DONTNEED),
        0,
    );
    let after = unsafe { core::slice::from_raw_parts(base.add(CHILD_AT), CHILD.len()) == CHILD };
    lin::check_true(
        &mut failed,
        b"the child's bytes come back from the object after the advice",
        after,
    );

    // --- direction two: the parent writes after the fork, the child looks ---
    let mut sp = [-1i32; 2];
    lin::check_eq(
        &mut failed,
        b"the ordering pipe is made",
        lin::pipe2(&mut sp, 0),
        0,
    );
    let pid = lin::fork();
    if pid == 0 {
        // Wait for the parent to say it has written, then read the page. The
        // pipe is the whole point: without it the child could look first and a
        // private copy would pass.
        let mut byte = [0u8; 1];
        let _ = lin::read(sp[0], &mut byte);
        let seen = unsafe { core::slice::from_raw_parts(base.add(LATER_AT), LATER.len()) == LATER };
        lin::exit_group(if seen { 0 } else { 2 });
    }
    lin::check_true(&mut failed, b"the reading child started", pid > 0);
    unsafe { core::ptr::copy_nonoverlapping(LATER.as_ptr(), base.add(LATER_AT), LATER.len()) };
    lin::check_eq(
        &mut failed,
        b"the parent signals the child",
        lin::write(sp[1], b"x"),
        1,
    );
    let mut status = 0i32;
    lin::check_eq(
        &mut failed,
        b"the reading child is reaped",
        lin::wait4(pid as usize, &mut status, 0),
        pid,
    );
    lin::check_true(
        &mut failed,
        b"the reading child exited normally",
        status & 0x7f == 0,
    );
    lin::check_eq(
        &mut failed,
        b"the child saw the parent's write through the mapping",
        ((status >> 8) & 0xff) as isize,
        0,
    );

    let _ = lin::munmap(s as usize, PAGE);
    let _ = lin::close(sp[0]);
    let _ = lin::close(sp[1]);
    lin::done(failed)
}

#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    lin::die(b"t-mmap-shared-fork: panicked\n")
}
