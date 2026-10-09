//! The `syscall` suite: the dispatch gate, entered the way ring 3 enters it.
//!
//! A child of `syscall` and compiled only into a test build. It is here rather
//! than beside `vfs` because that is where the edge points: a suite inside `vfs`
//! that calls [`crate::syscall::linux_dispatch`] closes a `vfs <-> syscall` mutual
//! pair the module graph does not allow, and the measurement said so the first
//! time it was tried (`docs/NOTES.md` §15.22 - "vfs names crate::syscall:: 1 times,
//! up from the recorded 0", and "9 mutual pairs, up from the recorded 8"). The
//! cases and the helper below were written in the `vfs` family and never belonged
//! to it.
//!
//! The tty and the signal path join these cases when the last family moves out of
//! the wiring layer's checks directory.

use alloc::string::String;

use crate::case_check;
use crate::kunit::{Case, Phase, Suite, record_verdict, take_verdict};
use crate::println;
use crate::sched::Priority;
use crate::syscall::{self, err, linux::nr as lnr};

/// The suite this module contributes to the boot's self-tests.
///
/// `Phase::Vfs` because that is the boot step this point in the sequence needs:
/// the gate's cases call descriptors and the scheduler, both of which are up by
/// then, and they have run there since they were written.
pub const SUITE: Suite = Suite {
    name: "syscall",
    phase: Phase::Vfs,
    cases: &[
        Case {
            name: "gate",
            run: gate,
        },
        Case {
            name: "dispatch_in_task",
            run: dispatch_in_task,
        },
    ],
};

/// The slot this suite's task stores its verdict in.
///
/// The slots are the shared vocabulary's ([`crate::kunit`]), so two suites that
/// both spawn a task cannot collide - and none of them can be read by the wrong
/// case.
const VERDICT_SLOT: usize = 2;

/// One entry into the dispatch table, as a case's helper.
///
/// The six arguments spelled out rather than taken as an array because the family
/// that follows this one out of the checks directory calls it that way, and it
/// borrows this helper until it lands here (`docs/NOTES.md` §15.22).
pub unsafe fn syscall(
    nr: u64,
    a1: usize,
    a2: usize,
    a3: usize,
    a4: usize,
    a5: usize,
    a6: usize,
) -> isize {
    unsafe { syscall::linux_dispatch(nr, [a1, a2, a3, a4, a5, a6], false, core::ptr::null_mut()) }
}

/// The gate itself: an unknown number, a zero-length read, bad descriptors, and a
/// write whose count the dispatcher must not drop.
fn gate(_info: &crate::boot::BootInfo) -> Result<(), String> {
    // A number beyond the table: 1024 is CLEANUP_AFTER_KTHREAD.
    case_check!(
        unsafe { syscall(1024, 0, 0, 0, 0, 0, 0) } == err::ENOSYS,
        "a syscall number beyond the table was not ENOSYS"
    );
    case_check!(
        unsafe { syscall(lnr::READ, 0, 0, 0, 0, 0, 0) } == 0,
        "a zero-length read is empty, not an error"
    );
    case_check!(
        unsafe { syscall(lnr::READ, 3, 0, 0, 0, 0, 0) } == err::EBADF,
        "a read on an unopened descriptor was not EBADF"
    );
    case_check!(
        unsafe { syscall(lnr::WRITE, 7, 0, 0, 0, 0, 0) } == err::EBADF,
        "a write on an unopened descriptor was not EBADF"
    );

    let msg = b"syscall: write through dispatch\n";
    let wrote = unsafe { syscall(lnr::WRITE, 1, msg.as_ptr() as usize, msg.len(), 0, 0, 0) };
    case_check!(
        wrote as usize == msg.len(),
        "a write through dispatch moved {wrote} of {} bytes",
        msg.len()
    );
    Ok(())
}

/// The same gate, driven from inside a real task: `getpid` answers that task's id,
/// yield and sleep suspend and resume it, and `exit_group` takes it down.
fn dispatch_in_task(_info: &crate::boot::BootInfo) -> Result<(), String> {
    crate::sched::spawn(
        || {
            let verdict = (|| -> Result<(), String> {
                let pid = unsafe { syscall(lnr::GETPID, 0, 0, 0, 0, 0, 0) };
                case_check!(
                    pid >= 2,
                    "getpid inside a task answered {pid}, which is not a task id"
                );

                let msg = b"syscall: write through dispatch in a spawned task\n";
                let wrote =
                    unsafe { syscall(lnr::WRITE, 1, msg.as_ptr() as usize, msg.len(), 0, 0, 0) };
                case_check!(
                    wrote as usize == msg.len(),
                    "dispatch dropped bytes: {wrote} of {}",
                    msg.len()
                );

                case_check!(
                    unsafe { syscall(lnr::SCHED_YIELD, 0, 0, 0, 0, 0, 0) } == 0,
                    "sched_yield through dispatch answered a failure"
                );

                // 2 ms, 1 byte, 0 padding - a request for the handler to exercise,
                // not one to time precisely.
                let req: [u64; 2] = [0, 2_000_000];
                case_check!(
                    unsafe { syscall(lnr::NANOSLEEP, req.as_ptr() as usize, 0, 0, 0, 0, 0) } == 0,
                    "a two-millisecond nanosleep through dispatch answered a failure"
                );
                Ok(())
            })();
            record_verdict(VERDICT_SLOT, verdict);
            unsafe { syscall(lnr::EXIT_GROUP, 0, 0, 0, 0, 0, 0) };
        },
        Priority::NORMAL,
    );
    crate::sched::kunit::drain();

    // `drain` waits for the task to be gone and this waits for it to have reached
    // an answer, so between them the claim "exit_group ended the task" is already
    // made - asserting the alive count again would compare `drain`'s own
    // postcondition with itself.
    take_verdict(VERDICT_SLOT, "the dispatch task")?;
    println!("M6_OK");
    Ok(())
}
