//! t-timer-order - the `timer_*` family judges in Linux's order, and its
//! flags word carries one meaningful bit.
//!
//! The five calls of the POSIX interval-timer family all answer two questions
//! at once: *is this request well-formed*, and *is the thing it names there*.
//! Which question is asked first is not cosmetic. A caller probing a refused
//! clock with a `sigevent` pointer it cannot read must hear `EFAULT`, because
//! that is the answer about the bytes it just handed over; hearing `EINVAL`
//! about the clock instead sends it down a fallback for a clock number that
//! was never its problem. Likewise, `timer_settime` on a timer that does not
//! exist must answer `EINVAL` even when the `old_value` pointer is broken -
//! Linux arms the timer *before* copying the old state out, measured as
//! `remaining=3599.999944756` seconds surviving a call that answered `EFAULT`
//! (`build/scratch/timerorder/oldarm.c`, host 7.2.14). A probe that checks the
//! output buffer first both answers the wrong errno and refuses to arm a timer
//! the host would have left running.
//!
//! All of it was measured at the raw-syscall level on a host Linux
//! (`build/scratch/timerorder/{final,truth,gaps,both,oldarm,abstime,edges}.c`,
//! 7.2.14) before this kernel was touched, because a man page does not say
//! which half of a refusal the caller branches on:
//!
//! * `timer_create`: an unreadable `sevp` is `EFAULT(14)` **before** the clock
//!   is judged - `create(999, sevp=1)` and `create(4, sevp=1)` both answer 14,
//!   not `EINVAL(22)`/`EOPNOTSUPP(95)`. A refused clock then outranks a broken
//!   sigevent's *meaning* (`create(4, bad notify)` is 95), but a broken
//!   sigevent's meaning outranks a bad `timer_t*` (`create(0, bad notify,
//!   out=1)` is 22, while `create(0, NULL, out=1)` is 14).
//! * `timer_settime` flags: the word is a 32-bit `unsigned int` whose only
//!   meaningful bit is `TIMER_ABSTIME` (bit 0). Every other bit is ignored and
//!   the timer is armed: `flags = 2, 4, 6, 8, 0x7ffffffe, -2, 2^32|2` all
//!   answer 0 with a live `remaining`. `ABSTIME` itself Linux *arms*; this
//!   kernel keeps one monotonic clock with no `CLOCK_REALTIME` epoch to name an
//!   absolute deadline against, so that bit stays the honest named gap of
//!   §6.124 (`EINVAL`, already pinned in `t-timer-create`).
//! * `timer_settime` order: new-value pointer -> flags -> id and both
//!   `timespec`s -> arm -> `old_value`. A bad id with a bad `old_value` is 22,
//!   a malformed `tv_nsec` with a bad `old_value` is 22, and a good arm with a
//!   bad `old_value` is 14 *yet the timer is armed*.
//! * `timer_gettime`: the id is judged **before** the output buffer -
//!   `gettime(bad, curr=1)` and `gettime(bad, curr=0)` are both 22, while
//!   `gettime(good, curr=1)` is 14. This kernel used to ask the buffer first.
//!
//! Before §6.124 this kernel judged the clock before it had even copied the
//! `sigevent`, refused every flag word carrying a bit past `TIMER_ABSTIME`,
//! checked `old_value` before the id and the timespecs, and read the
//! `curr_value` buffer before it asked whether the timer existed. Each of
//! those is a claim below that is **red** on the unfixed kernel: the
//! counterfactual is a `git stash` of the two source files, a rebuild of the
//! kernel *and* the UEFI loader that embeds it, and a boot of this same binary.
//!
//! The wire records are spelled here rather than taken from a binding, for the
//! reason `t-timer-create` measured: `rustix` and `libc` bind neither
//! `struct sigevent`, `struct itimerspec` nor `timer_t` on Linux, so the calls
//! go through `libc::syscall(libc::SYS_*, ..)` with the numbers from libc's
//! per-machine table and the offsets from the uapi. Each record's size is
//! asserted at compile time.

