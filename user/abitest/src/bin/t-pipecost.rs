//! t-pipecost - how fast bytes move through a pipe, and the proof they arrive.
//!
//! A pipe's buffer used to be a `VecDeque<u8>` moved one byte per operation:
//! a read was `pop_front` in a loop and a write was `extend(iter().copied())`,
//! so moving a pipeful cost one call per byte rather than one slice copy. The
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
//!   64 KiB chunk - two scheduler ticks, one per handover. The byte loop is a
//!   rounding error beside that, and any change to it is invisible here.
//! * **one process** - a `PIPE_BUF` written and read straight back, so the pipe
//!   never fills and no task ever parks. This isolates the data path: one
//!   syscall in, one out, and the copying between them.
//!
//! A single-process writer and reader in lockstep over 64 KiB would deadlock on
//! a full pipe, which is why the first phase needs the fork at all.

#![no_std]
#![no_main]

#[path = "../../lib/lin.rs"]
mod lin;

use core::panic::PanicInfo;

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

/// The monotonic clock in nanoseconds.
fn now() -> i64 {
    let mut ts = lin::TimeSpec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    lin::clock_gettime(lin::CLOCK_MONOTONIC, &mut ts);
    ts.tv_sec * 1_000_000_000 + ts.tv_nsec
}

