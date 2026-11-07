//! t-times - `times()`/`getrusage()` report real per-process CPU time.
//!
//! The contract pinned here is that CPU accounting is measured, not invented.
//! Both calls used to hand back zeros (a busy process reported `0` user time),
//! which is the silent-success `NOTES.md` §4 forbids: a program that times
//! itself - `time`, a shell's job accounting, a benchmark dividing CPU by
//! elapsed for a load factor - was told it spent no CPU doing obvious work.
//! The timer interrupt now charges the running process, split user/system by
//! the ring it interrupted, and these calls report it.
//!
//! The decisive pin is the SPLIT and monotonicity, not the absolute number (the
//! guest's clock is its own and the tick rate need not match a caller's
//! expectation):
//!
//! * a tight *user-mode* spin raises `tms_utime`/`ru_utime` and leaves `stime`
//!   far behind it - the split is what proves the ring attribution is real
//!   (charge everything to the kernel and this fails);
//! * values never decrease across successive reads - no double-counting;
//! * the *elapsed* clock strictly advances across a sleep of known length, and
//!   only monotonically across a spin - because a spin cannot prove more than
//!   that. `times()` returns the wall tick count and `tms_utime` the process's
//!   own CPU tick count, and `handle_times` scales both by the same truncating
//!   `ticks * 100 / TICK_HZ`; the spin below exits when the *CPU* counter
//!   crosses a `USER_HZ` step, which can leave the wall clock inside the step
//!   it has not yet left. Fifty milliseconds is five `USER_HZ` steps and cannot
//!   be swallowed that way, so the strict form of the check lives there;
//! * `times(NULL)` still answers the elapsed clock;
//! * an unknown `getrusage` who is `EINVAL` - and measured, `-2` is too, so
//!   `RUSAGE_CHILDREN`'s `-1` is the only negative the call accepts;
//! * `RUSAGE_CHILDREN` answers success with a *fully zeroed* record (its child
//!   aggregation is the documented `NOTES.md` §12.2 B6 gap, so it is zero, not
//!   an error), and after a child is reaped its CPU folds into that record and
//!   into `times()`'s `tms_cutime`.
//!
//! # The doors, and the record that is not libc's
//!
//! `nanosleep` is `rustix`'s. `times()` and `getrusage()` are not bound by
//! `rustix` at all, so both go through the raw door with the numbers from
//! `libc::SYS_*` - and that is the honest door anyway, because the claims here
//! *are* the numbers on the wire: which tick of which counter moved, that
//! `tv_usec` is a microsecond count and not a tick count wearing its name, and
//! that a byte the kernel never wrote is still the seed this test put there.
//!
//! The `times` record is `libc::tms`, which *is* the uapi's four `clock_t`s.
//! The `getrusage` record is **not** `libc::rusage`: measured, that struct is
//! **272** bytes on this target - a generic-unix superset, thirty `long`s to the
//! uapi's fourteen - so the 144-byte record is spelled out, the way
//! `t-unix-scm` reads `cmsghdr` at its offsets because the two libcs disagree
//! about that struct while the kernel writes one layout regardless. A size
//! claim about `libc::rusage` would be false rather than tight; the real wire
//! claim is the one about every byte coming back written. (It was spelled here
//! until the wait tests needed it too; it is `mpos_user_tests`' now.)
//!
//! # What the migration changed
//!
//! The "child buffer is zeroed, not uninitialised" claim was **vacuous** in the
//! no-std version: it declared `[0u8; 144]`, called the syscall, and checked
//! the buffer was still all zeros - which a kernel that wrote *nothing* would
//! also pass. The migrated claim seeds the whole record with `0xAA` first, so
//! what it pins is the kernel's write. Measured on a host Linux
//! (`.workbuddy/probe_times.c`): the whole 144 bytes come back zero.

use mpos_user_tests::{Report, Rusage, Timeval, host_run};
use rustix::io::Errno;
use rustix::thread::NanosleepRelativeResult;
use rustix::time::Timespec;

/// How many chunks to allow before giving up - a generous ceiling that still
/// keeps the worst-case load bounded. On the guest a single chunk usually
/// crosses a tick, so this rarely iterates.
const MAX_CHUNKS: u32 = 500;

