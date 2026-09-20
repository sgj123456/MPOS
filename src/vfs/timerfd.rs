//! `timerfd(2)`: a timer that reports its expirations through a descriptor.
//!
//! `setitimer` reports a timer through a signal; `timerfd` reports it
//! through a `read`. That is the whole of the difference and the whole of
//! the point: a program whose main loop is already a poll - a shell, a
//! daemon, `nlplug-findfs` - can wait for *time itself* in the same wait
//! that watches its pipes and its sockets, and systemd's "timer unit" is
//! built on nothing else.
//!
//! The state machine is deliberately plain: one absolute tick deadline, one
//! interval, one count of expirations no one has read yet. `read` takes
//! the count; `poll` says readable when there is one; `timerfd_gettime`
//! reports what is left of the current span and the interval between them.
//! All of it works in *ticks* - the one clock the machine keeps - and the
//! conversions from and to `struct timespec` are pure functions beside it,
//! so the whole module runs on the host under `cargo test`.
//!
//! The one exotic corner is `TFD_TIMER_CANCEL_ON_SET`: a realtime absolute
//! timer that wants to be told, by way of its read failing `ECANCELED`,
//! that somebody set the clock underneath it. Timer instances register
//! themselves here in a weak list, and the `clock_settime` handler walks
//! it; a timer nobody holds a descriptor to drops out of the list on the
//! next walk without anybody's help.
//!
//! Blocking reads park the way every other wait in this kernel parks: one
//! tick of sleep between looks, the same loop `poll` runs. The timer is
//! the clock's own work, so no arrival can slip past a look - the next
//! tick is the latest an expiration is noticed by.

use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::cell::RefCell;

use critical_section::Mutex;

use super::{Ino, Inode, Metadata, NodeKind, Ready, VfsError, VfsResult};

// ---------------------------------------------------------------------------
// The ABI: flags, clockids, and the itimerspec layout
// ---------------------------------------------------------------------------

/// `TFD_CLOEXEC`: the descriptor must not survive the next `execve`. The
/// value is `O_CLOEXEC`, as the header spells it.
pub const TFD_CLOEXEC: usize = 0o200_0000;
/// `TFD_NONBLOCK`: an empty read answers `EAGAIN` instead of parking.
/// The value is `O_NONBLOCK`, as the header spells it.
pub const TFD_NONBLOCK: usize = 0x800;
/// `TFD_TIMER_ABSTIME`: the value names a point on the clock, not a span
/// from now.
pub const TFD_TIMER_ABSTIME: usize = 0x1;
/// `TFD_TIMER_CANCEL_ON_SET`: only meaningful beside `TFD_TIMER_ABSTIME`
/// on a realtime clock - a realtime absolute timer that wants its read to
/// report `ECANCELED` after the clock was set.
pub const TFD_TIMER_CANCEL_ON_SET: usize = 0x2;

/// `struct itimerspec` of the x86_64 ABI: 32 bytes, `it_interval` first
/// and `it_value` second - the reverse of `struct itimerval`, which is
/// exactly the kind of thing a wait here would hand back wrong if the
/// order were guessed at rather than pinned.
pub const ITIMERSPEC_SIZE: usize = 32;
/// Where `it_value` sits inside a `struct itimerspec`.
pub const ITIMERSPEC_VALUE: usize = 16;

/// The clockids `timerfd_create` accepts, in Linux's numbering. The ALARM
/// pair want `CAP_WAKE_ALARM` on Linux; this kernel's one user is root,
/// and the alarm clocks behave as their base clocks do - there is no
/// suspend for an alarm to reach through.
pub const CLOCK_REALTIME: usize = 0;
pub const CLOCK_MONOTONIC: usize = 1;
pub const CLOCK_BOOTTIME: usize = 7;
pub const CLOCK_REALTIME_ALARM: usize = 8;
pub const CLOCK_BOOTTIME_ALARM: usize = 9;

