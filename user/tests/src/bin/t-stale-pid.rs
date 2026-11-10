//! t-stale-pid - a collected number is a promise, and it stops naming anybody.
//!
//! On Linux a pid is a refcounted `struct pid` whose lifetime is its own, so a
//! number a caller remembers stays *spent* after the child is collected: `kill`
//! answers `ESRCH`, `/proc/<it>` is `ENOENT`, and no later `fork` can hand the
//! number to somebody else. Measured on 7.2.14 (`build/scratch/pidreuse/reuse.c`):
//! ten children born and collected one at a time are `129232`..`129241`, fifty
//! children born after them reuse none of those numbers, and after collection
//! `kill(<n>, 0)` = `-1/ESRCH`, `/proc/<n>` = `-1/ENOENT`, `waitpid` =
//! `-1/ECHILD`, `pidfd_open` = `-1/ESRCH`.
//!
//! This kernel used to fail that, and the failure was at the allocation end: the
//! init namespace numbered its tasks *by slot*, and the slot allocator hands out
//! the lowest free index, so the very next `fork` took the number a caller had
//! just collected - `kill(<collected>, SIGTERM)` ended a process the caller had
//! never had anything to do with (`docs/notes/` §6.89's reading, §6.121's fix).
//!
//! The ordering below is what makes the *reuse* deterministic rather than a
//! coincidence: a helper process occupies its own slot first; only then does
//! this program collect its child, freeing the slot immediately above the
//! helper's; only then does the helper fork, so the slot allocator has exactly
//! one lowest-free slot to hand out and it is the one under test. Before the
//! fix that grandchild took the collected number; after it, it takes a number of
//! its own and the collected one names nobody for ever.
//!
//! The last claim is the boundary that stops the fix from overshooting: a
//! **zombie**'s number still names it - Linux keeps a process in the namespace
//! until it is reaped, so `kill` finds it and `/proc` lists it with state `Z`.

use mpos_user_tests::Report;
use std::io::Read;

/// What `/proc/<pid>/stat` says the process's state letter is, or how the read
/// failed. The letter is field 3, right after the parenthesised `comm` - which is
/// why the split starts after the *last* `)`: a `comm` may contain everything.
fn state_of(pid: libc::pid_t) -> String {
    let path = format!("/proc/{pid}/stat");
    let mut file = match std::fs::File::open(&path) {
        Ok(file) => file,
        Err(_) => return "ENOENT".to_string(),
    };
    let mut text = String::new();
    if file.read_to_string(&mut text).is_err() {
        return "ENOENT".to_string();
    }
    match text
        .rsplit_once(')')
        .map(|(_, rest)| rest.split_whitespace().next())
    {
        Some(Some(state)) => state.to_string(),
        _ => "?".to_string(),
    }
}

/// The errno of a failing call, or 0 - the pair is what the claims read.
fn errno() -> i32 {
    std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
}

