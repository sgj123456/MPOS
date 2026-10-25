//! t-kill-all - `kill(-1, sig)`: not a process group, but every **process** the
//! caller may signal - and one copy per process, which both group doors owe.
//!
//! The old answer here was the caller's own process group, which is a different
//! set entirely: a sibling that made itself a group leader of its own was
//! never reached. Every answer below was measured on a current host first
//! (`.workbuddy/probe_kill_neg1.c`, `.workbuddy/probe_kill_pgrp.c`, Linux
//! 7.2.11), with the probe standing as pid 1 of its own fresh pid namespace so
//! that "every process" could mean something small enough to observe:
//!
//! * `kill(-1, sig)` is `0` as soon as one process was a candidate, and the
//!   candidate set is every thread group with a number **above 1 in the
//!   caller's pid namespace**, minus the caller's own group;
//! * a sibling in a **different process group** receives it - the whole point
//!   of the fix - while the caller's own group does not;
//! * a *lone* caller gets `ESRCH`, for signal 0 and for a real signal alike,
//!   and its own handler does not run;
//! * a process in a **nested pid namespace** is not named at all: it has no
//!   number where the caller stands;
//! * one signal per process: a two-thread target runs its handler **once**,
//!   for `kill(-1)` and for `kill(-pgid)` alike (Linux puts one copy in the
//!   thread group's shared pending set).
//!
//! Every claim runs inside a fresh user+pid namespace, which is what makes this
//! program safe to host-run: an unguarded `kill(-1)` on a development host
//! would signal every process of the runner's user.
//!
//! The program names each child by the number **`fork` answered with**, which
//! is also the number a `wait` reports and `kill` accepts. That is worth saying
//! because this pin is what found those doors answering the scheduler's task
//! slot instead, and its first draft had to carry each child's own `getpid`
//! down a pipe to dodge it; with `docs/uapi/` item 107 landed, the answer is
//! usable directly. The comment would be a stale curiosity if the pin did not
//! depend on it, and it does.

use mpos_user_tests::Report;
use std::sync::atomic::{AtomicI32, Ordering};

/// Deliveries seen by this process. A signal handler runs on whichever thread
/// the kernel picks, and a process's threads share this word - which is what
/// makes "one copy" measurable at all.
static DELIVERED: AtomicI32 = AtomicI32::new(0);

extern "C" fn count_signal(_sig: libc::c_int) {
    DELIVERED.fetch_add(1, Ordering::SeqCst);
}

fn errno() -> i32 {
    std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
}

/// `kill(2)` through libc: the raw door, and its `errno` is musl's because
/// this goes through musl's own wrapper.
fn kill(pid: i32, sig: i32) -> (isize, i32) {
    let ret = unsafe { libc::kill(pid as libc::pid_t, sig) } as isize;
    (ret, if ret < 0 { errno() } else { 0 })
}

fn install_handler() -> bool {
    unsafe {
        libc::signal(
            libc::SIGUSR1,
            count_signal as *const () as libc::sighandler_t,
        ) != libc::SIG_ERR
    }
}

/// The blocking `wait4(pid)`'s exit code - the child named by the number `fork`
/// answered with - saying on the console why a child that did not exit normally
/// did not.
fn wait_pid(child: i32) -> Option<i32> {
    let mut status: libc::c_int = 0;
    let got = unsafe { libc::waitpid(child, &mut status, 0) };
    if got != child {
        eprintln!("t-kill-all: waitpid({child}) = {got}, errno {}", errno());
        return None;
    }
    if libc::WIFEXITED(status) {
        Some(libc::WEXITSTATUS(status))
    } else {
        if libc::WIFSIGNALED(status) {
            eprintln!(
                "t-kill-all: child {child} died of signal {}",
                libc::WTERMSIG(status)
            );
        } else {
            eprintln!("t-kill-all: child {child} status {status:#x}");
        }
        None
    }
}

/// Write one line of a `uid_map`-shaped file.
fn write_proc(path: &str, text: &str) {
    use std::io::Write;
    if let Ok(mut file) = std::fs::File::create(path) {
        let _ = file.write_all(text.as_bytes());
    }
}

