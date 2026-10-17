//! t-wait4 - one test for wait4, the call every shell spends its life in.
//!
//! The man page pins four things: `wait4(pid)` returns the child's **pid**,
//! encodes the exit in `*status` where `WEXITSTATUS` reads it, answers `ECHILD`
//! for a pid that is nobody's child, and answers `0` - not an error - for
//! `WNOHANG` while the child it was asked about is still running.
//!
//! The pid/status split is what the test is really about. A kernel that returned
//! the exit code itself would look fine in a shell that only prints `$?`, and
//! would break every libc that calls `WIFEXITED` on the status word.
//!
//! # The migration
//!
//! `rustix::process::waitpid` *is* this call - its linux_raw backend issues
//! `wait4(pid, &status, options, NULL)`, the shape the no-std version asked for -
//! and its answers are the maintained table's rather than this file's: `Ok
//! (Some((pid, status)))` for the reaped child, `WaitStatus::exited()` and
//! `exit_status()` for the two macros, `Ok(None)` for `WNOHANG` finding nobody
//! ready (the ABI's "0, not an error" is a *different variant* here, which makes
//! the claim exactly as strong), and `Err(Errno::CHILD)` for a pid that is
//! nobody's. The encoding itself is still reachable through
//! `WaitStatus::as_raw`, so nothing about the status word was lost.
//!
//! Starting a child is `libc::fork` - `rustix` binds no `fork`, and on the
//! second machine musl reaches `clone` there, `nr_aarch64.rs` recording `FORK =
//! 64007; // no arm64 number` - plus `libc::execve`: the crate *does* carry an
//! `execve`, but in its `runtime` module, which is doc-hidden, explicitly
//! experimental, and outside the enabled feature set (`all-apis` does not list
//! `runtime`), so this is clause (a) rather than a preference. The failed-exec
//! path leaves through the raw `SYS_exit_group` so that a child which inherited
//! the parent's buffered stdout cannot flush it a second time.
//!
//! One fixture moved and the claim did not: the no-std version's "still running"
//! child was `/bin/sleep 2`, and the second machine's `/bin` carries no `sleep`
//! at all - measured, its farm is a fixed applet list (`xtask/src/initramfs.rs`'s
//! `AARCH64_APPLETS`: sh, mount, umount, switch_root, pivot_root, mkdir, cat,
//! echo, ls, true, false), `/bin/true` and `/bin/false` are on both machines, so
//! those two children stay; the sleeper is now this test's own forked child,
//! asleep long enough that the `WNOHANG` below finds it running - which is the
//! cleaner probe anyway, the claim being about `wait4` on a running child rather
//! than about `sleep`.
//!
//! # The fourth argument
//!
//! The man page's four things are all about the *return* value and `*status`;
//! `wait4`'s fourth argument, the caller's `struct rusage`, is `NULL` in every
//! shell and in `rustix`'s `waitpid` alike, and this kernel used not to read it
//! at all - a caller that asked for the child's CPU was handed success and a
//! record it had to fill itself. It is filled now, for every event the call
//! *reports* (a death, a stop, a resume) and not for the "nothing yet" zero, and
//! the claims below are the shapes that tell those apart. Their measurements,
//! including the two orderings that differ from `waitid`'s, are in
//! `.workbuddy/probe_wait4_rusage.c`.

use mpos_user_tests::{Report, Rusage, burn_cpu_ms, host_run};
use rustix::io::Errno;
use rustix::process::{Pid, WaitOptions, WaitStatus, waitpid};
use std::ffi::CStr;

/// The children whose exit codes are the fixtures. Both names are on both
/// machines' `/bin` (the second machine's farm names them for `t-stdspawn`).
const TRUE_PROG: &[u8] = b"/bin/true\0";
const FALSE_PROG: &[u8] = b"/bin/false\0";
/// The `argv[0]` each is started with, spelled the way the no-std version did: a
/// busybox picks its applet by that name.
const TRUE_ARG0: &[u8] = b"true\0";
const FALSE_ARG0: &[u8] = b"false\0";