/// Whether `clockid` is one a timerfd may keep - the man page's five, and
/// the clocks whose progress the machine can actually name. Anything else
/// is `EINVAL`.
pub fn clock_ok(clockid: usize) -> bool {
    matches!(
        clockid,
        CLOCK_REALTIME
            | CLOCK_MONOTONIC
            | CLOCK_BOOTTIME
            | CLOCK_REALTIME_ALARM
            | CLOCK_BOOTTIME_ALARM
    )
}

// ---------------------------------------------------------------------------
// The tick <-> timespec conversions, pure over the clock's frequency
// ---------------------------------------------------------------------------

/// The tick span a `struct timespec` names. A timeout is a lower bound on
/// the wait, so the sub-tick remainder rounds *up* - one nanosecond asked
/// for is one tick owed - and the nanosecond field is validated the way
/// every timespec-carrying call validates it.
pub fn ticks_of_timespec(sec: u64, nsec: u64, hz: u64) -> Result<u64, isize> {
    if nsec >= 1_000_000_000 {
        return Err(crate::syscall::err::EINVAL);
    }
    // Saturation is the shape of an honest timer: a caller that named more
    // nanoseconds than a tick count can hold asked for a very long wait,
    // and clamping to "the longest this machine can mean" beats wrapping
    // into the past. The sub-second field rounds up - a timeout is a
    // lower bound on the wait.
    Ok(sec
        .saturating_mul(hz)
        .saturating_add(nsec.saturating_mul(hz).div_ceil(1_000_000_000)))
}

/// The `struct timespec` a tick span names, for `timerfd_gettime`'s answer
/// and for the old-value report `timerfd_settime` owes. Whole ticks are
/// seconds; the remainder is nanoseconds, truncating - the caller's
/// question is "how long is left", not "how long until the next whole
/// second".
pub fn timespec_of_ticks(ticks: u64, hz: u64) -> (u64, u64) {
    (ticks / hz, ticks % hz * 1_000_000_000 / hz)
}

// ---------------------------------------------------------------------------
// The timer itself
// ---------------------------------------------------------------------------

/// What a timer is holding: the deadline of its next expiration, the span
/// between expirations, and the expirations no read has taken yet.
#[derive(Clone, Copy)]
struct TimerState {
    /// The absolute tick of the next expiration, or `None` when disarmed.
    deadline: Option<u64>,
    /// Ticks between expirations; zero is the one-shot spelling.
    interval: u64,
    /// Expirations since the last arm or the last read, saturating as
    /// Linux's counter does.
    expirations: u64,
    /// A `TFD_TIMER_CANCEL_ON_SET` arm was cleared by a clock set: reads
    /// report `ECANCELED` until the next arm.
    canceled: bool,
    /// The last arm asked to be told about clock sets.
    cancel_on_set: bool,
}

/// One timerfd: an anonymous inode holding one timer.
pub struct TimerFd {
    ino: Ino,
    clockid: usize,
    /// `TFD_NONBLOCK`, as recorded at creation. Kept here because the
    /// empty-read refusal belongs to the timer's own contract; the
    /// descriptor's status word is set to match, so a `fcntl` caller reads
    /// back what it does.
    nonblock: core::sync::atomic::AtomicBool,
    state: Mutex<RefCell<TimerState>>,
}

impl TimerFd {
    /// A disarmed timer on `clockid`. The clock is checked by the caller -
    /// the syscall layer answers `EINVAL` before a descriptor exists.
    pub fn new(ino: Ino, clockid: usize, nonblock: bool) -> Arc<TimerFd> {
        let timer = Arc::new(TimerFd {
            ino,
            clockid,
            nonblock: core::sync::atomic::AtomicBool::new(nonblock),
            state: Mutex::new(RefCell::new(TimerState {
                deadline: None,
                interval: 0,
                expirations: 0,
                canceled: false,
                cancel_on_set: false,
            })),
        });
        register(timer.clone());
        timer
    }

    /// The clock this timer keeps, as the caller named it at creation.
    pub fn clockid(&self) -> usize {
        self.clockid
    }

    /// Whether the empty read is an `EAGAIN` rather than a park.
    pub fn nonblocking(&self) -> bool {
        self.nonblock.load(core::sync::atomic::Ordering::Relaxed)
    }

