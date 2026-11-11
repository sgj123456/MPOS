//! t-itimer-order - `setitimer`/`getitimer` judge in Linux's order, name a
//! clock with a 32-bit `int`, and arm before they copy the old value out.
//!
//! These two calls are the older interval-timer ABI, and they answer three
//! questions at once: *is this request well-formed*, *which clock does it
//! name*, and *what did the previous timer look like*. Which question is asked
//! first is not cosmetic. A caller whose `new_value` pointer is unreadable must
//! hear `EFAULT` even when its `which` is nonsense, because that is the answer
//! about the bytes it just handed over; hearing `EINVAL` about the clock sends
//! it down a fallback for a number that was never its problem. And a call that
//! answers `EFAULT` because `old_value` is broken has still **armed** the timer
//! on Linux - a probe that checks the output buffer up front returns the same
//! errno while leaving the machine in a different state.
//!
//! All of it was measured at the raw-syscall level on a host Linux
//! (`build/scratch/itimer/{width,final,side,nullvalue,order3}.c`, 7.2.14)
//! before this kernel was touched, because a man page does not say which half
//! of a refusal the caller branches on:
//!
//! * `which` is a 32-bit `int`, not the `usize` register it arrives in:
//!   `which = 2^32` names `ITIMER_REAL` and arms it (`rc = 0`, a following
//!   `getitimer` reads `armed=yes it_value=9.999976`), `which = 2^32 + 3` is
//!   `EINVAL(22)`, `getitimer(2^32) = 0`, and the sign-extended `-1`
//!   (`0xffff_ffff_ffff_ffff`) is `EINVAL`. Before §6.125 this kernel compared
//!   the whole register with `which > ITIMER_PROF`, so `2^32` was refused where
//!   Linux armed a timer, and a large word would have indexed past the
//!   three-slot timer array.
//! * `setitimer`'s order is `new_value` pointer -> `timeval` shape -> `which`
//!   -> **arm** -> `old_value` copy. `width.c` measured `setitimer(3, BAD, 0)`
//!   and `setitimer(-1, BAD, 0)` both answering `EFAULT(14)`, not the `EINVAL`
//!   of the bogus clock; `final.c` measured `setitimer(0, bad tv_usec, old=BAD)`
//!   answering `EINVAL(22)`, not the `EFAULT` of the bad old pointer.
//! * `side.c` is the side effect that makes the last step load-bearing:
//!   starting disarmed, `setitimer(0, {10s}, old = BAD)` answers `EFAULT(14)`
//!   yet a following `getitimer` reads `armed=yes it_value=9.999998`. Linux
//!   armed the timer before it failed to write the old one. This kernel used to
//!   check `old_value` first: the same errno, a disarmed machine.
//! * `getitimer` resolves `which` before it writes `curr_value`:
//!   `getitimer(3, BAD)` is `EINVAL`, `getitimer(0, BAD)` is `EFAULT`. That
//!   order was already right here; only the 32-bit `which` was missing.
//! * `nullvalue.c`: a NULL `new_value` **disarms** (`armed=no` afterwards) and
//!   still reports the old timer. The kernel already did this; the claim is a
//!   control, not a new behaviour.
//!
//! The wire record is spelled here rather than taken from a binding: musl's
//! `libc` has no `struct itimerval` for `SYS_setitimer` on these targets, so
//! the calls go through `libc::syscall(libc::SYS_*, ..)` with the numbers from
//! libc's per-machine table. Its size is asserted at compile time. The bad
//! pointer is the unmapped address `1` - never the address of a stack
//! variable, which would be readable and turn every `EFAULT` claim into a
//! green `rc = 0` (the lesson of `order.c`).
//!
//! Every claim that is red on the unfixed kernel is marked in the log by the
//! number it actually got: the counterfactual is a `git stash` of the source
//! files, a rebuild of the kernel *and* the UEFI loader that embeds it, and a
//! boot of this same binary. `ITIMER_VIRTUAL`/`ITIMER_PROF` never expiring is
//! §6.125's named gap, not something this pin asserts.

use mpos_user_tests::Report;

/// A pointer no process mapped. The host answers `EFAULT` for it, and this
/// machine's `user_accessible` walks the caller's own tables and does the
/// same, so one constant measures both arms without a host-only branch.
const BAD: usize = 1;

/// The errnos these claims are about, as raw numbers so neither arm's `Errno`
/// spelling can decide a verdict. `OK` is the success sentinel.
const OK: i32 = 0;
const EFAULT: i32 = 14;
const EINVAL: i32 = 22;

