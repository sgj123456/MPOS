//! t-oom-kill — running the machine out of memory ends the process, not the
//! machine.
//!
//! Measured on Linux 7.2.11 before this was pinned (`docs/notes/` §8.0): a
//! process whose touch cannot get a page is killed with **`SIGKILL`** - its
//! parent's `wait` reads `WIFSIGNALED` / `WTERMSIG == 9` - and every other
//! process, and the machine, run on; the pages come back with its address
//! space. The two answers this kernel used before were both wrong shapes: a
//! `SIGSEGV` makes "the machine has no memory" indistinguishable from "you
//! dereferenced a wild pointer", and a panic ends the machine over one
//! process's memory use.
//!
//! The fixture is re-executed rather than forked, the way `t-kill-ends-group`
//! does it: the parent has to keep a single-threaded, small-memory shape so the
//! process that dies is the one the pin means. Two modes, both of which touch an
//! anonymous mapping until the machine has nothing left to hand them:
//!
//! - `exhaust` — one thread. The parent's claim is the whole Linux answer: a
//!   signal, `SIGKILL`, and memory still available afterwards.
//! - `group` — the touching happens on a *thread* while the leader sleeps. The
//!   parent's claim is the group rule: the leader must not survive its thread,
//!   or the parent would read an ordinary exit 0 after the leader's own sleep.
//!
//! The fixture asks the **machine** how much memory it has rather than carrying
//! a number: it takes fresh 128 MiB chunks and touches them, never unmapping,
//! until the death it is there to meet. One fixed mapping cannot do that job -
//! sized by guesswork it is either smaller than the machine (the fixture
//! finishes and the claim is vacuous, which is how this program first measured
//! `1900 MiB` against a machine holding more) or wasted work.
//!
//! Neither claim can be made on a host run: the host has more memory than this
//! fixture asks for, and killing a real Linux process there is not this pin's
//! business. The host run says so and skips, per the crate's rule.

use std::os::unix::process::ExitStatusExt;
use std::process::Command;
use std::ptr::null_mut;
use std::time::{Duration, Instant};

use mpos_user_tests::{Report, host_run};
use rustix::mm::{MapFlags, ProtFlags, mmap_anonymous, munmap};

/// The page this fixture touches one byte of.
const PAGE: usize = 4096;

/// How much fresh memory one round takes. Small enough that the last round is
/// the one that finds the machine's end rather than overshooting it.
const CHUNK: usize = 128 << 20;

/// Take and touch fresh chunks until the machine has no frame left to give the
/// next page of them. The chunks are never unmapped: the frames are the point.
fn exhaust() -> ! {
    loop {
        let Ok(base) = (unsafe {
            mmap_anonymous(
                null_mut(),
                CHUNK,
                ProtFlags::READ | ProtFlags::WRITE,
                MapFlags::PRIVATE,
            )
        }) else {
            // A mapping refused outright is a different answer from a fault that
            // could not get a page, and it would make the parent's claim true for
            // the wrong reason (the child *did* end). Exit on a number nobody
            // reads as a memory death, so the parent's signal claim fails.
            std::process::exit(3);
        };
        let mut address = base as usize;
        let end = address + CHUNK;
        while address < end {
            unsafe { (address as *mut u8).write_volatile(1) };
            address += PAGE;
        }
    }
}

/// A process whose *thread* is the one that runs the machine out of memory.
///
/// The leader sleeps well past the kill, so "the thread died and the leader lived"
/// is not a hang: it is an exit 0 the parent reaps and reports. A `SIGKILL` to the
/// group, which is Linux's answer and this kernel's, reaps the parent at once.
fn group_fixture() -> ! {
    std::thread::spawn(exhaust);
    std::thread::sleep(Duration::from_secs(22));
    std::process::exit(0);
}

/// Run the fixture and poll for its death, because a `wait` that never returns
/// is the failure shape this pin must report rather than hang on.
fn reap(mode: &str, patience: Duration, report: &mut Report, claim: &str) -> Option<i32> {
    let child = Command::new(std::env::current_exe().expect("own path"))
        .arg(mode)
        .spawn();
    let Ok(mut child) = child else {
        report.check(claim, false);
        return None;
    };
    let deadline = Instant::now() + patience;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let signal = status.signal();
                report.check(claim, status.success() || signal.is_some());
                return Some(signal.unwrap_or(0));
            }
            Ok(None) => {
                if Instant::now() >= deadline {
                    // Leave nothing behind: a group still holding the machine's
                    // memory would poison every program after this one in the lane.
                    let _ = unsafe { libc::kill(child.id() as libc::pid_t, libc::SIGKILL) };
                    let _ = child.wait();
                    report.check(claim, false);
                    return None;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(_) => {
                report.check(claim, false);
                return None;
            }
        }
    }
}

/// Take and free a few pages, so "the machine still has memory" is a
/// measurement rather than an absence of evidence.
fn memory_is_back() -> bool {
    const SOME: usize = 16 << 20;
    let Ok(base) = (unsafe {
        mmap_anonymous(
            null_mut(),
            SOME,
            ProtFlags::READ | ProtFlags::WRITE,
            MapFlags::PRIVATE,
        )
    }) else {
        return false;
    };
    let mut address = base as usize;
    while address < base as usize + SOME {
        unsafe { (address as *mut u8).write_volatile(7) };
        address += PAGE;
    }
    unsafe { munmap(base, SOME) }.is_ok()
}

fn main() -> ! {
    match std::env::args().nth(1).as_deref() {
        Some("exhaust") => exhaust(),
        Some("group") => group_fixture(),
        _ => {}
    }

    let mut report = Report::new();
    let on_host = host_run();

    // --- a process that touches past the machine's last frame is killed ---
    let signal = reap(
        "exhaust",
        Duration::from_secs(90),
        &mut report,
        "a process that runs the machine out of memory is ended",
    );
    // On the host the fixture either completes (more memory than `TOUCH`) or a
    // real OOM killer takes a process this pin does not own; neither is a claim
    // about this kernel, so the shape claim is made on the guest alone.
    report.check(
        "the death is the signal Linux's OOM killer sends",
        on_host || signal == Some(libc::SIGKILL),
    );

    // --- and the machine, and this process, are still there ---
    report.check(
        "the machine hands out memory again after that process died",
        on_host || memory_is_back(),
    );

    // --- the whole group dies, not only the thread that faulted ---
    //
    // This reads `WTERMSIG` rather than "the child is gone": a kernel that ended
    // only the touching thread would let the leader finish its sleep and exit 0,
    // which is the answer the `group` fixture is built to make distinguishable.
    let signal = reap(
        "group",
        Duration::from_secs(30),
        &mut report,
        "a process whose thread runs the machine out of memory is ended too",
    );
    report.check(
        "and its death is SIGKILL, not an ordinary exit after the leader's sleep",
        on_host || signal == Some(libc::SIGKILL),
    );
    report.check(
        "the machine still serves after the group death",
        on_host || memory_is_back(),
    );

    if on_host {
        println!("(host run: the two OOM claims are the guest's; this is Linux's shape, measured)");
    }
    report.finish()
}