    /// Count the expirations `now` has brought. Idempotent and safe to
    /// call from a readiness look: the count is a fact about the clock,
    /// and computing it twice counts it once.
    pub fn advance(&self, now: u64) {
        critical_section::with(|cs| {
            expire_up_to(&mut self.state.borrow(cs).borrow_mut(), now);
        });
    }

    /// Arm the timer: `deadline` is the absolute tick of the first
    /// expiration and `interval` the tick span between the ones after it -
    /// or `None` for the disarm a zero `it_value` asks for. Answers the
    /// setting in force before the call, as `(remaining, interval)`, for
    /// `timerfd_settime`'s `old_value` report; `None` is a timer that was
    /// disarmed. An expiration that came due and was never read reports
    /// zero remaining - the timer has fired, and there is nothing left to
    /// wait for.
    ///
    /// The expiration count starts over: the man page is explicit that the
    /// count a read returns is "since the settings were last modified ...
    /// or since the last successful read", and an arm is that
    /// modification.
    pub fn set(
        &self,
        now: u64,
        deadline: Option<u64>,
        interval: u64,
        cancel_on_set: bool,
    ) -> Option<(u64, u64)> {
        critical_section::with(|cs| {
            let mut state = self.state.borrow(cs).borrow_mut();
            // The old setting is reported as `timerfd_gettime` would have
            // answered at the same instant, fired deadlines included.
            expire_up_to(&mut state, now);
            let old = match state.deadline {
                Some(at) => Some((at.saturating_sub(now), state.interval)),
                None if state.expirations > 0 => Some((0, state.interval)),
                None => None,
            };
            state.deadline = deadline;
            state.interval = interval;
            state.expirations = 0;
            state.canceled = false;
            state.cancel_on_set = cancel_on_set;
            old
        })
    }

    /// `timerfd_gettime`'s answer: what is left of the span to the next
    /// expiration and the interval between them, or `None` for a disarmed
    /// timer. A one-shot that has fired and not been read is *expired*,
    /// not disarmed: its remaining time is zero, which is what the caller
    /// is asking about.
    pub fn snapshot(&self, now: u64) -> Option<(u64, u64)> {
        critical_section::with(|cs| {
            let mut state = self.state.borrow(cs).borrow_mut();
            expire_up_to(&mut state, now);
            match state.deadline {
                Some(at) => Some((at.saturating_sub(now), state.interval)),
                None if state.expirations > 0 => Some((0, state.interval)),
                None => None,
            }
        })
    }

    /// The read: the expirations since the last one, taken away with the
    /// taking. `Err(VfsError::Again)` is "none yet"; a canceled
    /// `CANCEL_ON_SET` timer reports the cancellation *every* read until
    /// it is armed again, which is the state the man page describes.
    pub fn take_expirations(&self, now: u64) -> Result<u64, VfsError> {
        critical_section::with(|cs| {
            let mut state = self.state.borrow(cs).borrow_mut();
            if state.canceled {
                return Err(VfsError::Canceled);
            }
            expire_up_to(&mut state, now);
            if state.expirations == 0 {
                return Err(VfsError::Again);
            }
            Ok(core::mem::take(&mut state.expirations))
        })
    }

    /// Whether a read would answer something right now - `poll`'s
    /// question, and a cancellation counts as an answer.
    pub fn pending(&self, now: u64) -> bool {
        critical_section::with(|cs| {
            let mut state = self.state.borrow(cs).borrow_mut();
            expire_up_to(&mut state, now);
            state.expirations > 0 || state.canceled
        })
    }
}

/// Count the expirations `now` has brought past a state's deadline, moving
/// the deadline along as they are counted. A one-shot leaves the deadline
/// unset after firing - the expirations it counted are waiting for a read,
/// and the timer itself is spent.
fn expire_up_to(state: &mut TimerState, now: u64) {
    let Some(deadline) = state.deadline else {
        return;
    };
    if now < deadline {
        return;
    }
    if state.interval == 0 {
        state.expirations = state.expirations.saturating_add(1);
        state.deadline = None;
        return;
    }
    // A span timer: the expiration at the deadline, plus one for every
    // whole interval since. The count is a division rather than a loop
    // because an interval of one tick with a caller asleep for a day is
    // 8.64 million iterations of nothing; every term saturates so a clock
    // far past the wrap of the count still leaves a future deadline.
    let due = now - deadline;
    let periods = due / state.interval;
    state.expirations = state.expirations.saturating_add(periods).saturating_add(1);
    let next = deadline.saturating_add(periods.saturating_add(1).saturating_mul(state.interval));
    state.deadline = Some(next);
}