/// The three clocks, from the uapi rather than a libc macro.
const ITIMER_REAL: usize = 0;
const BIG_REAL: usize = 0x1_0000_0000; // 2^32 truncates back to REAL
const BIG_VIRTUAL: usize = 0x1_0000_0001; // 2^32 + 1 truncates to VIRTUAL
const BIG_BAD: usize = 0x1_0000_0003; // 2^32 + 3 truncates to 3: EINVAL
const BOGUS: usize = 3;
const SIGN_EXTENDED_MINUS_ONE: usize = usize::MAX;

/// The uapi's `struct timeval`, in `c_long` words (both targets are 64-bit).
#[repr(C)]
#[derive(Copy, Clone)]
struct Timeval {
    tv_sec: libc::c_long,
    tv_usec: libc::c_long,
}

impl Timeval {
    fn zero() -> Self {
        Self {
            tv_sec: 0,
            tv_usec: 0,
        }
    }
}

/// The uapi's `struct itimerval`: the reload interval first, then the first
/// expiry. Thirty-two bytes, the shape every libc passes.
#[repr(C)]
#[derive(Copy, Clone)]
struct Itimerval {
    it_interval: Timeval,
    it_value: Timeval,
}

const _: () = assert!(core::mem::size_of::<Itimerval>() == 32);

impl Itimerval {
    fn zero() -> Self {
        Self {
            it_interval: Timeval::zero(),
            it_value: Timeval::zero(),
        }
    }

    /// A one-shot armed for `seconds`. An hour is long enough that no tick
    /// expires while the claims run, on either machine's timer interrupt.
    fn one_shot(seconds: i64) -> Self {
        Self {
            it_interval: Timeval::zero(),
            it_value: Timeval {
                tv_sec: seconds,
                tv_usec: 0,
            },
        }
    }

    /// A `tv_usec` outside `[0, 1e6)`: a `setitimer` must refuse this before
    /// it looks at an `old_value` pointer.
    fn bad_usec() -> Self {
        Self {
            it_interval: Timeval::zero(),
            it_value: Timeval {
                tv_sec: 1,
                tv_usec: 2_000_000,
            },
        }
    }

