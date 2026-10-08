//! t-fork - one test for fork, the call every shell is built on.
//!
//! The man page pins three things at once: the child gets 0 back and the parent
//! gets the child's pid; both continue from the instruction after the call; and
//! the child gets a *copy* of the parent's memory, so a write on one side is
//! invisible on the other.
//!
//! The last one is what the test really exercises. A variable is set before the
//! fork and rewritten by the child; the parent must still see the old value.
//! That is copy-on-write doing its job - the page only splits when the child
//! writes it - and a kernel that handed both processes the same page would pass
//! everything else here while failing this.
//!
//! # The migration
//!
//! `fork` is one of the calls `rustix` does not bind at all, so the creation end
//! is `libc::fork` - a maintained per-machine binding of the call the program is
//! about (`__NR_fork` has no arm64 number, and musl's `fork` is `clone` there;
//! `nr_aarch64.rs` records `FORK = 64007; // no arm64 number`, so the second
//! machine pins the answer a caller gets rather than a syscall that does not
//! exist - the shape `t-dup2` and `t-access` already use for that list).
//!
//! The shared cell is a `static AtomicI32` rather than the no-std version's
//! `static mut SHARED`: the 2024 edition refuses to reference a `static mut`,
//! and the claim - the child's store stays in the child - is about which copy
//! of the page the write lands in, which an atomic states just as plainly.
//!
//! The child leaves through the raw `SYS_exit_group` and the parent reaps
//! through `rustix`'s `waitpid` (`wait4` with a null `rusage`); `WaitStatus` is
//! where the no-std `WIFEXITED`/`WEXITSTATUS` calls went. The raw exit is
//! load-bearing twice over: it is the syscall the process ends through, and it
//! cannot flush the child's inherited copy of the parent's buffered stdout.

use mpos_user_tests::Report;
use rustix::process::{Pid, WaitOptions, waitpid};
use std::sync::atomic::{AtomicI32, Ordering};

/// The variable the child updates and the parent must not see change. A `static`
/// rather than a local so both processes address it the same way and the
/// comparison is unambiguous.
static SHARED: AtomicI32 = AtomicI32::new(7);

/// The code the child leaves with, distinct from anything the parent uses.
const CHILD_CODE: i32 = 42;

fn main() {
    println!("t-fork");
    let mut r = Report::new();

    let pid = unsafe { libc::fork() };
    if pid < 0 {
        r.check("fork succeeds", false);
        r.finish()
    }

    if pid == 0 {
        // The child: write the shared cell, then leave with a code the parent
        // can recognise. Any print here would be reported against the child's
        // copy of `r` (and would flush the parent's own buffered lines a second
        // time), so the child stays quiet and lets the parent do the judging.
        SHARED.store(99, Ordering::Relaxed);
        unsafe {
            libc::syscall(libc::SYS_exit_group, CHILD_CODE as libc::c_long);
            // The door does not return; `_exit` is the maintained fallback.
            libc::_exit(CHILD_CODE)
        }
    }

    // The parent. `pid` is the child's pid, which is what fork promises.
    r.check("fork returns the child's pid to the parent", pid > 0);

    let want = Pid::from_raw(pid);
    let reaped = waitpid(want, WaitOptions::empty());
    r.check(
        "wait4 reaps the child and returns its pid",
        matches!(reaped, Ok(Some((got, _))) if Some(got) == want),
    );
    match reaped {
        Ok(Some((_, status))) => {
            r.check("the child exited normally, not by signal", status.exited());
            r.check(
                "the child's exit code is the one it passed to exit",
                status.exit_status() == Some(CHILD_CODE),
            );
        }
        _ => {
            r.check("the child exited normally, not by signal", false);
            r.check("the child's exit code is the one it passed to exit", false);
        }
    }

    // The heart of it: the child's write stayed in the child.
    r.check(
        "the child's write did not reach the parent (copy-on-write)",
        SHARED.load(Ordering::Relaxed) == 7,
    );

    r.finish()
}
