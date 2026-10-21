//! t-timer-create - the POSIX interval timer family, in the shape a libc
//! actually sends.
//!
//! The five calls are `timer_create`, `timer_settime`, `timer_gettime`,
//! `timer_getoverrun` and `timer_delete`, and the shape that matters is the one
//! musl builds for a *callback* timer: `timer_create(CLOCK_MONOTONIC, ...,
//! SIGEV_THREAD)` becomes a helper thread blocked in a wait, plus a
//! `SIGEV_THREAD_ID` timer aimed at that thread's tid carrying `SIGRTMIN` -
//! which is why this test creates a real thread, blocks `SIGRTMIN` in it, and
//! waits for the expiry with `rt_sigtimedwait`, exactly as musl's helper does.
//!
//! Pinned here, each shape measured on a host Linux at the **raw-syscall** level
//! (`.workbuddy/probe_timer_raw.c`): the libc wrappers answer different
//! questions, because glibc emulates `SIGEV_THREAD` in userspace, so its
//! `timer_create` accepts a value the kernel refuses.
//!
//! * a `clockid` this kernel has no clock for is `EINVAL`;
//! * a `sigev_notify` outside `{NONE, SIGNAL, THREAD_ID}` is `EINVAL` -
//!   including `SIGEV_THREAD` (2), which is a libc value that never reaches
//!   the kernel;
//! * a signal number outside `1..=_NSIG` (64) is `EINVAL`, and so is a
//!   `SIGEV_THREAD_ID` with no tid;
//! * a `SIGEV_NONE` timer counts down, reports its remaining time through
//!   `timer_gettime`, does not fire a signal, and a one-shot that expired
//!   reports zero remaining rather than an error;
//! * a `SIGEV_THREAD_ID` timer carrying `SIGRTMIN` wakes the named thread's
//!   `rt_sigtimedwait` with `si_code == SI_TIMER` and the `sigval` the creator
//!   set - the whole of what musl's helper checks before calling a callback;
//! * a NULL `sigevent` is a `SIGALRM` to the creating task, not a silent timer;
//! * `timer_delete` frees the id, and a deleted id is `EINVAL` afterwards.
//!
//! # The doors
//!
//! `rustix` binds none of this family - no `timer_create`, no `rt_sigtimedwait`,
//! no `rt_sigprocmask` - and `libc` binds no `sigevent`, `itimerspec`, `timer_t`
//! or `SIGEV_*` for linux at all (measured: those exist in the crate for
//! fuchsia, hurd, aix and vxworks only). So the calls go through
//! `libc::syscall(libc::SYS_*, ..)` with the numbers from `libc`'s per-machine
//! table, and the three wire records are spelled here at the uapi's offsets -
//! the same shape `t-unix-scm` takes for `cmsghdr`, and for the same reason: the
//! kernel writes one layout and no maintained binding carries it. Each record's
//! size is asserted at compile time, because the kernel writes all of it.
//!
//! # What the migration found
//!
//! `SIGEV_SIGNAL` is **0** and `SIGEV_NONE` is **1** in `linux/signal.h`, and
//! `libc`'s linux table says so. This kernel had the pair the other way round -
//! and the no-std suite's `lin.rs` had the same swap, so the two agreed with
//! each other and with no libc, and the suite passed. What that cost: a caller
//! sending the real `SIGEV_NONE` (1) was read as `SIGEV_SIGNAL`, and since a
//! silent timer leaves `sigev_signo` at 0 it was refused **`EINVAL`** - a
//! `timer_create` that could never succeed; and a caller sending the real
//! `SIGEV_SIGNAL` (0) got `SIGEV_NONE`, a timer that silently never fired. The
//! two claims below that name the numbers (`SIGEV_NONE (1)` is accepted, and a
//! `SIGEV_NONE` expiry queues nothing) are what found it.
//!
//! Two of the no-std claims are *divergences* rather than translations, both
//! about clocks this kernel does not keep: `CLOCK_PROCESS_CPUTIME_ID` answers
//! `EOPNOTSUPP` here, where Linux serves it (measured: 0), and `TIMER_ABSTIME`
//! is refused because this kernel keeps one clock and has no absolute domain to
//! measure a deadline against, where Linux arms the timer. Both accept the
//! host's own answer under `MPOS_ON_HOST=1`, printing what it was.