/// The uapi's `struct tms` is four `clock_t`s, and `libc::tms` is that.
const TMS_BYTES: usize = 32;

/// `times(2)`: the elapsed clock, and optionally the four CPU counters.
fn times(buf: Option<&mut libc::tms>) -> libc::clock_t {
    let ptr = match buf {
        Some(b) => b as *mut libc::tms as usize,
        None => 0,
    };
    unsafe { libc::syscall(libc::SYS_times, ptr) as libc::clock_t }
}

/// `getrusage(2)`, over the uapi's record.
fn getrusage(who: libc::c_int, buf: &mut Rusage) -> Result<isize, Errno> {
    let ret = unsafe { libc::syscall(libc::SYS_getrusage, who, buf as *mut Rusage as usize) };
    if ret == -1 {
        Err(Errno::from_raw_os_error(
            std::io::Error::last_os_error().raw_os_error().unwrap_or(0),
        ))
    } else {
        Ok(ret as isize)
    }
}

/// One pure user-mode chunk: arithmetic only, no syscall, so the ticks it spans
/// are charged to `utime`. `black_box` keeps it from being optimised away.
fn burn_chunk() {
    let mut acc: u64 = 1;
    for i in 0..200_000u64 {
        acc = acc.wrapping_mul(31).wrapping_add(i);
        std::hint::black_box(&acc);
    }
    std::hint::black_box(acc);
}

/// Burn chunks until the tick reported by `read` climbs above `base`. Returns
/// the latest reading; the caller asserts it grew. This is the low-footprint
/// way to guarantee at least one tick of user time regardless of core speed,
/// and to stop the moment one lands.
fn burn_until_grew<T: Copy + Ord>(base: T, mut read: impl FnMut() -> T) -> T {
    let mut last = base;
    for _ in 0..MAX_CHUNKS {
        burn_chunk();
        last = read();
        if last > base {
            break;
        }
    }
    last
}

/// Fill a record with a repeating byte, so a field the kernel never writes
/// shows up as the seed rather than as a zero it did not produce.
fn seed<T>(v: &mut T, byte: u8) {
    unsafe { core::ptr::write_bytes(v as *mut T as *mut u8, byte, core::mem::size_of::<T>()) };
}

/// The record's bytes, for a claim about a write that is *not* one of the named
/// fields.
fn bytes_of<T>(v: &T) -> &[u8] {
    unsafe { core::slice::from_raw_parts(v as *const T as *const u8, core::mem::size_of::<T>()) }
}

/// A claim that carries what it saw when it did not hold: these are claims about
/// numbers (a tick count, a record size), so a bare `FAIL` would not say which
/// number arrived.
fn check_with<T: std::fmt::Debug>(r: &mut Report, label: &str, ok: bool, detail: T) {
    if ok {
        r.check(label, true);
    } else {
        r.check(&format!("{label} (got {detail:?})"), false);
    }
}

/// A `timeval` as microseconds, the unit the record's second word must be in.
fn micros(tv: Timeval) -> i64 {
    tv.tv_sec * 1_000_000 + tv.tv_usec
}

/// Fields 14-17 of a `/proc/.../stat` line, in the `USER_HZ` ticks Linux prints
/// them in: `(utime, stime, cutime, cstime)`.
///
/// The split starts at the **last** `)` because field 2 is `(comm)` and a name
/// may hold spaces and parentheses of its own - the same reason Linux's own
/// reader scans for the closing one rather than splitting on whitespace.
fn stat_cpu(path: &str) -> Option<(u64, u64, u64, u64)> {
    let text = std::fs::read_to_string(path).ok()?;
    let rest = &text[text.rfind(')')? + 1..];
    let f: Vec<&str> = rest.split_ascii_whitespace().collect();
    // `f[0]` is field 3 (state), so field 14 is `f[11]`.
    Some((
        f[11].parse().ok()?,
        f[12].parse().ok()?,
        f[13].parse().ok()?,
        f[14].parse().ok()?,
    ))
}

/// One `USER_HZ` tick in microseconds. 100 is ABI on every Linux, and the
/// fields are counts of those ticks, not of this machine's timer ticks.
const USER_HZ_TICK_US: i64 = 10_000;