#[unsafe(no_mangle)]
extern "C" fn mpos_user_main(_argc: usize, _argv: *const *const u8) -> ! {
    lin::puts(b"t-pipecost\n");

    let mut failed = false;
    let chunks = TOTAL / CHUNK;

    let mut fds = [0i32; 2];
    let rc = lin::pipe2(&mut fds, 0);
    lin::check_eq(&mut failed, b"pipe2 opens a pipe", rc, 0);
    if rc != 0 {
        lin::done(failed);
    }
    let (read_fd, write_fd) = (fds[0], fds[1]);

    // One chunk of scratch, inherited by the child through the fork. Both ends
    // growing it the same way also means the frames are shared until one of
    // them writes, which is the ordinary shape of a pipe test.
    let mut scratch = [FILL; CHUNK];

    let pid = lin::fork();
    if pid < 0 {
        lin::check_true(&mut failed, b"fork succeeds", false);
        lin::done(failed);
    }

    if pid == 0 {
        // The writer. `write(2)` on a pipe blocks until every byte is in, so
        // this loop is one call per chunk however full the pipe gets.
        lin::close(read_fd);
        for k in 0..chunks {
            // Stamp the index into the chunk so the reader can tell a
            // reordering or a duplication from a lost byte.
            scratch[0] = k as u8;
            let mut done = 0;
            while done < CHUNK {
                let n = lin::write(write_fd, &scratch[done..]);
                if n <= 0 {
                    lin::exit_group(1);
                }
                done += n as usize;
            }
        }
        lin::close(write_fd);
        lin::exit_group(0);
    }

    // Both of the parent's ends stay open: the second phase writes back into
    // the same pipe. The usual reason to close the writer is so a dead writer
    // reads as end-of-stream rather than as a reader parked on itself, and that
    // is not needed here - this phase reads exactly `TOTAL` bytes, and reports a
    // short read as a failure rather than waiting for an end that may not come.
    // The child does close its copy of the read end, which is what keeps the
    // pipe a pipe.

    let start = now();
    let mut taken = 0usize;
    let mut stamped_right = 0usize;
    let mut first_chunk_intact = true;
    let mut mismatch_at = 0usize;

    while taken < TOTAL {
        let want = core::cmp::min(CHUNK, TOTAL - taken);
        let mut have = 0usize;
        while have < want {
            let n = lin::read(read_fd, &mut scratch[have..want]);
            if n <= 0 {
                lin::check_eq(&mut failed, b"the reader is not cut short", n, -1);
                lin::done(failed);
            }
            have += n as usize;
        }

        let index = taken / CHUNK;
        if scratch[0] == index as u8 {
            stamped_right += 1;
        } else if mismatch_at == 0 {
            mismatch_at = index;
        }
        if index == 0 {
            // The first chunk compared in full, so a body of zeros that
            // happened to start with the right byte is not mistaken for the
            // writer's data. The writer stamped byte 0 with the index and left
            // the remaining `CHUNK - 1` bytes as it filled them, so they must
            // still be `FILL` - a check that also fails on a chunk that was
            // never written at all.
            for byte in &scratch[1..] {
                if *byte != FILL {
                    first_chunk_intact = false;
                    break;
                }
            }
        }
        taken += want;
    }
    let elapsed = now() - start;

    lin::check_eq(
        &mut failed,
        b"every chunk arrived",
        taken as isize,
        TOTAL as isize,
    );
    lin::check_eq(
        &mut failed,
        b"every chunk carries the index its writer stamped",
        stamped_right as isize,
        chunks as isize,
    );
    lin::check_true(
        &mut failed,
        b"the first chunk's bytes are the writer's, not zeros",
        first_chunk_intact,
    );
    if mismatch_at != 0 {
        lin::puts(b"  first mismatch at chunk ");
        lin::puts_isize(mismatch_at as isize);
        lin::puts(b"\n");
    }

    let mut status: i32 = 0;
    let rc = lin::wait4(pid as usize, &mut status, 0);
    lin::check_eq(&mut failed, b"the writer is reaped", rc, pid);
    lin::check_true(
        &mut failed,
        b"the writer exited normally",
        lin::wifexited(status) && lin::wexitstatus(status) == 0,
    );

    // Reported, not asserted: the guest runs under TCG as often as under KVM
    // and a bound that holds on one is a flaky failure on the other.
    report(b"two processes, 64 KiB chunks", TOTAL, elapsed);

    // The second phase, and the one that actually isolates the data path: one
    // process writes a `PIPE_BUF` and reads it straight back, so the pipe never
    // fills and no task ever parks. What the phase above measures is bracketed
    // by two blocking handovers per chunk, and a handover that ends on the
    // scheduler's tick rather than on the event costs about 4 ms - which is
    // 2047 ms of the 2047 ms reported above, and would hide any change to the
    // byte loop completely.
    //
    // 4096 rather than 64 KiB per turn for the same reason: the syscall's own
    // cost is a fixed ~1.5 us, so the larger the write, the smaller the share
    // of the total that the copying can possibly be.
    const UNIT: usize = 4096;
    let mut unit = [FILL; UNIT];

    let start = now();
    let mut moved = 0usize;
    let mut units = 0usize;
    while moved < TOTAL {
        unit[0] = (units & 0xff) as u8;
        let n = lin::write(write_fd, &unit);
        if n != UNIT as isize {
            lin::check_eq(
                &mut failed,
                b"a PIPE_BUF-sized write lands whole",
                n,
                UNIT as isize,
            );
            break;
        }
        let n = lin::read(read_fd, &mut unit);
        if n != UNIT as isize {
            lin::check_eq(&mut failed, b"the whole unit reads back", n, UNIT as isize);
            break;
        }
        if unit[0] != (units & 0xff) as u8 {
            lin::check_eq(
                &mut failed,
                b"the unit that comes back is the one that went in",
                unit[0] as isize,
                (units & 0xff) as isize,
            );
            break;
        }
        moved += UNIT;
        units += 1;
    }
    let elapsed = now() - start;

    lin::check_eq(
        &mut failed,
        b"every unit made the round trip",
        moved as isize,
        TOTAL as isize,
    );
    report(b"one process, 4096 B units   ", moved, elapsed);

    lin::done(failed)
}

/// `bytes` in `elapsed` nanoseconds, as a throughput line.
fn report(what: &[u8], bytes: usize, elapsed: i64) {
    lin::puts(b"  ");
    lin::puts(what);
    lin::puts(b": ");
    lin::puts_isize(bytes as isize / 1024);
    lin::puts(b" KiB in ");
    lin::puts_isize((elapsed / 1000) as isize);
    lin::puts(b" us, ");
    // Bytes per nanosecond is MiB per millisecond, so one scaling and one
    // division answers MiB/s - and doing it in one place is what stops the
    // double division that printed `0 MiB/s` for 8.
    lin::puts_isize((bytes as i64 * 1_000_000_000 / elapsed.max(1) / (1024 * 1024)) as isize);
    lin::puts(b" MiB/s\n");
}

#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    lin::die(b"t-pipecost: panicked\n")
}
