//! t-ptrace-attach - naming a process is not the same as finding it.
//!
//! `ptrace` takes a pid the caller read from `fork`'s return, `getpid`, or a
//! `/proc` directory, and the kernel has to resolve that number back to a task.
//! Those are three different sources of the same word, and the one that is not
//! tested is the interesting one: a target the caller has no relationship with,
//! because that is what `strace -p <pid>` is - the only way to look at a process
//! that is already running, which is the whole reason anyone reaches for it.
//!
//! A kernel that resolves the pid to a task table slot by coincidence (a fork
//! that took the next free slot, so the number happens to match) works for its
//! own children and answers `ESRCH` for everything else. That reads exactly like
//! "ptrace is broken", and cost a debugging session believing the tracing tool
//! rather than the process it was pointed at - so both cases are pinned here,
//! with a nonexistent pid as the control: `ESRCH` must mean "no such process",
//! not "no such relation".
//!
//! The second half is what a detach gives back. Attaching stops the target with a
//! `SIGSTOP` the tracer brought, and detaching has to end *that* stop and only
//! that one: a process the user had stopped for itself stays stopped, while a
//! target that never reached its stop must not be handed back already frozen.
//! Both halves were measured against Linux before being pinned here, because the
//! difference between them is invisible until a `strace -p` leaves a shell
//! job hanging in mid-air.
//!
//! # The migration, and the one door the host would not open
//!
//! The named calls are `libc`'s (`ptrace` itself, `fork`, `kill`, `waitpid`,
//! `nanosleep` through `rustix`), with the requests and signals from
//! `libc::PTRACE_*`/`SIG*` - the no-std file carried `SYS_PTRACE = 101` and
//! the request words as literals, which is the x86_64 machine's memory of
//! them (`SYS_ptrace` is 117 on the second machine; this is exactly the
//! migration the `aarch64` sweep exists to catch). The orphan is built the
//! way the no-std file built it - a forked child forks in turn and exits,
//! leaving the grandchild reparented away - because pid 1 is a trap: an
//! attached init means nothing reaps and the machine stops moving.
//!
//! **What the host measured is a door that does not open at all.** The probe
//! (`.workbuddy/probe_ptrace.c`) cannot make one `ptrace` call on this
//! sandbox's host: the container's seccomp profile answers `EPERM` for *every*
//! request, before the pid is even resolved - attaching to the caller's own
//! child, which `yama`'s scope 1 would allow, fails the same way. So a host
//! run measures nothing about the door itself, and every `ptrace` claim
//! accepts that `EPERM` under `MPOS_ON_HOST`, named as such. What the probe
//! *did* measure is everything around the door, and it agrees with this
//! kernel on all of it: a user's `SIGSTOP` is reported through `WUNTRACED`
//! (`WIFSTOPPED`), a plain `detach(0)` leaves that stop alone, and the
//! user's own `SIGCONT` is what ends it - the split a debugger relies on,
//! pinned on both machines by the non-ptrace half of this test.

use mpos_user_tests::{Report, host_run};
use rustix::thread::clock_nanosleep_relative;
use rustix::time::{ClockId, Timespec};

/// `ptrace(request, pid, ..)` through `libc`, reduced to what the claims
/// here need: the syscall's own answer, or the errno it failed with.
fn ptrace(request: libc::c_int, pid: libc::pid_t, data: libc::c_long) -> isize {
    // SAFETY: ATTACH/DETACH/SETSIGINFO touch no caller memory, and `data` is
    // a signal number or zero.
    let r = unsafe {
        libc::ptrace(
            request,
            pid,
            std::ptr::null::<libc::c_void>(),
            data as *mut libc::c_void,
        )
    };
    if r < 0 {
        -(std::io::Error::last_os_error().raw_os_error().unwrap_or(0) as isize)
    } else {
        r as isize
    }
}

/// The host's seccomp wall: this sandbox's container answers `EPERM` for
/// every `ptrace` request before the pid is resolved (`.workbuddy/
/// probe_ptrace.c`), so a host run cannot measure the door. Accepted under
/// `MPOS_ON_HOST` only - the gate keeps pinning this kernel's answers.
fn refused_host(answer: isize) -> bool {
    host_run() && answer == -(libc::EPERM as isize)
}

fn sleep_ms(ms: i64) {
    let req = Timespec {
        tv_sec: ms / 1000,
        tv_nsec: (ms % 1000) * 1_000_000,
    };
    let _ = clock_nanosleep_relative(ClockId::Monotonic, &req);
}

/// The pid the intermediate child wrote down for the orphan it left behind.
fn read_pid_file() -> isize {
    std::fs::read_to_string("/tmp/t-ptrace-attach.pid")
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(-1)
}