use mpos_user_tests::{Report, host_run};
use rustix::io::Errno;
use rustix::time::Timespec as RustixTimespec;
use std::sync::atomic::{AtomicI32, AtomicU64, Ordering};

/// What the waiting thread saw: an `si_code` (0 means "nothing yet"), and the
/// `sigval` word the timer carried.
static THREAD_CODE: AtomicI32 = AtomicI32::new(0);
static THREAD_VALUE: AtomicU64 = AtomicU64::new(0);
static THREAD_RAN: AtomicI32 = AtomicI32::new(0);

/// The payload the timer must carry to whoever waits for it.
const PAYLOAD: u64 = 0x7469_6d65_7231_3233;

/// The kernel's `SIGRTMIN`, and its `_NSIG`: real-time signals start at 32 and
/// the table ends at 64. Written down rather than taken from `libc::SIGRTMIN()`,
/// which is a *libc's* reservation (musl's is 35, glibc's 34) and not the
/// kernel's floor - and it is the kernel's answers these claims are about. This
/// kernel queues exactly this one real-time number.
const SIGRTMIN: i32 = 32;
const NSIG: i32 = 64;

/// `SIGRTMIN`'s bit in the wire's mask: signal *n* is bit *n-1*.
const RTMIN_BIT: u64 = 1u64 << (SIGRTMIN - 1);

/// `SIGALRM`'s bit in the wire's mask.
const SIGALRM_BIT: u64 = 1u64 << (libc::SIGALRM - 1);

/// The uapi's `struct sigevent`: a `sigval` at 0, the signal number at 8, the
/// notification kind at 12, then a 48-byte union whose first `int` is the
/// `SIGEV_THREAD_ID` tid. Sixty-four bytes, which is what a libc passes.
#[repr(C)]
struct SigEvent {
    value: u64,
    signo: i32,
    notify: i32,
    /// `_sigev_un`: `int _pad[12]` / `int _tid` / the thread-function pair.
    un: [u8; 48],
}

const _: () = assert!(core::mem::size_of::<SigEvent>() == 64);

impl SigEvent {
    /// A silent timer: no signal, no payload.
    fn none() -> Self {
        Self {
            value: 0,
            signo: 0,
            notify: libc::SIGEV_NONE,
            un: [0; 48],
        }
    }

    /// A timer that signals the creating task (or, with a tid, one thread).
    fn signal(signo: i32) -> Self {
        Self {
            value: 0,
            signo,
            notify: libc::SIGEV_SIGNAL,
            un: [0; 48],
        }
    }

    /// A timer aimed at one thread of the creating task.
    fn thread_id(signo: i32, tid: i32, value: u64) -> Self {
        let mut ev = Self {
            value,
            signo,
            notify: libc::SIGEV_THREAD_ID,
            un: [0; 48],
        };
        ev.set_tid(tid);
        ev
    }

    fn set_tid(&mut self, tid: i32) {
        self.un[..4].copy_from_slice(&tid.to_ne_bytes());
    }
}

/// The uapi's `struct timeval`/`timespec` word pair, spelled with `libc`'s
/// `c_long` (which is what musl's `time_t`/`suseconds_t` are on these two
/// 64-bit targets) so no deprecated alias is named.
#[repr(C)]
#[derive(Copy, Clone)]
struct TimeSpec {
    tv_sec: libc::c_long,
    tv_nsec: libc::c_long,
}

impl TimeSpec {
    fn zero() -> Self {
        Self {
            tv_sec: 0,
            tv_nsec: 0,
        }
    }

    fn ms(millis: i64) -> Self {
        Self {
            tv_sec: millis / 1000,
            tv_nsec: (millis % 1000) * 1_000_000,
        }
    }
}

/// The uapi's `struct itimerspec`: the reload value, then the first expiry.
#[repr(C)]
struct Itimerspec {
    interval: TimeSpec,
    value: TimeSpec,
}

const _: () = assert!(core::mem::size_of::<Itimerspec>() == 32);