/// Run `body` as **pid 1 of a fresh pid namespace**, answering the code it
/// chose.
///
/// Two calls, in this order for the reason the namespace tests use: a user
/// namespace first, because only the owner of a pid namespace may make one;
/// then a pid namespace, which by design renumbers *children* - so the process
/// this helper forks is its pid 1 and `body` runs there. `None` is the helper's
/// own failure (it cannot build the namespace), never a claim.
fn in_fresh_pid_ns(body: fn() -> i32) -> Option<i32> {
    match unsafe { libc::fork() } {
        -1 => None,
        0 => {
            if unsafe { libc::unshare(libc::CLONE_NEWUSER) } != 0 {
                unsafe { libc::_exit(90) }
            }
            let uid = unsafe { libc::getuid() };
            let gid = unsafe { libc::getgid() };
            write_proc("/proc/self/setgroups", "deny\n");
            write_proc("/proc/self/uid_map", &format!("0 {uid} 1\n"));
            write_proc("/proc/self/gid_map", &format!("0 {gid} 1\n"));
            if unsafe { libc::unshare(libc::CLONE_NEWPID) } != 0 {
                unsafe { libc::_exit(91) }
            }
            match unsafe { libc::fork() } {
                -1 => unsafe { libc::_exit(92) },
                0 => unsafe { libc::_exit(body()) },
                kid => {
                    let code = wait_pid(kid).unwrap_or(93);
                    unsafe { libc::_exit(code) }
                }
            }
        }
        kid => wait_pid(kid),
    }
}

/// A namespace's own init has nobody to signal: the set is empty, so both
/// shapes answer `ESRCH` - and nothing is delivered to the caller itself.
fn body_lone_caller() -> i32 {
    if unsafe { libc::getpid() } != 1 {
        return 10;
    }
    if !install_handler() {
        return 11;
    }
    // The control the rest of the file is built on: a handler really runs for
    // this process's own SIGUSR1 - so a later "nothing arrived" is about the
    // set and not about the disposition machinery. A plain pid, not 0: the
    // caller's process group is shared with tasks outside this namespace (the
    // test process that forked us), and a group send would reach them too.
    DELIVERED.store(0, Ordering::SeqCst);
    let me = unsafe { libc::getpid() };
    let (own_ret, _) = kill(me, libc::SIGUSR1);
    let own_seen = DELIVERED.load(Ordering::SeqCst);
    eprintln!("t-kill-all: self kill({me},SIGUSR1) = {own_ret}, delivered {own_seen}");
    if own_ret != 0 || own_seen != 1 {
        return 15;
    }
    // The control has had its one delivery; the claims below start from zero.
    DELIVERED.store(0, Ordering::SeqCst);
    let (ret0, err0) = kill(-1, 0);
    let (ret1, err1) = kill(-1, libc::SIGUSR1);
    let seen = DELIVERED.load(Ordering::SeqCst);
    if ret0 != -1 || err0 != libc::ESRCH {
        return 12;
    }
    if ret1 != -1 || err1 != libc::ESRCH {
        return 13;
    }
    if seen != 0 {
        return 14;
    }
    0
}

/// The set is every process, not a group: a sibling in a process group of its
/// own receives it, and the caller's own group does not.
fn body_scope() -> i32 {
    if !install_handler() {
        return 20;
    }
    DELIVERED.store(0, Ordering::SeqCst);
    let kid = unsafe { libc::fork() };
    if kid == 0 {
        unsafe { libc::setpgid(0, 0) };
        for _ in 0..400 {
            if DELIVERED.load(Ordering::SeqCst) != 0 {
                break;
            }
            unsafe { libc::usleep(5000) };
        }
        let seen = DELIVERED.load(Ordering::SeqCst);
        unsafe { libc::_exit(if seen == 1 { 0 } else { 3 }) }
    }
    if kid < 0 {
        return 21;
    }
    // `kid` is the number `fork` answered with - the sibling's number in this
    // namespace, which is what `kill` accepts and what `wait` reports.
    unsafe { libc::usleep(150_000) };
    let (here, _) = kill(kid, 0);
    if here != 0 {
        return 22;
    }
    let (ret, ret_err) = kill(-1, libc::SIGUSR1);
    let own = DELIVERED.load(Ordering::SeqCst);
    let kid_code = wait_pid(kid);
    eprintln!(
        "t-kill-all: scope kill(-1) = {ret} (errno {ret_err}), own {own}, sibling (pid {kid}) {kid_code:?}"
    );
    if ret != 0 {
        return 23;
    }
    if own != 0 {
        return 24;
    }
    if kid_code != Some(0) {
        return 25;
    }
    0
}

