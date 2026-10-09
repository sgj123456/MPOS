//! t-pipecost - how fast bytes move through a pipe, and the proof they arrive.
//! On both machines.
//!
//! A pipe's buffer used to be a `VecDeque<u8>` moved one byte per operation: a
//! read was `pop_front` in a loop and a write was `extend(iter().copied())`, so
//! moving a pipeful cost one call per byte rather than one slice copy. The
//! numbers are the point here and the correctness side is the part that would
//! catch a ring buffer that got its wrap wrong, so both are checked: the total
//! throughput is reported, and every chunk's first byte is checked against the
//! index the writer stamped into it - which catches a buffer that reordered or
//! duplicated chunks, not just one that lost them - with one whole chunk
//! compared byte for byte.
//!
//! Two phases, because they measure two different things and the first one
//! turned out to be about the scheduler rather than about the pipe:
//!
//! * **two processes** - one writes 64 KiB at a time through a pipe that fills,
//!   the other drains it. This is what a shell pipeline does, and it is the case
//!   where the pipe blocks. It measured 2047 ms for 16 MiB, which is 8 ms per
//!   64 KiB chunk; the reading taken at the time was "two ticks, one per
//!   handover", and the count was right while the unit was wrong - a chunk is
//!   two blocking waits, the writer's and the reader's, so it was one tick per
//!   *wake*. That tick is gone (§10.2: the boot task's `park()`), and the phase
//!   now runs at the same speed as the one below it, which is the point of
//!   keeping both.
//! * **one process** - a `PIPE_BUF` written and read straight back, so the pipe
//!   never fills and no task ever parks. This isolates the data path: one
//!   syscall in, one out, and the copying between them.
//!
//! A single-process writer and reader in lockstep over 64 KiB would deadlock on
//! a full pipe, which is why the first phase needs the fork at all.
//!
//! # The doors
//!
//! The pipe is `rustix::pipe::pipe`; the reads and writes are `rustix::io`,
//! whose backend has no `EINTR` retry (the §15.11 note that makes a blocking
//! pipe read behave like the raw call). The child is `libc::fork` (rustix binds
//! none) and leaves through the raw `SYS_exit_group` - a flushing std child
//! would corrupt the parent's own report lines (`t-exit-group`'s rule); the
//! scratch buffer lives in a `static` so the child reaches it across the fork
//! without Rust-level aliasing. Reaping is `rustix::process::waitpid` with
//! `WaitStatus`. The clock is `rustix::time::clock_gettime(ClockId::Monotonic)`.
//!
//! Not run on the host: the figures print whatever machine answers them, but
//! the *bound* this file carries - the two-process phase running at the
//! one-process phase's speed, §10.2's wake made measurable - is a claim about
//! this kernel's scheduler, and a hosted run would compare Linux's pipe against
//! nothing. The instrument is the sweep pair.

use mpos_user_tests::Report;
use rustix::io::{read, write};
use rustix::pipe::{PipeFlags, pipe_with};
use rustix::process::{Pid, WaitOptions, waitpid};
use rustix::time::{ClockId, clock_gettime};

/// One pipe's whole capacity, and so the largest chunk that can be handed over
/// in one go. Nothing in the ABI reports the capacity - it is the kernel's own
/// `CAPACITY` - and a writer that asks for more than it simply blocks until the
/// reader has taken some, which still works but measures the scheduler too.
const CHUNK: usize = 64 * 1024;

/// How much is pushed through. 16 MiB is 256 chunks, so every chunk carries a
/// distinct index byte and the check below is discriminating; and it is long
/// enough that the per-byte cost of the old path is unmistakable rather than
/// lost in the fork.
const TOTAL: usize = 16 * 1024 * 1024;

/// The byte every buffer starts filled with. Arbitrary but not zero, so a chunk
/// that was never written reads as a mismatch rather than as a plausible value.
const FILL: u8 = 0x5a;

/// The chunk both processes work through, in a `static` so the forked child
/// reaches the parent's copy (COW-shared until one writes) exactly as the
/// no-std version's stack frame did.
static mut SCRATCH: [u8; CHUNK] = [FILL; CHUNK];

/// The monotonic clock in nanoseconds.
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

/// `bytes` in `elapsed` nanoseconds, as a throughput line.
fn report(what: &str, bytes: usize, elapsed: i64) {
    // Bytes per nanosecond is MiB per millisecond, so one scaling and one
    // division answers MiB/s - and doing it in one place is what stops the
    // double division that printed `0 MiB/s` for 8.
    let mib_s = bytes as i64 * 1_000_000_000 / elapsed.max(1) / (1024 * 1024);
    println!(
        "  {what}: {} KiB in {} us, {mib_s} MiB/s",
        bytes / 1024,
        elapsed / 1000
    );
}