impl Itimerspec {
    /// A one-shot armed for `millis`.
    fn one_shot(millis: i64) -> Self {
        Self {
            interval: TimeSpec::zero(),
            value: TimeSpec::ms(millis),
        }
    }

    /// The remaining time in whole milliseconds.
    fn remaining_ms(&self) -> i64 {
        self.value.tv_sec * 1000 + self.value.tv_nsec / 1_000_000
    }
}

/// The uapi's `siginfo_t`: three `int`s, four bytes of padding, then the
/// per-signal union at 16. `SI_TIMER`'s arm is `{ int _tid; int _overrun;
/// sigval_t _sigval; ... }`, so the payload the timer carried is at 24 -
/// which is where the no-std record read it too.
#[repr(C)]
struct Siginfo {
    signo: i32,
    errno: i32,
    code: i32,
    _pad: i32,
    fields: [u8; 112],
}

const _: () = assert!(core::mem::size_of::<Siginfo>() == 128);

impl Siginfo {
    fn zeroed() -> Self {
        unsafe { core::mem::zeroed() }
    }

    /// The `sigval` the timer handed over, as the raw word it is.
    fn payload(&self) -> u64 {
        u64::from_ne_bytes(self.fields[8..16].try_into().expect("8 bytes"))
    }
}

/// A raw syscall through `libc`'s per-machine number table, with the refusal in
/// `Errno` rather than in a stale `errno`.
fn raw(nr: libc::c_long, a1: usize, a2: usize, a3: usize, a4: usize) -> Result<isize, Errno> {
    let ret = unsafe { libc::syscall(nr, a1, a2, a3, a4) };
    if ret == -1 {
        Err(Errno::from_raw_os_error(
            std::io::Error::last_os_error().raw_os_error().unwrap_or(0),
        ))
    } else {
        Ok(ret as isize)
    }
}

fn timer_create(clockid: i32, event: *const SigEvent, id: &mut i32) -> Result<isize, Errno> {
    raw(
        libc::SYS_timer_create,
        clockid as usize,
        event as usize,
        id as *mut i32 as usize,
        0,
    )
}

fn timer_settime(id: i32, flags: usize, new: &Itimerspec) -> Result<isize, Errno> {
    raw(
        libc::SYS_timer_settime,
        id as usize,
        flags,
        new as *const Itimerspec as usize,
        0,
    )
}

fn timer_gettime(id: i32, cur: &mut Itimerspec) -> Result<isize, Errno> {
    raw(
        libc::SYS_timer_gettime,
        id as usize,
        cur as *mut Itimerspec as usize,
        0,
        0,
    )
}

fn timer_getoverrun(id: i32) -> Result<isize, Errno> {
    raw(libc::SYS_timer_getoverrun, id as usize, 0, 0, 0)
}

fn timer_delete(id: i32) -> Result<isize, Errno> {
    raw(libc::SYS_timer_delete, id as usize, 0, 0, 0)
}

/// `rt_sigprocmask(how, set, old, sigsetsize)`: the mask is one `u64` on the
/// wire - 64 signals, 8 bytes - which is this kernel's `MAX_SIGNAL` too.
fn sigmask(how: i32, set: u64) -> Result<isize, Errno> {
    raw(
        libc::SYS_rt_sigprocmask,
        how as usize,
        &raw const set as usize,
        0,
        8,
    )
}

/// `rt_sigpending(set, sigsetsize)`: which signals are queued for this task.
fn sigpending() -> Result<u64, Errno> {
    let mut set: u64 = 0;
    raw(libc::SYS_rt_sigpending, &raw mut set as usize, 8, 0, 0)?;
    Ok(set)
}

/// `rt_sigtimedwait(set, info, timeout, sigsetsize)`, with a null timeout the
/// blocking form musl's `SIGEV_THREAD` helper parks in.
fn sigtimedwait(set: u64, info: &mut Siginfo) -> Result<isize, Errno> {
    raw(
        libc::SYS_rt_sigtimedwait,
        &raw const set as usize,
        info as *mut Siginfo as usize,
        0,
        8,
    )
}

