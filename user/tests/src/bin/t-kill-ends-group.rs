//! t-kill-ends-group — a fatal signal ends the **whole thread group**, not only
//! the thread it was delivered to.
//!
//! `timeout(1)` kills a hung program with `SIGKILL` and then waits for it. On a
//! host Linux that always returns, however the program's other threads are
//! parked: `complete_signal` sets `SIGNAL_GROUP_EXIT` for a signal with no
//! handler whose default action terminates, and every thread is ended with the
//! group. This program is the smallest shape of that: the killed process has a
//! second thread parked in a futex, and the parent's `wait` has to hear the
//! death instead of parking on a group that cannot die.
//!
//! Measured on Linux 7.2.11 before it was pinned: the same fixture reaps the
//! child with `WIFSIGNALED`/`SIGKILL` within milliseconds. Without the rule the
//! parent parks in `wait4` forever — which is what this program catches with an
//! alarm, because a hang is not an answer this pin can report.
//!
//! The worker parks in `FUTEX_WAIT` on a word nobody will wake: that is the
//! state a signal cannot be delivered at a syscall-exit gate from, so it is the
//! one that shows whether group death is a group fact or a per-thread one.

use mpos_user_tests::Report;

use std::process::Command;

/// A worker thread that parks in `FUTEX_WAIT` forever, on a word of its own.
fn spawn_parker() -> std::thread::JoinHandle<()> {
    std::thread::spawn(|| {
        use std::sync::atomic::AtomicU32;
        // Leaked on purpose: the thread never returns, and the address has to
        // outlive this function's frame.
        let word: &'static AtomicU32 = Box::leak(Box::new(AtomicU32::new(7)));
        let address = word.as_ptr() as usize;
        loop {
            // `FUTEX_WAIT | FUTEX_PRIVATE_FLAG`, expected 7, no deadline: one
            // uninterrupted sleep, which is the state the fix has to reach.
            let rc = unsafe {
                libc::syscall(
                    libc::SYS_futex,
                    address as *mut u32,
                    128 | 0,
                    7u32,
                    0usize,
                    0usize,
                    0usize,
                )
            };
            // `EAGAIN`/`EINTR` would mean the value moved or a signal; neither
            // happens here, so a return is only the guard for the compiler.
            if rc == -1 {
                std::thread::sleep(std::time::Duration::from_millis(200));
            }
        }
    })
}

/// The process: one parked worker, then a leader that waits to be killed.
fn parked_process() -> ! {
    let _worker = spawn_parker();
    std::thread::sleep(std::time::Duration::from_millis(300));
    // The leader stays alive until the parent's signal arrives.
    loop {
        std::thread::sleep(std::time::Duration::from_secs(5));
    }
}

/// The same process, but the leader kills **itself** from inside a syscall.
///
/// This is the second way to die of a fatal signal, and it takes a different
/// road through the kernel. The kill above arrives from outside, so the sender
/// wakes the target and the signal is taken inside a blocking wait - the path
/// where taking a fatal signal ends the group. A self-kill is still pending
/// when `kill` returns, so it is taken at that call's **own exit gate**
/// (`linux::proc::deliver_signal`), which is a second place a fatal default
/// action ends a task. Both are "a signal with no handler whose default action
/// terminates", so both owe the group the same answer; this fixture is the one
/// that asks whether they give it.
///
/// `pause` is deliberately not reached: a leader parked in `pause` would be
/// woken by its own pending signal and take it inside that wait, which is the
/// path the first fixture already covers, so the observed death would say
/// nothing about the gate. What is left below is a *timed* sleep whose every
/// wake re-checks the pending set - a fallback, not the claim.
fn self_killed_process() -> ! {
    let _worker = spawn_parker();
    std::thread::sleep(std::time::Duration::from_millis(300));
    // SAFETY: no pointer arguments. On a kernel that takes the signal at this
    // call's gate the return never happens, so the code after it is only the
    // guard for the compiler and for a kernel that defers the death.
    unsafe { libc::kill(libc::getpid(), libc::SIGKILL) };
    loop {
        std::thread::sleep(std::time::Duration::from_secs(5));
    }
}

