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
//! The handler counts with an atomic because a signal handler may not lock; the
//! counter is process-private, so a forked child reads its own copy and a
//! process's threads share one.

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

/// Reap the one child this process has and answer its exit code, saying on the
/// console why a child that did not exit normally did not.
///
/// `waitpid(-1)` and not `waitpid(number)`: inside a pid namespace this kernel
/// answers the *machine's* slot for a wait's reported pid (and for `fork`'s
/// return) where Linux answers the caller's own number - a divergence this
/// program found and `docs/notes/` §2.10 names, with the item that fixes it.
/// A claim about *which processes a signal reaches* must not be built on the
/// number a wait prints, so the child is reaped by "any" and the exit code is
/// the whole answer.
fn reap_any() -> Option<i32> {
    let mut status: libc::c_int = 0;
    let got = unsafe { libc::waitpid(-1, &mut status, 0) };
    if got < 0 {
        eprintln!("t-kill-all: waitpid(-1) = {got}, errno {}", errno());
        return None;
    }
    if libc::WIFEXITED(status) {
        Some(libc::WEXITSTATUS(status))
    } else {
        if libc::WIFSIGNALED(status) {
            eprintln!(
                "t-kill-all: a child (pid {got}) died of signal {}",
                libc::WTERMSIG(status)
            );
        } else {
            eprintln!("t-kill-all: a child (pid {got}) status {status:#x}");
        }
        None
    }
}

/// Tell the parent this process's own number. The child's `getpid()` is the
/// number the caller's namespace knows it by - `kill` and `/proc` agree with
/// it - whereas **`fork`'s return value inside a pid namespace is this
/// kernel's machine slot**, a divergence `docs/notes/` §2.10 names rather than
/// builds a claim on.
fn announce(write_end: libc::c_int) {
    let me = unsafe { libc::getpid() };
    let bytes = me.to_ne_bytes();
    unsafe {
        libc::write(
            write_end,
            bytes.as_ptr() as *const libc::c_void,
            bytes.len(),
        );
        libc::close(write_end);
    }
}

/// The number a child announced, if the pipe carried it.
fn learn(read_end: libc::c_int) -> Option<i32> {
    let mut bytes = [0u8; 4];
    let got = unsafe { libc::read(read_end, bytes.as_mut_ptr() as *mut libc::c_void, 4) };
    unsafe { libc::close(read_end) };
    if got != 4 {
        return None;
    }
    Some(i32::from_ne_bytes(bytes))
}

/// One pipe whose read end learns a child's number.
fn number_pipe() -> Option<(libc::c_int, libc::c_int)> {
    let mut fds = [0 as libc::c_int; 2];
    if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
        return None;
    }
    Some((fds[0], fds[1]))
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
                _kid => {
                    let code = reap_any().unwrap_or(93);
                    unsafe { libc::_exit(code) }
                }
            }
        }
        _kid => reap_any(),
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
    // set and not about the disposition machinery.
    DELIVERED.store(0, Ordering::SeqCst);
    // `kill(pid)` and not `kill(0)`: a process group is shared with tasks
    // outside this namespace (the test process that forked us), and a group
    // send would reach them too.
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
    let Some((read_end, write_end)) = number_pipe() else {
        return 21;
    };
    let kid = unsafe { libc::fork() };
    if kid == 0 {
        unsafe { libc::close(read_end) };
        unsafe { libc::setpgid(0, 0) };
        if !install_handler() {
            unsafe { libc::_exit(2) }
        }
        announce(write_end);
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
        return 22;
    }
    unsafe { libc::close(write_end) };
    let Some(number) = learn(read_end) else {
        return 23;
    };
    unsafe { libc::usleep(150_000) };
    // The number the caller stands in names the sibling, and the sibling is
    // in a process group of its own - so a group send would not reach it.
    let (here, _) = kill(number, 0);
    if here != 0 {
        return 24;
    }
    let (ret, ret_err) = kill(-1, libc::SIGUSR1);
    let own = DELIVERED.load(Ordering::SeqCst);
    let kid_code = reap_any();
    eprintln!(
        "t-kill-all: scope kill(-1) = {ret} (errno {ret_err}), own {own}, sibling (pid {number}) {kid_code:?}"
    );
    if ret != 0 {
        return 25;
    }
    if own != 0 {
        return 26;
    }
    if kid_code != Some(0) {
        return 27;
    }
    0
}