/// The *polling* form - a zero timeout, not a null one: a null timeout parks
/// when there is no queued record to take, which is a hang rather than an
/// answer for a claim that is about whether the record is there.
fn sigtimedwait_poll(set: u64, info: &mut Siginfo) -> Result<isize, Errno> {
    let zero = TimeSpec::zero();
    raw(
        libc::SYS_rt_sigtimedwait,
        &raw const set as usize,
        info as *mut Siginfo as usize,
        &raw const zero as usize,
        8,
    )
}

/// The calling thread's tid, through the maintained table like the rest.
fn gettid() -> i32 {
    unsafe { libc::syscall(libc::SYS_gettid) as i32 }
}

/// Sleep for `millis`: `rustix`'s relative `nanosleep`, which is the call this
/// kernel serves (an absolute `clock_nanosleep` is not).
fn sleep_ms(millis: i64) {
    let ts = RustixTimespec {
        tv_sec: millis / 1000,
        tv_nsec: (millis % 1000) * 1_000_000,
    };
    let _ = rustix::thread::nanosleep(&ts);
}

/// A claim about a refusal, spelled with the answer when it is the wrong one.
fn err_is(r: &mut Report, label: &str, got: Result<isize, Errno>, want: Errno) {
    match got {
        Err(e) if e == want => r.check(label, true),
        other => r.check(&format!("{label} (got {other:?}, want {want})"), false),
    }
}

/// A claim about a success value.
fn ok_is(r: &mut Report, label: &str, got: Result<isize, Errno>, want: isize) {
    match got {
        Ok(v) if v == want => r.check(label, true),
        other => r.check(&format!("{label} (got {other:?}, want {want})"), false),
    }
}

/// A claim that carries what it saw when it did not hold.
fn check_with<T: std::fmt::Debug>(r: &mut Report, label: &str, ok: bool, detail: T) {
    if ok {
        r.check(label, true);
    } else {
        r.check(&format!("{label} (got {detail:?})"), false);
    }
}

/// A `SIGEV_NONE` timer armed for `arm_ms`, then read back after `wait_ms`:
/// how many whole milliseconds of `remaining` it reported, the overrun count,
/// and the `timer_gettime` answer - or a negative errno for either call. The
/// timer is deleted before returning, so a run leaves no slot behind.
fn none_timer_remaining_after(arm_ms: i64, wait_ms: i64) -> (Result<isize, Errno>, i64, isize) {
    let event = SigEvent::none();
    let mut id: i32 = -1;
    let created = timer_create(libc::CLOCK_MONOTONIC, &event, &mut id);
    if created != Ok(0) {
        return (created, -1, -2);
    }
    let armed = timer_settime(id, 0, &Itimerspec::one_shot(arm_ms));
    if armed != Ok(0) {
        let _ = timer_delete(id);
        return (armed, -1, -2);
    }
    sleep_ms(wait_ms);
    let mut now = Itimerspec {
        interval: TimeSpec::zero(),
        value: TimeSpec::zero(),
    };
    let read = timer_gettime(id, &mut now);
    let overrun = timer_getoverrun(id).unwrap_or(-1);
    let _ = timer_delete(id);
    (read, now.remaining_ms(), overrun)
}

