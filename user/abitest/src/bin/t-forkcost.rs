//! t-forkcost - what a `fork` costs, what the `wait4` after it costs, and what
//! each of those has to do with the size of the process.
//!
//! A `fork` has to hand the child a copy of every writable page the parent owns,
//! and it does that without copying anything: the frames are shared and marked
//! copy-on-write. That marking used to be followed by a full CR3 write per page,
//! which is where the first version of this test pointed - and it was wrong
//! about where the time goes, twice over. The first instrument timed `fork` and
//! `wait4` together and reported no difference for a change that removed
//! sixteen thousand TLB flushes; the second timed them apart and blamed the
//! figure on releasing the frames.
//!
//! The two phases below are what settled it. They run the *same* loop twice: once
//! with a 32 MiB arena mapped in the parent, and once after `munmap` has given
//! that arena back. The second phase's parent has a few pages of address space
//! instead of 8192, so anything that scales with the mapping shrinks by three
//! orders of magnitude - and if the numbers barely move, then neither `fork` nor
//! `wait4` is about the mapping at all, whatever the arithmetic looks like.
//!
//! That is the shape of the answer here: `fork` is the mapping and costs
//! microseconds, `wait4` is *waiting* and costs milliseconds - two clock ticks,
//! whether the process being reaped owned 32 MiB or nothing.

#![no_std]
#![no_main]

#[path = "../../lib/lin.rs"]
mod lin;

use core::panic::PanicInfo;

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

/// The monotonic clock in nanoseconds.
///
/// No check on the return: `clock_gettime(CLOCK_MONOTONIC)` is pinned by
/// `t-clock-gettime`, and a failed read here would show up as a wildly wrong
/// number rather than as a silent zero.
fn now() -> i64 {
    let mut ts = lin::TimeSpec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    lin::clock_gettime(lin::CLOCK_MONOTONIC, &mut ts);
    ts.tv_sec * 1_000_000_000 + ts.tv_nsec
}

/// One `fork`, the child's immediate exit, and the `wait4` that reaps it, timed
/// in its two halves.
///
/// The child touches nothing: it exits the moment it is a child, so what the
/// first figure measures is the marking and nothing else, and the second is the
/// wait. `false` if any step failed, which the caller reports as a check.
fn one_cycle(fork_ns: &mut i64, reap_ns: &mut i64) -> bool {
    let mut status: i32 = 0;
    let start = now();
    let pid = lin::fork();
    let forked_at = now();
    if pid < 0 {
        return false;
    }
    if pid == 0 {
        // A child that exits without touching anything: the frames stay
        // shared, the parent keeps every page, and the only thing under
        // measurement is what `fork` did before returning.
        lin::exit_group(0);
    }
    let rc = lin::wait4(pid as usize, &mut status, 0);
    let reaped_at = now();
    if rc != pid || !lin::wifexited(status) || lin::wexitstatus(status) != 0 {
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

/// `bytes` in `elapsed` nanoseconds, as a throughput line.
fn report(what: &[u8], fork_ns: i64, reap_ns: i64) {
    lin::puts(b"  ");
    lin::puts(what);
    lin::puts(b": fork ");
    lin::puts_isize(fork_ns as isize);
    lin::puts(b" ns, wait4 ");
    lin::puts_isize(reap_ns as isize);
    lin::puts(b" ns\n");
}

#[unsafe(no_mangle)]
extern "C" fn mpos_user_main(_argc: usize, _argv: *const *const u8) -> ! {
    lin::puts(b"t-forkcost\n");

    let mut failed = false;

    let arena = lin::mmap(
        0,
        ARENA_BYTES,
        lin::PROT_READ | lin::PROT_WRITE,
        lin::MAP_PRIVATE | lin::MAP_ANONYMOUS,
        -1,
        0,
    );
    if arena == lin::MAP_FAILED {
        lin::puts(b"  mmap of the arena failed\n");
        lin::done(true);
    }
    let arena = arena as usize;

    // Touch every page, so the fork's walk sees present PTEs rather than a
    // sparse mapping. This is also what the freshness check compares against.
    for offset in (0..ARENA_BYTES).step_by(PAGE) {
        unsafe { ((arena + offset) as *mut u8).write_volatile(FILL) };
    }
    let pages = ARENA_BYTES / PAGE;

    // --- phase 1: a parent holding 32 MiB ---
    match time_cycles() {
        Some((fork_ns, reap_ns)) => {
            report(b"32 MiB parent ", fork_ns, reap_ns);
        }
        None => {
            lin::check_true(&mut failed, b"the 32 MiB cycles complete", false);
            lin::done(failed);
        }
    }

    // The marking is only cheap if it is still a *share*. Both halves of that
    // are checked from the parent: its own bytes are the ones it wrote, and a
    // child that reads the arena before writing it sees the same bytes - the
    // property a fork that skipped the marking would break in the other
    // direction, by letting the child's write reach the parent's page.
    let mut intact = 0usize;
    for offset in (0..ARENA_BYTES).step_by(PAGE) {
        if unsafe { ((arena + offset) as *const u8).read_volatile() } == FILL {
            intact += 1;
        }
    }
    lin::check_eq(
        &mut failed,
        b"the parent's arena is unchanged after the forks",
        intact as isize,
        pages as isize,
    );

    let witness = lin::fork();
    if witness == 0 {
        // Read the parent's page and exit with the answer: `_exit` reports the
        // low byte, which is `FILL` if the frame really was shared.
        let seen = unsafe { ((arena + PAGE) as *const u8).read_volatile() };
        lin::exit_group(seen as i32);
    }
    let mut status: i32 = 0;
    let rc = lin::wait4(witness as usize, &mut status, 0);
    lin::check_eq(&mut failed, b"the witness child is reaped", rc, witness);
    lin::check_eq(
        &mut failed,
        b"a child reads the parent's bytes (the frame is shared, not zeroed)",
        lin::wexitstatus(status) as isize,
        FILL as isize,
    );

    // --- phase 2: the same loop with the arena given back ---
    //
    // This is the phase that says what the first one meant. Everything about
    // the two phases is identical except how much address space the parent
    // owns, so a figure that scales with the mapping is about the mapping and
    // a figure that does not is about something else entirely.
    lin::check_eq(
        &mut failed,
        b"the arena is given back",
        lin::munmap(arena, ARENA_BYTES),
        0,
    );
    match time_cycles() {
        Some((fork_ns, reap_ns)) => {
            report(b"no arena    ", fork_ns, reap_ns);
        }
        None => {
            lin::check_true(&mut failed, b"the no-arena cycles complete", false);
            lin::done(failed);
        }
    }

    lin::puts(b"  (both figures are the machine's answer, not a bound)\n");
    lin::done(failed)
}

#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    lin::die(b"t-forkcost: panicked\n")
}
