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

#![no_std]
#![no_main]

#[path = "../../lib/lin.rs"]
mod lin;

use core::panic::PanicInfo;

static PIDFILE: &[u8] = b"/tmp/t-ptrace-attach.pid\0";

const SYS_PTRACE: u64 = 101;
const PTRACE_ATTACH: usize = 16;
const PTRACE_DETACH: usize = 17;
const SIGCONT: usize = 18;
const SIGSTOP: usize = 19;
const WNOHANG: usize = 1;
const WUNTRACED: usize = 2;

fn ptrace(request: usize, pid: usize) -> isize {
    ptrace_with(request, pid, 0)
}

fn ptrace_with(request: usize, pid: usize, data: usize) -> isize {
    lin::raw_sys6(SYS_PTRACE, request, pid, 0, 0, data, 0)
}

/// The pid the intermediate child wrote down for the orphan it left behind.
fn read_pid_file() -> isize {
    let fd = lin::openat(lin::AT_FDCWD, PIDFILE, 0 /* O_RDONLY */, 0);
    if fd < 0 {
        return -1;
    }
    let mut buf = [0u8; 16];
    let n = lin::read(fd as i32, &mut buf);
    let _ = lin::close(fd as i32);
    if n <= 0 {
        return -1;
    }
    let mut value: isize = 0;
    for &b in &buf[..n as usize] {
        if !b.is_ascii_digit() {
            break;
        }
        value = value * 10 + (b - b'0') as isize;
    }
    value
}

