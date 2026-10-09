//! t-forkcost - what a `fork` costs, what the `wait4` after it costs, and what
//! each of those has to do with the size of the process. On both machines.
//!
//! A `fork` has to hand the child a copy of every writable page the parent owns,
//! and it does that without copying anything: the frames are shared and marked
//! copy-on-write. That marking used to be followed by a full CR3 write per page,
//! which is where the first version of this test pointed - and it was wrong about
//! where the time goes, twice over. The first instrument timed `fork` and
//! `wait4` together and reported no difference for a change that removed sixteen
//! thousand TLB flushes; the second timed them apart and blamed the figure on
//! releasing the frames.
//!
//! The two phases below are what settled it. They run the *same* loop twice:
//! once with a 32 MiB arena mapped in the parent, and once after `munmap` has
//! given that arena back. The second phase's parent has a few pages of address
//! space instead of 8192, so anything that scales with the mapping shrinks by
//! three orders of magnitude - and if the numbers barely move, then neither
//! `fork` nor `wait4` is about the mapping at all, whatever the arithmetic looks
//! like.
//!
//! That is the shape of the answer here: `fork` is the mapping and costs
//! microseconds, `wait4` is *waiting* - and `wait4` used to cost two clock ticks
//! whether the process being reaped owned 32 MiB or nothing, which is what
//! pointed at the wake rather than at any work. It is a scheduler turn now,
//! three orders of magnitude under a tick; §10.2 has the mechanism and the
//! figures, and the `fork` row here is the control that did not move for it.
//!
//! # The doors
//!
//! `fork` is `libc::fork` - rustix binds none (the reason `t-fork` gives), and
//! arm64 reaches `clone` through it, which is the call the kernel serves there.
//! The child leaves through the raw `SYS_exit_group`: it must not flush the
//! stdout copy it inherited from the parent whose figures are still printing
//! (`t-exit-group`'s rule, load-bearing here - a flushing child would corrupt
//! the very lines this program reports). Reaping is `rustix::process::waitpid`,
//! the `wait4`-with-null-rusage the no-std version made by hand, and its
//! `WaitStatus` carries `WIFEXITED`/`WEXITSTATUS`. The clock is
//! `rustix::time::clock_gettime(ClockId::Monotonic)` - the buffer claim belongs
//! to `t-clock-gettime`, not here. The arena is `mmap_anonymous` + `munmap`
//! through rustix (the six-word shape is `t-mmap`'s subject, not this one's) -
//! with the one named wrapper edit `t-mmap-shared-fork` records: it ORs
//! `MAP_ANONYMOUS` into the caller's flags, harmless precisely because the flag
//! word is not this program's subject.
//!
//! Not run on the host beyond its own bounds: the figures print whatever the
//! machine answers (they always have - "both figures are the machine's answer,
//! not a bound"), and the one asserted claim, the reap-is-a-wake bound, holds on
//! Linux too (a modern kernel's wake is far under a jiffy). The counterfactual
//! this pins - the eight-millisecond wait of §10.2's halted-but-runnable boot
//! task - is this kernel's history, and only the sweep can repeat it.

use mpos_user_tests::Report;
use rustix::mm::{MapFlags, ProtFlags, mmap_anonymous, munmap};
use rustix::process::{Pid, WaitOptions, waitpid};
use rustix::time::{ClockId, clock_gettime};

/// The guest's page size, spelled out because nothing in the syscall ABI
/// reports it - and this test is about pages, so the count below is quoted in
/// them.
const PAGE: usize = 4096;

/// How much anonymous memory the first phase's parent holds. 32 MiB is 8192
/// pages: far more than the page-table hierarchy itself costs to walk once, and
/// small enough to stay invisible in a 2 GiB guest.
const ARENA_BYTES: usize = 32 * 1024 * 1024;

/// Cycles timed. Two hundred is enough to average out the tick and the
/// scheduler without making the test the slowest one in the suite.
const ROUNDS: usize = 200;

