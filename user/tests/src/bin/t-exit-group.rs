//! t-exit-group - one test for exit_group, how musl terminates: the strace
//! closes with `exit_group(0)`.
//!
//! The code a process leaves with cannot be read from inside the dying
//! process, so it is read from just outside it: the test forks, the child
//! calls `exit_group(42)`, and the parent reaps it and checks the status the
//! kernel handed back. That is what "the code survives the scheduler to the
//! parent's wait" means, and it is observable - a test that simply exited 42
//! would leave the harness holding the code, reading it as a failure of the
//! very test that produced it.
//!
//! # The migration
//!
//! The child leaves through the raw `SYS_exit_group` rather than through
//! `libc::_exit`, because the syscall is this program's whole subject - and the
//! number comes from libc's per-machine table (231 here, 94 on the second
//! machine) rather than from this repository's memory of it. The raw door is
//! also the only one that *cannot flush*: musl's `_exit` is the same call
//! (measured from the crate's own static `libc.a`: `__syscall(SYS_exit_group,
//! status)`, with a loop of `SYS_exit` after it for the return that cannot
//! happen), but a forked std child also holds a copy of the parent's buffered
//! stdout, and an exit that flushed would print every line above it twice.
//!
//! The reaping end is `rustix::process::waitpid`, which *is* `wait4` with a null
//! `rusage` - the call the no-std version made by hand - and its `WaitStatus` is
//! where this program's `WIFEXITED`/`WEXITSTATUS` went: `exited()` and
//! `exit_status()` are those two rules, taken from the maintained table instead
//! of spelled out here.

use mpos_user_tests::Report;
use rustix::process::{Pid, WaitOptions, waitpid};

/// The exit code the parent must observe.
const CODE: i32 = 42;

/// Leave through `exit_group`, the call this program is about.
fn leave(code: i32) -> ! {
    unsafe {
        libc::syscall(libc::SYS_exit_group, code as libc::c_long);
        // The door does not return; `_exit` is the maintained fallback.
        libc::_exit(code)
    }
}

fn main() {
    println!("t-exit-group");
    let mut r = Report::new();

    let child = unsafe { libc::fork() };
    if child == 0 {
        // Nothing after this runs: the group ends here, with `CODE`.
        leave(CODE);
    }
    r.check("fork answered a child", child > 0);

    let want = Pid::from_raw(child);
    let reaped = waitpid(want, WaitOptions::empty());
    r.check(
        "the exit_group'ing child is reaped by its pid",
        matches!(reaped, Ok(Some((got, _))) if Some(got) == want),
    );
    match reaped {
        Ok(Some((_, status))) => {
            r.check("the child left a status, not a signal", status.exited());
            r.check(
                "the code passed to exit_group is the one the parent waits for",
                status.exit_status() == Some(CODE),
            );
        }
        _ => {
            r.check("the child left a status, not a signal", false);
            r.check(
                "the code passed to exit_group is the one the parent waits for",
                false,
            );
        }
    }

    r.finish()
}