#[unsafe(no_mangle)]
pub extern "C" fn mpos_user_main(_argc: i32, _argv: *const *const u8) -> ! {
    lin::puts(b"t-ptrace-attach\n");
    let mut failed = false;

    // The control: a number no task answers to.
    lin::check_eq(
        &mut failed,
        b"attaching to a pid with no task is ESRCH",
        ptrace(PTRACE_ATTACH, 9999),
        lin::ESRCH,
    );

    // A descendant: the case every `ptrace` test reaches first, and the one a
    // coincidental slot-index lookup gets right.
    let child = lin::fork();
    if child == 0 {
        // Sleep, then leave - the parent attaches and detaches in between. With
        // no branch here the child fell through into the parent's own test and
        // ran it all again, including the fork below.
        let ts = lin::TimeSpec {
            tv_sec: 2,
            tv_nsec: 0,
        };
        let mut rem = ts;
        let _ = lin::nanosleep(&ts, &mut rem);
        lin::exit(0);
    }
    lin::check_true(&mut failed, b"a child to attach to exists", child > 0);
    if child > 0 {
        lin::check_eq(
            &mut failed,
            "attaching to my own child by the pid fork gave me".as_bytes(),
            ptrace(PTRACE_ATTACH, child as usize),
            0,
        );
        // Detaching with SIGCONT, not plain: a detach leaves the tracee in the
        // state it was in, and a stopped child never runs to its exit - so the
        // `waitpid` at the end of this test would block on a child the test
        // itself had parked. That is the deadlock this whole section learned
        // about the hard way, and it is the reason `strace` says "detached"
        // rather than leaving every target it touched stopped.
        lin::check_eq(
            &mut failed,
            b"detaching resumes what it attached",
            ptrace_with(PTRACE_DETACH, child as usize, SIGCONT),
            0,
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
    let _ = lin::unlinkat(lin::AT_FDCWD, PIDFILE, 0);
    let intermediate = lin::fork();
    if intermediate == 0 {
        let orphan = lin::fork();
        if orphan == 0 {
            let ts = lin::TimeSpec {
                tv_sec: 10,
                tv_nsec: 0,
            };
            let mut rem = ts;
            let _ = lin::nanosleep(&ts, &mut rem);
            lin::exit(0);
        }
        // Say which pid is the orphan, rather than going looking for one: a scan
        // for "some process whose parent is init" is just as likely to find the
        // test runner and stop it, which is the freeze this avoids.
        let mut out = [0u8; 16];
        let mut at = 0usize;
        let mut n = orphan as usize;
        let mut digits = [0u8; 12];
        let mut k = 0;
        loop {
            digits[k] = b'0' + (n % 10) as u8;
            k += 1;
            n /= 10;
            if n == 0 {
                break;
            }
        }
        while k > 0 {
            k -= 1;
            out[at] = digits[k];
            at += 1;
        }
        let fd = lin::openat(
            lin::AT_FDCWD,
            PIDFILE,
            1 | 0o100, /* WRONLY|CREAT */
            0o644,
        );
        if fd >= 0 {
            let _ = lin::write(fd as i32, &out[..at]);
            let _ = lin::close(fd as i32);
        }
        lin::exit(0);
    }
    let mut status = 0i32;
    let _ = lin::waitpid(intermediate.max(0) as usize, &mut status, 0);
    // Give the reparenting a moment: the orphan exists either way, and the only
    // failure that matters here is ESRCH for a pid that is plainly alive.
    let nap = lin::TimeSpec {
        tv_sec: 1,
        tv_nsec: 0,
    };
    let mut rem = nap;
    let _ = lin::nanosleep(&nap, &mut rem);

    let target = read_pid_file();
    lin::check_true(
        &mut failed,
        b"the orphan to attach to reported itself",
        target > 0,
    );
    if target > 0 {
        lin::check_eq(
            &mut failed,
            b"attaching to a process that is not ours works - that is strace -p",
            ptrace(PTRACE_ATTACH, target as usize),
            0,
        );
        lin::check_eq(
            &mut failed,
            b"and detaching hands it back running",
            ptrace_with(PTRACE_DETACH, target as usize, SIGCONT),
            0,
        );
    }

    // The proof that a detach gave the process back is that it runs to its exit.
    // Bounded rather than a plain `waitpid`, because this is the check that hung
    // the whole test machine for ten minutes while `detach_traced` reported
    // success: a non-blocking poll that *times out* says the same thing as the
    // hang, and leaves the rest of the suite running to be told about it.
    let mut collected = false;
    let mut status = 0i32;
    for _ in 0..60 {
        let nap = lin::TimeSpec {
            tv_sec: 0,
            tv_nsec: 50_000_000,
        };
        let mut rem = nap;
        let _ = lin::nanosleep(&nap, &mut rem);
        if lin::waitpid(child as usize, &mut status, 1 /* WNOHANG */) > 0 {
            collected = true;
            break;
        }
    }
    lin::check_true(
        &mut failed,
        b"a detached tracee runs on and is collectable",
        collected,
    );

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
    let stopped = lin::fork();
    if stopped == 0 {
        // Eight tenths of a second, which is the length of the window every check
        // below reads: long enough that a process left *running* reaches its exit
        // inside the poll, short enough that the poll after the `SIGCONT` still
        // sees it. What it must not be is so long that "nothing happened" looks
        // the same either way.
        let nap = lin::TimeSpec {
            tv_sec: 0,
            tv_nsec: 800_000_000,
        };
        let mut rem = nap;
        let _ = lin::nanosleep(&nap, &mut rem);
        lin::exit(0);
    }
    if stopped > 0 {
        // The user's own hand on the brake, and the parent told about it -
        // `WUNTRACED` is the only reason `wait` has that flag.
        lin::check_eq(
            &mut failed,
            b"stopping a child for itself",
            lin::kill(stopped as i32, SIGSTOP as i32),
            0,
        );
        let mut saw_stop = false;
        let mut st = 0i32;
        for _ in 0..40 {
            let nap = lin::TimeSpec {
                tv_sec: 0,
                tv_nsec: 25_000_000,
            };
            let mut rem = nap;
            let _ = lin::nanosleep(&nap, &mut rem);
            if lin::waitpid(stopped as usize, &mut st, WNOHANG | WUNTRACED) > 0 {
                // `WIFSTOPPED`, spelled out: the child reaching its *exit* here
                // would also make `waitpid` return, and the rest of this section
                // means nothing if what it stopped for was not a stop.
                saw_stop = st & 0x7f == 0x7f;
                break;
            }
        }
        lin::check_true(
            &mut failed,
            b"the user's SIGSTOP stopped it and was reported",
            saw_stop,
        );
        // That stop is the *user's* half; now the trace, and give it back.
        lin::check_eq(
            &mut failed,
            b"attaching to a process the user stopped",
            ptrace(PTRACE_ATTACH, stopped as usize),
            0,
        );
        lin::check_eq(
            &mut failed,
            b"and detaching it",
            ptrace_with(PTRACE_DETACH, stopped as usize, 0),
            0,
        );
        // One and a half seconds of polling, longer than the child's whole sleep:
        // a detach that had handed it back running would be collected in it, and
        // nothing collected is the proof it is still parked.
        let mut gone = false;
        for _ in 0..30 {
            let nap = lin::TimeSpec {
                tv_sec: 0,
                tv_nsec: 50_000_000,
            };
            let mut rem = nap;
            let _ = lin::nanosleep(&nap, &mut rem);
            if lin::waitpid(stopped as usize, &mut status, 1) > 0 {
                gone = true;
                break;
            }
        }
        lin::check_true(
            &mut failed,
            b"a detach leaves a stop the user asked for alone",
            !gone,
        );
        let _ = lin::kill(stopped as i32, SIGCONT as i32);
        for _ in 0..40 {
            let nap = lin::TimeSpec {
                tv_sec: 0,
                tv_nsec: 50_000_000,
            };
            let mut rem = nap;
            let _ = lin::nanosleep(&nap, &mut rem);
            if lin::waitpid(stopped as usize, &mut status, 1) > 0 {
                gone = true;
                break;
            }
        }
        lin::check_true(
            &mut failed,
            b"and the user's own SIGCONT is what ends it",
            gone,
        );
    }
    lin::done(failed)
}

#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    lin::die(b"t-ptrace-attach: panicked\n")
}
