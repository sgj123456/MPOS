//! A timer built on a CPU clock counts CPU time, not machine uptime (§6.123).
//!
//! `timer_create(CLOCK_PROCESS_CPUTIME_ID, ..)` is a timer whose deadline is
//! reached by *running*, not by the clock on the wall. Linux serves it, and the
//! two behaviours it has are both visible in one program:
//!
//! * a 30 ms CPU-time timer had fired **zero** times after 200 ms of `usleep`
//!   and once after a CPU burn;
//! * a periodic 10/10 ms CPU-time timer fired zero times across 100 ms of
//!   sleeping and 65 times once CPU was burned;
//! * its `timer_gettime` remaining did not move across the sleep at all, and
//!   read 0 once the value had been burned through.
//!
//! Measured on Linux 7.2.14 with `build/scratch/timerclock/fire.c` (`SIGEV_SIGNAL`
//! to a `SIGUSR1` handler). That is the whole claim here: a timer that expires
//! against the tick counter instead would fire during the sleep - which is what
//! this kernel did before the fix, and why the sleeping half is the
//! counterfactual half.
//!
//! Two things this pin deliberately does **not** claim: `CLOCK_THREAD_CPUTIME_ID`
//! is still refused here (`EOPNOTSUPP`, where Linux serves it) - the clock
//! belongs to the creating thread while the timer hangs off the group and
//! outlives it, and this table cannot express the frozen reading Linux's dead
//! thread leaves behind - and the raw/coarse clocks stay refused, which is
//! Linux's own answer for a timer on them.
//!
//! # The doors
//!
//! `libc` binds no `sigevent`, `itimerspec`, `timer_t` or `SIGEV_*` for linux,
//! so the four timer calls go through `libc::syscall` with the per-machine
//! numbers and the records are spelled here at the uapi's offsets - the same
//! shape `t-timer-create` takes, and for the same reason.

use mpos_user_tests::Report;
use rustix::io::Errno;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicUsize, Ordering};

/// How many times the CPU-clock handler ran.
static CPU_FIRED: AtomicUsize = AtomicUsize::new(0);
/// How many times the wall-clock handler ran.
static WALL_FIRED: AtomicUsize = AtomicUsize::new(0);

extern "C" fn on_cpu_signal(_sig: libc::c_int) {
    CPU_FIRED.fetch_add(1, Ordering::SeqCst);
}

extern "C" fn on_wall_signal(_sig: libc::c_int) {
    WALL_FIRED.fetch_add(1, Ordering::SeqCst);
}

/// Install a handler for one signal, answering whether the kernel took it.
fn on(sig: libc::c_int, handler: extern "C" fn(libc::c_int)) -> bool {
    unsafe {
        let mut action: libc::sigaction = core::mem::zeroed();
        action.sa_sigaction = handler as usize;
        action.sa_flags = libc::SA_RESTART;
        libc::sigemptyset(&mut action.sa_mask);
        libc::sigaction(sig, &action, core::ptr::null_mut()) == 0
    }
}

/// The uapi's `struct sigevent`: a `sigval` at 0, the signal number at 8, the
/// notification kind at 12, then the 48-byte union. Sixty-four bytes.
#[repr(C)]
struct SigEvent {
    value: u64,
    signo: i32,
    notify: i32,
    un: [u8; 48],
}

const _: () = assert!(core::mem::size_of::<SigEvent>() == 64);

impl SigEvent {
    /// `SIGEV_NONE`: the timer counts and reports but fires nothing, which is
    /// all the thread-clock claims need - the *clock* is the subject there.
    fn none() -> Self {
        Self {
            value: 0,
            signo: 0,
            notify: libc::SIGEV_NONE,
            un: [0; 48],
        }
    }

    fn signal(signo: i32) -> Self {
        Self {
            value: 0,
            signo,
            notify: libc::SIGEV_SIGNAL,
            un: [0; 48],
        }
    }
}

/// The uapi's `struct timespec` word pair.
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
#[derive(Copy, Clone)]
struct Itimerspec {
    interval: TimeSpec,
    value: TimeSpec,
}