/// A NUL-terminated literal as a `CStr`. Built before any fork, so the child
/// never runs the check.
fn cstr(bytes: &'static [u8]) -> &'static CStr {
    CStr::from_bytes_with_nul(bytes).expect("each literal above ends in one NUL")
}

/// Start `prog` with `arg0` as a child and return the child's pid, or a negative
/// number.
fn start(prog: &CStr, arg0: &CStr) -> i32 {
    // `libc`'s `c_char` is the machine's own, which is why these are spelled
    // through it rather than as `*const u8` (it is signed here and unsigned on
    // the second machine).
    let argv: [*const libc::c_char; 2] = [arg0.as_ptr(), core::ptr::null()];
    let envp: [*const libc::c_char; 1] = [core::ptr::null()];
    let pid = unsafe { libc::fork() };
    if pid != 0 {
        return pid;
    }
    unsafe {
        libc::execve(prog.as_ptr(), argv.as_ptr(), envp.as_ptr());
        // Reached only when the exec failed: a code the parent will notice, and
        // no flush of the stdout copy this child inherited.
        libc::syscall(libc::SYS_exit_group, 127 as libc::c_long);
        libc::_exit(127)
    }
}

/// How long the "still running" child stays asleep.
const SLEEP_MS: i64 = 500;

/// `wait4(pid, *status, options, *rusage)` through the raw door, with the
/// caller's own record. Returns `(answer, status word)`.
///
/// The fourth argument is the reason this is not `rustix`'s `waitpid`: that
/// wrapper *is* this call with a null `rusage` (`t-wait4`'s header says so), so
/// every claim about what the kernel puts in the record is outside its shape -
/// clause (a) of the crate's rule.
fn wait4_rusage(pid: i32, options: i32, rusage: *mut Rusage) -> (isize, i32) {
    let mut status: i32 = 0;
    let answer = unsafe {
        libc::syscall(
            libc::SYS_wait4,
            pid as libc::c_long,
            &mut status as *mut i32 as libc::c_long,
            options as libc::c_long,
            rusage as libc::c_long,
        )
    };
    (answer as isize, status)
}

/// `getrusage(RUSAGE_CHILDREN)` over the uapi's record: the running total a
/// reaped child's CPU is folded into, which is what the `wait4` answer has to
/// agree with.
fn children_rusage() -> Rusage {
    let mut ru: Rusage = unsafe { core::mem::zeroed() };
    unsafe {
        libc::syscall(
            libc::SYS_getrusage,
            libc::RUSAGE_CHILDREN,
            &mut ru as *mut Rusage,
        )
    };
    ru
}

/// What the parent should read when a reaped child's CPU arrives in the record,
/// as one whole microsecond count - `ru_utime`.
fn utime_micros(ru: &Rusage) -> i64 {
    ru.ru_utime.micros()
}

/// A child that burns `ms` of CPU and then exits 0, leaving through
/// `exit_group` so it never flushes the stdout copy it inherited.
fn start_burner(ms: i64) -> i32 {
    let pid = unsafe { libc::fork() };
    if pid != 0 {
        return pid;
    }
    burn_cpu_ms(ms);
    unsafe {
        libc::syscall(libc::SYS_exit_group, 0 as libc::c_long);
        libc::_exit(0)
    }
}

/// A child that burns `ms` of CPU and then stops itself, for the two claims that
/// a *stop* and a *resume* report the record too. A stopper with no work behind
/// it would make "the child's CPU arrived" and "the record was merely zeroed"
/// the same answer, which is exactly the pair of shapes these claims exist to
/// tell apart.
fn start_burner_then_stop(ms: i64) -> i32 {
    let pid = unsafe { libc::fork() };
    if pid != 0 {
        return pid;
    }
    burn_cpu_ms(ms);
    unsafe {
        libc::raise(libc::SIGSTOP);
    }
    // Sit under the stop until the parent's `SIGCONT`, then leave.
    let nap = libc::timespec {
        tv_sec: 2,
        tv_nsec: 0,
    };
    unsafe {
        libc::nanosleep(&nap, core::ptr::null_mut());
        libc::syscall(libc::SYS_exit_group, 0 as libc::c_long);
        libc::_exit(0)
    }
}

/// The "still running" child, with no program behind it (see the migration note
/// above), and no flush on the way out.
fn start_sleeper() -> i32 {
    let pid = unsafe { libc::fork() };
    if pid != 0 {
        return pid;
    }
    let nap = libc::timespec {
        tv_sec: 0,
        tv_nsec: (SLEEP_MS * 1_000_000) as _,
    };
    unsafe {
        libc::nanosleep(&nap, core::ptr::null_mut());
        libc::syscall(libc::SYS_exit_group, 0 as libc::c_long);
        libc::_exit(0)
    }
}

/// The blocking `wait4(pid)`: `Some(status)` when that child was the one
/// collected, `None` when the call answered anything else. The claim "wait4
/// returns the child's pid" is this function's match; the status claims read the
/// `WaitStatus` it hands back.
fn collect(pid: i32) -> Option<WaitStatus> {
    let want = Pid::from_raw(pid);
    match waitpid(want, WaitOptions::empty()) {
        Ok(Some((got, status))) if Some(got) == want => Some(status),
        _ => None,
    }
}

fn main() {
    println!("t-wait4");
    let mut r = Report::new();

    // A child that exits 0. The return value is its pid; the status word says
    // "exited normally with 0".
    let pid = start(cstr(TRUE_PROG), cstr(TRUE_ARG0));
    r.check("a /bin/true child was started", pid > 0);
    if pid > 0 {
        let status = collect(pid);
        r.check("wait4 returns the child's pid", status.is_some());
        r.check(
            "WIFEXITED holds for a normal exit",
            status.is_some_and(WaitStatus::exited),
        );
        r.check(
            "WEXITSTATUS reads back 0",
            status.and_then(WaitStatus::exit_status) == Some(0),
        );
    }

    // A pid that is nobody's child. One past the whole task table cannot be
    // anyone's, and the answer is ECHILD.
    r.check(
        "wait4 on a non-child pid is ECHILD",
        matches!(
            waitpid(Pid::from_raw(9999), WaitOptions::empty()),
            Err(Errno::CHILD)
        ),
    );

    // A child that exits non-zero: the code survives the round trip through the
    // status word, which is the part a naive implementation gets wrong.
    let pid = start(cstr(FALSE_PROG), cstr(FALSE_ARG0));
    r.check("a /bin/false child was started", pid > 0);
    if pid > 0 {
        let status = collect(pid);
        r.check("the failing child is reaped by pid", status.is_some());
        r.check(
            "WEXITSTATUS reads back 1, the code /bin/false exits with",
            status.and_then(WaitStatus::exit_status) == Some(1),
        );
    }

    // WNOHANG while the child is still sleeping: 0 means "nothing yet", which is
    // not the same as an error.
    let sleeper = start_sleeper();
    r.check("a still-running child was started", sleeper > 0);
    if sleeper > 0 {
        let peek = waitpid(Pid::from_raw(sleeper), WaitOptions::NOHANG);
        r.check(
            "WNOHANG on a running child answers 0, not an error",
            matches!(peek, Ok(None)),
        );

        // The blocking call waits the nap out and collects it.
        let status = collect(sleeper);
        r.check(
            "the blocking wait4 then collects the sleeping child",
            status.is_some(),
        );
        r.check(
            "the sleeping child exited 0",
            status.and_then(WaitStatus::exit_status) == Some(0),
        );
    }

    // ---- the fourth argument: `struct rusage` ------------------------------
    //
    // `wait4(pid, *status, options, *rusage)` fills the record with the reaped
    // child's CPU, and it does so for **every** event it reports - a death, a
    // stop and a resume alike - while leaving the caller's bytes alone when it
    // reports nothing. Every shape below was measured against real Linux first
    // (`.workbuddy/probe_wait4_rusage.c`, Linux 7.2.9): a child that burnt
    // 200 ms answers `ru_utime` 0.1997, the *same* number
    // `getrusage(RUSAGE_CHILDREN)` reports once the reap has folded it in; a
    // `WNOHANG` that finds nothing and an `ECHILD` both leave a seeded record
    // untouched; a grandchild's CPU reaches the record through the child that
    // reaped it (Linux's `RUSAGE_BOTH`, not the child's own totals alone); and
    // an unwritable pointer is `EFAULT` with the child already collected.
    let burner = start_burner(150);
    r.check("a CPU-burning child was started", burner > 0);
    if burner > 0 {
        // Read the running total first: `RUSAGE_CHILDREN` accumulates across
        // every child this process has reaped, so the claim is about what
        // *this* reap moved it by.
        let before = children_rusage().cpu_micros();
        let mut ru = Rusage::seeded();
        let (answer, _status) = wait4_rusage(burner, 0, &mut ru);
        r.check(
            "wait4 returns the burning child's pid beside the record",
            answer == burner as isize,
        );
        r.check(
            "the record carries the child's CPU (ru_utime > 0)",
            utime_micros(&ru) > 0,
        );
        // The running total has to *agree* with what the record says, and two
        // things make that less than an equality. The two sides truncate
        // nanoseconds to microseconds independently - the running total is read
        // once before the reap and once after - so the difference carries that
        // rounding and nothing else: measured, 200 reaps land in 0..=2 us
        // (`.workbuddy/probe_wait4_delta.c`). And the fold itself runs on the
        // scheduler's *next* turn here rather than inside the wait - Linux
        // accumulates in `wait_task_zombie`, this kernel in `reap()`, which
        // `t-times` records for the same claim - so the re-read is a bounded
        // loop that burns a little CPU between attempts to let a tick fire.
        let mut grew = children_rusage().cpu_micros() - before;
        for _ in 0..40 {
            if grew >= ru.cpu_micros() {
                break;
            }
            burn_cpu_ms(2);
            grew = children_rusage().cpu_micros() - before;
        }
        r.check(
            "and it is the CPU the reap folded into RUSAGE_CHILDREN",
            (0..=2).contains(&(grew - ru.cpu_micros())),
        );
        // The fourteen counters are not measured by this kernel, and the whole
        // record is zeroed before the two `timeval`s are filled - an honest
        // "nothing measured" rather than a plausible number. Linux fills
        // several of them from the child (`ru_maxrss`, the fault counts, the
        // context switches), so this is mpos's answer and is named rather than
        // made on a host run.
        if host_run() {
            r.check(
                "note: the host fills ru_maxrss/fault/switch counters from the child; this \
                 kernel measures the CPU and leaves them zero",
                true,
            );
        } else {
            r.check(
                "the unmeasured counters are zero, not the caller's own seed",
                ru.counters.iter().all(|c| *c == 0),
            );
        }
    }

    // A wait that reports *nothing* writes nothing: `WNOHANG` on a child that
    // has not exited is the zero the caller polls for, not a report, and the
    // caller's own record survives it (measured, scenario J).
    let peeker = start_sleeper();
    r.check("a child to peek at was started", peeker > 0);
    if peeker > 0 {
        let mut ru = Rusage::seeded();
        let (answer, _) = wait4_rusage(peeker, libc::WNOHANG, &mut ru);
        r.check("WNOHANG with nothing ready answers 0", answer == 0);
        r.check("and leaves the caller's record untouched", !ru.written());
        // Collect it: a fixture left running would be reaped by a later claim
        // and fold its (small) CPU into the running child total.
        let _ = wait4_rusage(peeker, 0, core::ptr::null_mut());
    }

    // An unwritable `rusage` pointer is `EFAULT`, and the child is collected
    // anyway - Linux writes `*status` inside the wait and copies the record
    // afterwards, so the failure lands on the record alone, and a second wait
    // finds nobody left (measured, scenario N).
    let gone = start_burner(0);
    r.check("a child for the EFAULT claim was started", gone > 0);
    if gone > 0 {
        let bad = wait4_rusage(gone, 0, 1usize as *mut Rusage);
        r.check(
            "an unwritable rusage pointer is EFAULT",
            bad.0 == -1 && std::io::Error::last_os_error().raw_os_error() == Some(libc::EFAULT),
        );
        r.check(
            "while the status word beside it was written (the child was reaped)",
            bad.1 & 0x7f == 0,
        );
        let again = wait4_rusage(gone, libc::WNOHANG, core::ptr::null_mut());
        r.check(
            "and a second wait for it is ECHILD",
            again.0 == -1 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ECHILD),
        );
    }

    // A stop and a resume are *reports* like a death, and Linux fills the record
    // for both (measured, scenarios C and I: a child that burnt 150 ms and then
    // stopped answers 0.149642, and the resume that ends the stop answers the
    // same). This is the half of the change that needed the wait's stop and
    // resume events to carry the child's CPU at all - and it comes off the
    // *live* group, because the child is still in the task table when the report
    // is made, unlike a death whose record has to carry it.
    let halter = start_burner_then_stop(120);
    r.check(
        "a child that burns CPU and then stops was started",
        halter > 0,
    );
    if halter > 0 {
        let mut ru = Rusage::seeded();
        let (answer, status) = wait4_rusage(halter, libc::WUNTRACED, &mut ru);
        r.check(
            "WUNTRACED reports the stop",
            answer == halter as isize
                && status & 0xff == 0x7f
                && (status >> 8) & 0xff == libc::SIGSTOP,
        );
        r.check(
            "and the stopped child's CPU is in the record",
            utime_micros(&ru) > 0,
        );

        // The resume, which is the same report for the same reason.
        unsafe { libc::kill(halter, libc::SIGCONT) };
        let mut ru = Rusage::seeded();
        let (answer, status) = wait4_rusage(halter, libc::WCONTINUED, &mut ru);
        r.check(
            "WCONTINUED reports the resume (the all-ones status word)",
            answer == halter as isize && status == 0xffff,
        );
        r.check(
            "and the resumed child's CPU is in the record too",
            utime_micros(&ru) > 0,
        );

        // Collect it, so the fixture cannot be reaped by a later claim.
        let _ = wait4_rusage(halter, 0, core::ptr::null_mut());
    }

    // ---- a death by signal: the status word classifies it ------------------
    //
    // `WIFSIGNALED` is true and `WTERMSIG` is the signal; and the core-dump bit
    // (0x80, `WCOREDUMP`) follows the *signal*, not a core file written -
    // measured on this host with `.workbuddy/probe_coredump_signals.c` at
    // `RLIMIT_CORE` 0, which walks every fatal signal: `SIGSEGV` and the other
    // nine core-dumping signals set the bit while `SIGKILL`, `SIGTERM` and the
    // rest do not. `rustix`'s `WaitStatus` exposes the signal but not the bit,
    // so the claim reads the raw word the way a libc's `WCOREDUMP` macro does.
    //
    // The bit is the signal's default action **and** the dying child's own
    // `dumpable` flag: `.workbuddy/probe_wcore_dumpable.c` shows the same
    // `SIGSEGV` death clearing the bit under `PR_SET_DUMPABLE 0`, which is the
    // gate this kernel would otherwise miss - the "core-dumping signal" half
    // alone is not the rule. The non-dumpable case below pins it.
    fn start_doomed(sig: libc::c_int) -> i32 {
        start_doomed_as(sig, None)
    }

    /// The same death, with the child first moving its own `dumpable` flag when
    /// `dumpable` names one - `0` is `SUID_DUMP_DISABLE`,
    /// `PR_SET_DUMPABLE 0`, the flag `do_coredump()` checks first.
    fn start_doomed_as(sig: libc::c_int, dumpable: Option<libc::c_int>) -> i32 {
        let pid = unsafe { libc::fork() };
        if pid != 0 {
            return pid;
        }
        unsafe {
            if let Some(flag) = dumpable {
                libc::prctl(libc::PR_SET_DUMPABLE, flag, 0, 0, 0);
            }
            libc::signal(sig, libc::SIG_DFL);
            libc::raise(sig);
            // Reached only when the signal did not end the child.
            libc::syscall(libc::SYS_exit_group, 111 as libc::c_long);
            libc::_exit(111)
        }
    }

    let sigsegv = start_doomed(libc::SIGSEGV);
    r.check("a child that will take SIGSEGV was started", sigsegv > 0);
    if sigsegv > 0 {
        let status = collect(sigsegv);
        r.check(
            "a SIGSEGV death is a signal death, not an exit",
            status.is_some_and(WaitStatus::signaled),
        );
        r.check(
            "WTERMSIG reads back SIGSEGV",
            status.and_then(WaitStatus::terminating_signal) == Some(libc::SIGSEGV),
        );
        r.check(
            "and WCOREDUMP (0x80) is set for a core-dumping signal",
            status.is_some_and(|s| s.as_raw() & 0x80 != 0),
        );
    }

    let sigterm = start_doomed(libc::SIGTERM);
    r.check("a child that will take SIGTERM was started", sigterm > 0);
    if sigterm > 0 {
        let status = collect(sigterm);
        r.check(
            "WTERMSIG reads back SIGTERM",
            status.and_then(WaitStatus::terminating_signal) == Some(libc::SIGTERM),
        );
        r.check(
            "and WCOREDUMP stays clear for a plain termination",
            status.is_some_and(|s| s.as_raw() & 0x80 == 0),
        );
    }

    // The gate: the *same* core-dumping signal for a child that turned its own
    // `dumpable` off is a plain kill, bit clear - the second half of the rule.
    let nodump = start_doomed_as(libc::SIGSEGV, Some(0));
    r.check(
        "a non-dumpable child that will take SIGSEGV was started",
        nodump > 0,
    );
    if nodump > 0 {
        let status = collect(nodump);
        r.check(
            "a non-dumpable SIGSEGV death is still a signal death",
            status.is_some_and(WaitStatus::signaled),
        );
        r.check(
            "WTERMSIG still reads back SIGSEGV",
            status.and_then(WaitStatus::terminating_signal) == Some(libc::SIGSEGV),
        );
        r.check(
            "but WCOREDUMP stays clear: the bit needs the child to be dumpable",
            status.is_some_and(|s| s.as_raw() & 0x80 == 0),
        );
    }

    r.finish()
}