fn main() {
    let mut report = Report::new();

    let mut to_helper = [0i32; 2];
    let mut from_helper = [0i32; 2];
    if unsafe { libc::pipe(to_helper.as_mut_ptr()) } != 0
        || unsafe { libc::pipe(from_helper.as_mut_ptr()) } != 0
    {
        println!("t-stale-pid: pipes failed");
        std::process::exit(1);
    }

    let helper = unsafe { libc::fork() };
    if helper == 0 {
        unsafe {
            libc::close(to_helper[1]);
            libc::close(from_helper[0]);
        }
        // Wait for the signal that our slot is taken and the number is free.
        let mut byte = [0u8; 1];
        if unsafe { libc::read(to_helper[0], byte.as_mut_ptr().cast(), 1) } != 1 {
            unsafe { libc::_exit(1) };
        }
        let grandchild = unsafe { libc::fork() };
        if grandchild == 0 {
            // Alive and asleep until somebody kills it: the claim that it *was
            // not* killed is what the SIGTERM below would take away. Sleeping
            // rather than `pause`-ing because this is the one claim whose whole
            // point is that the process is still there to be found.
            loop {
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
        }
        let text = format!("{grandchild}");
        unsafe {
            libc::write(
                from_helper[1],
                text.as_ptr().cast(),
                text.len() as libc::size_t,
            )
        };
        // Hold the slot until this program says it has finished reading.
        let _ = unsafe { libc::read(to_helper[0], byte.as_mut_ptr().cast(), 1) };
        unsafe { libc::_exit(0) };
    }
    unsafe {
        libc::close(to_helper[0]);
        libc::close(from_helper[1]);
    }

    // A child of our own, collected immediately: this is the number we hold.
    let mine = unsafe { libc::fork() };
    if mine == 0 {
        unsafe { libc::_exit(0) };
    }
    let mut status = 0i32;
    unsafe { libc::waitpid(mine, &mut status, 0) };

    // Now let somebody else's child take the pool slot this number just left.
    unsafe { libc::write(to_helper[1], b"g".as_ptr().cast(), 1) };
    let mut buffer = [0u8; 32];
    let read = unsafe {
        libc::read(
            from_helper[0],
            buffer.as_mut_ptr().cast(),
            buffer.len() as libc::size_t,
        )
    };
    let stranger: libc::pid_t = std::str::from_utf8(&buffer[..read.max(0) as usize])
        .ok()
        .and_then(|text| text.trim().parse().ok())
        .unwrap_or(-1);

    // The claim this whole program exists for: the slot was taken, and the
    // number was not.
    report.check(
        &format!("the freed slot's next tenant is somebody else ({stranger} is not {mine})"),
        stranger != -1 && stranger != mine,
    );

    // And the number itself is spent: three ways of asking, one answer.
    let probe = unsafe { libc::kill(mine, 0) };
    let probe_errno = errno();
    report.check(
        &format!(
            "a collected number is ESRCH to kill ({probe} errno={probe_errno}, want -1/{})",
            libc::ESRCH
        ),
        probe == -1 && probe_errno == libc::ESRCH,
    );

    let stale_state = state_of(mine);
    report.check(
        &format!("and /proc does not name it any more ({stale_state})"),
        stale_state == "ENOENT",
    );

    let terminated = unsafe { libc::kill(mine, libc::SIGTERM) };
    let terminated_errno = errno();
    report.check(
        &format!(
            "and it cannot end the process that took its slot ({terminated} errno={terminated_errno})",
        ),
        terminated == -1 && terminated_errno == libc::ESRCH,
    );
    // Long enough that a SIGTERM the kernel really delivered would have landed:
    // without the wait, a kernel that sent the signal reads as "still running"
    // because the read wins the race. That is the shape this claim had before
    // the counterfactual showed it passing on the broken kernel.
    std::thread::sleep(std::time::Duration::from_millis(100));
    report.check(
        &format!(
            "so the stranger is still running after that (sits at {})",
            state_of(stranger)
        ),
        matches!(state_of(stranger).as_str(), "S" | "R" | "D"),
    );

    // The layering behind it: numbers only move forward, so a number used twice
    // would be a number handed backwards.
    let mut previous = mine;
    let mut forward = true;
    for _ in 0..4 {
        let next = unsafe { libc::fork() };
        if next == 0 {
            unsafe { libc::_exit(0) };
        }
        if next <= previous {
            forward = false;
        }
        unsafe { libc::waitpid(next, &mut status, 0) };
        previous = next;
    }
    report.check(
        &format!("numbers only move forward across reuse (up to {previous})"),
        forward,
    );

    // The boundary: a *zombie* is still in the namespace - Linux keeps it there
    // until it is reaped, so its number is still its own. A fix that dropped the
    // registration at exit instead of at reap would take this away.
    let zombie = unsafe { libc::fork() };
    if zombie == 0 {
        unsafe { libc::_exit(0) };
    }
    // Let it die without collecting it, and wait for the state letter to turn.
    let mut state = String::new();
    for _ in 0..200 {
        state = state_of(zombie);
        if state == "Z" {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    let zombie_probe = unsafe { libc::kill(zombie, 0) };
    report.check(
        &format!("an unreaped zombie's number still names it (state {state}, kill {zombie_probe})"),
        state == "Z" && zombie_probe == 0,
    );
    unsafe { libc::waitpid(zombie, &mut status, 0) };
    // ...and once reaped it is spent like any other.
    report.check(
        &format!(
            "and reaping it spends the number too ({} -> {})",
            zombie,
            state_of(zombie)
        ),
        state_of(zombie) == "ENOENT",
    );

    // Let the helper and its child go, so this program exits rather than leaving
    // two tasks for the runner to reap.
    unsafe { libc::write(to_helper[1], b"x".as_ptr().cast(), 1) };
    unsafe { libc::kill(stranger, libc::SIGKILL) };
    unsafe { libc::waitpid(helper, &mut status, 0) };
    report.finish()
}