const _: () = assert!(core::mem::size_of::<Itimerspec>() == 32);

impl Itimerspec {
    fn one_shot(millis: i64) -> Self {
        Self {
            interval: TimeSpec::zero(),
            value: TimeSpec::ms(millis),
        }
    }

    fn periodic(interval_ms: i64, first_ms: i64) -> Self {
        Self {
            interval: TimeSpec::ms(interval_ms),
            value: TimeSpec::ms(first_ms),
        }
    }

    /// The remaining time in whole milliseconds.
    fn remaining_ms(&self) -> i64 {
        self.value.tv_sec * 1000 + self.value.tv_nsec / 1_000_000
    }
}

/// A raw syscall through `libc`'s per-machine number table, refusal in `Errno`.
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

fn timer_create(clockid: i32, event: &SigEvent, id: &mut i32) -> Result<isize, Errno> {
    raw(
        libc::SYS_timer_create,
        clockid as usize,
        event as *const SigEvent as usize,
        id as *mut i32 as usize,
        0,
    )
}

fn timer_settime(id: i32, new: &Itimerspec) -> Result<isize, Errno> {
    raw(
        libc::SYS_timer_settime,
        id as usize,
        0,
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

fn timer_delete(id: i32) -> Result<isize, Errno> {
    raw(libc::SYS_timer_delete, id as usize, 0, 0, 0)
}

/// The timer's remaining time in milliseconds, or -1 when the call refused.
fn left(id: i32) -> i64 {
    let mut now = Itimerspec {
        interval: TimeSpec::zero(),
        value: TimeSpec::zero(),
    };
    match timer_gettime(id, &mut now) {
        Ok(_) => now.remaining_ms(),
        Err(_) => -1,
    }
}

/// Burn CPU in chunks, checking `fired` between them, until it reaches `want`
/// expiries or the chunk budget runs out. Chunks are the unit because the
/// handler only runs when this thread comes back to userspace, which is between
/// chunks.
fn burn_until(fired: &AtomicUsize, want: usize, budget: u32) -> u32 {
    let mut x = 0x5_1d3_700d_u64;
    let mut chunk = 0;
    while chunk < budget {
        for i in 0..5_000_000u64 {
            x = x.wrapping_mul(6364136223846793005).wrapping_add(i);
        }
        std::hint::black_box(x);
        // One cheap system call per chunk, and it is not decoration: a signal
        // with a handler is handed over when the task next enters the kernel
        // (`deliver_pending_signal`'s doc: the syscall return path is the only
        // place this kernel has a register file for), so a program that never
        // called anything would burn its whole value and see no handler. Linux
        // hands it over on an interrupt return as well; that difference is
        // `§7.4h`'s business and not this clock's, so the pin asks its question
        // the way an ordinary program does - a loop with a call in it.
        unsafe { libc::getpid() };
        chunk += 1;
        if fired.load(Ordering::SeqCst) >= want {
            break;
        }
    }
    chunk
}

/// Released by the main thread once the pair's target is blocked and listening:
/// a target that read too early would report 0 records for a reason that is the
/// fixture's, not the kernel's.
static PAIR_RELEASE: AtomicBool = AtomicBool::new(false);
/// The pair's target thread's tid, published once it is blocked.
static PAIR_TARGET_TID: AtomicI32 = AtomicI32::new(0);

/// This thread's own kernel tid, which is what `SIGEV_THREAD_ID` names.
fn current_tid() -> i32 {
    // SAFETY: `gettid` takes no arguments and cannot fail.
    unsafe { libc::syscall(libc::SYS_gettid) as i32 }
}

/// `signalfd(-1, {signals}, NONBLOCK|CLOEXEC)`, or `None` when refused.
fn make_signalfd(signals: &[i32]) -> Option<OwnedFd> {
    // SAFETY: a zeroed `sigset_t` is the empty set; the calls below fill this one.
    unsafe {
        let mut set: libc::sigset_t = core::mem::zeroed();
        libc::sigemptyset(&mut set);
        for sig in signals {
            libc::sigaddset(&mut set, *sig);
        }
        let raw = libc::signalfd(-1, &set, libc::SFD_NONBLOCK | libc::SFD_CLOEXEC);
        (raw >= 0).then(|| OwnedFd::from_raw_fd(raw))
    }
}

/// How many complete 128-byte records are queued on `fd` right now.
fn drain_records(fd: &OwnedFd) -> usize {
    let mut buf = [0u8; 128];
    let mut count = 0;
    loop {
        // SAFETY: `buf` is this program's own 128 bytes and `fd` is live.
        let n = unsafe {
            libc::read(
                fd.as_raw_fd(),
                buf.as_mut_ptr().cast::<libc::c_void>(),
                buf.len(),
            )
        };
        if n != 128 {
            return count;
        }
        count += 1;
    }
}

/// Block `sig` on this thread.
fn block_signal(sig: i32) -> bool {
    // SAFETY: a set this thread owns; `SIG_BLOCK` with it cannot fail for a
    // valid signal number.
    unsafe {
        let mut set: libc::sigset_t = core::mem::zeroed();
        libc::sigemptyset(&mut set);
        libc::sigaddset(&mut set, sig);
        libc::pthread_sigmask(libc::SIG_BLOCK, &set, core::ptr::null_mut()) == 0
    }
}

/// Unblock `sig` on this thread, ignoring it first so a queued record cannot end
/// the program when the mask comes off (the shape `t-sfd-timer-record` records).
fn unblock_signal(sig: i32) -> bool {
    // SAFETY: as `block_signal`, plus a `SIG_IGN` action this thread owns.
    unsafe {
        let mut action: libc::sigaction = core::mem::zeroed();
        action.sa_sigaction = libc::SIG_IGN;
        libc::sigaction(sig, &action, core::ptr::null_mut());
        let mut set: libc::sigset_t = core::mem::zeroed();
        libc::sigemptyset(&mut set);
        libc::sigaddset(&mut set, sig);
        libc::pthread_sigmask(libc::SIG_UNBLOCK, &set, core::ptr::null_mut()) == 0
    }
}

fn main() {
    let mut r = Report::new();

    let takes = on(libc::SIGUSR1, on_cpu_signal) && on(libc::SIGUSR2, on_wall_signal);
    r.check(
        &format!("two handlers to tell the clocks apart ({takes})"),
        takes,
    );

    // === the CPU clock's timer ==============================================
    let silent = SigEvent::signal(libc::SIGUSR1);
    let mut cpu_id: i32 = -1;
    let created = timer_create(2 /* CLOCK_PROCESS_CPUTIME_ID */, &silent, &mut cpu_id);
    r.check(
        &format!("a timer can be built on the process's own CPU clock ({created:?})"),
        created == Ok(0),
    );

    // A one-shot of 200 ms of CPU time. The test's own printing costs a few
    // milliseconds of CPU; 150 ms of sleeping must cost almost none of it.
    let armed = timer_settime(cpu_id, &Itimerspec::one_shot(200));
    r.check(
        &format!("200 ms of CPU time can be armed ({armed:?})"),
        armed == Ok(0),
    );
    println!(
        "note: a 200 ms CPU-time one-shot armed with {} ms left",
        left(cpu_id)
    );

    let before_sleep = left(cpu_id);
    std::thread::sleep(std::time::Duration::from_millis(150));
    let after_sleep = left(cpu_id);
    let fired_after_sleep = CPU_FIRED.load(Ordering::SeqCst);

    r.check(
        &format!(
            "sleeping 150 ms does not fire a 200 ms CPU-time timer (fired {fired_after_sleep})"
        ),
        fired_after_sleep == 0,
    );
    // The discriminating number: the kernel must not have spent the 150 ms
    // against a clock that only CPU time moves. A wall-clock timer would read
    // 50 ms or less here, and would have fired.
    r.check(
        &format!(
            "and it does not eat the time left ({before_sleep} ms -> {after_sleep} ms of 200 ms)"
        ),
        after_sleep > 0 && after_sleep >= before_sleep - 20,
    );

    let chunks = burn_until(&CPU_FIRED, 1, 400);
    let fired = CPU_FIRED.load(Ordering::SeqCst);
    r.check(
        &format!("burning CPU reaches the deadline (fired {fired} after {chunks} chunk(s))"),
        fired >= 1,
    );

    let spent = left(cpu_id);
    r.check(
        &format!("and a one-shot timer has spent its value (left {spent} ms)"),
        spent == 0,
    );

    // The periodic half: a reload is 20 ms of *CPU*, so a sleep reloads nothing
    // and a burn reloads more than once.
    CPU_FIRED.store(0, Ordering::SeqCst);
    let periodic = timer_settime(cpu_id, &Itimerspec::periodic(20, 20));
    r.check(
        &format!("a periodic CPU-time timer can be armed ({periodic:?})"),
        periodic == Ok(0),
    );
    std::thread::sleep(std::time::Duration::from_millis(120));
    let periodic_slept = CPU_FIRED.load(Ordering::SeqCst);
    r.check(
        &format!("a 20 ms period does not reload on a 120 ms sleep (fired {periodic_slept})"),
        periodic_slept == 0,
    );
    // Three expiries, so the reload rule itself is what fires them and not the
    // single first one.
    let chunks = burn_until(&CPU_FIRED, 3, 400);
    let periodic_fired = CPU_FIRED.load(Ordering::SeqCst);
    r.check(
        &format!(
            "and it reloads as CPU is burned ({periodic_fired} expiries in {chunks} chunk(s) at least)"
        ),
        periodic_fired >= 2,
    );
    let _ = timer_delete(cpu_id);

    // === the wall clock, same value, for contrast ===========================
    // One program, two clocks, one sleep: the wall timer of the same order of
    // value fires across the sleep the CPU timer sat through, which is what
    // makes the sleeping half above a measurement of the clock and not of the
    // test's own timing.
    let wall_signal = SigEvent::signal(libc::SIGUSR2);
    let mut wall_id: i32 = -1;
    let wall_created = timer_create(1 /* CLOCK_MONOTONIC */, &wall_signal, &mut wall_id);
    r.check(
        &format!("a wall-clock timer is still built the same way ({wall_created:?})"),
        wall_created == Ok(0),
    );
    let wall_armed = timer_settime(wall_id, &Itimerspec::one_shot(120));
    r.check(
        &format!("120 ms of wall time can be armed ({wall_armed:?})"),
        wall_armed == Ok(0),
    );
    std::thread::sleep(std::time::Duration::from_millis(400));
    let wall_fired = WALL_FIRED.load(Ordering::SeqCst);
    r.check(
        &format!(
            "the wall timer of the same value does fire across that sleep (fired {wall_fired})"
        ),
        wall_fired >= 1,
    );
    let _ = timer_delete(wall_id);

    // === the creating thread's own clock ===================================
    //
    // `CLOCK_THREAD_CPUTIME_ID` counts the **creating thread's** CPU and stops
    // when that thread exits, so the timer outlives its clock. Linux 7.3.4's
    // three measured answers (`build/scratch/thrcpu/{t4,disarm}.c`, `§6.168`):
    // the creator's CPU fires it and another thread's does not; once the creator
    // is gone `timer_gettime` still succeeds with `value = interval = 0`
    // (disarmed, not deleted), `timer_getoverrun` is 0, `timer_settime` answers
    // **`ESRCH`**, and `timer_delete` still succeeds. This kernel refused the
    // clock outright until item 288.
    let mut id: i32 = -1;
    let silent_ev = SigEvent::none();
    let thread_clock = timer_create(3 /* CLOCK_THREAD_CPUTIME_ID */, &silent_ev, &mut id);
    r.check(
        &format!("a timer can be built on the creating thread's own CPU clock ({thread_clock:?})"),
        thread_clock == Ok(0),
    );
    if thread_clock == Ok(0) {
        let threaded = id;
        // Arm far out (60 s of CPU) so nothing fires while this is measured.
        let armed = timer_settime(threaded, &Itimerspec::one_shot(60_000));
        r.check(
            &format!("60 s of that thread's CPU time can be armed ({armed:?})"),
            armed == Ok(0),
        );
        // While the creator (this thread) lives, the clock counts and the
        // reading is near the arm value.
        let alive = left(threaded);
        r.check(
            &format!("while the creating thread lives the clock counts ({alive} ms left)"),
            (0..=60_000).contains(&alive),
        );

        // Now hand the timer to a thread that exits, which is where the clock
        // dies. The timer is created *by* that thread so it owns the clock.
        let handoff = std::thread::spawn(|| {
            let mut ev = SigEvent::none();
            ev.signo = 0;
            let mut inner: i32 = -1;
            let made = timer_create(3, &ev, &mut inner);
            if made != Ok(0) {
                return (false, -1);
            }
            let armed = timer_settime(inner, &Itimerspec::one_shot(60_000));
            (armed == Ok(0), inner)
        });
        let (made, inner) = handoff.join().expect("the creating thread joins");
        r.check("a thread can own a timer on its own clock", made);
        if made && inner >= 0 {
            // The creator is gone by now (joined). Linux disarms the timer and
            // answers 0/0 from `timer_gettime` - not `EINVAL`, which is what a
            // *deleted* timer answers.
            let mut now = Itimerspec {
                interval: TimeSpec::zero(),
                value: TimeSpec::zero(),
            };
            let read = timer_gettime(inner, &mut now);
            r.check(
                &format!("after the clock's thread exits the timer still exists ({read:?})"),
                read == Ok(0),
            );
            r.check(
                &format!(
                    "and it is disarmed, value and interval both zero ({} ms / {} ms)",
                    now.value.tv_sec * 1000 + now.value.tv_nsec / 1_000_000,
                    now.interval.tv_sec * 1000 + now.interval.tv_nsec / 1_000_000,
                ),
                now.value.tv_sec == 0
                    && now.value.tv_nsec == 0
                    && now.interval.tv_sec == 0
                    && now.interval.tv_nsec == 0,
            );
            let overrun = raw(libc::SYS_timer_getoverrun, inner as usize, 0, 0, 0);
            r.check(
                &format!("its overrun count is 0 ({overrun:?})"),
                overrun == Ok(0),
            );
            // The refusal that says the clock is gone rather than the timer:
            // Linux answers `ESRCH` to an arm, because there is no reading left
            // to build a deadline against. A deleted timer would be `EINVAL`.
            let rearm = timer_settime(inner, &Itimerspec::one_shot(1000));
            r.check(
                &format!("re-arming it answers ESRCH, not EINVAL ({rearm:?})"),
                rearm == Err(Errno::SRCH),
            );
            // And it is still deletable: the timer exists.
            let gone = timer_delete(inner);
            r.check(
                &format!("and it still deletes, so it was never removed ({gone:?})"),
                gone == Ok(0),
            );
        }
        let _ = timer_delete(threaded);
    }

    // === the clock's thread is not the signal's thread ======================
    //
    // Item 288 left this pair unwitnessed: a timer whose **clock** names the
    // creating thread's CPU while its `SIGEV_THREAD_ID` names a *different*
    // thread. Measured on Linux 7.3.4 (`build/scratch/thrsig/t.c`), and the two
    // roles are independent:
    //
    //  * `timer_create` accepts the pair;
    //  * the expiry is driven by the **clock's** thread's CPU - the target thread
    //    burns none, and the records appear only while the creator burns;
    //  * the record goes to the **named** thread, not to the one whose CPU fired
    //    it and not to the process;
    //  * and when the clock's thread exits the clock still ends (`timer_gettime`
    //    0/0, re-arm `ESRCH`) even though the named thread is alive and still
    //    has somewhere to put a record.
    //
    // The pin asserts the *routing* and the *end*, not how many expiries there
    // were: the count is how long the burn ran, which is the machine's speed
    // (the same 120 M-iteration loop is 499 ms on the host and 1635 ms under
    // TCG), so a count claim would be a claim about the host.
    let pair_signo = libc::SIGRTMIN() + 5;
    let pair_ok = block_signal(pair_signo);
    r.check(
        &format!("the pair's signal can be blocked ({pair_ok})"),
        pair_ok,
    );
    if pair_ok {
        // The TARGET thread: blocks the signal, owns a signalfd, burns no CPU,
        // and reports how many records it was handed.
        let target = std::thread::spawn(move || {
            let tid = current_tid();
            // Publish the tid the instant this thread is the one that will
            // receive the record: the main thread waits for exactly this, since
            // aiming at 0 (or at another thread) is `EINVAL`.
            PAIR_TARGET_TID.store(tid, Ordering::SeqCst);
            let mut got = 0;
            let fd = make_signalfd(&[pair_signo]);
            // Told the main thread it is ready by parking until released.
            while !PAIR_RELEASE.load(Ordering::SeqCst) {
                if let Some(fd) = fd.as_ref() {
                    got += drain_records(fd);
                }
                std::thread::sleep(std::time::Duration::from_millis(2));
            }
            if let Some(fd) = fd.as_ref() {
                got += drain_records(fd);
            }
            (tid, got)
        });
        // Wait for the target to be blocked and listening.
        std::thread::sleep(std::time::Duration::from_millis(50));
        let target_tid = PAIR_TARGET_TID.load(Ordering::SeqCst);

        // The CLOCK's thread creates the timer, so `CLOCK_THREAD_CPUTIME_ID` is
        // *its* clock, and aims the signal at the target's tid.
        let creator = std::thread::spawn(move || {
            let mut ev = SigEvent::none();
            ev.signo = pair_signo;
            ev.notify = libc::SIGEV_THREAD_ID;
            // The tid lives in the union at byte **16**, not in `sigval` at 0 -
            // measured on both libcs (`offsetof(sigevent, sigev_notify_thread_id)`
            // is 16 in musl and glibc alike), and putting it at 0 is `EINVAL`
            // from a kernel that cannot find the thread it was told to aim at.
            ev.un[0..4].copy_from_slice(&target_tid.to_ne_bytes());
            let mut timer: i32 = -1;
            let made = timer_create(3 /* CLOCK_THREAD_CPUTIME_ID */, &ev, &mut timer);
            if made != Ok(0) {
                return Err(made.err());
            }
            if timer_settime(timer, &Itimerspec::periodic(20, 20)) != Ok(0) {
                return Err(None);
            }
            // Burn THIS thread's CPU: the clock is this thread's.
            let mut x = 0.0f64;
            for i in 0..40_000_000u64 {
                x += (i & 1) as f64;
            }
            std::hint::black_box(x);
            Ok(timer)
        });
        let made = creator.join().expect("the clock's thread joins");
        r.check(
            &format!("a timer can hold a thread clock and a different target thread ({made:?})"),
            made.is_ok(),
        );
        if let Ok(timer) = made {
            PAIR_RELEASE.store(true, Ordering::SeqCst);
            let (tid, got) = target.join().expect("the target thread joins");
            r.check(
                &format!("the record goes to the named thread ({got} records on tid {tid})"),
                got > 0,
            );
            // The clock's thread is gone; the named thread is not. The clock
            // still ends, which is the half that says the two roles are
            // independent.
            let mut now = Itimerspec {
                interval: TimeSpec::zero(),
                value: TimeSpec::zero(),
            };
            let read = timer_gettime(timer, &mut now);
            r.check(
                &format!("when the clock's thread exits the timer is still readable ({read:?})"),
                read == Ok(0),
            );
            r.check(
                "and disarmed, though the named thread still lives",
                now.value.tv_sec == 0 && now.value.tv_nsec == 0,
            );
            let rearm = timer_settime(timer, &Itimerspec::one_shot(1000));
            r.check(
                &format!("re-arming answers ESRCH ({rearm:?})"),
                rearm == Err(Errno::SRCH),
            );
            let _ = timer_delete(timer);
        }
        let _ = unblock_signal(pair_signo);
    }

    // === the clocks that stay refused ======================================
    let raw_clock = timer_create(4 /* CLOCK_MONOTONIC_RAW */, &silent, &mut id);
    r.check(
        &format!("a raw clock timer is EOPNOTSUPP, as Linux answers ({raw_clock:?})"),
        raw_clock == Err(Errno::OPNOTSUPP),
    );

    r.finish();
}
