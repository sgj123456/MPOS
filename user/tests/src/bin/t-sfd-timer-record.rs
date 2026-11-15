//! t-sfd-timer-record - what a POSIX timer's expiry puts in its `siginfo_t`,
//! read through both doors a caller can use.
//!
//! A `timer_create` expiry is not "a signal with a payload". It is a signal
//! whose record carries the **handle the timer was created with** and the
//! **count of expiries that arrived while the previous record was still
//! unread**, in the `_timer` union member - which is the same eight bytes a
//! `kill` spends on `si_pid`/`si_uid`. A kernel that drops them hands a reader
//! a zero where its own timer number belongs, and nothing else fails: the
//! `timespec`-shaped value is there, the signal arrives, and the program that
//! wanted to know *which* timer fired (or how far behind it is) is told `0`.
//!
//! That is item 286's subject, measured on Linux 7.3.4 before any code was
//! written (`build/scratch/sfdhost/{dump,gov,sem3,life2,frozen3,it2,rearm,
//! first}.c`; the contract they produced is that directory's `CONTRACT.md`,
//! and the lesson is `docs/notes/` §6.166). The measurements this pin encodes:
//!
//!   * a `timer_create` expiry's record carries `si_code` **-2** (`SI_TIMER`),
//!     `si_timerid` = the handle `timer_create` answered, `si_overrun` = the
//!     live count of misses, and the creator's `sigval`;
//!   * the count is **live, not frozen at queue time**: a 20 ms timer left
//!     unread for 500 ms delivers a record reading 24, and `timer_getoverrun`
//!     reads **0** while the record is still pending and that same 24 only
//!     after it was read (whose number it then keeps until the next read);
//!   * an `ITIMER_REAL` (`setitimer`) expiry is **not** this record: it has no
//!     handle and no counter, and Linux answers `SI_KERNEL` (128) with both
//!     `_timer` words zero. A kernel that queued it as `SI_TIMER` told a caller
//!     it had a timer handle it never had;
//!   * a **one-shot** timer's record carries `si_overrun` 0.
//!
//! # The two doors, and why both are pinned
//!
//! `siginfo_t` is a **union** and `signalfd_siginfo` is a **flat struct**, so
//! the same facts land at different offsets: the handler's frame keeps
//! `si_timerid`/`si_overrun` at bytes **16**/**20** (raw words 4 and 5, where a
//! sender's pid/uid sit), while the descriptor's `ssi_tid`/`ssi_overrun` are at
//! bytes **24**/**32** and its `ssi_int` at **44** beside `ssi_ptr` at 48.
//! A kernel can fill one and none of the other - this one did, for both - so
//! each door gets its own claims below rather than one claim read twice.
//!
//! The handler half reads the frame as **raw `u32` words** on purpose: a claim
//! about *where* a field sits is not the same claim as "this libc's accessor
//! agrees with itself", and the offsets are the ABI. The descriptor half reads
//! `libc::signalfd_siginfo`, whose layout this crate pins against the ABI's 128
//! bytes in `t-signalfd` already.
//!
//! # The doors
//!
//! `rustix` binds none of the timer family (`t-timer-create` measured that:
//! `libc` binds no `sigevent`/`itimerspec`/`timer_t` for Linux either), and it
//! binds no `signalfd` shape that takes a raw mask, so the calls go through
//! `libc::syscall(libc::SYS_*, ..)` with the numbers from `libc`'s
//! per-machine table - the same convention `t-timer-create` and `t-signalfd`
//! follow. What `rustix` *can* express is used: the descriptor is an `OwnedFd`,
//! its reads are `rustix::io::read` (with the `EAGAIN` an empty nonblocking
//! descriptor owes), and the sleeps are `rustix::thread::nanosleep`.
//!
//! # Not covered here
//!
//! Two things sit beside this record and are pinned elsewhere on purpose:
//! whether a **deleted** timer's already-queued record survives its timer (a
//! named divergence, `t-timer-create`) and what `timer_getoverrun` answers
//! after a *disarm* (`build/scratch/sfdhost/rearm.c` measured 0; that is the
//! timer's own lifecycle, and pinning it here would mix two subjects).

use mpos_user_tests::Report;
use rustix::fd::OwnedFd;
use rustix::io::{Errno, read};
use rustix::time::Timespec as RustixTimespec;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};

/// The `sival_int` the creator puts on every timer here. A value a kernel
/// could not have invented: it is not a handle, not a pid, and not zero.
const PAYLOAD: i32 = 4242;

/// The repeating interval. Twenty milliseconds is five ticks on the guest's
/// 250 Hz clock and thousands of nanoseconds on a host - so "several expiries
/// have gone by" is a fact about elapsed time either way, and a bounded
/// deadline rather than a count of loops is what makes that true.
const INTERVAL_MS: i64 = 20;