fn main() {
    let mut r = Report::new();

    // The `times` record's size is the uapi's; `libc::tms` is that struct, and
    // a `libc` whose layout drifted would have the kernel write past the
    // buffer. (The `getrusage` record is this file's own - see `Rusage`.)
    let tms_size = core::mem::size_of::<libc::tms>();
    check_with(
        &mut r,
        "the tms record libc hands the kernel is the uapi's 32 bytes",
        tms_size == TMS_BYTES,
        tms_size,
    );

    // --- times(): the split is real ---
    let mut t0: libc::tms = unsafe { core::mem::zeroed() };
    let elapsed0 = times(Some(&mut t0));
    let utime0 = t0.tms_utime;
    let stime0 = t0.tms_stime;

    let mut t1: libc::tms = unsafe { core::mem::zeroed() };
    let mut elapsed1 = elapsed0;
    let utime1 = burn_until_grew(utime0, || {
        elapsed1 = times(Some(&mut t1));
        t1.tms_utime
    });
    // stime after the same burn; for a pure user spin utime must outgrow it.
    let stime1 = t1.tms_stime;

    r.check(
        "times() returns a non-negative elapsed clock",
        elapsed1 >= 0,
    );
    r.check(
        "the elapsed clock never goes backwards across a spin",
        elapsed1 >= elapsed0,
    );

    // The *strict* advance is pinned where it is deterministic, which the spin
    // above cannot be: its exit condition is the process's own CPU counter
    // crossing a `USER_HZ` step, and both that counter and the wall clock go
    // through the same truncating `ticks * 100 / TICK_HZ`, so the wall reading
    // is allowed to land inside the step the CPU one has just left. A sleep of
    // five steps is not: nothing truncates five steps to nothing.
    let before_sleep = times(None);
    let req = Timespec {
        tv_sec: 0,
        tv_nsec: 50_000_000,
    };
    r.check(
        "a 50 ms sleep answers Ok",
        matches!(rustix::thread::nanosleep(&req), NanosleepRelativeResult::Ok),
    );
    let after_sleep = times(None);
    r.check(
        "the elapsed clock advances across a 50 ms sleep",
        after_sleep > before_sleep,
    );
    r.check(
        "a user-mode spin raises tms_utime (the ring split is real)",
        utime1 > utime0,
    );
    r.check(
        "a user spin charges utime, not stime",
        (utime1 - utime0) > (stime1 - stime0),
    );
    r.check(
        "tms_utime never decreases",
        utime1 >= utime0 && stime1 >= stime0,
    );

    // A null buffer still answers the elapsed clock.
    let null_elapsed = times(None);
    r.check(
        "times(NULL) answers the elapsed clock",
        null_elapsed >= elapsed0,
    );

    // --- getrusage(RUSAGE_SELF): the same CPU as a real timeval ---
    let mut r0: Rusage = unsafe { core::mem::zeroed() };
    match getrusage(libc::RUSAGE_SELF, &mut r0) {
        Ok(0) => r.check("getrusage(RUSAGE_SELF) succeeds", true),
        other => r.check(
            &format!("getrusage(RUSAGE_SELF) succeeds (got {other:?})"),
            false,
        ),
    }
    let ru0 = micros(r0.ru_utime);

    let mut r1: Rusage = unsafe { core::mem::zeroed() };
    let ru1 = burn_until_grew(ru0, || {
        let _ = getrusage(libc::RUSAGE_SELF, &mut r1);
        micros(r1.ru_utime)
    });
    r.check(
        "a user spin raises getrusage ru_utime (as a real timeval)",
        ru1 > ru0,
    );
    // tv_usec is microseconds, not a raw tick count masquerading as one.
    r.check(
        "ru_utime tv_usec is in [0, 1_000_000)",
        (0..1_000_000).contains(&r1.ru_utime.tv_usec),
    );

    // An unknown who is EINVAL, and so is a negative that is not
    // `RUSAGE_CHILDREN` - measured on Linux, where -1 is the only one accepted.
    match getrusage(7, &mut r1) {
        Err(e) if e == Errno::INVAL => r.check("getrusage with an unknown who is EINVAL", true),
        other => r.check(
            &format!("getrusage with an unknown who is EINVAL (got {other:?})"),
            false,
        ),
    }
    match getrusage(-2, &mut r1) {
        Err(e) if e == Errno::INVAL => r.check("getrusage(-2) is EINVAL", true),
        other => r.check(&format!("getrusage(-2) is EINVAL (got {other:?})"), false),
    }

    // --- getrusage(RUSAGE_THREAD): this thread's own CPU, not the group's ---
    //
    // Measured on Linux 7.2.14 (`build/scratch/batch/b.c`): a spinner thread
    // moved `RUSAGE_SELF`'s `ru_utime` 0.033 s -> 0.046 s while `RUSAGE_THREAD`
    // in the joined-but-idle main thread stayed at 0.031 s. So `who == 1` is a
    // valid constant that Linux serves with the *task's* own CPU, and a kernel
    // answering `EINVAL` refuses a shape the caller cannot recover from - while
    // a kernel answering it with the group's totals would pass a "it succeeds"
    // claim and still be a body with the wrong contents.
    let mut th: Rusage = unsafe { core::mem::zeroed() };
    seed(&mut th, 0xAA);
    match getrusage(libc::RUSAGE_THREAD, &mut th) {
        Ok(0) => r.check("getrusage(RUSAGE_THREAD) succeeds", true),
        other => r.check(
            &format!("getrusage(RUSAGE_THREAD) succeeds (got {other:?})"),
            false,
        ),
    }
    let thread_before = micros(th.ru_utime);

    let mut sb: Rusage = unsafe { core::mem::zeroed() };
    let _ = getrusage(libc::RUSAGE_SELF, &mut sb);
    let self_before = micros(sb.ru_utime);

    // The spinner is its own yardstick: it burns until the *group's* user time
    // has grown 20 ms, so the claim does not depend on how fast this machine
    // runs. A ceiling keeps a machine whose CPU is never charged from hanging
    // the program; that shape reddens the growth claim instead.
    let spinner = std::thread::spawn(move || {
        let mut chunks = 0u32;
        loop {
            burn_chunk();
            chunks += 1;
            if chunks % 8 == 0 {
                let mut now: Rusage = unsafe { core::mem::zeroed() };
                if getrusage(libc::RUSAGE_SELF, &mut now).is_ok()
                    && micros(now.ru_utime) >= self_before + 20_000
                {
                    break;
                }
            }
            if chunks >= 4_000 {
                break;
            }
        }
    });
    spinner.join().unwrap();

    let mut sa: Rusage = unsafe { core::mem::zeroed() };
    let _ = getrusage(libc::RUSAGE_SELF, &mut sa);
    let self_after = micros(sa.ru_utime);
    let mut ta: Rusage = unsafe { core::mem::zeroed() };
    let _ = getrusage(libc::RUSAGE_THREAD, &mut ta);
    let thread_after = micros(ta.ru_utime);

    let group_grew = self_after - self_before;
    let thread_grew = thread_after - thread_before;
    r.check(
        &format!("a spinner thread's CPU lands in RUSAGE_SELF (grew {group_grew} us)"),
        group_grew >= 20_000,
    );
    // The group grew at least the 20 ms the spinner waited for; this thread's
    // own record must not carry it. A kernel that answered `RUSAGE_THREAD` with
    // the group's totals would report exactly that growth here - which is why
    // the bound is the spinner's target and not a ratio: an idle thread can
    // still pick up a tick or two of accounting slop, and a claim that reddened
    // on that would be pinning the scheduler rather than the record's owner.
    r.check(
        &format!(
            "and is not charged to this thread's own RUSAGE_THREAD (grew {thread_grew} us, the group {group_grew} us)"
        ),
        thread_grew < 20_000,
    );
    // The other half, so a kernel that answered zero forever cannot pass: this
    // thread's own record moves when *this* thread burns CPU.
    let own_before = thread_after;
    let mut own_now: Rusage = unsafe { core::mem::zeroed() };
    let mut read_own = || {
        let _ = getrusage(libc::RUSAGE_THREAD, &mut own_now);
        micros(own_now.ru_utime)
    };
    let own_after = burn_until_grew(own_before, &mut read_own);
    r.check(
        &format!("this thread's own record grows when this thread burns CPU ({own_after} us)"),
        own_after > own_before,
    );

    // --- reaped-child aggregation: a child's CPU folds into the parent ---
    //
    // A fresh process has waited no children, so its child CPU is zero. Fork a
    // child that burns user CPU and exits; the reap that frees it folds its CPU
    // into this process's child totals, which both `getrusage(RUSAGE_CHILDREN)`
    // and `times()`'s `tms_cutime` then report - Linux accumulates on `wait`.
    let mut rc0: Rusage = unsafe { core::mem::zeroed() };
    seed(&mut rc0, 0xAA);
    match getrusage(libc::RUSAGE_CHILDREN, &mut rc0) {
        Ok(0) => r.check(
            "getrusage(RUSAGE_CHILDREN) succeeds with no children waited yet",
            true,
        ),
        other => r.check(
            &format!(
                "getrusage(RUSAGE_CHILDREN) succeeds with no children waited yet (got {other:?})"
            ),
            false,
        ),
    }
    r.check(
        "and the whole record comes back zeroed - the kernel wrote every byte",
        bytes_of(&rc0).iter().all(|&b| b == 0),
    );
    let cutime0 = micros(rc0.ru_utime);

    let pid = unsafe { libc::fork() };
    if pid == 0 {
        // The child: burn user CPU, then leave. The parent's child totals must
        // then grow. `_exit` is `exit_group` in musl, so no `std` teardown runs
        // in a forked copy.
        let _ = burn_until_grew(0i64, || {
            let mut b: libc::tms = unsafe { core::mem::zeroed() };
            times(Some(&mut b));
            b.tms_utime
        });
        unsafe { libc::_exit(0) };
    }
    r.check("fork of a CPU-burning child succeeded", pid > 0);
    if pid > 0 {
        let mut status = 0i32;
        let w = unsafe { libc::waitpid(pid, &mut status, 0) };
        r.check("the parent waited the child", w == pid);
        // The fold lands on the reap the scheduler runs after the wait, so a
        // short bounded re-read (each pass burns a chunk, letting a tick fire)
        // observes it without depending on a fixed wait.
        let mut cutime1 = cutime0;
        for _ in 0..60 {
            burn_chunk();
            let mut b: Rusage = unsafe { core::mem::zeroed() };
            if getrusage(libc::RUSAGE_CHILDREN, &mut b).is_ok() {
                cutime1 = micros(b.ru_utime);
                if cutime1 > cutime0 {
                    break;
                }
            }
        }
        r.check(
            "a waited child's CPU folds into getrusage(RUSAGE_CHILDREN)",
            cutime1 > cutime0,
        );
        let mut tb: libc::tms = unsafe { core::mem::zeroed() };
        times(Some(&mut tb));
        r.check("and into times() tms_cutime", tb.tms_cutime > 0);

        // --- /proc/<pid>/stat carries the same numbers through procfs ---
        //
        // Measured on Linux 7.2.14 (`build/scratch/batch/s8.c`): field 14
        // tracked `RUSAGE_SELF` while a second thread spun (`66` USER_HZ ticks =
        // 0.667 s) and not the reading thread's own `RUSAGE_THREAD`
        // (0.030 s) - the process file carries the *group's* CPU. A kernel that
        // never counted, or that printed the raw timer ticks, is the shape
        // `ps`/`top` shows as `0:00` for a process that has been busy.
        // The two numbers come from two doors read at two instants, and the
        // group's CPU keeps accruing between them: this process burned chunks
        // above, and on an emulated aarch64 even a handful of syscalls is
        // milliseconds of CPU. So the file's reading is **bracketed** - it must
        // lie between a `RUSAGE_SELF` read taken before the file and one taken
        // after it, each with one tick of slack for the field's truncation
        // toward zero. The shape this replaced compared against `self_after`,
        // a record read before the forked child and the burn loops, and the
        // second machine reddened by the CPU the file had honestly counted and
        // the stale record had not (`110000 us against 88000 us`).
        let mut bracketing: Rusage = unsafe { core::mem::zeroed() };
        let before_us = if getrusage(libc::RUSAGE_SELF, &mut bracketing).is_ok() {
            micros(bracketing.ru_utime)
        } else {
            self_after
        };
        match stat_cpu("/proc/self/stat") {
            Some((utime, _stime, cutime, _cstime)) => {
                let shown_us = utime as i64 * USER_HZ_TICK_US;
                let mut bracketed: Rusage = unsafe { core::mem::zeroed() };
                let after_us = if getrusage(libc::RUSAGE_SELF, &mut bracketed).is_ok() {
                    micros(bracketed.ru_utime)
                } else {
                    before_us
                };
                r.check(
                    &format!(
                        "/proc/self/stat's utime is the group CPU getrusage reports ({shown_us} us, bracketed by {before_us}..{after_us} us)"
                    ),
                    shown_us + USER_HZ_TICK_US >= before_us
                        && shown_us <= after_us + USER_HZ_TICK_US,
                );
                let child_us = cutime as i64 * USER_HZ_TICK_US;
                r.check(
                    &format!(
                        "and its cutime is the waited child's CPU ({child_us} us against {cutime1} us)"
                    ),
                    child_us > 0 && (child_us - cutime1).abs() <= 2 * USER_HZ_TICK_US,
                );
                // The thread spelling is the other half of Linux's `whole`
                // flag: `/proc/<pid>/task/<tid>/stat` carries *this thread's*
                // own CPU. It is checked against `RUSAGE_THREAD` rather than by
                // being smaller than the group's number: when this thread is
                // the one that burned the CPU the two agree to the tick, and a
                // claim demanding `thread < group` reddened on the emulated
                // aarch64 arm for exactly that reason (`5 ticks against the
                // group's 5`, an honest reading failing a shape assertion).
                // Bracket it the way the group's number is bracketed, with the
                // monotone relation stated separately. The host this suite also
                // runs on (`MPOS_ON_HOST=1`) hides thread directories, so the
                // claim is the machine's alone and skipping it there is stated
                // rather than silent.
                if !host_run() {
                    let tid = unsafe { libc::syscall(libc::SYS_gettid) };
                    let mut before_thread: Rusage = unsafe { core::mem::zeroed() };
                    let before_own = if getrusage(libc::RUSAGE_THREAD, &mut before_thread).is_ok() {
                        micros(before_thread.ru_utime)
                    } else {
                        thread_after
                    };
                    match stat_cpu(&format!("/proc/self/task/{tid}/stat")) {
                        Some((thread_utime, _ts, _tc, _tcs)) => {
                            let mut after_thread: Rusage = unsafe { core::mem::zeroed() };
                            let after_own =
                                if getrusage(libc::RUSAGE_THREAD, &mut after_thread).is_ok() {
                                    micros(after_thread.ru_utime)
                                } else {
                                    before_own
                                };
                            let thread_us = thread_utime as i64 * USER_HZ_TICK_US;
                            r.check(
                                &format!(
                                    "and the thread spelling carries this thread's own CPU ({thread_us} us, bracketed by {before_own}..{after_own} us)"
                                ),
                                thread_us + USER_HZ_TICK_US >= before_own
                                    && thread_us <= after_own + USER_HZ_TICK_US,
                            );
                            // A thread's own CPU is at most its group's **at any
                            // one instant**, but these are two files read one after
                            // the other while the counters move: a tick landing
                            // between them makes the later reading the *earlier*
                            // one's past. So the group is read on both sides of the
                            // thread's file and the relation is stated against the
                            // larger of the two - the same bracketing §6.97's rule
                            // asks for, applied to §6.96's fields. Measured twice
                            // on the aarch64 arm: `6 ticks against 5` with the
                            // stale reading, then `7 ticks against 6` with only the
                            // later one.
                            let group_before = utime;
                            let group_after = stat_cpu("/proc/self/stat")
                                .map_or(group_before, |(utime_now, _s, _c, _cs)| utime_now);
                            let group_high = group_before.max(group_after);
                            r.check(
                                &format!(
                                    "and never more than its group's at either end of the read ({thread_utime} ticks against {group_before}..{group_after})"
                                ),
                                thread_utime <= group_high,
                            );
                        }
                        None => r.check(
                            "and the thread spelling carries this thread's own CPU (its stat file did not open)",
                            false,
                        ),
                    }
                }
            }
            None => r.check("/proc/self/stat has a stat line", false),
        }
    }

    r.finish()
}