fn main() {
    println!("t-ptrace-attach");
    let mut r = Report::new();
    let mut status: libc::c_int = 0;

    // The control: a number no task answers to.
    let control = ptrace(libc::PTRACE_ATTACH, 9999, 0);
    r.check(
        "attaching to a pid with no task is ESRCH",
        control == -(libc::ESRCH as isize) || refused_host(control),
    );

    // A descendant: the case every `ptrace` test reaches first, and the one a
    // coincidental slot-index lookup gets right.
    let child = unsafe { libc::fork() };
    if child == 0 {
        // Sleep, then leave - the parent attaches and detaches in between. With
        // no branch here the child fell through into the parent's own test and
        // ran it all again, including the fork below.
        sleep_ms(100);
        // SAFETY: `_exit` never returns, and the child touches nothing else.
        unsafe { libc::_exit(0) };
    }
    r.check("a child to attach to exists", child > 0);
    if child > 0 {
        // Before anything is attached: PTRACE_SETSIGINFO on a live but
        // untraced pid is ESRCH, because a `siginfo_t` can only be planted on
        // a task already stopped under us. This kernel used to answer `0` here
        // for any pid - a success for a request with no tracee behind it.
        // (Linux's own `ptrace_check_attach` refuses the not-a-stopped-tracee
        // case with ESRCH before it ever copies the struct.) Placed before the
        // ATTACH below, since attaching turns the child into a stopped tracee.
        let untraced = ptrace(libc::PTRACE_SETSIGINFO, child, 0);
        r.check(
            "PTRACE_SETSIGINFO on a live but untraced child is ESRCH, not a silent 0",
            untraced == -(libc::ESRCH as isize) || refused_host(untraced),
        );
        let attached = ptrace(libc::PTRACE_ATTACH, child, 0);
        r.check(
            "attaching to my own child by the pid fork gave me",
            attached == 0 || refused_host(attached),
        );
        // Detaching with SIGCONT, not plain: a detach leaves the tracee in the
        // state it was in, and a stopped child never runs to its exit - so the
        // `waitpid` at the end of this test would block on a child the test
        // itself had parked. That is the deadlock this whole section learned
        // about the hard way, and it is the reason `strace` says "detached"
        // rather than leaving every target it touched stopped.
        let detached = ptrace(libc::PTRACE_DETACH, child, libc::SIGCONT as libc::c_long);
        r.check(
            "detaching resumes what it attached",
            detached == 0 || refused_host(detached),
        );
    }

    // The case that matters: a target the caller is not related to, named by
    // the number `ps` would show - because that is what `strace -p` is.
    //
    // It is built rather than borrowed. The obvious non-descendant, pid 1, is a
    // trap this test walked straight into: `PTRACE_ATTACH` stops its target, and
    // a stopped init means nothing reaps children and nothing moves the machine
    // on - the guest froze with the checks above still reporting `ok:`. So here
    // a child is forked, made to fork in turn and then exit, which leaves the
    // grandchild raised by init and no longer a descendant of this test: the
    // relation the check is about, without the dependency that stalls the run.
    //
    // It is also detached with SIGCONT rather than plain, because a detach that
    // leaves the tracee stopped only looks like it gave the process back.
    let _ = std::fs::remove_file("/tmp/t-ptrace-attach.pid");
    let intermediate = unsafe { libc::fork() };
    if intermediate == 0 {
        let orphan = unsafe { libc::fork() };
        if orphan == 0 {
            sleep_ms(10_000);
            // SAFETY: `_exit` never returns.
            unsafe { libc::_exit(0) };
        }
        // Say which pid is the orphan, rather than going looking for one: a scan
        // for "some process whose parent is init" is just as likely to find the
        // test runner and stop it, which is the freeze this avoids.
        let _ = std::fs::write("/tmp/t-ptrace-attach.pid", format!("{orphan}"));
        // SAFETY: `_exit` never returns.
        unsafe { libc::_exit(0) };
    }
    unsafe { libc::waitpid(intermediate.max(0), &mut status, 0) };
    // Give the reparenting a moment: the orphan exists either way, and the only
    // failure that matters here is ESRCH for a pid that is plainly alive. A
    // budget like the rest (`docs/notes/` §15.93): 50ms is some twenty times
    // the `waitpid`-returns-to-child-visible gap the sweep measured.
    sleep_ms(50);

    let target = read_pid_file();
    r.check("the orphan to attach to reported itself", target > 0);
    if target > 0 {
        let attached = ptrace(libc::PTRACE_ATTACH, target as libc::pid_t, 0);
        r.check(
            "attaching to a process that is not ours works - that is strace -p",
            attached == 0 || refused_host(attached),
        );
        let detached = ptrace(
            libc::PTRACE_DETACH,
            target as libc::pid_t,
            libc::SIGCONT as libc::c_long,
        );
        r.check(
            "and detaching hands it back running",
            detached == 0 || refused_host(detached),
        );
    }

    // The proof that a detach gave the process back is that it runs to its exit.
    // Bounded rather than a plain `waitpid`, because this is the check that hung
    // the whole test machine for ten minutes while `detach_traced` reported
    // success: a non-blocking poll that *times out* says the same thing as the
    // hang, and leaves the rest of the suite running to be told about it.
    let mut collected = false;
    // 400ms of 10ms polls, four times this child's own 100ms sleep: the check is
    // "the detach let it run", so the poll only has to outlast the sleep.
    for _ in 0..40 {
        sleep_ms(10);
        status = 0;
        if unsafe { libc::waitpid(child.max(0), &mut status, libc::WNOHANG) } > 0 {
            collected = true;
            break;
        }
    }
    r.check("a detached tracee runs on and is collectable", collected);

    // The other half of the same question, and the one that says a detach may
    // *not* resume: a process the user stopped for itself stays stopped. Measured
    // against Linux - `kill -STOP`, attach, `detach(0)` - the process is still
    // `T` afterwards, and stays `T` even when the detach names `SIGCONT`, because
    // the signal a detach injects goes to the tracee rather than to its stop.
    // What the detach does drop is the `SIGSTOP` *it* brought with it, and that is
    // the difference between the two stops: one `kill -CONT` from outside ends the
    // user's, whereas a kernel that kept the attach's would stop the process
    // again the moment it was released.
    //
    // A debugger relies on this split. Resurrecting a job the user had suspended -
    // which is what `gdb -p` on a `^Z`'d process reaches - turns "look at it and
    // give it back" into "run it", and the user has no way to undo that from the
    // terminal.
    let stopped = unsafe { libc::fork() };
    if stopped == 0 {
        // A tenth of a second, which is the length of the window every check
        // below reads: long enough that a process left *running* reaches its exit
        // inside the poll, short enough that the poll after the `SIGCONT` still
        // sees it. What it must not be is so long that "nothing happened" looks
        // the same either way. The two polls below are 200ms each, twice this
        // window, which is the ratio the 800ms/1.5s pair had (`docs/notes/`
        // §15.93).
        sleep_ms(100);
        // SAFETY: `_exit` never returns.
        unsafe { libc::_exit(0) };
    }
    if stopped > 0 {
        // The user's own hand on the brake, and the parent told about it -
        // `WUNTRACED` is the only reason `wait` has that flag.
        r.check(
            "stopping a child for itself",
            unsafe { libc::kill(stopped, libc::SIGSTOP) } == 0,
        );
        let mut saw_stop = false;
        for _ in 0..40 {
            sleep_ms(5);
            status = 0;
            if unsafe { libc::waitpid(stopped, &mut status, libc::WNOHANG | libc::WUNTRACED) } > 0 {
                // `WIFSTOPPED`, spelled out: the child reaching its *exit* here
                // would also make `waitpid` return, and the rest of this section
                // means nothing if what it stopped for was not a stop.
                saw_stop = status & 0x7f == 0x7f;
                break;
            }
        }
        r.check("the user's SIGSTOP stopped it and was reported", saw_stop);
        // That stop is the *user's* half; now the trace, and give it back.
        let attached = ptrace(libc::PTRACE_ATTACH, stopped, 0);
        r.check(
            "attaching to a process the user stopped",
            attached == 0 || refused_host(attached),
        );
        let detached = ptrace(libc::PTRACE_DETACH, stopped, 0);
        r.check("and detaching it", detached == 0 || refused_host(detached));
        // One and a half seconds of polling, longer than the child's whole sleep:
        // a detach that had handed it back running would be collected in it, and
        // nothing collected is the proof it is still parked. On a host whose
        // ptrace never opened there was no trace to detach from, and the stop
        // simply sits there unrelieved until the SIGCONT below - the same
        // answer, for the narrower reason the probe records.
        let mut gone = false;
        for _ in 0..20 {
            sleep_ms(10);
            status = 0;
            if unsafe { libc::waitpid(stopped, &mut status, libc::WNOHANG) } > 0 {
                gone = true;
                break;
            }
        }
        r.check("a detach leaves a stop the user asked for alone", !gone);
        unsafe { libc::kill(stopped, libc::SIGCONT) };
        for _ in 0..20 {
            sleep_ms(10);
            status = 0;
            if unsafe { libc::waitpid(stopped, &mut status, libc::WNOHANG) } > 0 {
                gone = true;
                break;
            }
        }
        r.check("and the user's own SIGCONT is what ends it", gone);
    }

    r.finish()
}