/// How long a record is allowed to take to appear. Generous: a TCG lane runs
/// the guest on the same host as the host's own kernel, and a claim that
/// fired 200 ms after the arm is still 200 ms late, never early.
const RECORD_DEADLINE_MS: i64 = 3000;

/// The `_timer` member, in both of the ABI's two spellings.
///
/// `siginfo_t`'s union puts it at 16 and 20 - the eight bytes a `kill` spends
/// on `si_pid`/`si_uid` - which is the whole reason the record's *class*
/// decides what those bytes mean. `signalfd_siginfo` is flat, so the descriptor
/// puts `ssi_tid` at 24 and `ssi_overrun` at 32 (and the handler frame's
/// measured words 4 and 5 are the same facts at 16 and 20:
/// `CONTRACT.md` §1, `build/scratch/sfdhost/dump.c`).
const SI_TIMERID_AT: usize = 16;
const SI_OVERRUN_AT: usize = 20;
/// The `sigval` in a handler's frame: `_timer`'s third word, byte 24.
const SI_VALUE_AT: usize = 24;

/// `SI_KERNEL`: what an `ITIMER_REAL` expiry carries, not `SI_TIMER`. Taken
/// from `libc` (0x80 == 128) rather than spelled here, so a claim about the
/// number is a claim about the ABI table both arms build against.
const SI_KERNEL: i32 = libc::SI_KERNEL;

/// The record size the descriptor writes, and the size a read must ask for.
const SIGINFO_BYTES: usize = 128;

/// The uapi's `struct sigevent`: a `sigval` at 0, the signal number at 8, the
/// notification kind at 12, then a 48-byte union whose first `int` is the
/// `SIGEV_THREAD_ID` tid. Sixty-four bytes, which is what a libc passes.
#[repr(C)]
struct SigEvent {
    value: u64,
    signo: i32,
    notify: i32,
    un: [u8; 48],
}

const _: () = assert!(core::mem::size_of::<SigEvent>() == 64);

impl SigEvent {
    /// A timer that signals the creating task, carrying `sival_int = value`.
    fn signal(signo: i32, value: i32) -> Self {
        Self {
            // `sigval`'s low word. The record reads it back as `ssi_int` and
            // as `ssi_ptr`, which is one field seen twice.
            value: value as u32 as u64,
            signo,
            notify: libc::SIGEV_SIGNAL,
            un: [0; 48],
        }
    }
}

/// The uapi's `timespec`, spelled with `libc::c_long` (which is what musl's
/// `time_t`/`suseconds_t` are on these two 64-bit targets) so no deprecated
/// alias is named.
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
    fn zero() -> Self {
        Self {
            interval: TimeSpec::zero(),
            value: TimeSpec::zero(),
        }
    }

    /// A repeating timer: first expiry and reload both `millis`.
    fn repeating(millis: i64) -> Self {
        Self {
            interval: TimeSpec::ms(millis),
            value: TimeSpec::ms(millis),
        }
    }

    /// A one-shot: one expiry, no reload.
    fn one_shot(millis: i64) -> Self {
        Self {
            interval: TimeSpec::zero(),
            value: TimeSpec::ms(millis),
        }
    }
}

/// The uapi's `struct itimerval`, which `setitimer` takes - and its two halves
/// are `struct timeval`, not `struct timespec`.
///
/// That is a distinction the first version of this program got wrong and the host
/// run caught: the field is `tv_usec`, so a "20 million nanoseconds" value put
/// into it asks for twenty *seconds* and `setitimer` answers `EINVAL` (22) - the
/// call rejects a `tv_usec` outside `0..1_000_000` rather than rounding it. Every
/// other timer record in this program is a `timespec` (`timer_create`,
/// `timer_settime`), which is exactly why the two were confused.
#[repr(C)]
struct Itimerval {
    interval: Timeval,
    value: Timeval,
}

/// `struct timeval`: seconds and **microseconds**, the shape `setitimer`,
/// `getitimer` and every `SO_*TIMEO` take.
#[repr(C)]
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

    fn usec(micros: i64) -> Self {
        Self {
            tv_sec: micros / 1_000_000,
            tv_usec: micros % 1_000_000,
        }
    }
}

/// A raw syscall through `libc`'s per-machine number table, with the refusal
/// in `Errno` rather than in a stale `errno`.
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