/// Cycles thrown away first, so the first-touch costs of the page allocator and
/// the address space are not in the average.
const WARMUP: usize = 20;

/// What every page of the arena is filled with, so the freshness check below has
/// something discriminating to look for. Not zero: a page that was never written
/// reads as zero, which is the shape a *broken* share would also take.
const FILL: u8 = 0xa5;

/// One clock tick, in nanoseconds - 250 Hz, the scheduler's own period.
const ONE_TICK_NS: i64 = 4_000_000;

/// The monotonic clock in nanoseconds.
///
/// No check on the return: `clock_gettime(CLOCK_MONOTONIC)` is pinned by
/// `t-clock-gettime`, and a failed read here would show up as a wildly wrong
/// number rather than as a silent zero.
fn now() -> i64 {
    let ts = clock_gettime(ClockId::Monotonic);
    ts.tv_sec * 1_000_000_000 + ts.tv_nsec
}

/// Leave through `exit_group`, the door that cannot flush the stdout copy this
/// child inherited.
fn leave(code: i32) -> ! {
    // SAFETY: a scalar argument; this call does not return.
    unsafe {
        libc::syscall(libc::SYS_exit_group, code as libc::c_long);
        libc::_exit(code)
    }
}

/// One `fork`, the child's immediate exit, and the `wait4` that reaps it, timed
/// in its two halves.
///
/// The child touches nothing: it exits the moment it is a child, so what the
/// first figure measures is the marking and nothing else, and the second is the
/// wait. `false` if any step failed, which the caller reports as a check.
fn one_cycle(fork_ns: &mut i64, reap_ns: &mut i64) -> bool {
    let start = now();
    // SAFETY: `fork` in a single-threaded program; the child only calls the
    // async-signal-safe doors above before leaving.
    let pid = unsafe { libc::fork() };
    let forked_at = now();
    if pid < 0 {
        return false;
    }
    if pid == 0 {
        // A child that exits without touching anything: the frames stay
        // shared, the parent keeps every page, and the only thing under
        // measurement is what `fork` did before returning.
        leave(0);
    }
    // The parent's branch of the fork: `pid` is positive here, so the child's
    // pid names a live task and `waitpid` on it is the reap under measurement.
    let want = Pid::from_raw(pid);
    let reaped = match waitpid(want, WaitOptions::empty()) {
        Ok(Some((got, status))) if Some(got) == want => status.exited(),
        _ => false,
    };
    let reaped_at = now();
    if !reaped {
        return false;
    }
    *fork_ns += forked_at - start;
    *reap_ns += reaped_at - forked_at;
    true
}

/// `ROUNDS` timed cycles after `WARMUP` thrown away, answering the per-cycle
/// averages in nanoseconds, or `None` if a cycle failed.
fn time_cycles() -> Option<(i64, i64)> {
    let (mut fork_ns, mut reap_ns) = (0i64, 0i64);
    let (mut done_f, mut done_r) = (0i64, 0i64);
    for round in 0..(WARMUP + ROUNDS) {
        let (mut f, mut r) = (0i64, 0i64);
        if !one_cycle(&mut f, &mut r) {
            return None;
        }
        if round >= WARMUP {
            fork_ns += f;
            reap_ns += r;
            done_f += 1;
            done_r += 1;
        }
    }
    if done_f != ROUNDS as i64 || done_r != ROUNDS as i64 {
        return None;
    }
    Some((fork_ns / done_f, reap_ns / done_r))
}