use mpos_user_tests::{Report, host_run};

/// A pointer no process mapped. The host answers `EFAULT` for it, and this
/// machine's `user_accessible` walks the caller's own tables and does the
/// same, so one constant measures both arms without a host-only branch.
const BAD: usize = 1;

/// An id no timer of this process will ever wear: the table is eight slots,
/// and a deleted id is not reused as a live one (`timer_remove`).
const BAD_ID: usize = 999;

/// The errnos these claims are about, as raw numbers so neither arm's `Errno`
/// spelling can decide a verdict. `OK` is the success sentinel.
const OK: i32 = 0;
const EFAULT: i32 = 14;
const EINVAL: i32 = 22;
const EOPNOTSUPP: i32 = 95;

/// `timer_create`'s `clockid`, from the host's `linux/time.h` rather than a
/// libc macro whose value a libc may reserve for its own helpers.
const CLOCK_REALTIME: usize = 0;
const CLOCK_MONOTONIC_RAW: usize = 4;

/// A bogus clock number: no clock has it.
const NO_CLOCK: usize = 999;

/// The uapi's `struct sigevent`: a `sigval` at 0, the signal number at 8, the
/// notification kind at 12, then a 48-byte union. Sixty-four bytes, the shape
/// a libc passes. This kernel reads the first thirty-two, which is all the
/// family's fields live in.
#[repr(C)]
struct SigEvent {
    value: u64,
    signo: i32,
    notify: i32,
    un: [u8; 48],
}

const _: () = assert!(core::mem::size_of::<SigEvent>() == 64);

impl SigEvent {
    /// A `sigev_notify` no kernel accepts: not `SIGEV_SIGNAL` (0), not
    /// `SIGEV_NONE` (1), not `SIGEV_THREAD_ID` (4).
    fn bad_notify() -> Self {
        Self {
            value: 0,
            signo: libc::SIGALRM,
            notify: 99,
            un: [0; 48],
        }
    }
}

/// The uapi's `struct timespec`, in `c_long` words (both 64-bit targets).
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
    /// A one-shot armed for `millis`. An hour is long enough that no tick
    /// expires while the claims run, on either machine's timer interrupt.
    fn one_shot(millis: i64) -> Self {
        Self {
            interval: TimeSpec::zero(),
            value: TimeSpec::ms(millis),
        }
    }

    /// A `tv_nsec` outside `[0, 1e9)`: the timespec a `settime` must refuse
    /// before it looks at an `old_value` pointer.
    fn bad_nsec() -> Self {
        Self {
            interval: TimeSpec::zero(),
            value: TimeSpec {
                tv_sec: 1,
                tv_nsec: 2_000_000_000,
            },
        }
    }

    fn remaining_ms(&self) -> i64 {
        self.value.tv_sec * 1000 + self.value.tv_nsec / 1_000_000
    }
}

/// A raw syscall, with the refusal as a plain errno rather than an `Errno`
/// whose variant names differ between a host and a bare-metal libc.
fn raw(nr: libc::c_long, a1: usize, a2: usize, a3: usize, a4: usize) -> Result<isize, i32> {
    let ret = unsafe { libc::syscall(nr, a1, a2, a3, a4) };
    if ret == -1 {
        Err(std::io::Error::last_os_error().raw_os_error().unwrap_or(0))
    } else {
        Ok(ret as isize)
    }
}

/// A call's outcome as one comparable number: success is `OK`, a refusal is
/// its errno. Every call in this family answers zero on success.
fn outcome(r: Result<isize, i32>) -> i32 {
    match r {
        Ok(_) => OK,
        Err(e) => e,
    }
}

fn timer_create(clockid: usize, event: usize, out: usize) -> Result<isize, i32> {
    raw(libc::SYS_timer_create, clockid, event, out, 0)
}

fn timer_settime(id: usize, flags: usize, new: usize, old: usize) -> Result<isize, i32> {
    raw(libc::SYS_timer_settime, id, flags, new, old)
}