// ---------------------------------------------------------------------------
// The clock-set registry: CANCEL_ON_SET's audience
// ---------------------------------------------------------------------------

/// Every timer that exists, weakly: a timer drops out when its last
/// descriptor goes, and the walk prunes the tombstones.
static TIMERS: Mutex<RefCell<Vec<Weak<TimerFd>>>> = Mutex::new(RefCell::new(Vec::new()));

fn register(timer: Arc<TimerFd>) {
    critical_section::with(|cs| {
        let mut timers = TIMERS.borrow(cs).borrow_mut();
        if timers.len() >= 1024 {
            // A bound, not a policy: pruning the dead first keeps a
            // long-lived machine's list from growing without end, and a
            // thousand live timers is a machine doing something else.
            timers.retain(|weak| weak.strong_count() > 0);
        }
        timers.push(Arc::downgrade(&timer));
    });
}

/// The `clock_settime` hook: a discontinuous change to the real-time clock
/// cancels every realtime absolute timer that asked to be told - its
/// expirations go unread, its deadline is dropped, and its next read
/// answers `ECANCELED` until it is armed again.
///
/// Monotonic and boottime timers are untouched, which is the point of
/// their names: setting the wall clock is not an event on their clocks.
pub fn cancel_on_clock_set() {
    critical_section::with(|cs| {
        let mut timers = TIMERS.borrow(cs).borrow_mut();
        // The dead go first: a timer nobody holds a descriptor to cannot
        // be told anything, and its tombstone is only in the way.
        timers.retain(|weak| weak.strong_count() > 0);
        for weak in timers.iter() {
            let Some(timer) = weak.upgrade() else {
                continue;
            };
            let is_realtime = matches!(timer.clockid(), CLOCK_REALTIME | CLOCK_REALTIME_ALARM);
            let mut state = timer.state.borrow(cs).borrow_mut();
            if is_realtime && state.cancel_on_set && state.deadline.is_some() {
                state.deadline = None;
                state.expirations = 0;
                state.canceled = true;
            }
        }
    });
}

// ---------------------------------------------------------------------------
// The Inode: what descriptors see
// ---------------------------------------------------------------------------

/// Where "now" comes from. On the machine it is the tick counter - the one
/// clock there is; on the host there is no clock, and the tests that need
/// time to pass say which `now` they mean instead.
#[cfg(target_os = "none")]
fn now_ticks() -> u64 {
    crate::arch::timer::ticks()
}

#[cfg(not(target_os = "none"))]
fn now_ticks() -> u64 {
    0
}

impl Inode for TimerFd {
    fn metadata(&self) -> Metadata {
        Metadata {
            ino: 0x8000_0000 + self.ino,
            kind: NodeKind::CharDev,
            size: 0,
        }
    }

    /// `poll`'s answer: readable when expirations are waiting or a
    /// cancellation is, never writable - nothing writes to a timer, and a
    /// writable answer would invite a caller to try.
    fn poll_ready(&self) -> Ready {
        Ready {
            read: self.pending(now_ticks()),
            write: false,
        }
    }

    /// The read: the number of expirations since the last one, as a
    /// little-endian `u64`. Fewer than 8 bytes is `EINVAL`, the man page's
    /// own refusal; none yet is a park - or `EAGAIN` when the caller made
    /// the descriptor nonblocking; a canceled `CANCEL_ON_SET` timer
    /// answers its cancellation.
    fn read_at(&self, _offset: u64, buf: &mut [u8]) -> VfsResult<usize> {
        if buf.len() < 8 {
            return Err(VfsError::Invalid);
        }
        loop {
            match self.take_expirations(now_ticks()) {
                Ok(count) => {
                    buf[..8].copy_from_slice(&count.to_ne_bytes());
                    return Ok(8);
                }
                Err(VfsError::Canceled) => return Err(VfsError::Canceled),
                Err(VfsError::Again) => {
                    if self.nonblocking() {
                        return Err(VfsError::Again);
                    }
                    self.wait_for_expiry();
                }
                Err(_) => return Err(VfsError::Invalid),
            }
        }
    }

