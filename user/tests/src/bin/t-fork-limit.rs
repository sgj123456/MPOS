//! t-fork-limit — a machine with no room for another task refuses the call, it
//! does not die of it.
//!
//! Linux answers `EAGAIN` at its own task limit, and answers *again* once the
//! children are gone: measured on a current host before this was written
//! (`build/scratch/forklimit/p.c`, `gcc -O1`), `forked 196 children before it
//! refused: errno=11 (Resource temporarily unavailable)` and `after reaping them
//! all, fork answers 208`. The count is live, so the refusal is a statement
//! about right now rather than a permanent exhaustion.
//!
//! This kernel keeps its tasks in a table of fixed slots, and until 2026-11-04
//! a `fork`/`clone` that found every slot taken ended the machine:
//!
//! ```text
//! KERNEL PANIC: panicked at src/sched/runtime/task.rs:2156:32:
//! all 64 task slots are in use
//! ```
//!
//! That is the worst answer available. `EAGAIN` is the one every caller already
//! handles — `pthread_create` reports it, a shell prints "Resource temporarily
//! unavailable" and carries on — while a panic ends the machine and takes every
//! other process with it, including the ones that had nothing to do with the
//! request (`docs/notes/` §6.80).
//!
//! The program cleans up after itself, because the limit it is testing is a live
//! count: each child parks on a pipe this process holds, so it is a *live* task
//! rather than a zombie, and the last thing this program does is close that pipe
//! and reap every child. A pin that left sixty tasks behind would make every
//! program after it in the same lane fail.

use mpos_user_tests::{Report, host_run};
use std::io;

/// What Linux answers for "there is no room for another task" (`EAGAIN`, 11 on
/// every architecture this kernel runs).
const EAGAIN: i32 = 11;

/// Enough attempts to reach this kernel's table (64 slots) with room to spare,
/// and few enough that a host run does not fork a whole machine's worth.
const MOST: i32 = 300;

fn main() {
    let mut r = Report::new();

    let mut ends = [0 as libc::c_int; 2];
    assert_eq!(
        unsafe { libc::pipe(ends.as_mut_ptr()) },
        0,
        "pipe for the children to park on"
    );
    let (read_end, write_end) = (ends[0], ends[1]);

    let mut kids: Vec<libc::pid_t> = Vec::new();
    let mut refusal: Option<i32> = None;
    for _ in 0..MOST {
        let pid = unsafe { libc::fork() };
        if pid < 0 {
            refusal = io::Error::last_os_error().raw_os_error();
            break;
        }
        if pid == 0 {
            // The child: stay a live task on this pipe until the parent closes
            // it, then leave. Nothing here reports anything - the claims belong
            // to the parent. The inherited *write* end has to go first: a reader
            // only sees the pipe close when every write end is closed, and each
            // child holding one would keep all its siblings parked forever.
            unsafe { libc::close(write_end) };
            let mut byte = 0u8;
            unsafe { libc::read(read_end, &mut byte as *mut u8 as *mut libc::c_void, 1) };
            unsafe { libc::_exit(0) };
        }
        kids.push(pid);
    }

    // The table's width is measured here beside the refusal, because a machine
    // with room for sixty-odd tasks cannot host the sweep's own lane: the
    // runner's `busybox timeout` holds a watchdog child for every program's
    // whole budget, so a lane of ~50 programs wants ~50 slots for them alone
    // (`docs/notes/` §6.80). A hundred is the floor that makes a lane possible;
    // the shipped table is 256 and this claim fails at 64.
    r.check(
        &format!(
            "the machine has room for more than a hundred tasks at once ({} made)",
            kids.len()
        ),
        kids.len() > 100 || host_run(),
    );
    r.check(
        &format!(
            "a fork with no room for another task is refused, not fatal ({} children made, answer {refusal:?})",
            kids.len()
        ),
        // A host run's own limit is far wider than this program's cap, so it
        // reaches the cap rather than the limit; the refusal's *shape* is
        // measured by the probe named in this file's header, and the machine's
        // is what the next claim is about.
        refusal.is_some() || host_run(),
    );
    r.check(
        "and the refusal is EAGAIN, the answer Linux gives at its own task limit",
        refusal == Some(EAGAIN) || host_run(),
    );

    // Give the children their byte-less release, then collect every one: the
    // count that refused the fork has to go back down, or a later fork in this
    // same boot inherits the exhaustion.
    unsafe { libc::close(write_end) };
    let mut collected = 0usize;
    let mut all_exited = true;
    for &pid in &kids {
        let mut status = 0 as libc::c_int;
        let reaped = unsafe { libc::waitpid(pid, &mut status, 0) };
        if reaped == pid {
            collected += 1;
            all_exited &= libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0;
        }
    }
    r.check(
        &format!(
            "every child this program made is collected ({collected} of {})",
            kids.len()
        ),
        collected == kids.len() && all_exited,
    );

    // The live-count half: the same call the machine refused a moment ago now
    // works, which is the difference between "full" and "broken".
    let again = unsafe { libc::fork() };
    if again == 0 {
        unsafe { libc::_exit(0) };
    }
    let mut status = 0 as libc::c_int;
    let reaped = if again > 0 {
        unsafe { libc::waitpid(again, &mut status, 0) }
    } else {
        -1
    };
    r.check(
        &format!("a fork after them works again (answer {again}, collected {reaped})"),
        again > 0 && reaped == again && libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0,
    );

    unsafe { libc::close(read_end) };
    r.finish();
}
