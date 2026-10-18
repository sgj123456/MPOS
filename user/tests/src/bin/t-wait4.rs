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
//! # Where the status word lands
//!
//! `*status` is four bytes written with one `put_user`, and Linux checks that
//! reach **atomically**: a word that starts inside the caller's mapping and ends
//! outside it is `EFAULT` with not one byte written, where the `struct rusage`
//! beside it is a range copy that leaves its head behind. The two answers that
//! are not reports - the `WNOHANG` zero and `ECHILD` - are decided before the
//! word is reached at all, so an unwritable pointer costs nothing there; a
//! refusal that *does* reach a report has already taken the event, which is why
//! a refused stop is gone on the next look. Those are the fourteen shapes
//! `.workbuddy/probe_wait4_status.c` walked, and the claims below pin them.
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

    // ---- the other half: `*status` is a word, not a range ------------------
    //
    // The record above is a range copy, so a pointer that runs off the end of
    // the mapping leaves its head behind. `*status` is not: Linux writes it
    // with one `put_user`, and a word that starts inside the mapping and ends
    // outside is `EFAULT` with **not one byte** written - measured,
    // `.workbuddy/probe_wait4_status.c` scenarios B, C and D, against scenario
    // AH of `probe_waitid_record.c` for the contrast. Both doors here answer
    // that way already (a whole-word `buffer_ok` before a single write), which
    // is what the shapes below pin: they are the fourteen that probe walked,
    // kept so that a later "just copy four bytes" cannot land quietly.
    use core::ffi::c_void;

    /// `wait4` with the caller's own `*status` *pointer*, so that the pointer
    /// can be one the kernel has to refuse. The word is not read back through
    /// a `&mut i32` - the addresses below are not one.
    fn wait4_at(pid: i32, status: *mut i32, options: i32, rusage: *mut Rusage) -> (isize, i32) {
        let answer = unsafe {
            libc::syscall(
                libc::SYS_wait4,
                pid as libc::c_long,
                status as libc::c_long,
                options as libc::c_long,
                rusage as libc::c_long,
            )
        };
        // `errno` is only the call's when the call failed - a successful
        // `wait4` leaves it exactly as it found it, so reading it here would
        // pin whatever the previous failure happened to leave, which is how
        // the first draft of these claims went red on this very host.
        let err = if answer == -1 {
            std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
        } else {
            0
        };
        (answer as isize, err)
    }

    /// A child that leaves with `code`, for the control shape: a word of zero
    /// would not tell "the kernel wrote it" from "the kernel never reached it".
    fn start_exiting(code: i32) -> i32 {
        let pid = unsafe { libc::fork() };
        if pid != 0 {
            return pid;
        }
        unsafe {
            libc::syscall(libc::SYS_exit_group, code as libc::c_long);
            libc::_exit(code)
        }
    }

    const PAGE: usize = 4096;
    // The seed every byte the kernel did not write has to read back as. Zero
    // would not tell "left alone" from "written zero", which is the whole
    // difference between an atomic word and a partial copy.
    const SEED: u8 = 0xa5;

    // Two pages, the second one unwritable: the four-byte word can then be
    // made to straddle the boundary, to fit before it, or to lie wholly
    // inside it.
    let pair = unsafe {
        libc::mmap(
            core::ptr::null_mut(),
            2 * PAGE,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
            -1,
            0,
        )
    };
    r.check(
        "two pages for the status-word fixtures were mapped",
        pair != libc::MAP_FAILED,
    );
    if pair != libc::MAP_FAILED {
        let base = pair as *mut u8;
        let rc = unsafe { libc::mprotect(base.add(PAGE) as *mut c_void, PAGE, libc::PROT_NONE) };
        r.check("the second page was made unwritable", rc == 0);
        unsafe { core::ptr::write_bytes(base, SEED, PAGE) };

        // The control, and the one claim whose subject is a word the kernel
        // *can* reach: it lands in the writable page and reads back the exit
        // status the caller would decode (measured, scenario A, where a child
        // exiting 7 leaves `0x700`). Without it the section could pin a refusal
        // at every pointer and never show the write it is refusing.
        let control = start_exiting(7);
        r.check("a child for the control status was started", control > 0);
        if control > 0 {
            let good = base as *mut i32;
            let (answer, err) = wait4_at(control, good, 0, core::ptr::null_mut());
            r.check(
                "a status word wholly inside the writable page is written",
                answer == control as isize && err == 0 && unsafe { core::ptr::read(good) } == 0x700,
            );
        }

        // Two bytes in, two out - the shape that tells an atomic word from a
        // copy that writes what it can reach.
        let straddle = unsafe { base.add(PAGE - 2) } as *mut i32;
        let half = start_burner(0);
        r.check("a child for the straddling status was started", half > 0);
        if half > 0 {
            let (answer, err) = wait4_at(half, straddle, 0, core::ptr::null_mut());
            r.check(
                "a status word that runs off the end of the mapping is EFAULT",
                answer == -1 && err == libc::EFAULT,
            );
            r.check(
                "and not one byte of it was written",
                unsafe { core::ptr::read(base.add(PAGE - 2)) } == SEED
                    && unsafe { core::ptr::read(base.add(PAGE - 1)) } == SEED,
            );
            // The refusal lands *after* the collection, which is the ordering
            // the record's own `EFAULT` has: the child is gone either way.
            let (again, again_err) = wait4_at(
                half,
                core::ptr::null_mut(),
                libc::WNOHANG,
                core::ptr::null_mut(),
            );
            r.check(
                "though the child had already been collected",
                again == -1 && again_err == libc::ECHILD,
            );
        }

        // The same rule at the other two splits - one byte in and three out,
        // then three in and one out (measured, scenarios C and D) - because the
        // refusal is about where the *word* ends and not about how much of it
        // the caller's mapping happens to hold: each leaves every reachable byte
        // exactly as the caller wrote it.
        let split_c = unsafe { base.add(PAGE - 1) } as *mut i32;
        let one_in = start_burner(0);
        r.check("a child for the one-byte-in status was started", one_in > 0);
        if one_in > 0 {
            let (answer, err) = wait4_at(one_in, split_c, 0, core::ptr::null_mut());
            r.check(
                "a status word one byte inside the mapping is EFAULT",
                answer == -1 && err == libc::EFAULT,
            );
            r.check(
                "and that one reachable byte was not written either",
                unsafe { core::ptr::read(base.add(PAGE - 1)) } == SEED,
            );
        }

        let split_d = unsafe { base.add(PAGE - 3) } as *mut i32;
        let three_in = start_burner(0);
        r.check(
            "a child for the three-byte-in status was started",
            three_in > 0,
        );
        if three_in > 0 {
            let (answer, err) = wait4_at(three_in, split_d, 0, core::ptr::null_mut());
            r.check(
                "a status word three bytes inside the mapping is EFAULT",
                answer == -1 && err == libc::EFAULT,
            );
            r.check(
                "and all three reachable bytes were left alone",
                (0..3).all(|i| unsafe { core::ptr::read(base.add(PAGE - 3 + i)) } == SEED),
            );
        }

        // Wholly inside the unwritable page: there is nothing to read back,
        // only the refusal.
        let inside = unsafe { base.add(PAGE) } as *mut i32;
        let unreachable = start_burner(0);
        r.check(
            "a child for the unwritable-page status was started",
            unreachable > 0,
        );
        if unreachable > 0 {
            let (answer, err) = wait4_at(unreachable, inside, 0, core::ptr::null_mut());
            r.check(
                "a status word wholly inside an unwritable page is EFAULT",
                answer == -1 && err == libc::EFAULT,
            );
        }

        // A refused status does not touch the record beside it: the two writes
        // happen in that order, `*status` first (measured, scenario G - the
        // mirror of the claim above, where the record is the refused one).
        let paired = start_burner(0);
        r.check(
            "a child for the refused-status pair was started",
            paired > 0,
        );
        if paired > 0 {
            let mut ru = Rusage::seeded();
            let (answer, err) = wait4_at(paired, straddle, 0, &mut ru);
            r.check(
                "a refused status word leaves the record beside it untouched",
                answer == -1 && err == libc::EFAULT && !ru.written(),
            );
        }

        // Both pointers refused at once (measured, scenario I): the word is
        // written first, so its own refusal is the answer and the record is
        // never reached - the two claims above are each half of this, and this
        // is the shape that says the order is what decides which is asked.
        let both = start_burner(0);
        r.check("a child for the both-refused shape was started", both > 0);
        if both > 0 {
            let mut ru = Rusage::seeded();
            let (answer, err) = wait4_at(both, straddle, 0, &mut ru);
            r.check(
                "both pointers refused is EFAULT and touches neither",
                answer == -1
                    && err == libc::EFAULT
                    && !ru.written()
                    && unsafe { core::ptr::read(base.add(PAGE - 2)) } == SEED
                    && unsafe { core::ptr::read(base.add(PAGE - 1)) } == SEED,
            );
        }

        unsafe { libc::munmap(pair, 2 * PAGE) };
    }

    // A page the caller may read but not write: the word is inside a mapping
    // and still refused, which is the *write* half of the reach rather than the
    // *mapped* half.
    let ro = unsafe {
        libc::mmap(
            core::ptr::null_mut(),
            PAGE,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
            -1,
            0,
        )
    };
    r.check(
        "a page for the read-only status was mapped",
        ro != libc::MAP_FAILED,
    );
    if ro != libc::MAP_FAILED {
        unsafe { core::ptr::write_bytes(ro as *mut u8, SEED, PAGE) };
        let rc = unsafe { libc::mprotect(ro, PAGE, libc::PROT_READ) };
        r.check("and made read-only", rc == 0);
        let unwritable = start_burner(0);
        r.check(
            "a child for the read-only status was started",
            unwritable > 0,
        );
        if unwritable > 0 {
            let (answer, err) = wait4_at(unwritable, ro as *mut i32, 0, core::ptr::null_mut());
            r.check(
                "a status word in a read-only page is EFAULT",
                answer == -1 && err == libc::EFAULT,
            );
            r.check(
                "and the page still holds the caller's own bytes",
                unsafe { core::ptr::read(ro as *const u8) } == SEED,
            );
        }
        unsafe { libc::munmap(ro, PAGE) };
    }

    // A null `*status` is "not interested", not an error: the child is reaped
    // and the answer is its pid (measured, scenario F).
    let mute = start_burner(0);
    r.check("a child for the null-status claim was started", mute > 0);
    if mute > 0 {
        let (answer, err) = wait4_at(mute, core::ptr::null_mut(), 0, core::ptr::null_mut());
        r.check(
            "a null status pointer is not an error",
            answer == mute as isize && err == 0,
        );
    }

    // The two answers that are not *reports* never reach the word: the
    // `WNOHANG` zero and `ECHILD` are decided before anything is written, so a
    // pointer the kernel could not write to costs nothing (scenarios J and K).
    let waiting = start_sleeper();
    r.check(
        "a running child for the WNOHANG shape was started",
        waiting > 0,
    );
    if waiting > 0 {
        let (answer, err) = wait4_at(
            waiting,
            1usize as *mut i32,
            libc::WNOHANG,
            core::ptr::null_mut(),
        );
        r.check(
            "WNOHANG with nothing ready answers 0 even with an unwritable status",
            answer == 0 && err == 0,
        );
        let _ = wait4_at(waiting, core::ptr::null_mut(), 0, core::ptr::null_mut());
    }
    let (answer, err) = wait4_at(
        unsafe { libc::getpid() },
        1usize as *mut i32,
        0,
        core::ptr::null_mut(),
    );
    r.check(
        "ECHILD comes before the status word is reached",
        answer == -1 && err == libc::ECHILD,
    );

    // A refused *stop* is consumed like a refused death: the event was taken
    // before the word was written, so the second look finds nothing (measured,
    // scenario L - where the loss is the point, not an accident).
    let stopper = start_burner_then_stop(10);
    r.check(
        "a child that stops was started for the refused-stop claim",
        stopper > 0,
    );
    if stopper > 0 {
        let (answer, err) = wait4_at(
            stopper,
            1usize as *mut i32,
            libc::WUNTRACED,
            core::ptr::null_mut(),
        );
        r.check(
            "a refused status word on a stop is EFAULT",
            answer == -1 && err == libc::EFAULT,
        );
        let (again, again_err) = wait4_at(
            stopper,
            core::ptr::null_mut(),
            libc::WUNTRACED | libc::WNOHANG,
            core::ptr::null_mut(),
        );
        r.check(
            "and the stop does not come back a second time",
            again == 0 && again_err == 0,
        );
        unsafe { libc::kill(stopper, libc::SIGCONT) };
        let _ = wait4_at(stopper, core::ptr::null_mut(), 0, core::ptr::null_mut());
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

    // ---- which *kind* of child a wait may be answered about ----------------
    //
    // `clone`'s low byte is not a flag: it is the signal the child's death
    // should report, and it is what tells the two kinds of child apart at the
    // other end. `fork` is `clone(SIGCHLD)` - a "non-clone" child - and a
    // `clone(0)` is a "clone" child; `wait4` may only be answered about the
    // second kind when the caller sets `__WCLONE`, or `__WALL` for both. The
    // narrowing is not a hint: a caller that did not ask is answered `ECHILD`,
    // the same word as having no child at all - not `WNOHANG`'s 0, and not a
    // park. Measured on a current host first (`.workbuddy/probe_wclone.c`,
    // Linux 7.2.9), and these claims are that file's B, D and G scenarios.
    //
    // Both `__W*` bits are Linux's own private spelling rather than the man
    // page's: they are what the kernel reads, and `docs/uapi/` §3 spells them
    // the same way for the same reason.
    const WCLONE: i32 = 0x8000_0000u32 as i32;
    const WALL: i32 = 0x4000_0000u32 as i32;
    /// The code the clone-kind children exit with, so the status word proves
    /// *which* child was collected and not merely that one was.
    const CLONE_CODE: i32 = 7;

    /// A child started through raw `clone` with `sig` in the low byte - the
    /// byte `fork` always sets to `SIGCHLD` and which is the whole difference
    /// between the two kinds. `code` `None` leaves the child paused until the
    /// parent kills it, which is the fixture for the shapes that are about a
    /// *live* child a wait cannot see.
    ///
    /// Raw `clone` rather than `libc::fork` because `fork` cannot choose the
    /// byte: it is `clone(SIGCHLD)` and nothing else, on every libc.
    fn start_clone(sig: i32, code: Option<i32>) -> i32 {
        let pid = unsafe {
            libc::syscall(
                libc::SYS_clone,
                sig as libc::c_long,
                0 as libc::c_long,
                0 as libc::c_long,
                0 as libc::c_long,
                0 as libc::c_long,
            )
        };
        if pid != 0 {
            return pid as i32;
        }
        match code {
            Some(code) => unsafe {
                // `exit_group` rather than `_exit`: this child inherited the
                // parent's buffered stdout and must not flush a second copy.
                libc::syscall(libc::SYS_exit_group, code as libc::c_long);
                libc::_exit(code)
            },
            None => loop {
                unsafe { libc::pause() };
            },
        }
    }

    // A clone child that exits on its own: invisible to a wait that did not ask
    // for that kind, whatever selector it uses.
    let clone_child = start_clone(0, Some(CLONE_CODE));
    r.check("a clone(0) child was started", clone_child > 0);
    if clone_child > 0 {
        // Let it die before asking, so "invisible" cannot be confused with "not
        // dead yet".
        let nap = libc::timespec {
            tv_sec: 0,
            tv_nsec: 60 * 1_000_000,
        };
        unsafe { libc::nanosleep(&nap, core::ptr::null_mut()) };
        let (answer, _) = wait4_rusage(-1, libc::WNOHANG, core::ptr::null_mut());
        r.check(
            "WNOHANG finds no child to report: a clone child is not one a plain wait sees",
            answer == -1 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ECHILD),
        );
        // Named or not: the option word is what admits a kind, and `waitpid`
        // does not widen it.
        let (answer, _) = wait4_rusage(clone_child, libc::WNOHANG, core::ptr::null_mut());
        r.check(
            "and naming the clone child does not widen it either",
            answer == -1 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ECHILD),
        );
        let (answer, status) = wait4_rusage(-1, WCLONE, core::ptr::null_mut());
        r.check(
            "__WCLONE collects it, with its own exit code",
            answer == clone_child as isize
                && status & 0x7f == 0
                && (status >> 8) & 0xff == CLONE_CODE,
        );
    }

    // The mixed set, which is the shape that tells a filter from a widening: a
    // forked child and a clone child, both alive. The default wait must return
    // the forked one and then say there is nobody left, while the clone child is
    // still running - the answer Linux gives (`.workbuddy/probe_wclone.c` D1,
    // D2).
    let forked = start(cstr(FALSE_PROG), cstr(FALSE_ARG0));
    let mixed = start_clone(0, None);
    r.check(
        "a forked child and a clone child were started together",
        forked > 0 && mixed > 0,
    );
    if forked > 0 && mixed > 0 {
        // `false` exits 1, not 3: the code is what `FALSE_PROG` gives, and the
        // claim below reads it rather than assuming one.
        let (answer, status) = wait4_rusage(-1, 0, core::ptr::null_mut());
        r.check(
            "the default wait returns the forked child, not the clone one",
            answer == forked as isize && status & 0x7f == 0,
        );
        let (answer, _) = wait4_rusage(-1, libc::WNOHANG, core::ptr::null_mut());
        r.check(
            "and then answers ECHILD with the clone child still alive",
            answer == -1 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ECHILD),
        );
        unsafe { libc::kill(mixed, libc::SIGKILL) };
        let (answer, _) = wait4_rusage(-1, libc::WNOHANG, core::ptr::null_mut());
        r.check(
            "which stays invisible after it has died too",
            answer == -1 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ECHILD),
        );
        // `__WALL` is the word that asks for both kinds, and it is also what
        // `strace` waits with.
        let (answer, status) = wait4_rusage(-1, WALL, core::ptr::null_mut());
        r.check(
            "__WALL collects it, and the status word says it was killed",
            answer == mixed as isize && status & 0x7f == libc::SIGKILL,
        );
    }

    // The blocking shape, which is the one a scan can get wrong while every
    // non-blocking answer is right: a wait that may not see its only child must
    // answer at once. The alarm is the guard rather than a hang - a kernel that
    // parks is woken by `SIGALRM` with `EINTR`, which is a failure the claim can
    // report instead of a sweep that stops dead.
    extern "C" fn on_alarm(_sig: libc::c_int) {}
    let parked_only = start_clone(0, None);
    r.check(
        "a lone clone child was started for the blocking claim",
        parked_only > 0,
    );
    if parked_only > 0 {
        let echild = unsafe {
            let mut act: libc::sigaction = core::mem::zeroed();
            act.sa_sigaction = on_alarm as *const () as usize;
            libc::sigemptyset(&mut act.sa_mask);
            // No `SA_RESTART`: the point is for the alarm to *interrupt* the
            // wait if it parked.
            act.sa_flags = 0;
            libc::sigaction(libc::SIGALRM, &act, core::ptr::null_mut());
            libc::alarm(2);
            let (answer, _) = wait4_rusage(-1, 0, core::ptr::null_mut());
            let code = std::io::Error::last_os_error().raw_os_error();
            libc::alarm(0);
            answer == -1 && code == Some(libc::ECHILD)
        };
        r.check(
            "a blocking wait4 with only a clone child answers ECHILD instead of parking",
            echild,
        );
        unsafe { libc::kill(parked_only, libc::SIGKILL) };
        let _ = wait4_rusage(-1, WALL, core::ptr::null_mut());
    }

    r.finish()
}