    fn write_at(&self, _offset: u64, _buf: &[u8]) -> VfsResult<usize> {
        // Nobody writes to a timer: the arm is `timerfd_settime`, and a
        // write that pretended to land would make a caller believe it had
        // changed something. Linux refuses with EINVAL; so does this.
        Err(VfsError::Invalid)
    }

    fn as_timerfd(&self) -> Option<&TimerFd> {
        Some(self)
    }
}

impl TimerFd {
    /// Park until the clock brings the next expiration. One tick of sleep
    /// per look, the same wait `poll` runs - the timer's arrival is the
    /// clock's own work, so the next look is at most a tick too late.
    #[cfg(target_os = "none")]
    fn wait_for_expiry(&self) {
        crate::sched::sleep_ticks(1);
    }

    #[cfg(not(target_os = "none"))]
    fn wait_for_expiry(&self) {
        panic!("blocking on a timerfd needs the kernel scheduler; call advance() in tests");
    }
}

#[cfg(all(test, not(target_os = "none")))]
mod tests {
    use super::{
        CLOCK_BOOTTIME, CLOCK_BOOTTIME_ALARM, CLOCK_MONOTONIC, CLOCK_REALTIME,
        CLOCK_REALTIME_ALARM, ITIMERSPEC_SIZE, ITIMERSPEC_VALUE, TFD_CLOEXEC, TFD_NONBLOCK,
        TFD_TIMER_ABSTIME, TFD_TIMER_CANCEL_ON_SET, TimerFd, cancel_on_clock_set, clock_ok,
        ticks_of_timespec, timespec_of_ticks,
    };
    use crate::syscall::err;
    use crate::vfs::{Inode, VfsError};

    /// The flags, clockids and layout are the ABI, pinned against the
    /// headers: `TFD_NONBLOCK`/`TFD_CLOEXEC` are the O_ flags under other
    /// names, and `struct itimerspec` is interval-first - the reverse of
    /// `struct itimerval`, and the one thing a guess would get wrong.
    #[test]
    fn tfd_flags_clocks_and_layout_match_the_headers() {
        assert_eq!(TFD_NONBLOCK, 0x800);
        assert_eq!(TFD_CLOEXEC, 0o200_0000);
        assert_eq!(TFD_TIMER_ABSTIME, 0x1);
        assert_eq!(TFD_TIMER_CANCEL_ON_SET, 0x2);
        assert_eq!(CLOCK_REALTIME, 0);
        assert_eq!(CLOCK_MONOTONIC, 1);
        assert_eq!(CLOCK_BOOTTIME, 7);
        assert_eq!(CLOCK_REALTIME_ALARM, 8);
        assert_eq!(CLOCK_BOOTTIME_ALARM, 9);
        assert_eq!(ITIMERSPEC_SIZE, 32);
        assert_eq!(ITIMERSPEC_VALUE, 16);
        // The man page's five clocks, and nothing else.
        for clockid in [
            CLOCK_REALTIME,
            CLOCK_MONOTONIC,
            CLOCK_BOOTTIME,
            CLOCK_REALTIME_ALARM,
            CLOCK_BOOTTIME_ALARM,
        ] {
            assert!(clock_ok(clockid));
        }
        assert!(!clock_ok(2)); // CLOCK_PROCESS_CPUTIME_ID: no CPU accounting
        assert!(!clock_ok(10));
        assert!(!clock_ok(usize::MAX));
    }