/// The same process again, but the leader **faults** instead of being signalled.
///
/// The third way to die of a fatal signal, and the one that never passes through
/// the signal machinery at all in this kernel: a faulting instruction cannot
/// wait for a syscall boundary, so the machine's own fault handler asks the
/// disposition and ends the process where it faulted. That is a third place a
/// fatal default action ends a task, and it owes the group the same answer - on
/// Linux a `SIGSEGV` with no handler kills the *process*, so its parked thread
/// goes with it and the parent's `wait` reads `WIFSIGNALED`/`SIGSEGV`.
fn faulted_process() -> ! {
    let _worker = spawn_parker();
    std::thread::sleep(std::time::Duration::from_millis(300));
    // A `std` program does not start with `SIG_DFL` for `SIGSEGV`: the runtime
    // installs its stack-overflow guard, which is a handler. Restoring the
    // default is what makes this a *fatal* default action, and it is also what
    // an ordinary C program or a shell would have without asking for one.
    // SAFETY: no pointer argument; `SIG_DFL` is the value the kernel starts with.
    unsafe { libc::signal(libc::SIGSEGV, libc::SIG_DFL) };
    // An address nothing maps, kept opaque so the compiler cannot fold the
    // store away or turn a known-null write into a trap instruction: the pin is
    // about the machine's page fault, not about a guard the optimiser inserted.
    let bad = std::hint::black_box(0xdead_0000 as *mut u8);
    // SAFETY: this store is the whole test. It must fault.
    unsafe { std::ptr::write_volatile(bad, 1) };
    loop {
        std::thread::sleep(std::time::Duration::from_secs(5));
    }
}

/// Spawn one fixture, wait for its worker to park, and report what became of it.
///
/// `kill_it` is the difference between the two shapes: `Some(sig)` sends that
/// signal from **here**, after the park, which is the outside-kill path;
/// `None` leaves the fixture to kill itself, which is the syscall-gate path.
/// Returns whether the death was reaped and which signal the kernel named as the
/// cause (`None` when it exited by code, or never answered), so `main` can turn
/// each into its own claim.
fn run_fixture(arg: &str, kill_it: Option<libc::c_int>) -> Option<(bool, Option<libc::c_int>)> {
    let child = Command::new(std::env::current_exe().expect("own path"))
        .arg(arg)
        .spawn();

    let mut child = child.ok()?;

    // Let the worker take its place in the futex before the kill, so the kill
    // meets the state this pin is about - on either path.
    std::thread::sleep(std::time::Duration::from_millis(700));

    let pid = child.id() as libc::pid_t;
    if let Some(sig) = kill_it {
        let _ = unsafe { libc::kill(pid, sig) };
    }

    // Poll with `WNOHANG` rather than block: the failure shape is a `wait`
    // that never returns, and a hang is not an answer this pin can report.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let mut reaped = false;
    let mut killed_by = None;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                use std::os::unix::process::ExitStatusExt;
                reaped = true;
                killed_by = status.signal();
                break;
            }
            Ok(None) => {
                if std::time::Instant::now() >= deadline {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(25));
            }
            Err(_) => break,
        }
    }
    if !reaped {
        // Leave nothing behind: the group is still parked, so it goes with the
        // machine rather than with this program's exit.
        let _ = unsafe { libc::kill(pid, libc::SIGKILL) };
    }
    Some((reaped, killed_by))
}

fn main() -> ! {
    // The spawn above re-executes this binary with the one word that makes it
    // the fixture rather than the test.
    match std::env::args().nth(1).as_deref() {
        Some("child") => parked_process(),
        Some("selfkill") => self_killed_process(),
        Some("fault") => faulted_process(),
        _ => {}
    }

    let mut report = Report::new();

    // Shape one: the signal arrives from **outside** while the leader is parked
    // in a timed sleep, so it is taken inside that wait.
    let child = run_fixture("child", Some(libc::SIGKILL));
    report.check("the fixture process starts", child.is_some());
    let (reaped, killed_by) = child.unwrap_or((false, None));
    report.check(
        "the process is reaped even though a thread was parked in a futex",
        reaped,
    );
    report.check(
        "the death is the signal that killed it",
        killed_by == Some(libc::SIGKILL),
    );

    // Shape two: the leader kills itself, so the death is taken at the `kill`
    // call's own exit gate rather than inside a wait. Same group, same question.
    let selfkill = run_fixture("selfkill", None);
    report.check(
        "the self-killing fixture process starts",
        selfkill.is_some(),
    );
    let (reaped, killed_by) = selfkill.unwrap_or((false, None));
    report.check(
        "a process that kills itself is reaped though its thread is parked in a futex",
        reaped,
    );
    report.check(
        "a self-kill is still the signal that is reported as the cause",
        killed_by == Some(libc::SIGKILL),
    );

    // Shape three: the leader takes a fault with `SIG_DFL`, so the machine's own
    // fault handler ends it - no syscall gate, no blocking wait, and the same
    // question about the parked sibling.
    let faulted = run_fixture("fault", None);
    report.check("the faulting fixture process starts", faulted.is_some());
    let (reaped, killed_by) = faulted.unwrap_or((false, None));
    report.check(
        "a process whose thread faults is reaped though its thread is parked in a futex",
        reaped,
    );
    report.check(
        "the death is the fault that thread took",
        killed_by == Some(libc::SIGSEGV),
    );

    report.finish()
}
