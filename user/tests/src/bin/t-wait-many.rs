//! t-wait-many — a machine remembers every child that died, however many died
//! before the parent asked.
//!
//! A child's exit status is owed to whoever waits for it, and it is owed until
//! it is collected - not until some store fills up. Linux keeps the zombie's
//! `struct pid` and its code until `wait` takes them, so a parent that sleeps
//! while forty children die still hears forty deaths. Measured on a current
//! host before this was written (`build/scratch/retired/t-retired.c`, `gcc -O1`):
//! `heard 40 of 40 deaths`.
//!
//! This kernel frees the slot the moment a task is reaped and keeps the code in
//! a separate store, which before 2026-11-04 held thirty-two records and dropped
//! the oldest once it was full. A parent that was not looking lost the deaths
//! that fell off: the same probe read `heard 32 of 40 deaths` here, `waitpid`
//! answering `ECHILD` for the eight it had forgotten - and a caller already
//! *parked* on one of them never woke at all, because the predicate it slept on
//! could no longer come true (`docs/notes/` §6.81).
//!
//! The program does what a parent does: fork a crowd, let them all die while it
//! looks at nothing, then collect them one by one and check that each death is
//! the one it asked for.

use mpos_user_tests::Report;
use std::io;

/// More children than the store used to hold, so a run that reaches this many
/// is a run that would have lost a death before the cap was raised.
const MOST: i32 = 40;

fn main() {
    let mut r = Report::new();

    let mut kids: Vec<libc::pid_t> = Vec::new();
    for i in 0..MOST {
        let pid = unsafe { libc::fork() };
        if pid < 0 {
            // A lane with a crowded table can refuse; the count is what the
            // claims below are about, and a refusal is `t-fork-limit`'s claim,
            // not this one's.
            let _ = io::Error::last_os_error();
            break;
        }
        if pid == 0 {
            // Its own index is its exit status, so a record that came back for
            // the wrong child is caught rather than counted.
            unsafe { libc::_exit(i) };
        }
        kids.push(pid);
        // Give it time to die *and* for the machine to free the slot it held:
        // this program's subject is the store of uncollected deaths, not the
        // table's width, so it must fit in a lane that is short of slots. A
        // slot comes back the moment its task is reaped, while the record of
        // its death stays until a `wait` takes it - which is the whole point.
        std::thread::sleep(std::time::Duration::from_millis(25));
    }

    r.check(
        &format!(
            "enough children died at once to fill the old store ({} of {MOST})",
            kids.len()
        ),
        kids.len() as i32 > 32,
    );

    // Nothing is collected while they die: every death has to be remembered
    // until this loop asks for it.
    std::thread::sleep(std::time::Duration::from_millis(300));

    let mut heard = 0usize;
    let mut right_code = 0usize;
    for (i, &pid) in kids.iter().enumerate() {
        let mut status = 0 as libc::c_int;
        let got = unsafe { libc::waitpid(pid, &mut status, 0) };
        if got != pid {
            continue;
        }
        heard += 1;
        if libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == i as libc::c_int {
            right_code += 1;
        }
    }
    r.check(
        &format!("every death is still there to be collected ({heard} of {})", kids.len()),
        heard == kids.len(),
    );
    r.check(
        &format!("and each one is the child it belongs to ({right_code} of {})", kids.len()),
        right_code == kids.len(),
    );

    r.finish();
}