    /// The timespec<->tick conversions: validation as every timespec
    /// carrier validates, sub-tick timeouts rounded up, and a back
    /// conversion that truncates rather than rounding into a later answer.
    #[test]
    fn timespec_conversions_validate_and_round_up() {
        const HZ: u64 = 100;
        // A nanosecond is a whole tick: a timeout is a lower bound.
        assert_eq!(ticks_of_timespec(0, 1, HZ), Ok(1));
        assert_eq!(ticks_of_timespec(0, 0, HZ), Ok(0));
        assert_eq!(ticks_of_timespec(1, 0, HZ), Ok(100));
        // 1.5 seconds at 100Hz: 150 ticks exactly.
        assert_eq!(ticks_of_timespec(1, 500_000_000, HZ), Ok(150));
        assert_eq!(ticks_of_timespec(0, 999_999_999, HZ), Ok(100));
        // The one field every carrier validates.
        assert_eq!(ticks_of_timespec(0, 1_000_000_000, HZ), Err(err::EINVAL));
        // Saturation, never wrap: more seconds than ticks exist.
        assert_eq!(ticks_of_timespec(u64::MAX, 0, HZ), Ok(u64::MAX));

        // Back the other way: whole ticks are seconds, the rest is
        // nanoseconds, truncating.
        assert_eq!(timespec_of_ticks(0, HZ), (0, 0));
        assert_eq!(timespec_of_ticks(150, HZ), (1, 500_000_000));
        assert_eq!(timespec_of_ticks(99, HZ), (0, 990_000_000));
    }

    /// A one-shot: nothing before the deadline, one expiration after it,
    /// the read takes it, and the read after that finds none.
    #[test]
    fn a_one_shot_counts_once_and_the_read_takes_it() {
        let fd = TimerFd::new(1, CLOCK_MONOTONIC, false);
        fd.set(0, Some(10), 0, false);

        assert_eq!(fd.take_expirations(9), Err(VfsError::Again));
        assert!(!fd.pending(9));
        // Fired and not yet read: expired, not disarmed - the difference
        // `timerfd_gettime`'s zero remaining is answering.
        fd.advance(20);
        assert_eq!(fd.snapshot(20), Some((0, 0)));
        assert_eq!(fd.take_expirations(10), Ok(1), "the deadline is inclusive");
        assert_eq!(
            fd.take_expirations(10),
            Err(VfsError::Again),
            "taken is taken"
        );

        // The read spent it: what is left of a spent one-shot is nothing,
        // and `timerfd_gettime` answers a disarmed timer with zeros.
        assert_eq!(fd.snapshot(10), None);
    }

    /// An interval timer: expirations accumulate while nobody reads, the
    /// deadline walks forward, and a read takes the whole pile.
    #[test]
    fn an_interval_timer_counts_while_nobody_reads() {
        let fd = TimerFd::new(2, CLOCK_MONOTONIC, false);
        fd.set(0, Some(10), 5, false);

        assert_eq!(fd.take_expirations(10), Ok(1));
        // Asleep for a while: 10, 15, 20 and 25 have all passed by 27.
        assert_eq!(fd.take_expirations(27), Ok(3));
        // And the next expiration is the interval away from the last one.
        assert_eq!(fd.snapshot(27), Some((3, 5)));
    }

    /// The man page's saturation: a clock carried to the very top of the
    /// tick count with a one-tick interval owes "the most there can be",
    /// and the answer stops there rather than wrapping into a small
    /// number.
    #[test]
    fn an_expiry_count_saturates_rather_than_wrapping() {
        let fd = TimerFd::new(3, CLOCK_MONOTONIC, false);
        fd.set(0, Some(0), 1, false);
        fd.advance(u64::MAX);
        assert_eq!(fd.take_expirations(u64::MAX), Ok(u64::MAX));
    }