fn timer_gettime(id: usize, cur: usize) -> Result<isize, i32> {
    raw(libc::SYS_timer_gettime, id, cur, 0, 0)
}

fn timer_delete(id: usize) -> Result<isize, i32> {
    raw(libc::SYS_timer_delete, id, 0, 0, 0)
}

/// One claim about an errno, with the number it actually got in the label so a
/// red log says *which* door answered and with what.
fn want(r: &mut Report, label: &str, got: Result<isize, i32>, expect: i32) {
    let actual = outcome(got);
    r.check(
        &format!("{label} (got {actual}, want {expect})"),
        actual == expect,
    );
}

/// A no-op `SIGALRM` handler. `timer_create(..., event=0)` defaults its
/// notification to `SIGALRM`, and on the *host* the `TIMER_ABSTIME` call below
/// arms an already-expired absolute timer: the signal fires at once. Without a
/// handler its default disposition would kill the probe mid-run. This kernel
/// refuses `TIMER_ABSTIME`, so only the host ever needs this; a no-op handler
/// changes no claim, which are all about errnos and the timer's armed state.
extern "C" fn ignore_alarm(_: libc::c_int) {}

fn install_alarm_handler() {
    unsafe {
        libc::signal(
            libc::SIGALRM,
            ignore_alarm as *const () as libc::sighandler_t,
        );
    }
}