/// The one figure here that is *asserted* rather than reported, and the reason
/// the test exists.
///
/// A woken task must resume on a scheduler turn and not on the clock: `wait4`
/// returning to the parent is the child having exited and the picker having
/// chosen the parent, which is microseconds of work. Before the boot task
/// stopped halting the core while staying runnable (NOTES.md §10.2) both rows
/// read ~8 ms - *two* ticks on a task that had already been woken - and no
/// amount of work removed from the syscall moved it. The bound is a tick
/// because that is what the broken figure was quantized to: a wait that ends on
/// the clock cannot land between ticks, so it cannot drift down to meet this,
/// while the fixed figure sits tens of microseconds under it. Asserted where
/// the figures above it are not, and safely so: a 200-round average cannot be
/// moved by one starved round.
fn check_the_reap_is_a_wake(r: &mut Report, reap_ns: i64) {
    r.check(
        "the reap is a wake, not a clock tick",
        reap_ns < ONE_TICK_NS,
    );
}

fn main() {
    println!("t-forkcost");
    let mut r = Report::new();

    let arena = unsafe {
        mmap_anonymous(
            core::ptr::null_mut(),
            ARENA_BYTES,
            ProtFlags::READ | ProtFlags::WRITE,
            MapFlags::PRIVATE,
        )
    };
    r.check(&format!("the 32 MiB arena maps ({arena:?})"), arena.is_ok());
    let Ok(arena) = arena else { r.finish() };

    // Touch every page, so the fork's walk sees present PTEs rather than a
    // sparse mapping. This is also what the freshness check compares against.
    let slice = unsafe { core::slice::from_raw_parts_mut(arena as *mut u8, ARENA_BYTES) };
    for page in slice.chunks_mut(PAGE) {
        page[0] = FILL;
    }
    let pages = ARENA_BYTES / PAGE;

    // --- phase 1: a parent holding 32 MiB ---
    match time_cycles() {
        Some((fork_ns, reap_ns)) => {
            println!("  32 MiB parent : fork {fork_ns} ns, wait4 {reap_ns} ns");
            check_the_reap_is_a_wake(&mut r, reap_ns);
        }
        None => {
            r.check("the 32 MiB cycles complete", false);
            r.finish();
        }
    }

    // The marking is only cheap if it is still a *share*. Both halves of that
    // are checked from the parent: its own bytes are the ones it wrote, and a
    // child that reads the arena before writing it sees the same bytes - the
    // property a fork that skipped the marking would break in the other
    // direction, by letting the child's write reach the parent's page.
    let intact = slice.chunks(PAGE).filter(|page| page[0] == FILL).count();
    r.check(
        "the parent's arena is unchanged after the forks",
        intact == pages,
    );

    // SAFETY: as above - the child reads one byte and leaves through the raw
    // door, touching nothing that could flush.
    let witness = unsafe { libc::fork() };
    if witness == 0 {
        // Read the parent's page and exit with the answer: `_exit` reports the
        // low byte, which is `FILL` if the frame really was shared.
        let seen = unsafe { *((arena as *mut u8).add(PAGE) as *const u8) };
        leave(seen as i32);
    }
    let want = Pid::from_raw(witness);
    let status = waitpid(want, WaitOptions::empty()).ok().flatten();
    r.check(
        "the witness child is reaped",
        status
            .as_ref()
            .is_some_and(|(got, _)| Some(got) == want.as_ref()),
    );
    r.check(
        "a child reads the parent's bytes (the frame is shared, not zeroed)",
        status
            .as_ref()
            .is_some_and(|(_, st)| st.exit_status() == Some(FILL as i32)),
    );

    // --- phase 2: the same loop with the arena given back ---
    //
    // This is the phase that says what the first one meant. Everything about
    // the two phases is identical except how much address space the parent
    // owns, so a figure that scales with the mapping is about the mapping and
    // a figure that does not is about something else entirely.
    let given_back = unsafe { munmap(arena, ARENA_BYTES) };
    r.check("the arena is given back", given_back.is_ok());
    match time_cycles() {
        Some((fork_ns, reap_ns)) => {
            println!("  no arena      : fork {fork_ns} ns, wait4 {reap_ns} ns");
            check_the_reap_is_a_wake(&mut r, reap_ns);
        }
        None => {
            r.check("the no-arena cycles complete", false);
            r.finish();
        }
    }

    println!("  (both figures are the machine's answer, not a bound)");
    r.finish();
}
