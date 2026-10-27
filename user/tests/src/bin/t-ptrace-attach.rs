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
///
/// The request word is `c_int` here and the callers cast `libc`'s constants
/// into it, because glibc's `PTRACE_*` are `u32` (they arrive through its
/// headers) and musl's are `c_int` - one spelling fits exactly one target
/// otherwise, measured when the host sweep first built this crate for the
/// host target. The number is `libc`'s either way.
fn ptrace(request: libc::c_int, pid: libc::pid_t, data: libc::c_long) -> isize {
    // SAFETY: ATTACH/DETACH/SETSIGINFO touch no caller memory, and `data` is
    // a signal number or zero.
    let r = unsafe {
        libc::ptrace(
            request as _,
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

/// A detach after a *refused* attach has no tracee to hand back, so Linux
/// answers `ESRCH` - the attach's refusal showing through, not a second
/// failure to pin. The runner's own seccomp profile is one such refusal.
fn detach_without_a_tracee(detached: isize, attached: isize) -> bool {
    host_run() && attached != 0 && detached == -(libc::ESRCH as isize)
}
/// Consume the stop `PTRACE_ATTACH` sends before asking for another request.
///
/// `PTRACE_ATTACH` returns once the stop is *queued*, not once the tracer has
/// seen it. A `PTRACE_DETACH` in that window answers `ESRCH` - the child is not
/// in a ptrace-stop the tracer has been told about yet - which is the flake a
/// direct run showed (attached 0, detached -3, three times in five). Waiting
/// for the stop first is what strace's own attach loop does. Only a host run
/// does it: this kernel's attach stop is already part of the stop `waitpid`
/// returns right after `PTRACE_ATTACH`, and waiting for a second one would hang
/// the lane (measured: `t-ptrace-attach timeout=120s`).
fn wait_for_attach_stop(r: &mut Report, pid: libc::pid_t, what: &str) {
    let mut status: libc::c_int = 0;
    // SAFETY: `pid` is the task just attached and `status` is a local.
    let n = unsafe { libc::waitpid(pid, &mut status, 0) };
    r.check(
        &format!("the {what} reports its attach stop"),
        n == pid && libc::WIFSTOPPED(status),
    );
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
    let control = ptrace(libc::PTRACE_ATTACH as libc::c_int, 9999, 0);
    r.check(
        "attaching to a pid with no task is ESRCH",
        control == -(libc::ESRCH as isize) || refused_host(control),
    );

    // A descendant: the case every `ptrace` test reaches first, and the one a
    // coincidental slot-index lookup gets right.
    // The child waits on this pipe rather than a 100 ms sleep: a sleep can
    // expire between ATTACH and DETACH on a busy host, and then the detach has
    // no tracee and answers ESRCH. The parent writes the byte *after* the
    // detach, so "the detach let it run" stays a measurement.
    let mut gate = [0i32; 2];
    let gate_made = unsafe { libc::pipe(gate.as_mut_ptr()) };
    r.check("the child's release pipe is made", gate_made == 0);
    let child = unsafe { libc::fork() };
    if child == 0 {
        // Wait for the parent's release, then leave. With no branch here the
        // child fell through into the parent's own test and ran it all again,
        // including the fork below.
        unsafe {
            libc::close(gate[1]);
            let mut byte = 0u8;
            let _ = libc::read(gate[0], &mut byte as *mut u8 as *mut libc::c_void, 1);
            libc::close(gate[0]);
            // SAFETY: `_exit` never returns, and the child touches nothing else.
            libc::_exit(0);
        }
    }
    if child > 0 {
        unsafe { libc::close(gate[0]) };
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
        let untraced = ptrace(libc::PTRACE_SETSIGINFO as libc::c_int, child, 0);
        r.check(
            "PTRACE_SETSIGINFO on a live but untraced child is ESRCH, not a silent 0",
            untraced == -(libc::ESRCH as isize) || refused_host(untraced),
        );
        let attached = ptrace(libc::PTRACE_ATTACH as libc::c_int, child, 0);
        r.check(
            "attaching to my own child by the pid fork gave me",
            attached == 0 || refused_host(attached),
        );
        if attached == 0 && host_run() {
            wait_for_attach_stop(&mut r, child, "own child");
        }
        // Detaching with SIGCONT, not plain: a detach leaves the tracee in the
        // state it was in, and a stopped child never runs to its exit - so the
        // `waitpid` at the end of this test would block on a child the test
        // itself had parked. That is the deadlock this whole section learned
        // about the hard way, and it is the reason `strace` says "detached"
        // rather than leaving every target it touched stopped.
        if attached != 0 && host_run() {
            println!(
                "note: this host refused PTRACE_ATTACH, so there is no tracee to detach; \
                 the detach claim is this kernel's"
            );
        }
        let detached = ptrace(
            libc::PTRACE_DETACH as libc::c_int,
            child,
            libc::SIGCONT as libc::c_long,
        );
        r.check(
            &format!(
                "detaching resumes what it attached (attached {attached}, detached {detached})"
            ),
            detached == 0 || refused_host(detached) || detach_without_a_tracee(detached, attached),
        );
        // Let the child leave now that the detach has had its turn.
        unsafe {
            let _ = libc::write(gate[1], b"x".as_ptr() as *const libc::c_void, 1);
            libc::close(gate[1]);
        }
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
        let attached = ptrace(libc::PTRACE_ATTACH as libc::c_int, target as libc::pid_t, 0);
        r.check(
            "attaching to a process that is not ours works - that is strace -p",
            attached == 0 || refused_host(attached),
        );
        if attached == 0 && host_run() {
            wait_for_attach_stop(&mut r, target as libc::pid_t, "orphan");
        }
        let detached = ptrace(
            libc::PTRACE_DETACH as libc::c_int,
            target as libc::pid_t,
            libc::SIGCONT as libc::c_long,
        );
        r.check(
            "and detaching hands it back running",
            detached == 0 || refused_host(detached) || detach_without_a_tracee(detached, attached),
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
        let attached = ptrace(libc::PTRACE_ATTACH as libc::c_int, stopped, 0);
        r.check(
            "attaching to a process the user stopped",
            attached == 0 || refused_host(attached),
        );
        if attached == 0 && host_run() {
            wait_for_attach_stop(&mut r, stopped, "stopped child");
        }
        let detached = ptrace(libc::PTRACE_DETACH as libc::c_int, stopped, 0);
        r.check(
            "and detaching it",
            detached == 0 || refused_host(detached) || detach_without_a_tracee(detached, attached),
        );
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