/// One signal per **process**: a two-thread target runs its handler once,
/// whichever door named it - `kill(-1)` or `kill(-pgid)`.
fn one_copy(via_all: bool) -> i32 {
    let Some((read_end, write_end)) = number_pipe() else {
        return 30;
    };
    let kid = unsafe { libc::fork() };
    if kid == 0 {
        unsafe { libc::close(read_end) };
        unsafe { libc::setpgid(0, 0) };
        if !install_handler() {
            unsafe { libc::_exit(2) }
        }
        DELIVERED.store(0, Ordering::SeqCst);
        // A second thread of this process, and **runnable**: it sleeps in
        // short takes so a copy aimed at this thread is delivered and counted
        // too. A parked thread would hold a per-thread copy pending until
        // this process left, which is exactly the shape that would hide a
        // fan-out bug from the claim.
        let _worker = std::thread::spawn(|| {
            loop {
                unsafe { libc::usleep(1000) };
            }
        });
        announce(write_end);
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
        return 31;
    }
    unsafe { libc::close(write_end) };
    let Some(number) = learn(read_end) else {
        return 32;
    };
    unsafe { libc::usleep(200_000) };
    let (ret, _) = if via_all {
        kill(-1, libc::SIGUSR1)
    } else {
        kill(-number, libc::SIGUSR1)
    };
    let kid_code = reap_any();
    if ret != 0 {
        return 33;
    }
    match kid_code {
        Some(0) => 0,
        Some(seen) => 40 + seen,
        None => 34,
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
    let Some((read_end, write_end)) = number_pipe() else {
        return 51;
    };
    let outer = unsafe { libc::fork() };
    if outer == 0 {
        unsafe {
            libc::close(read_end);
            libc::setpgid(0, 0);
        }
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
        announce(write_end);
        // Long enough that the caller's `kill(-1)` (below, at 350 ms) lands
        // while this process is still here: a set question asked after the
        // process left answers `ESRCH`, which is a race and not a claim.
        unsafe { libc::usleep(900_000) };
        // Alive or not is read off the *reap*, not off `kill(inner, 0)`: the
        // number `fork` answered with is this kernel's machine slot (the
        // divergence the helper's header names), and a lookup by it would
        // read "gone" for a child that is very much there. `WNOHANG` answers
        // 0 while a child is alive and reaps it if it is not, which is the
        // question without a number in it.
        let mut status: libc::c_int = 0;
        let reaped = unsafe { libc::waitpid(-1, &mut status, libc::WNOHANG) };
        let alive = reaped == 0;
        if alive {
            unsafe {
                // Ending it by *set* and not by number: the number `fork`
                // answered with names nothing the outer can use. `SIGKILL`
                // from an ancestor namespace is forced through an init, and
                // this namespace's own init (our parent) is not an ancestor,
                // so it is discarded there - which is Linux's rule too.
                libc::kill(-1, libc::SIGKILL);
                libc::waitpid(-1, &mut status, 0);
            }
        }
        unsafe { libc::_exit(if alive { 0 } else { 4 }) }
    }
    if outer < 0 {
        return 52;
    }
    unsafe { libc::close(write_end) };
    let Some(number) = learn(read_end) else {
        return 53;
    };
    // The outer process is nameable where the caller stands, even though its
    // own child is not: the one relation `kill(-1)` is built on.
    let (here, _) = kill(number, 0);
    eprintln!("t-kill-all: nested outer pid {number}, kill(pid,0) = {here}");
    if here != 0 {
        return 56;
    }
    unsafe { libc::usleep(350_000) };
    let (ret, _) = kill(-1, libc::SIGUSR1);
    let outer_code = reap_any();
    if ret != 0 {
        return 54;
    }
    match outer_code {
        Some(0) => 0,
        Some(failed) => 60 + failed,
        None => 55,
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