/// One signal per **process**: a two-thread target runs its handler once,
/// whichever door named it - `kill(-1)` or `kill(-pgid)`.
fn one_copy(via_all: bool) -> i32 {
    let kid = unsafe { libc::fork() };
    if kid == 0 {
        unsafe { libc::setpgid(0, 0) };
        if !install_handler() {
            unsafe { libc::_exit(2) }
        }
        DELIVERED.store(0, Ordering::SeqCst);
        // A second thread of this process, and **runnable**: it sleeps in
        // short takes so a copy aimed at this thread is delivered and counted
        // too. A parked thread would hold a per-thread copy pending until this
        // process left, which is exactly the shape that would hide a fan-out
        // bug from the claim.
        let _worker = std::thread::spawn(|| {
            loop {
                unsafe { libc::usleep(1000) };
            }
        });
        for _ in 0..400 {
            if DELIVERED.load(Ordering::SeqCst) != 0 {
                break;
            }
            unsafe { libc::usleep(5000) };
        }
        // A grace period after the first delivery: a per-thread fan-out's
        // second copy arrives in its own thread within a tick or two, and a
        // count read at the first delivery would miss it.
        for _ in 0..40 {
            unsafe { libc::usleep(5000) };
        }
        let seen = DELIVERED.load(Ordering::SeqCst);
        // `3` is "none", so a zero-delivery run cannot pass for a one-copy one.
        unsafe {
            libc::_exit(if seen == 1 {
                0
            } else if seen == 0 {
                3
            } else {
                seen
            })
        }
    }
    if kid < 0 {
        return 30;
    }
    unsafe { libc::usleep(200_000) };
    let (ret, _) = if via_all {
        kill(-1, libc::SIGUSR1)
    } else {
        kill(-kid, libc::SIGUSR1)
    };
    let kid_code = wait_pid(kid);
    if ret != 0 {
        return 31;
    }
    match kid_code {
        Some(0) => 0,
        Some(seen) => 40 + seen,
        None => 32,
    }
}

fn body_one_copy() -> i32 {
    let all = one_copy(true);
    if all != 0 {
        return all;
    }
    one_copy(false)
}

/// A process in a **nested** pid namespace is not named where the caller
/// stands: its default `SIGUSR1` action would end it if the set reached it.
fn body_nested_ns() -> i32 {
    if !install_handler() {
        return 50;
    }
    let outer = unsafe { libc::fork() };
    if outer == 0 {
        unsafe { libc::setpgid(0, 0) };
        if unsafe { libc::unshare(libc::CLONE_NEWPID) } != 0 {
            unsafe { libc::_exit(2) }
        }
        let inner = unsafe { libc::fork() };
        if inner == 0 {
            unsafe {
                libc::signal(libc::SIGUSR1, libc::SIG_DFL);
                loop {
                    libc::pause();
                }
            }
        }
        if inner < 0 {
            unsafe { libc::_exit(3) }
        }
        // Long enough that the caller's `kill(-1)` (below, at 350 ms) lands
        // while this process is still here: a set question asked after the
        // process left answers `ESRCH`, which is a race and not a claim.
        unsafe { libc::usleep(900_000) };
        // `inner` is the number `fork` answered with, so it is nameable here
        // even though it is the init of a namespace of its own.
        let alive = unsafe { libc::kill(inner, 0) } == 0;
        if alive {
            // `SIGKILL` from an ancestor namespace is forced through an init,
            // and this namespace's own init (our parent) is not an ancestor.
            unsafe {
                libc::kill(inner, libc::SIGKILL);
                let mut status: libc::c_int = 0;
                libc::waitpid(inner, &mut status, 0);
            }
        }
        unsafe { libc::_exit(if alive { 0 } else { 4 }) }
    }
    if outer < 0 {
        return 51;
    }
    unsafe { libc::usleep(350_000) };
    let (ret, _) = kill(-1, libc::SIGUSR1);
    let outer_code = wait_pid(outer);
    if ret != 0 {
        return 52;
    }
    match outer_code {
        Some(0) => 0,
        Some(failed) => 60 + failed,
        None => 53,
    }
}

fn main() {
    println!("t-kill-all");
    let mut r = Report::new();

    let lone = in_fresh_pid_ns(body_lone_caller);
    r.check(
        &format!("a lone caller's kill(-1) is ESRCH, and reaches nobody (got {lone:?})"),
        lone == Some(0),
    );

    let scope = in_fresh_pid_ns(body_scope);
    r.check(
        &format!(
            "kill(-1) reaches a sibling in another process group, and not the caller's own (got {scope:?})"
        ),
        scope == Some(0),
    );

    let copies = in_fresh_pid_ns(body_one_copy);
    r.check(
        &format!(
            "one copy per process: a two-thread target's handler runs once for kill(-1) and once for kill(-pgid) (got {copies:?})"
        ),
        copies == Some(0),
    );

    let nested = in_fresh_pid_ns(body_nested_ns);
    r.check(
        &format!("a process in a nested pid namespace is not named by kill(-1) (got {nested:?})"),
        nested == Some(0),
    );

    r.finish()
}