fn main() {
    install_alarm_handler();
    let mut r = Report::new();
    let mut id: i32 = -1;
    let mut spare: i32 = -1;

    // === the two plain doors stay served on both machines ===================
    want(
        &mut r,
        "a plain timer_create is served",
        timer_create(CLOCK_REALTIME, 0, &raw mut id as *mut i32 as usize),
        OK,
    );
    let good = id as usize;
    let hour = Itimerspec::one_shot(3_600_000);
    want(
        &mut r,
        "a plain arm is served",
        timer_settime(good, 0, &raw const hour as *const Itimerspec as usize, 0),
        OK,
    );

    // === timer_create: the sigevent pointer is read before the clock ========
    // A bad `sevp` is EFAULT on the host even for a bogus or refused clock;
    // the unfixed kernel judged the clock first and answered 22 / 95.
    let mut out: i32 = -1;
    let out_at = &raw mut out as *mut i32 as usize;
    want(
        &mut r,
        "an unreadable sigevent outranks a bogus clock",
        timer_create(NO_CLOCK, BAD, out_at),
        EFAULT,
    );
    want(
        &mut r,
        "an unreadable sigevent outranks a refused clock",
        timer_create(CLOCK_MONOTONIC_RAW, BAD, out_at),
        EFAULT,
    );

    // The clock's refusal still outranks a broken sigevent's *meaning*, and a
    // broken sigevent's meaning outranks a bad `timer_t*`. The second of
    // these is red on the unfixed kernel, which checked `out` first.
    let bad_ev = SigEvent::bad_notify();
    let bad_ev_at = &raw const bad_ev as *const SigEvent as usize;
    want(
        &mut r,
        "a refused clock outranks a broken sigevent's meaning",
        timer_create(CLOCK_MONOTONIC_RAW, bad_ev_at, out_at),
        EOPNOTSUPP,
    );
    want(
        &mut r,
        "a broken sigevent outranks a bad timer_t*",
        timer_create(CLOCK_REALTIME, bad_ev_at, BAD),
        EINVAL,
    );
    want(
        &mut r,
        "a bad timer_t* is still EFAULT once the sigevent is fine",
        timer_create(CLOCK_REALTIME, 0, BAD),
        EFAULT,
    );

    // === timer_settime: one meaningful flag bit ============================
    // Every word whose low bit is clear is served and arms the timer, on both
    // arms. The unfixed kernel refused every one of them with EINVAL.
    for (label, flags) in [
        ("an unknown flag bit is ignored", 2usize),
        ("many unknown flag bits are ignored", 0x7fff_fffe),
        (
            "the flags word is 32-bit, so its high bits are ignored",
            0x1_0000_0002,
        ),
    ] {
        let new = Itimerspec::one_shot(3_600_000);
        let set = timer_settime(good, flags, &raw const new as *const Itimerspec as usize, 0);
        let mut cur = Itimerspec::one_shot(0);
        let read = timer_gettime(good, &raw mut cur as *mut Itimerspec as usize);
        let set_ok = set.is_ok();
        let armed = read.is_ok() && cur.remaining_ms() > 0;
        r.check(
            &format!(
                "{label} and the timer is armed (set {set:?}, left {} ms)",
                cur.remaining_ms()
            ),
            set_ok && armed,
        );
    }

    // `TIMER_ABSTIME` stays the honest named gap: EINVAL here, armed on the
    // host. Both arms accept their own answer, as `t-timer-create` does.
    let one_ms = Itimerspec::one_shot(1);
    let abstime = timer_settime(good, 1, &raw const one_ms as *const Itimerspec as usize, 0);
    let held = match abstime {
        Err(e) if e == EINVAL => true,
        Ok(0) if host_run() => true,
        _ => false,
    };
    r.check("TIMER_ABSTIME is still refused here, the named gap", held);

    // === timer_settime: the new pointer, the id, the timespec, then old =====
    want(
        &mut r,
        "a bad new_value pointer is EFAULT before the id",
        timer_settime(BAD_ID, 0, BAD, 0),
        EFAULT,
    );
    let short = Itimerspec::one_shot(100);
    want(
        &mut r,
        "a bad id outranks a bad old_value",
        timer_settime(
            BAD_ID,
            0,
            &raw const short as *const Itimerspec as usize,
            BAD,
        ),
        EINVAL,
    );
    let malformed = Itimerspec::bad_nsec();
    want(
        &mut r,
        "a malformed tv_nsec outranks a bad old_value",
        timer_settime(
            good,
            0,
            &raw const malformed as *const Itimerspec as usize,
            BAD,
        ),
        EINVAL,
    );

    // A *fresh*, never-armed timer is the witness for "the arm happened before
    // the old_value copy-out": the call answers EFAULT either way, but only a
    // kernel that armed first reports a live remaining time afterwards.
    want(
        &mut r,
        "a fresh timer for the arm-before-copy claim is made",
        timer_create(CLOCK_REALTIME, 0, &raw mut spare as *mut i32 as usize),
        OK,
    );
    let fresh = spare as usize;
    let fresh_hour = Itimerspec::one_shot(3_600_000);
    want(
        &mut r,
        "a bad old_value is EFAULT for a good arm",
        timer_settime(
            fresh,
            0,
            &raw const fresh_hour as *const Itimerspec as usize,
            BAD,
        ),
        EFAULT,
    );
    let mut after = Itimerspec::one_shot(0);
    let read = timer_gettime(fresh, &raw mut after as *mut Itimerspec as usize);
    r.check(
        &format!(
            "and that EFAULT still armed the timer (read {read:?}, left {} ms)",
            after.remaining_ms()
        ),
        read.is_ok() && after.remaining_ms() > 0,
    );
    let _ = timer_delete(fresh);

    // === timer_gettime: the id is judged before the output buffer ==========
    want(
        &mut r,
        "a bad id outranks a bad curr_value buffer",
        timer_gettime(BAD_ID, BAD),
        EINVAL,
    );
    want(
        &mut r,
        "a bad id outranks a NULL curr_value too",
        timer_gettime(BAD_ID, 0),
        EINVAL,
    );
    want(
        &mut r,
        "a good id with a bad curr_value is EFAULT",
        timer_gettime(good, BAD),
        EFAULT,
    );
    let mut cur = Itimerspec::one_shot(0);
    want(
        &mut r,
        "a plain timer_gettime is served",
        timer_gettime(good, &raw mut cur as *mut Itimerspec as usize),
        OK,
    );

    // === the id dies with its slot =========================================
    want(&mut r, "timer_delete frees the id", timer_delete(good), OK);
    want(
        &mut r,
        "a deleted id is EINVAL afterwards",
        timer_delete(good),
        EINVAL,
    );

    r.finish();
}
