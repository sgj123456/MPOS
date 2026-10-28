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

fn main() -> ! {
    // The spawn above re-executes this binary with the one word that makes it
    // the fixture rather than the test.
    if std::env::args().nth(1).as_deref() == Some("child") {
        parked_process();
    }

    let mut report = Report::new();

    let child = Command::new(std::env::current_exe().expect("own path"))
        .arg("child")
        .spawn();

    let Ok(mut child) = child else {
        report.check("the fixture process starts", false);
        report.finish()
    };
    report.check("the fixture process starts", true);

    // Let the worker take its place in the futex before the kill, so the kill
    // meets the state this pin is about.
    std::thread::sleep(std::time::Duration::from_millis(700));

    let pid = child.id();
    let killed = unsafe { libc::kill(pid as libc::pid_t, libc::SIGKILL) };
    report.check("SIGKILL to the process is accepted", killed == 0);

    // Poll with `WNOHANG` rather than block: the failure shape is a `wait`
    // that never returns, and a hang is not an answer this pin can report.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let mut reaped = false;
    let mut signalled = false;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                use std::os::unix::process::ExitStatusExt;
                reaped = true;
                signalled = status.signal() == Some(libc::SIGKILL);
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
    report.check(
        "the process is reaped even though a thread was parked in a futex",
        reaped,
    );
    report.check("the death is the signal that killed it", signalled);
    if !reaped {
        // Leave nothing behind: the group is still parked, so it goes with the
        // machine rather than with this program's exit.
        let _ = unsafe { libc::kill(pid as libc::pid_t, libc::SIGKILL) };
    }
    report.finish()
}