    /// "Armed" for these claims means a nonzero `it_value`. The two machines
    /// report the remaining time with different precision, so no exact number
    /// is asserted - only that the timer exists or does not.
    fn armed(&self) -> bool {
        self.it_value.tv_sec != 0 || self.it_value.tv_usec != 0
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

fn setitimer(which: usize, new: usize, old: usize) -> Result<isize, i32> {
    raw(libc::SYS_setitimer, which, new, old, 0)
}

fn getitimer(which: usize, cur: usize) -> Result<isize, i32> {
    raw(libc::SYS_getitimer, which, cur, 0, 0)
}

/// One claim about an errno, with the number it actually got in the label so a
/// red log says *which* door answered and with what.
fn want(r: &mut Report, label: &str, got: Result<isize, i32>, expect: i32) {
    let actual = match got {
        Ok(_) => OK,
        Err(e) => e,
    };
    r.check(
        &format!("{label} (got {actual}, want {expect})"),
        actual == expect,
    );
}

/// A no-op `SIGALRM` handler. The host arms a real `ITIMER_REAL` for an hour,
/// which cannot fire during this probe - but a handler means a mistake in a
/// value cannot kill the process mid-run and hide every claim after it. This
/// kernel's REAL timer is likewise long; a no-op changes no claim.
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
    let hour = Itimerval::one_shot(3600);
    let hour_at = &raw const hour as *const Itimerval as usize;
    let bad_usec = Itimerval::bad_usec();
    let bad_usec_at = &raw const bad_usec as *const Itimerval as usize;

    // === the two plain doors stay served on both machines ===================
    want(
        &mut r,
        "a plain setitimer(ITIMER_REAL) is served",
        setitimer(ITIMER_REAL, hour_at, 0),
        OK,
    );
    let mut cur = Itimerval::zero();
    want(
        &mut r,
        "a plain getitimer(ITIMER_REAL) is served",
        getitimer(ITIMER_REAL, &raw mut cur as *mut Itimerval as usize),
        OK,
    );

    // === which is a 32-bit int, not the register it arrived in =============
    // `2^32`, `2^32 + 1` and `2^32 + 2` name the three clocks again. The
    // unfixed kernel compared the whole usize and refused all three.
    want(
        &mut r,
        "2^32 names ITIMER_REAL and arms it",
        setitimer(BIG_REAL, hour_at, 0),
        OK,
    );
    want(
        &mut r,
        "2^32 + 1 names ITIMER_VIRTUAL and is served",
        setitimer(BIG_VIRTUAL, hour_at, 0),
        OK,
    );
    want(
        &mut r,
        "2^32 + 3 truncates to 3 and is EINVAL",
        setitimer(BIG_BAD, hour_at, 0),
        EINVAL,
    );
    want(
        &mut r,
        "getitimer(2^32) is served",
        getitimer(BIG_REAL, &raw mut cur as *mut Itimerval as usize),
        OK,
    );
    want(
        &mut r,
        "getitimer(2^32 + 3) is EINVAL",
        getitimer(BIG_BAD, &raw mut cur as *mut Itimerval as usize),
        EINVAL,
    );
    want(
        &mut r,
        "a sign-extended -1 is EINVAL",
        setitimer(SIGN_EXTENDED_MINUS_ONE, hour_at, 0),
        EINVAL,
    );

    // === setitimer: an unreadable new_value outranks a bogus which =========
    // `width.c` measured EFAULT(14) for both of these; the unfixed kernel
    // judged `which` first and answered EINVAL(22).
    want(
        &mut r,
        "an unreadable new_value outranks a bogus which",
        setitimer(BOGUS, BAD, 0),
        EFAULT,
    );
    want(
        &mut r,
        "an unreadable new_value outranks a sign-extended -1",
        setitimer(SIGN_EXTENDED_MINUS_ONE, BAD, 0),
        EFAULT,
    );
    want(
        &mut r,
        "an unreadable new_value outranks 2^32's clock too",
        setitimer(BIG_REAL, BAD, 0),
        EFAULT,
    );
    want(
        &mut r,
        "a bogus which is still EINVAL when new_value is fine",
        setitimer(BOGUS, hour_at, 0),
        EINVAL,
    );
    want(
        &mut r,
        "a bogus which outranks a bad old_value",
        setitimer(BOGUS, hour_at, BAD),
        EINVAL,
    );

    // === the timeval shape is judged before the old_value pointer ==========
    // `final.c`: `setitimer(0, bad tv_usec, old=BAD)` is EINVAL(22). The
    // unfixed kernel checked old_value up front and answered EFAULT(14).
    want(
        &mut r,
        "a malformed tv_usec outranks a bad old_value",
        setitimer(ITIMER_REAL, bad_usec_at, BAD),
        EINVAL,
    );
    want(
        &mut r,
        "a malformed tv_usec is EINVAL with a good old_value",
        setitimer(ITIMER_REAL, bad_usec_at, 0),
        EINVAL,
    );

    // === the arm happens before the old_value copy =========================
    // Start from a known disarmed state, then answer EFAULT on a bad old
    // pointer. Both kernels and Linux answer 14 - the difference is only in
    // what a following getitimer sees, which is the claim that pins the fix.
    want(
        &mut r,
        "a NULL new_value disarms the clock before the witness",
        setitimer(ITIMER_REAL, 0, 0),
        OK,
    );
    want(
        &mut r,
        "a bad old_value is EFAULT for a good arm",
        setitimer(ITIMER_REAL, hour_at, BAD),
        EFAULT,
    );
    let mut after = Itimerval::zero();
    let read = getitimer(ITIMER_REAL, &raw mut after as *mut Itimerval as usize);
    r.check(
        &format!(
            "and that EFAULT still armed the timer (read {read:?}, armed {})",
            after.armed()
        ),
        read.is_ok() && after.armed(),
    );
    let _ = setitimer(ITIMER_REAL, 0, 0); // disarm again; no timer fires later

    // === getitimer: which is judged before the output buffer ===============
    want(
        &mut r,
        "a bogus which outranks a bad curr_value",
        getitimer(BOGUS, BAD),
        EINVAL,
    );
    want(
        &mut r,
        "a sign-extended -1 outranks a bad curr_value",
        getitimer(SIGN_EXTENDED_MINUS_ONE, BAD),
        EINVAL,
    );
    want(
        &mut r,
        "a good which with a bad curr_value is EFAULT",
        getitimer(ITIMER_REAL, BAD),
        EFAULT,
    );
    want(
        &mut r,
        "a good which with a NULL curr_value is EFAULT",
        getitimer(ITIMER_REAL, 0),
        EFAULT,
    );

    // === a NULL new_value disarms and still reports the old timer =========
    // `nullvalue.c` measured `armed=no` afterwards. This kernel already did
    // it right; the three claims are controls that would pass either way.
    want(
        &mut r,
        "a timer to be replaced is armed",
        setitimer(ITIMER_REAL, hour_at, 0),
        OK,
    );
    let mut old = Itimerval::zero();
    want(
        &mut r,
        "a NULL new_value is served",
        setitimer(ITIMER_REAL, 0, &raw mut old as *mut Itimerval as usize),
        OK,
    );
    r.check(
        &format!(
            "and it reports the old timer's remaining value (armed {})",
            old.armed()
        ),
        old.armed(),
    );
    let mut disarmed = Itimerval::zero();
    want(
        &mut r,
        "a getitimer after the disarm is served",
        getitimer(ITIMER_REAL, &raw mut disarmed as *mut Itimerval as usize),
        OK,
    );
    r.check("and the timer is disarmed afterwards", !disarmed.armed());

    r.finish();
}