    /// A zero `it_value` disarms - the man page's spelling - and
    /// `timerfd_settime` reports the old setting on the way out, rounded
    /// up to the tick that is still owed.
    #[test]
    fn a_zero_value_disarms_and_reports_the_old_setting() {
        let fd = TimerFd::new(4, CLOCK_MONOTONIC, false);
        fd.set(0, Some(100), 10, false);
        assert_eq!(fd.set(50, None, 0, false), Some((50, 10)));
        assert_eq!(fd.snapshot(50), None, "disarmed is nothing left");
        assert_eq!(fd.take_expirations(1000), Err(VfsError::Again));

        // Disarming also clears what had expired unread, matching the
        // "since the settings were last modified" wording; the old
        // setting reports the fired one-shot as spent - zero left.
        fd.set(0, Some(10), 0, false);
        fd.advance(20);
        let old = fd.set(20, None, 0, false);
        assert_eq!(old, Some((0, 0)));
        assert_eq!(fd.take_expirations(20), Err(VfsError::Again));
    }

    /// `TFD_TIMER_CANCEL_ON_SET`: setting the clock cancels a realtime
    /// absolute timer that asked to know, and only those - monotonic
    /// timers are not on the clock that changed, and a realtime timer
    /// without the flag has nothing to be told.
    #[test]
    fn a_clock_set_cancels_only_the_timers_that_asked() {
        let canceled = TimerFd::new(5, CLOCK_REALTIME, false);
        canceled.set(0, Some(100), 0, true);

        let uncanceled = TimerFd::new(6, CLOCK_REALTIME, false);
        uncanceled.set(0, Some(100), 0, false);

        let monotonic = TimerFd::new(7, CLOCK_MONOTONIC, false);
        monotonic.set(0, Some(100), 0, true);

        // And a CANCEL_ON_SET timer that already fired: its expirations
        // are waiting to be read, and a clock set does not reach in and
        // erase news that has already happened.
        let fired = TimerFd::new(8, CLOCK_REALTIME, false);
        fired.set(0, Some(10), 0, true);
        fired.advance(20);
        assert_eq!(fired.take_expirations(20), Ok(1));

        cancel_on_clock_set();

        // The canceled one answers ECANCELED - and keeps answering it,
        // because the cancellation is a state, not an event.
        assert_eq!(canceled.take_expirations(100), Err(VfsError::Canceled));
        assert_eq!(canceled.take_expirations(100), Err(VfsError::Canceled));
        assert!(canceled.pending(100), "a cancellation is an answer");
        // The others carry on as though nothing happened.
        assert_eq!(uncanceled.take_expirations(100), Ok(1));
        assert_eq!(monotonic.take_expirations(100), Ok(1));
        // And re-arming clears the cancellation.
        canceled.set(100, Some(200), 0, false);
        assert_eq!(canceled.take_expirations(200), Ok(1));
    }

    /// The descriptor's contract: reads shorter than eight bytes are
    /// EINVAL, a nonblocking empty read is EAGAIN, and the eight bytes
    /// that come back are the count.
    #[test]
    fn the_read_answers_the_count_and_validates_its_buffer() {
        let fd = TimerFd::new(9, CLOCK_MONOTONIC, true);
        assert!(fd.nonblocking());
        assert_eq!(
            fd.read_at(0, &mut [0u8; 4]).err(),
            Some(VfsError::Invalid),
            "the man page's own refusal for a short buffer"
        );

        assert_eq!(fd.read_at(0, &mut [0u8; 8]).err(), Some(VfsError::Again));
        fd.set(0, Some(10), 2, false);
        fd.advance(35);
        let mut buf = [0u8; 8];
        assert_eq!(fd.read_at(0, &mut buf), Ok(8));
        assert_eq!(
            u64::from_ne_bytes(buf),
            13,
            "10, 12, ... 34: thirteen ticks"
        );
        // Writing to a timer is not a thing: the arm has its own call.
        assert_eq!(fd.write_at(0, b"x").err(), Some(VfsError::Invalid));
    }

    /// `poll`'s question: a timer with expirations waiting is readable,
    /// one without them is not, and neither is ever writable.
    #[test]
    fn poll_reports_readability_and_never_writability() {
        let fd = TimerFd::new(10, CLOCK_MONOTONIC, false);
        assert!(!fd.poll_ready().read);
        assert!(!fd.poll_ready().write);
        fd.set(0, Some(10), 0, false);
        fd.advance(10);
        assert!(fd.poll_ready().read);
        assert!(!fd.poll_ready().write);
    }
}