fn main() {
    println!("t-pipecost");
    let mut r = Report::new();
    let chunks = TOTAL / CHUNK;

    // `O_CLOEXEC` is not decoration here: the writer child must hold *only* its
    // own two ends. A third open descriptor into the read side would keep the
    // pipe non-empty for the parent's phase-2 reads and change what is timed.
    let (read_fd, write_fd) = match pipe_with(PipeFlags::CLOEXEC) {
        Ok(fds) => fds,
        Err(e) => {
            r.check(&format!("pipe2 opens a pipe ({e:?})"), false);
            r.finish();
        }
    };
    r.check("pipe opens a pipe", true);

    let pid = unsafe { libc::fork() };
    if pid < 0 {
        r.check("fork succeeds", false);
        r.finish();
    }

    if pid == 0 {
        // The writer. `write(2)` on a pipe blocks until every byte is in, so
        // this loop is one call per chunk however full the pipe gets.
        drop(read_fd);
        for k in 0..chunks {
            // Stamp the index into the chunk so the reader can tell a
            // reordering or a duplication from a lost byte.
            let scratch = unsafe { &mut *core::ptr::addr_of_mut!(SCRATCH) };
            scratch[0] = k as u8;
            let mut done = 0;
            while done < CHUNK {
                match write(&write_fd, &scratch[done..]) {
                    Ok(n) => done += n,
                    Err(_) => leave(1),
                }
            }
        }
        leave(0);
    }

    // Both of the parent's ends stay open: the second phase writes back into
    // the same pipe. The usual reason to close the writer is so a dead writer
    // reads as end-of-stream rather than as a reader parked on itself, and that
    // is not needed here - this phase reads exactly `TOTAL` bytes, and reports
    // a short read as a failure rather than waiting for an end that may not
    // come. The child does close its copy of the read end, which is what keeps
    // the pipe a pipe.
    let start = now();
    let mut taken = 0usize;
    let mut stamped_right = 0usize;
    let mut first_chunk_intact = true;
    let mut mismatch_at = None;

    while taken < TOTAL {
        let want = core::cmp::min(CHUNK, TOTAL - taken);
        let mut have = 0usize;
        while have < want {
            let scratch = unsafe { &mut *core::ptr::addr_of_mut!(SCRATCH) };
            match read(&read_fd, &mut scratch[have..want]) {
                Ok(n) => have += n,
                Err(e) => {
                    r.check(&format!("the reader is not cut short ({e:?})"), false);
                    r.finish();
                }
            }
        }

        let scratch = unsafe { &*core::ptr::addr_of!(SCRATCH) };
        let index = taken / CHUNK;
        if scratch[0] == index as u8 {
            stamped_right += 1;
        } else if mismatch_at.is_none() {
            mismatch_at = Some(index);
        }
        if index == 0 {
            // The first chunk compared in full, so a body of zeros that
            // happened to start with the right byte is not mistaken for the
            // writer's data. The writer stamped byte 0 with the index and left
            // the remaining `CHUNK - 1` bytes as it filled them, so they must
            // still be `FILL` - a check that also fails on a chunk that was
            // never written at all.
            first_chunk_intact = scratch[1..].iter().all(|&b| b == FILL);
        }
        taken += want;
    }
    let elapsed = now() - start;

    r.check("every chunk arrived", taken == TOTAL);
    r.check(
        "every chunk carries the index its writer stamped",
        stamped_right == chunks,
    );
    r.check(
        "the first chunk's bytes are the writer's, not zeros",
        first_chunk_intact,
    );
    if let Some(at) = mismatch_at {
        println!("  first mismatch at chunk {at}");
    }

    let want = Pid::from_raw(pid);
    let reaped = waitpid(want, WaitOptions::empty()).ok().flatten();
    r.check(
        "the writer is reaped",
        reaped.as_ref().is_some_and(|(got, _)| Some(*got) == want),
    );
    r.check(
        "the writer exited normally",
        reaped
            .as_ref()
            .is_some_and(|(_, st)| st.exited() && st.exit_status() == Some(0)),
    );

    // Reported, not asserted: the guest runs under TCG as often as under KVM
    // and a bound that holds on one is a flaky failure on the other.
    report("two processes, 64 KiB chunks", TOTAL, elapsed);

    // The second phase, and the one that isolates the data path alone: one
    // process writes a `PIPE_BUF` and reads it straight back, so the pipe never
    // fills and no task ever parks. The phase above is bracketed by two blocking
    // waits per chunk, and while a wake cost a clock tick those two dominated
    // it - the 2047 ms above was 2047 ms of waiting, and any change to the byte
    // loop was invisible behind them. Now that a wake is a scheduler turn
    // (§10.2) the two rows report the same figure, so this one is the control:
    // the data path is the only thing left in either.
    //
    // 4096 rather than 64 KiB per turn for the same reason: the syscall's own
    // cost is a fixed ~1.5 us, so the larger the write, the smaller the share
    // of the total that the copying can possibly be.
    const UNIT: usize = 4096;
    let mut unit = [FILL; UNIT];

    let start = now();
    let mut moved = 0usize;
    let mut units = 0usize;
    let mut broke = false;
    while moved < TOTAL && !broke {
        unit[0] = (units & 0xff) as u8;
        match write(&write_fd, &unit) {
            Ok(n) if n == UNIT => {}
            n => {
                r.check(
                    &format!("a PIPE_BUF-sized write lands whole (got {n:?})"),
                    false,
                );
                broke = true;
            }
        }
        if broke {
            break;
        }
        match read(&read_fd, &mut unit) {
            Ok(n) if n == UNIT => {}
            n => {
                r.check(&format!("the whole unit reads back (got {n:?})"), false);
                broke = true;
            }
        }
        if broke {
            break;
        }
        if unit[0] != (units & 0xff) as u8 {
            r.check("the unit that comes back is the one that went in", false);
            broke = true;
        }
        moved += UNIT;
        units += 1;
        // One full-chunk comparison, at the same place the no-std version made
        // it: the first unit whose body should still be all FILL apart from the
        // stamped byte. Reading it back proves the ring handed this exact bytes
        // across, not merely the right count.
        if units == 1 {
            r.check(
                "the first unit's bytes are the writer's, not zeros",
                unit[1..].iter().all(|&b| b == FILL),
            );
        }
    }
    let elapsed = now() - start;

    r.check("every unit made the round trip", moved == TOTAL);
    report("one process, 4096 B units   ", moved, elapsed);

    r.finish();
}
