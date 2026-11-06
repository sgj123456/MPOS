//! t-steal - a death belongs to the parent that has it, not to whoever asks.
//!
//! The instrument behind `docs/notes/` §6.90 and `docs/uapi/` Item 203. It is a
//! **check**, not a reading: Linux's answer is the answer this kernel owes, and
//! it is measured on a current host by the same shape.
//!
//! The fault it pins: a process waited on a pid number that was never its child
//! and **collected that child's death**. The record of an exit outlives the task
//! table's slot, and the ownership question used to be asked of the *slot* -
//! "is it empty? then the pending record must be mine" - which is backwards,
//! because a slot is empty precisely *because* the record outlived its task.
//! The real parent was then left with `ECHILD` for a child it had buried, and a
//! stranger had its exit status.
//!
//! Measured, before the fix (`build/boot-steal1.log`):
//!
//! ```text
//! the helper's child is 7 (its record is pending, uncollected)
//! MY waitpid(7, WNOHANG) = 7 errno=0 (I TOOK SOMEONE ELSE'S DEATH) status=0x700
//! parent of it: waitpid(7) = -1 errno=10 status=0x0
//! ```
//!
//! and on Linux, for the same sequence (`.workbuddy`-style probe `steal.c`,
//! reproduced in `build/scratch/reapwait/`):
//!
//! ```text
//! MY waitpid(<the helper's child>, WNOHANG) = -1 errno=10 (ECHILD)
//! ```
//!
//! The fix is `Retired::parent` - the owning **group**, written when the death
//! is recorded and rewritten when the reparenting walk moves the corpse to an
//! adopter - and every reader of the record now asks it instead of the table.
//!
//! ## Why a helper process rather than a thread
//!
//! A thread of this program shares its group, and a death in the group is the
//! group's to collect - so a thread's child would *legitimately* be collectable
//! here and the claim would be about the wrong thing. The death has to belong to
//! somebody else entirely: a separate process, whose child this program has no
//! relationship with, and whose record is deliberately left uncollected.

use std::io::Read;

/// Whether `pid` answers as Linux's `kill(pid, 0)` would for a number this
/// program has no claim on: gone is the answer, present is the fault.
fn gone(pid: libc::pid_t) -> bool {
    let answer = unsafe { libc::kill(pid, 0) };
    answer == -1 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
}

/// Read one line from `fd` into `buffer`, returning how many bytes arrived.
fn read_line(fd: libc::c_int, buffer: &mut [u8]) -> usize {
    let read = unsafe { libc::read(fd, buffer.as_mut_ptr().cast(), buffer.len() as libc::size_t) };
    usize::try_from(read.max(0)).unwrap_or(0)
}

/// The whole fixture: a helper process forks a child that exits at once and
/// leaves it uncollected, tells this program its number over a pipe, and then
/// waits. This program asks for that child's death and must be refused.
fn main() {
    let mut report = mpos_user_tests::Report::new();

    let mut number_pipe = [0i32; 2];
    let mut done_pipe = [0i32; 2];
    if unsafe { libc::pipe(number_pipe.as_mut_ptr()) } != 0
        || unsafe { libc::pipe(done_pipe.as_mut_ptr()) } != 0
    {
        println!("FAIL:  the fixture could not make its pipes");
        std::process::exit(1);
    }

    let helper = unsafe { libc::fork() };
    if helper == 0 {
        // The helper. Its child exits immediately and is **not** collected until
        // this program has had its chance to try to take it.
        unsafe {
            libc::close(number_pipe[0]);
            libc::close(done_pipe[1]);
        }
        let child = unsafe { libc::fork() };
        if child == 0 {
            unsafe { libc::_exit(7) };
        }
        let text = format!("{child}");
        let _ = unsafe {
            libc::write(
                number_pipe[1],
                text.as_ptr().cast(),
                text.len() as libc::size_t,
            )
        };
        // Give the parent time to try to steal, and only then collect the death
        // - which is the point: the record is pending for the whole window.
        let mut byte = [0u8; 1];
        let _ = unsafe { libc::read(done_pipe[0], byte.as_mut_ptr().cast(), 1) };
        let mut status = 0i32;
        let collected = unsafe { libc::waitpid(child, &mut status, 0) };
        unsafe { libc::_exit(if collected == child { 0 } else { 1 }) };
    }

    unsafe {
        libc::close(number_pipe[1]);
        libc::close(done_pipe[0]);
    }
    let mut buffer = [0u8; 32];
    let read = read_line(number_pipe[0], &mut buffer);
    let child: libc::pid_t = std::str::from_utf8(&buffer[..read])
        .ok()
        .and_then(|text| text.trim().parse().ok())
        .unwrap_or(-1);
    report.check(
        &format!("the helper's child {child} exists and is not ours"),
        child > 0 && !gone(child),
    );

    // The claim. `WNOHANG` because a stealing wait that parked would hang the
    // program instead of answering, and the answer is the measurement.
    let mut status = 0i32;
    let stolen = unsafe { libc::waitpid(child, &mut status, libc::WNOHANG) };
    let stolen_errno = std::io::Error::last_os_error().raw_os_error().unwrap_or(0);
    report.check(
        &format!(
            "a wait for somebody else's uncollected child is ECHILD (waitpid {stolen}, errno {stolen_errno})"
        ),
        stolen == -1 && stolen_errno == libc::ECHILD,
    );

    // The far side of the same fact: the death is still on file for the parent
    // it belongs to. A steal would have consumed it, leaving this answer too.
    let _ = unsafe { libc::write(done_pipe[1], b"x".as_ptr().cast(), 1) };
    let mut helper_status = 0i32;
    let reaped = unsafe { libc::waitpid(helper, &mut helper_status, 0) };
    let collected_its_own =
        reaped == helper && libc::WIFEXITED(helper_status) && libc::WEXITSTATUS(helper_status) == 0;
    report.check(
        &format!(
            "and its own parent still collects it (helper exit {:?})",
            libc::WEXITSTATUS(helper_status)
        ),
        collected_its_own,
    );

    // A number that was never anybody's: the same answer, for a different
    // reason, and the shape `t-kslot` records as still open (§6.89).
    report.check(
        "a number this program never forked answers ESRCH",
        gone(1 << 20),
    );

    // The other half of the ownership question, on this side of the fence: a
    // child *of this program's own group* is collectable here, however it was
    // forked. Without this the claim above could be satisfied by refusing
    // everything.
    let mine = unsafe { libc::fork() };
    if mine == 0 {
        unsafe { libc::_exit(3) };
    }
    let mut mine_status = 0i32;
    let collected = unsafe { libc::waitpid(mine, &mut mine_status, 0) };
    report.check(
        &format!(
            "this program's own child is still collectable (waitpid {collected}, exit {:?})",
            libc::WEXITSTATUS(mine_status)
        ),
        collected == mine && libc::WIFEXITED(mine_status) && libc::WEXITSTATUS(mine_status) == 3,
    );

    let _ = std::fs::File::open("/proc/self/stat")
        .and_then(|mut file| {
            let mut text = String::new();
            file.read_to_string(&mut text).map(|_| text)
        })
        .map(|text| {
            // A `/proc` read of our own number still works: the ownership rule is
            // about *deaths*, not about looking a running task up (§6.89 leaves the
            // stale-number door open, and names it).
            let state = text
                .rsplit_once(')')
                .and_then(|(_, rest)| rest.split_whitespace().next())
                .unwrap_or("?")
                .to_string();
            report.check(
                &format!("and this program still reads its own /proc entry (state {state})"),
                state != "?" && !state.is_empty(),
            );
        });

    report.finish()
}