/// `timer_create(clockid, evp, &id)`. The handle comes back in the caller's
/// `int`, which is the number the record must carry.
fn timer_create(clockid: i32, event: &SigEvent) -> Result<i32, Errno> {
    let mut id: i32 = -1;
    raw(
        libc::SYS_timer_create,
        clockid as usize,
        event as *const SigEvent as usize,
        &raw mut id as usize,
        0,
    )?;
    Ok(id)
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

fn timer_getoverrun(id: i32) -> Result<isize, Errno> {
    raw(libc::SYS_timer_getoverrun, id as usize, 0, 0, 0)
}

fn timer_delete(id: i32) -> Result<isize, Errno> {
    raw(libc::SYS_timer_delete, id as usize, 0, 0, 0)
}

/// `rt_sigprocmask(how, set, old, sigsetsize=8)`: one `u64` is the wire's
/// 64-signal mask, built here through the libc's own `sigemptyset`/`sigaddset`
/// so the bit arithmetic stays the machine's business.
fn mask_change(how: libc::c_int, signals: &[i32]) -> Result<isize, Errno> {
    let mut set: libc::sigset_t = unsafe { core::mem::zeroed() };
    for &sig in signals {
        // SAFETY: `set` is a `sigset_t` this call owns and `sig` is a number
        // the caller chose from this program's own set.
        unsafe { libc::sigaddset(&mut set, sig) };
    }
    raw(
        libc::SYS_rt_sigprocmask,
        how as usize,
        &raw const set as usize,
        0,
        8,
    )
}

fn block(signals: &[i32]) -> Result<isize, Errno> {
    mask_change(libc::SIG_BLOCK, signals)
}

fn unblock(signals: &[i32]) -> Result<isize, Errno> {
    mask_change(libc::SIG_UNBLOCK, signals)
}

/// `signalfd(-1, mask, flags)`: the descriptor this program reads signals
/// through, nonblocking so an empty read is `EAGAIN` rather than a park.
fn signalfd_new(signals: &[i32]) -> Option<OwnedFd> {
    let mut set: libc::sigset_t = unsafe { core::mem::zeroed() };
    for &sig in signals {
        // SAFETY: as in `mask_change`.
        unsafe { libc::sigaddset(&mut set, sig) };
    }
    // SAFETY: a set this call owns, -1 for "make a new descriptor", and flags
    // from the `SFD_*` pair.
    let fd = unsafe { libc::signalfd(-1, &set, libc::SFD_NONBLOCK | libc::SFD_CLOEXEC) };
    (fd >= 0).then(|| {
        // SAFETY: the call just handed this process a fresh descriptor and
        // `fd` is the only copy of the number.
        unsafe { std::os::fd::FromRawFd::from_raw_fd(fd) }
    })
}

/// `setitimer(ITIMER_REAL, ..)`: the `SIGALRM` timer that is **not** a
/// `timer_create` timer. `libc` may or may not bind the wrapper, so this goes
/// through the maintained table like the rest of the family.
fn setitimer_real(new: &Itimerval) -> Result<isize, Errno> {
    raw(
        libc::SYS_setitimer,
        libc::ITIMER_REAL as usize,
        new as *const Itimerval as usize,
        0,
        0,
    )
}

/// Disarm `ITIMER_REAL`.
fn itimer_off() -> Result<isize, Errno> {
    setitimer_real(&Itimerval {
        interval: Timeval::zero(),
        value: Timeval::zero(),
    })
}

/// `rt_sigtimedwait` with a **zero timeout** - the polling form. A null timeout
/// would park when there is nothing queued, which is a hang rather than an
/// answer for a drain that is only cleaning up.
fn sigtimedwait_poll(mask: u64) -> Result<isize, Errno> {
    let zero = TimeSpec::zero();
    raw(
        libc::SYS_rt_sigtimedwait,
        &raw const mask as usize,
        0,
        &raw const zero as usize,
        8,
    )
}

/// `rt_sigpending`: which signals are queued for this task, in the wire's
/// 64-signal mask (signal *n* at bit *n-1*).
fn sigpending() -> Result<u64, Errno> {
    let mut set: u64 = 0;
    raw(libc::SYS_rt_sigpending, &raw mut set as usize, 8, 0, 0)?;
    Ok(set)
}

/// Signal *n*'s bit in the wire's mask.
fn bit(sig: i32) -> u64 {
    1u64 << (sig - 1)
}

/// Sleep for `millis`, in the relative form this kernel serves.
fn sleep_ms(millis: i64) {
    let ts = RustixTimespec {
        tv_sec: millis / 1000,
        tv_nsec: (millis % 1000) * 1_000_000,
    };
    let _ = rustix::thread::nanosleep(&ts);
}

/// The record a `signalfd` read returns, or `None` when nothing is queued.
///
/// A single **nonblocking** read: `EAGAIN` means "no record yet", and saying so
/// is what lets the callers below retry with a deadline instead of parking.
fn read_record(fd: &OwnedFd) -> Result<libc::signalfd_siginfo, Errno> {
    let mut buf = [0u8; SIGINFO_BYTES];
    match read(fd, &mut buf) {
        Ok(SIGINFO_BYTES) => {
            // SAFETY: the kernel just wrote the whole record into `buf`, and
            // `signalfd_siginfo` is that record's type on this machine.
            Ok(unsafe { std::ptr::read_unaligned(buf.as_ptr().cast::<libc::signalfd_siginfo>()) })
        }
        // A short read is not a shape this descriptor produces; read it as the
        // refusal it is rather than as a record with uninitialised fields.
        Ok(_) => Err(Errno::INVAL),
        Err(e) => Err(e),
    }
}

/// Wait up to [`RECORD_DEADLINE_MS`] for one record, then take it.
///
/// The wait polls with a 2 ms nap: a descriptor read is the *only* thing that
/// consumes a record, so "is there one yet?" and "give me it" have to be the
/// same call - which is why this cannot be a `poll(2)` followed by a read on a
/// kernel whose readiness answer might disagree with what the read returns.
fn take_record(fd: &OwnedFd, label: &str, r: &mut Report) -> Option<libc::signalfd_siginfo> {
    let mut waited = 0;
    let mut last: Result<libc::signalfd_siginfo, Errno> = Err(Errno::AGAIN);
    while waited < RECORD_DEADLINE_MS {
        match read_record(fd) {
            Ok(rec) => {
                r.check(label, true);
                return Some(rec);
            }
            Err(e) => last = Err(e),
        }
        sleep_ms(2);
        waited += 2;
    }
    r.check(
        &format!("{label} (no record in {RECORD_DEADLINE_MS} ms; last {last:?})"),
        false,
    );
    None
}

/// Give every queued copy of `sig` back, so restoring the mask cannot run a
/// default action. Bounded: a kernel that kept a pending *bit* with no queued
/// record behind it answers `EAGAIN` for ever, and this must not spin on that.
fn drain(sig: i32) {
    for _ in 0..64 {
        if sigtimedwait_poll(bit(sig)).is_err() {
            return;
        }
    }
}

/// What a handler's frame carried, recorded field by field because `printf` is
/// not async-signal-safe. The raw words are the ABI's offsets, not a libc's
/// accessor.
static HANDLER_RAN: AtomicBool = AtomicBool::new(false);
static HANDLER_SIGNO: AtomicI32 = AtomicI32::new(-1);
static HANDLER_CODE: AtomicI32 = AtomicI32::new(0x7fff_ffff);
static HANDLER_TIMERID: AtomicI32 = AtomicI32::new(-1);
static HANDLER_OVERRUN: AtomicI32 = AtomicI32::new(-1);
static HANDLER_VALUE: AtomicI32 = AtomicI32::new(-1);

/// Read one `siginfo_t` word out of the frame at a byte offset.
///
/// # Safety
/// `info` must point at the record the kernel wrote for this delivery, and
/// `at + 4` must be inside it. Both hold for the offsets this program asks
/// for: the kernel writes the ABI's 128 bytes.
unsafe fn word_at(info: *const libc::siginfo_t, at: usize) -> u32 {
    // SAFETY: the caller upholds "inside the record".
    unsafe { info.cast::<u8>().add(at).cast::<u32>().read_unaligned() }
}

/// The `SA_SIGINFO` handler: records the **first** record it is handed.
///
/// Keeping only the first is load-bearing rather than tidy. The timer repeats, so
/// a second delivery can arrive in the window between the first one returning and
/// the program's own check - reliably on an emulated machine, where that window
/// is milliseconds - and a later record's `si_overrun` is a *fresh* record's
/// count (0), not the pile-up this claim is about. The guest run caught exactly
/// that: `got 0` where the first delivery had carried 6, on a machine slow enough
/// for a second expiry to land first. Linux's own first delivery is the one that
/// carries the count (measured: `build/scratch/sfdhost/gprobe2.c` reads
/// `[4]=1 [5]=6 [6]=4242` on run 1 and `[5]=6` again on run 2, both
/// `sival_int` 4242).
///
/// The rest of the frame - the signal, the handle, the payload - does not move
/// between deliveries, so only the count needed the rule.
extern "C" fn record_handler(
    sig: libc::c_int,
    info: *mut libc::siginfo_t,
    _ctx: *mut libc::c_void,
) {
    // SAFETY: the kernel hands a handler a live `siginfo_t` for its signal; a
    // null one is not a shape it delivers, and is checked rather than assumed.
    unsafe {
        if info.is_null() {
            return;
        }
        if HANDLER_RAN.load(Ordering::SeqCst) {
            return;
        }
        HANDLER_SIGNO.store(sig, Ordering::Relaxed);
        // `si_code` is the ABI's third word, at byte 8, and is a plain field on
        // both libcs where `si_pid`/`si_status` are accessors.
        HANDLER_CODE.store((*info).si_code, Ordering::Relaxed);
        // The `_timer` member: bytes 16 and 20, the slots a sender spends on
        // `si_pid`/`si_uid`. Read as raw words, because *where* they sit is
        // half of what this program pins.
        HANDLER_TIMERID.store(word_at(info, SI_TIMERID_AT) as i32, Ordering::Relaxed);
        HANDLER_OVERRUN.store(word_at(info, SI_OVERRUN_AT) as i32, Ordering::Relaxed);
        // `_timer.si_sigval`, byte 24, whose low word is the `sival_int` the
        // creator set.
        HANDLER_VALUE.store(word_at(info, SI_VALUE_AT) as i32, Ordering::Relaxed);
    }
    HANDLER_RAN.store(true, Ordering::SeqCst);
}

/// Install [`record_handler`] with `SA_SIGINFO` for `sig`.
fn install_handler(sig: i32) -> bool {
    let mut sa: libc::sigaction = unsafe { core::mem::zeroed() };
    sa.sa_sigaction = record_handler
        as extern "C" fn(libc::c_int, *mut libc::siginfo_t, *mut libc::c_void)
        as usize;
    sa.sa_flags = libc::SA_SIGINFO;
    // SAFETY: a `sigaction` this process owns, and no old one is wanted.
    unsafe { libc::sigaction(sig, &sa, std::ptr::null_mut()) == 0 }
}

/// Set `SIG_IGN` for `sig`: a pending copy of an ignored signal is discarded,
/// which is how the mask can be handed back without a queued record deciding
/// this program's exit status.
fn ignore(sig: i32) -> bool {
    let mut sa: libc::sigaction = unsafe { core::mem::zeroed() };
    sa.sa_sigaction = libc::SIG_IGN;
    // SAFETY: as above.
    unsafe { libc::sigaction(sig, &sa, std::ptr::null_mut()) == 0 }
}

fn main() {
    println!("t-sfd-timer-record");
    let mut r = Report::new();

    // The two records are the ABI's sizes, not numbers written here: 128 bytes
    // for `signalfd_siginfo`, which is what a read has to ask for.
    r.check(
        "the signalfd record is the size the ABI names (128)",
        core::mem::size_of::<libc::signalfd_siginfo>() == SIGINFO_BYTES,
    );

    // The signal classes this program uses. `SIGRTMIN` comes from the libc
    // running the program (musl reserves 32..=34 for itself, glibc 32..=33),
    // and +3 is inside the kernel's real-time range on both: 32..=64, which is
    // the range whose expiries are *queued* rather than coalesced. The classes
    // differ in the ABI, not in what the record carries - measured identical on
    // 7.3.4 for `SIGALRM` and `SIGRTMIN+3` (`CONTRACT.md` §3) - but a kernel
    // that only filled the standard-signal slot is exactly the bug a single
    // class could not see, so a real-time number is what this pin uses.
    let rt_signo = libc::SIGRTMIN() + 3;
    let handler_signo = libc::SIGRTMIN() + 6;
    let oneshot_signo = libc::SIGRTMIN() + 8;

    // === 1-4, 6: the record through a signalfd ==============================
    //
    // A decoy timer is created **first**, so the handle under test is not 0 and
    // a record whose `ssi_tid` was simply never written cannot pass claim 2.
    let signalfd_section = block(&[rt_signo]).is_ok();
    r.check(
        "SIGRTMIN+3 is blocked before the timer is made",
        signalfd_section,
    );
    if signalfd_section {
        let fd = signalfd_new(&[rt_signo]);
        r.check(
            "signalfd(-1, SIGRTMIN+3, NONBLOCK|CLOEXEC) hands back a descriptor",
            fd.is_some(),
        );
        if let Some(fd) = fd.as_ref() {
            let decoy = timer_create(libc::CLOCK_MONOTONIC, &SigEvent::signal(rt_signo, 0));
            let created = timer_create(libc::CLOCK_MONOTONIC, &SigEvent::signal(rt_signo, PAYLOAD));
            r.check(
                "a decoy timer and the timer under test are both created",
                decoy.is_ok() && created.is_ok(),
            );
            if let (Ok(decoy_id), Ok(timer_id)) = (decoy, created) {
                r.check(
                    "the handle under test is not 0, so an unwritten slot cannot pass for it",
                    timer_id != 0 && timer_id != decoy_id,
                );
                r.check(
                    "the repeating timer is armed",
                    timer_settime(timer_id, &Itimerspec::repeating(INTERVAL_MS)).is_ok(),
                );

                // Let several intervals pass **unread**: one record is queued at
                // the first expiry, and every expiry after it finds that record
                // still waiting and moves its count. Nothing is read until the
                // claims below have taken the "while pending" reading.
                sleep_ms(200);

                let while_pending = timer_getoverrun(timer_id);
                r.check(
                    "timer_getoverrun reads 0 while the record is still unread",
                    while_pending.is_ok_and(|n| n == 0),
                );

                if let Some(rec) =
                    take_record(fd, "the expiry is readable through the signalfd", &mut r)
                {
                    r.check(
                        "the record's ssi_signo is the timer's signal",
                        rec.ssi_signo == rt_signo as u32,
                    );
                    r.check(
                        &format!(
                            "its ssi_code is SI_TIMER (-2), not {} (SI_KERNEL)",
                            SI_KERNEL
                        ),
                        rec.ssi_code == libc::SI_TIMER,
                    );
                    // Claim 2: the handle, and not the decoy's.
                    r.check(
                        &format!(
                            "its ssi_tid is the handle timer_create answered ({timer_id}), \
                             not the decoy's ({decoy_id})"
                        ),
                        rec.ssi_tid == timer_id as u32 && rec.ssi_tid != decoy_id as u32,
                    );
                    // Claim 3: the count of expiries that found the record unread,
                    // which is `timer_getoverrun`'s number once the record is
                    // handed over - and the live accumulation until then.
                    r.check(
                        &format!(
                            "its ssi_overrun is the expiries that found it unread (> 0), got {}",
                            rec.ssi_overrun
                        ),
                        rec.ssi_overrun > 0,
                    );
                    // Claim 4: the sigval, both views of the one field.
                    r.check(
                        &format!(
                            "its ssi_int is the sival_int the creator set ({PAYLOAD}), got {}",
                            rec.ssi_int
                        ),
                        rec.ssi_int == PAYLOAD,
                    );
                    r.check(
                        &format!(
                            "its ssi_ptr carries the same value ({PAYLOAD}), got {:#x}",
                            rec.ssi_ptr
                        ),
                        rec.ssi_ptr == PAYLOAD as u32 as u64,
                    );
                    // Claim 6, second half: after the read the door agrees with
                    // the record, and it is the record's number rather than the
                    // live accumulation.
                    let after = timer_getoverrun(timer_id);
                    r.check(
                        &format!(
                            "after the read timer_getoverrun reads the count the record carried \
                             ({}, got {after:?})",
                            rec.ssi_overrun
                        ),
                        after.is_ok_and(|n| n as u32 == rec.ssi_overrun),
                    );

                    // Coalescing, asked in the shape a repeating timer can
                    // actually answer: expiries that arrive while a record waits
                    // move that record's **count**; they do not each queue a
                    // record. So the *next* record - the one the arm already
                    // left waiting - carries the misses that piled up between
                    // the read above and now, and it is the only record that was
                    // waiting.
                    //
                    // Not "a drain finds at most one record": a drain loop on a
                    // repeating timer races the timer, so how many records it
                    // collects depends on how long it runs, which is the
                    // machine's speed and not the kernel's shape. On the
                    // aarch64 arm that claim read `drained 2` and failed, and
                    // the claim was the wrong side - measured on Linux 7.3.4,
                    // `build/scratch/sfdhost/coalesce.c` finds **one** record
                    // with `si_overrun` 2 after three unread intervals, because
                    // it disarms before draining. The count is the property that
                    // is true on every machine: a queueing kernel would leave a
                    // record whose `si_overrun` is 0 (each expiry got its own
                    // copy), a coalescing one leaves the pile-up.
                    sleep_ms(65);
                    if let Ok(next) = read_record(fd) {
                        r.check(
                            &format!(
                                "the expiries coalesce into that record's count ({} > 0)",
                                next.ssi_overrun
                            ),
                            next.ssi_overrun > 0,
                        );
                    } else {
                        r.check("the coalescing record is readable", false);
                    }
                }

                // Clean up: disarm, drain, then delete - in that order, so no
                // record is left in flight when the timer number dies.
                let _ = timer_settime(timer_id, &Itimerspec::zero());
                sleep_ms(20);
                drain(rt_signo);
                r.check(
                    "the timer under test deletes",
                    timer_delete(timer_id).is_ok(),
                );
                r.check("the decoy timer deletes", timer_delete(decoy_id).is_ok());
            }
        }
        let _ = ignore(rt_signo);
        drain(rt_signo);
        r.check(
            "SIGRTMIN+3 is unblocked again",
            unblock(&[rt_signo]).is_ok(),
        );
    }

    // === 5: the same facts through a handler's siginfo_t ====================
    //
    // The eight bytes at 16/20 are the point: `siginfo_t` is a union, so the
    // `_timer` member lives exactly where a `kill`'s `si_pid`/`si_uid` do, and
    // a kernel that only fills the descriptor's flat slots leaves a handler
    // reading a pid that is really its timer's handle.
    //
    // The signal is **blocked while the record accumulates** and unblocked to
    // let the handler run: that is how a handler sees a non-zero `si_overrun`
    // at all (a promptly-delivered signal has missed nothing, which is what
    // `build/scratch/sfdhost/dump.c` measured at a 1 ms interval - words
    // `[4]=1 [5]=0`). The count in the record is live while it waits, so the
    // unblock delivers the accumulated number, the same reading
    // `CONTRACT.md` §2 took through the descriptor.
    let handler_blocked = block(&[handler_signo]).is_ok();
    r.check(
        "SIGRTMIN+6 is blocked before a handler could run on it",
        handler_blocked,
    );
    if handler_blocked {
        r.check(
            "an SA_SIGINFO handler is installed for SIGRTMIN+6",
            install_handler(handler_signo),
        );
        let decoy = timer_create(libc::CLOCK_MONOTONIC, &SigEvent::signal(handler_signo, 0));
        let created = timer_create(
            libc::CLOCK_MONOTONIC,
            &SigEvent::signal(handler_signo, PAYLOAD),
        );
        r.check(
            "a decoy and a timer for the handler are both created",
            decoy.is_ok() && created.is_ok(),
        );
        if let (Ok(_decoy_id), Ok(timer_id)) = (decoy, created) {
            r.check(
                "the handler's timer is armed as a repeating one",
                timer_settime(timer_id, &Itimerspec::repeating(INTERVAL_MS)).is_ok(),
            );
            // Several expiries land while the record waits for its handler.
            sleep_ms(150);
            HANDLER_RAN.store(false, Ordering::SeqCst);
            r.check(
                "the signal is unblocked, so the queued record runs the handler",
                unblock(&[handler_signo]).is_ok(),
            );
            let mut waited = 0;
            while !HANDLER_RAN.load(Ordering::SeqCst) && waited < RECORD_DEADLINE_MS {
                sleep_ms(2);
                waited += 2;
            }
            r.check(
                "the handler ran on the timer's expiry",
                HANDLER_RAN.load(Ordering::SeqCst),
            );
            if HANDLER_RAN.load(Ordering::SeqCst) {
                r.check(
                    "the handler's frame carries the timer's signal",
                    HANDLER_SIGNO.load(Ordering::SeqCst) == handler_signo,
                );
                r.check(
                    &format!(
                        "the frame's si_code is SI_TIMER (-2), got {}",
                        HANDLER_CODE.load(Ordering::SeqCst)
                    ),
                    HANDLER_CODE.load(Ordering::SeqCst) == libc::SI_TIMER,
                );
                // Claim 5's first half: byte 16, raw word 4.
                r.check(
                    &format!(
                        "the frame's si_timerid (byte {SI_TIMERID_AT}, raw word 4) is the handle \
                         ({timer_id}), got {}",
                        HANDLER_TIMERID.load(Ordering::SeqCst)
                    ),
                    HANDLER_TIMERID.load(Ordering::SeqCst) == timer_id,
                );
                // Claim 5's second half: byte 20, raw word 5.
                r.check(
                    &format!(
                        "the frame's si_overrun (byte {SI_OVERRUN_AT}, raw word 5) is the count \
                         that piled up ({}), got {}",
                        HANDLER_OVERRUN.load(Ordering::SeqCst),
                        HANDLER_OVERRUN.load(Ordering::SeqCst)
                    ),
                    HANDLER_OVERRUN.load(Ordering::SeqCst) > 0,
                );
                // The payload rides the same member's third word.
                r.check(
                    &format!(
                        "the frame's sigval (byte {SI_VALUE_AT}) is the sival_int the creator set \
                         ({PAYLOAD}), got {}",
                        HANDLER_VALUE.load(Ordering::SeqCst)
                    ),
                    HANDLER_VALUE.load(Ordering::SeqCst) == PAYLOAD,
                );
            }
            let _ = timer_settime(timer_id, &Itimerspec::zero());
            sleep_ms(20);
            drain(handler_signo);
            r.check(
                "the handler's timer deletes",
                timer_delete(timer_id).is_ok(),
            );
        }
        let _ = ignore(handler_signo);
        drain(handler_signo);
        r.check(
            "SIGRTMIN+6 is unblocked again",
            unblock(&[handler_signo]).is_ok(),
        );
    }

    // === 7: an ITIMER_REAL expiry is SI_KERNEL, not SI_TIMER ================
    //
    // `setitimer` has no `timer_create` handle and no overrun counter, and
    // Linux says so in the record's own code: 128 (`SI_KERNEL`) with both
    // `_timer` words zero (`CONTRACT.md` §4, `build/scratch/sfdhost/it2.c`).
    // Read through a descriptor with `SIGALRM` blocked so no default action
    // can run - the default action of `SIGALRM` is to end this process.
    let itimer_blocked = block(&[libc::SIGALRM]).is_ok();
    r.check(
        "SIGALRM is blocked before the itimer is armed",
        itimer_blocked,
    );
    if itimer_blocked {
        let fd = signalfd_new(&[libc::SIGALRM]);
        r.check(
            "a signalfd for SIGALRM hands back a descriptor",
            fd.is_some(),
        );
        if let Some(fd) = fd.as_ref() {
            let armed = setitimer_real(&Itimerval {
                interval: Timeval::zero(),
                value: Timeval::usec(20_000),
            });
            r.check(
                &format!("ITIMER_REAL is armed for one 20 ms expiry (got {armed:?})"),
                armed.is_ok(),
            );
            if let Some(rec) = take_record(fd, "the itimer's expiry is readable", &mut r) {
                r.check(
                    "the record's signo is SIGALRM",
                    rec.ssi_signo == libc::SIGALRM as u32,
                );
                r.check(
                    &format!(
                        "its ssi_code is SI_KERNEL ({SI_KERNEL}), not SI_TIMER (-2), got {}",
                        rec.ssi_code
                    ),
                    rec.ssi_code == SI_KERNEL,
                );
                // Both `_timer` words zero: an itimer expiry has no handle and
                // no miss count, and a kernel that answered SI_TIMER here would
                // have to invent both.
                r.check(
                    &format!("its ssi_tid is 0 (no timer handle), got {}", rec.ssi_tid),
                    rec.ssi_tid == 0,
                );
                r.check(
                    &format!(
                        "its ssi_overrun is 0 (no miss counter), got {}",
                        rec.ssi_overrun
                    ),
                    rec.ssi_overrun == 0,
                );
            }
            r.check("ITIMER_REAL is disarmed again", itimer_off().is_ok());
        }
        let _ = ignore(libc::SIGALRM);
        drain(libc::SIGALRM);
        r.check(
            "SIGALRM is unblocked again",
            unblock(&[libc::SIGALRM]).is_ok(),
        );
    }

    // === 8: a one-shot timer's record carries no overrun ====================
    //
    // `si_overrun` counts expiries that arrived while the previous record was
    // unread. A timer with no interval cannot have one, so Linux answers 0 -
    // and a kernel that copied the live counter regardless would report the
    // last periodic timer's number here (`CONTRACT.md` §2, `it2.c`).
    let oneshot_blocked = block(&[oneshot_signo]).is_ok();
    r.check(
        "SIGRTMIN+8 is blocked before the one-shot timer is made",
        oneshot_blocked,
    );
    if oneshot_blocked {
        let fd = signalfd_new(&[oneshot_signo]);
        r.check(
            "a signalfd for the one-shot's signal hands back a descriptor",
            fd.is_some(),
        );
        if let Some(fd) = fd.as_ref() {
            let decoy = timer_create(libc::CLOCK_MONOTONIC, &SigEvent::signal(oneshot_signo, 0));
            let created = timer_create(
                libc::CLOCK_MONOTONIC,
                &SigEvent::signal(oneshot_signo, PAYLOAD),
            );
            r.check(
                "a decoy and a one-shot timer are both created",
                decoy.is_ok() && created.is_ok(),
            );
            if let (Ok(decoy_id), Ok(timer_id)) = (decoy, created) {
                r.check(
                    "the one-shot timer is armed with no interval",
                    timer_settime(timer_id, &Itimerspec::one_shot(INTERVAL_MS)).is_ok(),
                );
                if let Some(rec) = take_record(fd, "the one-shot's expiry is readable", &mut r) {
                    r.check(
                        "its ssi_code is SI_TIMER too (the class is the timer, not the interval)",
                        rec.ssi_code == libc::SI_TIMER,
                    );
                    r.check(
                        "its ssi_tid is the one-shot's handle",
                        rec.ssi_tid == timer_id as u32,
                    );
                    r.check(
                        &format!(
                            "its ssi_overrun is 0 (a one-shot cannot miss), got {}",
                            rec.ssi_overrun
                        ),
                        rec.ssi_overrun == 0,
                    );
                }
                r.check("the one-shot timer deletes", timer_delete(timer_id).is_ok());
                r.check(
                    "the one-shot's decoy deletes",
                    timer_delete(decoy_id).is_ok(),
                );
            }
        }
        let _ = ignore(oneshot_signo);
        drain(oneshot_signo);
        r.check(
            "SIGRTMIN+8 is unblocked again",
            unblock(&[oneshot_signo]).is_ok(),
        );
    }

    // A last, cheap guard on the cleanup above: nothing this program armed may
    // still be queued. A pending bit left behind would be this program's own
    // mess rather than the kernel's answer, so it is reported as a claim.
    let left = sigpending();
    r.check(
        &format!("no signal this program armed is left pending ({left:?})"),
        left.is_ok_and(|set| {
            set & (bit(rt_signo) | bit(handler_signo) | bit(oneshot_signo) | bit(libc::SIGALRM))
                == 0
        }),
    );

    r.finish()
}
