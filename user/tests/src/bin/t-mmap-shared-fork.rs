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
//!
//! # The migration
//!
//! This is the memory family's *pure-`rustix`* program, and that is the batch's
//! finding: where `t-mmap` and `t-madvise` had to ride `libc` (their claims are
//! about the six-word call and the advice vocabulary, which the typed wrappers
//! edit or cannot name), this program's claims are about what the pages *are* -
//! whether two processes alias them - and for that the maintained table is
//! enough: `mmap_anonymous`, `munmap`, and `madvise(.., Advice::DontNeed)`,
//! whose variants carry Linux's own `MADV_*` words (its `Normal` *is*
//! `MADV_NORMAL`'s number, so the advice reaching the kernel is the advice this
//! claim is about). `mmap_anonymous` does edit one word of its own - it ORs
//! `MAP_ANONYMOUS` in, and `MapFlags` has no name for the bit - which is why
//! the *flag word* is not this program's subject: where it is, `t-mmap` goes
//! through the raw door instead.
//!
//! Creation and reaping are batch 18's: `libc::fork`, the raw `SYS_exit_group`
//! for the code each child leaves with (a forked std child holds a copy of the
//! parent's buffered stdout, and only a non-flushing exit leaves it behind), and
//! `rustix::process::waitpid` - `wait4` with a null `rusage` - with
//! `WaitStatus::exited`/`exit_status` where the no-std version read the status
//! word by hand.

use mpos_user_tests::Report;
use rustix::io::{read, write};
use rustix::mm::{Advice, MapFlags, ProtFlags, madvise, mmap_anonymous, munmap};
use rustix::pipe::pipe;
use rustix::process::{Pid, WaitOptions, WaitStatus, waitpid};

const PAGE: usize = 4096;

/// The bytes the child leaves behind, at an offset the parent never writes.
const CHILD_AT: usize = 64;
const CHILD: &[u8] = b"CHILD";
/// And the ones the parent leaves for a child that is already running.
const LATER_AT: usize = 128;
const LATER: &[u8] = b"LATER";
/// The code each child leaves with when the mapping it read was the one it
/// should have been: a nonzero code is this test's own "the alias was missing",
/// and the parent asserts on that code rather than on a print.
const ALIASED: i32 = 0;

/// Leave through `exit_group`: the code is the claim, and a forked std child's
/// inherited copy of the parent's stdout must not be flushed.
fn leave(code: i32) -> ! {
    unsafe {
        libc::syscall(libc::SYS_exit_group, code as libc::c_long);
        // The door does not return; `_exit` is the maintained fallback.
        libc::_exit(code)
    }
}

/// The blocking `wait4(pid)`: `Some(status)` when that child was the one
/// collected, `None` when the call answered anything else.
fn collect(pid: i32) -> Option<WaitStatus> {
    let want = Pid::from_raw(pid);
    match waitpid(want, WaitOptions::empty()) {
        Ok(Some((got, status))) if Some(got) == want => Some(status),
        _ => None,
    }
}

fn main() {
    println!("t-mmap-shared-fork");
    let mut r = Report::new();

    let s = unsafe {
        mmap_anonymous(
            core::ptr::null_mut(),
            PAGE,
            ProtFlags::READ | ProtFlags::WRITE,
            MapFlags::SHARED,
        )
    };
    r.check("the shared anonymous mapping lands", s.is_ok());
    let Ok(base) = s else { r.finish() };
    let base = base.cast::<u8>();
    unsafe { core::ptr::copy_nonoverlapping(b"PARENT".as_ptr(), base, 6) };

    // --- direction one: the child writes, the parent looks ---
    let pid = unsafe { libc::fork() };
    if pid == 0 {
        // Read what the parent wrote, then leave a marker the parent can only
        // see if the frames are the same memory. Reading the parent's bytes is
        // not the discriminating check - a copy has those too - so it is
        // reported as its own exit code and the marker is what the parent
        // asserts on.
        let inherited = unsafe { core::slice::from_raw_parts(base, 6) == b"PARENT" };
        unsafe {
            core::ptr::copy_nonoverlapping(CHILD.as_ptr(), base.add(CHILD_AT), CHILD.len());
        }
        leave(if inherited { 0 } else { 1 });
    }
    r.check("the writing child started", pid > 0);
    let status = collect(pid);
    r.check("the writing child is reaped", status.is_some());
    r.check(
        "the child exited normally",
        status.is_some_and(WaitStatus::exited),
    );
    r.check(
        "the child read the parent's bytes through the mapping",
        status.and_then(WaitStatus::exit_status) == Some(0),
    );
    r.check(
        "the child's write is visible in the parent: the frames are the same memory",
        unsafe { core::slice::from_raw_parts(base.add(CHILD_AT), CHILD.len()) == CHILD },
    );

    // --- the pages come from the object, not from a copy ---
    r.check(
        "MADV_DONTNEED on the shared mapping answers 0",
        unsafe { madvise(base.cast(), PAGE, Advice::DontNeed) }.is_ok(),
    );
    r.check(
        "the child's bytes come back from the object after the advice",
        unsafe { core::slice::from_raw_parts(base.add(CHILD_AT), CHILD.len()) == CHILD },
    );

    // --- direction two: the parent writes after the fork, the child looks ---
    let pair = pipe();
    r.check("the ordering pipe is made", pair.is_ok());
    let Ok((sp_r, sp_w)) = pair else { r.finish() };
    let pid = unsafe { libc::fork() };
    if pid == 0 {
        // Wait for the parent to say it has written, then read the page. The
        // pipe is the whole point: without it the child could look first and a
        // private copy would pass.
        let mut byte = [0u8; 1];
        let _ = read(&sp_r, &mut byte);
        let seen = unsafe { core::slice::from_raw_parts(base.add(LATER_AT), LATER.len()) == LATER };
        leave(if seen { ALIASED } else { 2 });
    }
    r.check("the reading child started", pid > 0);
    unsafe { core::ptr::copy_nonoverlapping(LATER.as_ptr(), base.add(LATER_AT), LATER.len()) };
    r.check(
        "the parent signals the child",
        write(&sp_w, b"x").is_ok_and(|n| n == 1),
    );
    let status = collect(pid);
    r.check("the reading child is reaped", status.is_some());
    r.check(
        "the reading child exited normally",
        status.is_some_and(WaitStatus::exited),
    );
    r.check(
        "the child saw the parent's write through the mapping",
        status.and_then(WaitStatus::exit_status) == Some(ALIASED),
    );

    r.check(
        "the mapping goes",
        unsafe { munmap(base.cast(), PAGE) }.is_ok(),
    );
    r.finish()
}