fn main() {
    let mut r = Report::new();

    // === the refusals =======================================================
    let silent = SigEvent::none();
    let mut id: i32 = -1;

    // Measured on Linux 6.x at the raw-syscall level: `timer_create` on a
    // CPU-time clock *succeeds* there (0), so this kernel's `EOPNOTSUPP` is its
    // own answer - it keeps no per-task CPU accounting, and a timer that was
    // accepted and silently never fired would be worse than one that says so.
    let cputime = timer_create(2 /* CLOCK_PROCESS_CPUTIME_ID */, &silent, &mut id);
    match cputime {
        Err(e) if e == Errno::OPNOTSUPP => {
            r.check("a clock with no accounting behind it is EOPNOTSUPP", true)
        }
        Ok(0) if host_run() => {
            println!(
                "note: this host's kernel serves CLOCK_PROCESS_CPUTIME_ID, where mpos \
                 keeps no per-task CPU accounting and refuses it"
            );
            r.check("a clock with no accounting behind it is EOPNOTSUPP", true);
        }
        other => r.check(
            &format!("a clock with no accounting behind it is EOPNOTSUPP (got {other:?})"),
            false,
        ),
    }

    // A clock id that names no clock at all is EINVAL, as on Linux.
    err_is(
        &mut r,
        "a clock id no clock has is EINVAL",
        timer_create(99, &silent, &mut id),
        Errno::INVAL,
    );

    // `SIGEV_THREAD` is a libc value the kernel never sees: musl builds a helper
    // thread and a `SIGEV_THREAD_ID` timer underneath it. Measured raw: EINVAL.
    let mut thread_event = SigEvent::none();
    thread_event.notify = libc::SIGEV_THREAD;
    err_is(
        &mut r,
        "SIGEV_THREAD (2) is EINVAL at the kernel",
        timer_create(libc::CLOCK_MONOTONIC, &thread_event, &mut id),
        Errno::INVAL,
    );

    // A signal number of 0 with the signalling kind is EINVAL.
    err_is(
        &mut r,
        "a signal number of 0 is EINVAL",
        timer_create(libc::CLOCK_MONOTONIC, &SigEvent::signal(0), &mut id),
        Errno::INVAL,
    );

    // Inside `SIGRTMIN..=_NSIG`: accepted. Measured: 32, 40 and 64 all answer 0.
    let mut rt: i32 = -1;
    ok_is(
        &mut r,
        "a real-time signal number (40) is accepted",
        timer_create(libc::CLOCK_MONOTONIC, &SigEvent::signal(40), &mut rt),
        0,
    );
    if rt >= 0 {
        let _ = timer_delete(rt);
    }

    err_is(
        &mut r,
        "a signal number past _NSIG (64) is EINVAL",
        timer_create(libc::CLOCK_MONOTONIC, &SigEvent::signal(NSIG + 1), &mut id),
        Errno::INVAL,
    );

    err_is(
        &mut r,
        "SIGEV_THREAD_ID without a tid is EINVAL",
        timer_create(
            libc::CLOCK_MONOTONIC,
            &SigEvent::thread_id(SIGRTMIN, 0, 0),
            &mut id,
        ),
        Errno::INVAL,
    );

    // === SIGEV_NONE is the number Linux gives it ============================
    // The pair that used to be swapped: 1 is the silent kind, 0 is the
    // signalling one. A silent timer leaves `sigev_signo` at 0, which is why
    // the swap made this create answer EINVAL.
    {
        let blocked = sigmask(libc::SIG_BLOCK, SIGALRM_BIT);
        check_with(
            &mut r,
            "SIGALRM is blocked for the silent-timer check",
            blocked == Ok(0),
            blocked,
        );
        let mut none_id: i32 = -1;
        ok_is(
            &mut r,
            "SIGEV_NONE (1) is accepted with no signo set",
            timer_create(libc::CLOCK_MONOTONIC, &SigEvent::none(), &mut none_id),
            0,
        );
        if none_id >= 0 {
            ok_is(
                &mut r,
                "the silent timer arms",
                timer_settime(none_id, 0, &Itimerspec::one_shot(10)),
                0,
            );
            // A budget: the timer is armed 10ms out, and the queue is read
            // after it must have fired (`docs/notes/` §15.93).
            sleep_ms(30);
            let pending = sigpending();
            check_with(
                &mut r,
                "a SIGEV_NONE expiry queues no signal",
                pending.is_ok_and(|p| p & SIGALRM_BIT == 0),
                pending,
            );
            ok_is(&mut r, "the silent timer deletes", timer_delete(none_id), 0);
        }
        let unblocked = sigmask(libc::SIG_UNBLOCK, SIGALRM_BIT);
        check_with(
            &mut r,
            "SIGALRM is unblocked again",
            unblocked == Ok(0),
            unblocked,
        );
    }

    // === a NULL sigevent is SIGEV_SIGNAL + SIGALRM ==========================
    //
    // Measured at the raw level: a NULL `evp` creates a timer whose expiry
    // queues `SIGALRM` - not a silent `SIGEV_NONE` timer, which is what reading
    // only "create succeeded" would suggest. Blocked `SIGALRM` makes that
    // visible in `rt_sigpending`, and the record is taken with the *polling*
    // form of `rt_sigtimedwait` (a zero timeout, not a null one - see
    // [`sigtimedwait_poll`]), so neither block here can hang.
    {
        let blocked = sigmask(libc::SIG_BLOCK, SIGALRM_BIT);
        check_with(
            &mut r,
            "SIGALRM is blocked for the null-event checks",
            blocked == Ok(0),
            blocked,
        );
        let mut null_id: i32 = -1;
        ok_is(
            &mut r,
            "a null sigevent is accepted",
            timer_create(libc::CLOCK_MONOTONIC, core::ptr::null(), &mut null_id),
            0,
        );
        if null_id >= 0 {
            ok_is(
                &mut r,
                "the null-event timer arms",
                timer_settime(null_id, 0, &Itimerspec::one_shot(10)),
                0,
            );
            // A budget: the timer is armed 10ms out, and the queue is read
            // after it must have fired (`docs/notes/` §15.93).
            sleep_ms(30);
            let pending = sigpending();
            check_with(
                &mut r,
                "the expiry queued SIGALRM (a null event is SIGEV_SIGNAL/SIGALRM)",
                pending.is_ok_and(|p| p & SIGALRM_BIT != 0),
                pending,
            );
            // Take it out before unblocking: unblocking a pending, unhandled
            // signal runs its default action, and the default action of SIGALRM
            // is to end this process.
            let mut seen = Siginfo::zeroed();
            let taken = sigtimedwait_poll(SIGALRM_BIT, &mut seen);
            check_with(
                &mut r,
                "the queued SIGALRM can be taken without running a handler",
                taken == Ok(libc::SIGALRM as isize),
                taken,
            );
            check_with(
                &mut r,
                "and its record is the timer's SI_TIMER",
                seen.code == libc::SI_TIMER,
                seen.code,
            );
            ok_is(
                &mut r,
                "the null-event timer deletes",
                timer_delete(null_id),
                0,
            );
        }

        // === one divergence, named: a deleted timer's queued record =========
        //
        // Measured on a host Linux (`.workbuddy/probe_timer_delete.c`): after
        // deleting a timer whose expiry had already queued `SIGALRM`, the
        // pending *bit* is still set in `sigpending`, but there is no queued
        // record behind it - the delete freed the `sigqueue` - so the polling
        // take answers `EAGAIN`. This kernel keeps the record and the take
        // succeeds. The claim is this kernel's answer, which is the one the
        // no-std suite pinned; a host run prints what its own kernel did.
        let mut gone_id: i32 = -1;
        ok_is(
            &mut r,
            "a timer whose record outlives it is made",
            timer_create(libc::CLOCK_MONOTONIC, core::ptr::null(), &mut gone_id),
            0,
        );
        if gone_id >= 0 {
            ok_is(
                &mut r,
                "it arms",
                timer_settime(gone_id, 0, &Itimerspec::one_shot(10)),
                0,
            );
            sleep_ms(30);
            ok_is(
                &mut r,
                "it deletes with its record still queued",
                timer_delete(gone_id),
                0,
            );
            let mut seen = Siginfo::zeroed();
            match sigtimedwait_poll(SIGALRM_BIT, &mut seen) {
                Ok(v) if v == libc::SIGALRM as isize => {
                    r.check("a deleted timer's queued record is still takeable", true)
                }
                Err(e) if e == Errno::AGAIN && host_run() => {
                    println!(
                        "note: this host's kernel frees a deleted timer's `sigqueue`, so \
                         `sigpending` still shows the SIGALRM bit while the take answers \
                         EAGAIN; mpos keeps the record"
                    );
                    r.check("a deleted timer's queued record is still takeable", true);
                }
                other => r.check(
                    &format!("a deleted timer's queued record is still takeable (got {other:?})"),
                    false,
                ),
            }
        }

        let unblocked = sigmask(libc::SIG_UNBLOCK, SIGALRM_BIT);
        check_with(
            &mut r,
            "SIGALRM is unblocked again",
            unblocked == Ok(0),
            unblocked,
        );
    }

    // === a SIGEV_NONE timer: it counts, it reports, it stays silent =========
    //
    // The remaining time comes back at the machine's tick granularity and is
    // rounded *up* when armed, so a 50 ms request can read as a little more -
    // the timer never fires before its deadline. The bounds say that rather
    // than pretending to nanosecond precision the clock does not have.
    let (made, remaining, overrun) = none_timer_remaining_after(50, 0);
    ok_is(&mut r, "a SIGEV_NONE timer is made and armed", made, 0);
    check_with(
        &mut r,
        "its remaining time is within the 50 ms it was armed for, plus a tick",
        (0..=60).contains(&remaining),
        remaining,
    );
    let (read, expired, overrun_expired) = none_timer_remaining_after(50, 80);
    ok_is(&mut r, "reading an expired timer succeeds", read, 0);
    check_with(
        &mut r,
        "a one-shot that expired reports zero remaining",
        expired == 0,
        expired,
    );
    check_with(
        &mut r,
        "a SIGEV_NONE timer counts no overruns",
        overrun == 0 && overrun_expired == 0,
        (overrun, overrun_expired),
    );

    // The id dies with the timer.
    let mut victim: i32 = -1;
    ok_is(
        &mut r,
        "a timer to delete is made",
        timer_create(libc::CLOCK_MONOTONIC, &silent, &mut victim),
        0,
    );
    ok_is(&mut r, "timer_delete succeeds", timer_delete(victim), 0);
    err_is(
        &mut r,
        "a deleted id is EINVAL for timer_settime",
        timer_settime(victim, 0, &Itimerspec::one_shot(1)),
        Errno::INVAL,
    );
    let mut now = Itimerspec {
        interval: TimeSpec::zero(),
        value: TimeSpec::zero(),
    };
    err_is(
        &mut r,
        "a deleted id is EINVAL for timer_gettime",
        timer_gettime(victim, &mut now),
        Errno::INVAL,
    );

    // === TIMER_ABSTIME: the honest refusal, measured against a host =========
    let mut abs_id: i32 = -1;
    ok_is(
        &mut r,
        "a timer for the abstime check is made",
        timer_create(libc::CLOCK_MONOTONIC, &silent, &mut abs_id),
        0,
    );
    let abstime = timer_settime(abs_id, 1 /* TIMER_ABSTIME */, &Itimerspec::one_shot(1));
    match abstime {
        Err(e) if e == Errno::INVAL => r.check(
            "TIMER_ABSTIME is EINVAL (one clock, no absolute domain)",
            true,
        ),
        Ok(0) if host_run() => {
            println!(
                "note: this host's kernel arms an absolute timer, where mpos has one clock and no absolute domain"
            );
            r.check(
                "TIMER_ABSTIME is EINVAL (one clock, no absolute domain)",
                true,
            );
        }
        other => r.check(
            &format!("TIMER_ABSTIME is EINVAL (one clock, no absolute domain) (got {other:?})"),
            false,
        ),
    }
    let _ = timer_delete(abs_id);

    // === the musl shape: a thread, SIGRTMIN, SIGEV_THREAD_ID ================
    //
    // The creator blocks SIGRTMIN *before* the thread exists, so the thread
    // inherits the block and the expiry can never run a default action.
    let blocked = sigmask(libc::SIG_BLOCK, RTMIN_BIT);
    check_with(
        &mut r,
        "the creator blocks SIGRTMIN",
        blocked == Ok(0),
        blocked,
    );

    THREAD_CODE.store(0, Ordering::SeqCst);
    THREAD_VALUE.store(0, Ordering::SeqCst);
    THREAD_RAN.store(0, Ordering::SeqCst);
    THREAD_TID.store(0, Ordering::SeqCst);
    let waiter = std::thread::Builder::new()
        .name("t-timer-waiter".into())
        .spawn(|| {
            // The thread's whole life: publish its tid (the timer must be aimed
            // at it, and it is the only one who knows it), then wait on the
            // blocking form of `rt_sigtimedwait` for `SIGRTMIN` - the exact call
            // musl's `SIGEV_THREAD` helper parks in - and record what arrived.
            // The mask was inherited from the creator, so there is no window in
            // which the signal could run a default action instead.
            THREAD_TID.store(gettid(), Ordering::SeqCst);
            let mut info = Siginfo::zeroed();
            let got = sigtimedwait(RTMIN_BIT, &mut info);
            if got == Ok(SIGRTMIN as isize) {
                THREAD_VALUE.store(info.payload(), Ordering::SeqCst);
                THREAD_CODE.store(
                    if info.code == 0 { -1 } else { info.code },
                    Ordering::SeqCst,
                );
            } else {
                THREAD_CODE.store(-1000, Ordering::SeqCst);
            }
            THREAD_RAN.store(1, Ordering::SeqCst);
        });

    match waiter {
        Ok(handle) => {
            let tid = wait_for_tid();
            check_with(&mut r, "the waiting thread was created", tid > 0, tid);
            if tid > 0 {
                let mut timer_id: i32 = -1;
                ok_is(
                    &mut r,
                    "a SIGEV_THREAD_ID timer aimed at the thread is made",
                    timer_create(
                        libc::CLOCK_MONOTONIC,
                        &SigEvent::thread_id(SIGRTMIN, tid, PAYLOAD),
                        &mut timer_id,
                    ),
                    0,
                );
                if timer_id >= 0 {
                    ok_is(
                        &mut r,
                        "the 10 ms timer is armed",
                        timer_settime(timer_id, 0, &Itimerspec::one_shot(10)),
                        0,
                    );
                    // Wait for the thread to be woken by the expiry. A bounded
                    // wait: a timer that never fires must fail the test, not
                    // hang it.
                    let mut waited = 0;
                    while THREAD_CODE.load(Ordering::SeqCst) == 0 && waited < 2000 {
                        sleep_ms(1);
                        waited += 1;
                    }
                    check_with(
                        &mut r,
                        "the thread was woken by the timer with SI_TIMER",
                        THREAD_CODE.load(Ordering::SeqCst) == libc::SI_TIMER,
                        THREAD_CODE.load(Ordering::SeqCst),
                    );
                    check_with(
                        &mut r,
                        "the sigval the creator set arrived with it",
                        THREAD_VALUE.load(Ordering::SeqCst) == PAYLOAD,
                        THREAD_VALUE.load(Ordering::SeqCst),
                    );
                    let mut now = Itimerspec {
                        interval: TimeSpec::zero(),
                        value: TimeSpec::zero(),
                    };
                    ok_is(
                        &mut r,
                        "the timer is one-shot, so it read zero",
                        timer_gettime(timer_id, &mut now),
                        0,
                    );
                    ok_is(
                        &mut r,
                        "the thread's timer is deleted",
                        timer_delete(timer_id),
                        0,
                    );
                }
            }
            // Let the thread finish before unblocking, so the process never
            // sees a pending SIGRTMIN with no disposition. Bounded, and the
            // join is only taken once it has: a thread still parked in
            // `rt_sigtimedwait` (a timer that never fired) must fail this test
            // rather than hang it, and `finish` ends the process for it.
            let mut spins = 0usize;
            while THREAD_RAN.load(Ordering::SeqCst) == 0 && spins < 5_000_000 {
                spins += 1;
                std::hint::spin_loop();
            }
            if THREAD_RAN.load(Ordering::SeqCst) != 0 {
                let _ = handle.join();
            }
        }
        Err(e) => r.check(&format!("the waiting thread was created ({e})"), false),
    }

    let unblocked = sigmask(libc::SIG_UNBLOCK, RTMIN_BIT);
    check_with(
        &mut r,
        "SIGRTMIN is unblocked again",
        unblocked == Ok(0),
        unblocked,
    );

    r.finish()
}

/// The tid the waiting thread publishes about itself, once it has one.
///
/// `std` names a thread by `ThreadId`, not by the kernel's tid, and the tid is
/// not readable from outside on this machine (its procfs has no `task/`). So the
/// number comes from the thread's own `gettid`, published before it parks.
static THREAD_TID: AtomicI32 = AtomicI32::new(0);

fn wait_for_tid() -> i32 {
    let mut waited = 0;
    while THREAD_TID.load(Ordering::SeqCst) == 0 && waited < 2000 {
        sleep_ms(1);
        waited += 1;
    }
    THREAD_TID.load(Ordering::SeqCst)
}
