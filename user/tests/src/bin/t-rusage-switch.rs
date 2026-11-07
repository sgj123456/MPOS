//! `getrusage`'s two context-switch counters are counted.
//!
//! Linux's `struct rusage` has been the same 144 bytes for decades: two
//! `timeval`s of CPU, twelve counters of page faults, block I/O and IPC, and
//! then the pair a *scheduler* answers - `ru_nvcsw` (the times the task gave the
//! CPU up because it could not use it) and `ru_nivcsw` (the times the CPU was
//! taken while it was still runnable). This kernel wrote both as zero, beside a
//! CPU pair it had been counting for years, so a program that asked "did I block
//! or was I preempted" was told "neither, ever".
//!
//! Measured on Linux 7.2.14 (`build/scratch/rusage/switch.c`, musl static):
//! 200 `nanosleep(1ms)` calls moved `ru_nvcsw` by exactly +200 and `ru_nivcsw`
//! by +0, while 200 `sched_yield` calls moved them +0/+1 - the yield returns to
//! the same task, so it is one switch, and it lands in the *involuntary* column
//! because the task was runnable when it gave the CPU up. A hundred sleeps in a
//! joined thread moved the process's `RUSAGE_SELF` pair 0/0 -> 101/1 while that
//! thread's own `RUSAGE_THREAD` read 100/0 (`.workbuddy` sibling
//! `build/scratch/rusage/threadsw.c`), which is why the group has the totals and
//! the thread has its own.
//!
//! Every claim here is a **movement**, never a constant: a host's own switch
//! count at program start is nonzero (this container reads 0/0, but a shell's
//! child does not), so a claim that named a number would be a claim about the
//! machine that spawned the test.

use mpos_user_tests::Report;
use std::time::Duration;

/// The two counters, `(voluntary, involuntary)`.
#[derive(Clone, Copy, Debug)]
struct Switches {
    voluntary: i64,
    involuntary: i64,
}

/// One `getrusage` call for `who` (`RUSAGE_SELF` 0, `RUSAGE_THREAD` 1,
/// `RUSAGE_CHILDREN` -1). `None` when the call itself failed, so a claim cannot
/// silently pass off a missing reading.
fn switches(who: i32) -> Option<Switches> {
    // The record is built by hand because the two offsets are the claim: Linux
    // writes `ru_nvcsw` at 128 and `ru_nivcsw` at 136 of a 144-byte struct
    // (`struct rusage`'s own layout), and a test that took them through a
    // wrapper could not tell a wrong offset from a wrong count.
    let mut record = [0u8; 160];
    let rc = unsafe {
        libc::syscall(
            libc::SYS_getrusage,
            who as libc::c_long,
            record.as_mut_ptr() as *mut libc::c_void,
        )
    };
    if rc != 0 {
        return None;
    }
    let word = |at: usize| {
        let mut bytes = [0u8; 8];
        bytes.copy_from_slice(&record[at..at + 8]);
        i64::from_le_bytes(bytes)
    };
    Some(Switches {
        voluntary: word(128),
        involuntary: word(136),
    })
}

fn sleep_a_millisecond() {
    std::thread::sleep(Duration::from_millis(1));
}

fn main() {
    let mut r = Report::new();

    let start = switches(0);
    r.check(
        &format!("getrusage fills the two switch counters ({start:?})"),
        start.is_some_and(|s| s.voluntary >= 0 && s.involuntary >= 0),
    );
    let before = start.unwrap_or(Switches {
        voluntary: 0,
        involuntary: 0,
    });

    // A sleep is the shape of a voluntary switch: the task asks the scheduler
    // to run something else because it cannot use the CPU for a millisecond.
    // Linux answers one switch per sleep, so fifty sleeps are fifty.
    for _ in 0..50 {
        sleep_a_millisecond();
    }
    let after_sleeps = switches(0).unwrap_or(before);
    r.check(
        &format!(
            "fifty sleeps land fifty voluntary switches ({} -> {})",
            before.voluntary, after_sleeps.voluntary
        ),
        after_sleeps.voluntary - before.voluntary >= 50,
    );
    r.check(
        &format!(
            "and none of them is a switch the machine took ({})",
            after_sleeps.involuntary - before.involuntary
        ),
        after_sleeps.involuntary - before.involuntary <= 10,
    );

    // A yield is the other column: the task is still runnable and gives the
    // CPU away anyway, which is exactly the fact `ru_nivcsw` names (Linux
    // records +1 for 200 back-to-back yields because there was nothing else to
    // run; this kernel counts each hand-over, which is the same event).
    let before_yield = switches(0).unwrap_or(after_sleeps);
    for _ in 0..200 {
        unsafe { libc::sched_yield() };
    }
    let after_yield = switches(0).unwrap_or(before_yield);
    r.check(
        &format!(
            "a yield is an involuntary switch ({})",
            after_yield.involuntary - before_yield.involuntary
        ),
        after_yield.involuntary - before_yield.involuntary >= 1,
    );

    // A thread's own switches are its own; the process's are the group's. A
    // worker that sleeps fifty times must not show in this thread's
    // `RUSAGE_THREAD`, and must show in the process's `RUSAGE_SELF`.
    let self_before = switches(0).unwrap_or(after_yield);
    let (sender, receiver) = std::sync::mpsc::channel();
    let worker = std::thread::spawn(move || {
        for _ in 0..50 {
            sleep_a_millisecond();
        }
        let own = switches(1).map(|s| s.voluntary);
        let _ = sender.send(own);
    });
    let own = receiver.recv().ok().flatten();
    let _ = worker.join();
    let self_after = switches(0).unwrap_or(self_before);
    r.check(
        &format!("a sibling's sleeps are the process's ({own:?} in that thread)"),
        own.is_some_and(|value| value >= 40),
    );
    r.check(
        &format!(
            "and are not this thread's ({:?} -> {:?})",
            self_before.voluntary, self_after.voluntary
        ),
        self_after.voluntary - self_before.voluntary >= 1,
    );

    // A waited child's switches fold into the parent's child totals, the way
    // its CPU does (`cutime`/`cstime`), because `RUSAGE_CHILDREN` is the
    // process's own children and the counters go with the dead child's slot.
    let children_before = switches(-1).unwrap_or(Switches {
        voluntary: 0,
        involuntary: 0,
    });
    let child = unsafe { libc::fork() };
    if child == 0 {
        for _ in 0..50 {
            sleep_a_millisecond();
        }
        unsafe { libc::_exit(0) };
    }
    let mut status = 0i32;
    if child > 0 {
        unsafe { libc::waitpid(child, &mut status, 0) };
    }
    let children_after = switches(-1).unwrap_or(children_before);
    r.check(
        &format!(
            "a waited child's switches fold into the child totals ({} -> {})",
            children_before.voluntary, children_after.voluntary
        ),
        children_after.voluntary - children_before.voluntary >= 50,
    );

    r.finish();
}
