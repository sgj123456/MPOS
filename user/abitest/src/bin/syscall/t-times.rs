//! t-times - `times()`/`getrusage()` report real per-process CPU time.
//!
//! The contract pinned here is that CPU accounting is measured, not invented.
//! Both calls used to hand back zeros (a busy process reported `0` user time),
//! which is the silent-success `PLAN.md` §4 forbids: a program that times
//! itself - `time`, a shell's job accounting, a benchmark dividing CPU by
//! elapsed for a load factor - was told it spent no CPU doing obvious work.
//! The timer interrupt now charges the running process, split user/system by
//! the ring it interrupted, and these calls report it.
//!
//! The decisive pin is the SPLIT and monotonicity, not the absolute number (the
//! guest's clock is its own and the tick rate need not match a caller's
//! expectation):
//!
//! * a tight *user-mode* spin raises `tms_utime`/`ru_utime` and leaves `stime`
//!   far behind it - the split is what proves the ring attribution is real
//!   (charge everything to the kernel and this fails);
//! * values never decrease across successive reads - no double-counting;
//! * `times(NULL)` still answers the elapsed clock;
//! * an unknown `getrusage` who is `EINVAL`, and `RUSAGE_CHILDREN` answers
//!   success with a zeroed buffer (its child aggregation is the documented
//!   `PLAN.md` §12.2 B6 gap, so it is zero, not an error).
//!
//! The work is applied in small chunks and the reading re-checked after each,
//! so the test stops at the first detected tick: it loads the single-core
//! machine only as long as it takes to prove the point, rather than burning a
//! fixed wall-clock time that would both flake on a fast core and starve the
//! timing-sensitive network tests sharing the sweep.

#![no_std]
#![no_main]

#[path = "../../../lib/lin.rs"]
mod lin;

use core::panic::PanicInfo;

const RUSAGE_SELF: usize = 0;
const RUSAGE_CHILDREN: usize = -1isize as usize;

/// How many chunks to allow before giving up - a generous ceiling that still
/// keeps the worst-case load bounded. On the guest a single chunk usually
/// crosses a tick, so this rarely iterates.
const MAX_CHUNKS: u32 = 500;

fn read_i64(buf: &[u8], off: usize) -> i64 {
    let w = [
        buf[off],
        buf[off + 1],
        buf[off + 2],
        buf[off + 3],
        buf[off + 4],
        buf[off + 5],
        buf[off + 6],
        buf[off + 7],
    ];
    i64::from_ne_bytes(w)
}

/// One pure user-mode chunk: arithmetic only, no syscall, so the ticks it spans
/// are charged to `utime`. `black_box` keeps it from being optimised away.
fn burn_chunk() {
    let mut acc: u64 = 1;
    for i in 0..200_000u64 {
        acc = acc.wrapping_mul(31).wrapping_add(i);
        core::hint::black_box(&acc);
    }
    core::hint::black_box(acc);
}

/// Burn chunks until the user tick reported by `read` climbs above `base`.
/// Returns the latest reading; the caller asserts it grew. This is the
/// low-footprint way to guarantee at least one tick of user time regardless of
/// core speed, and to stop the moment one lands.
fn burn_until_grew<T: Copy + Ord>(base: T, mut read: impl FnMut() -> T) -> T {
    let mut last = base;
    for _ in 0..MAX_CHUNKS {
        burn_chunk();
        last = read();
        if last > base {
            break;
        }
    }
    last
}

#[unsafe(no_mangle)]
extern "C" fn mpos_user_main(_argc: usize, _argv: *const *const u8) -> ! {
    lin::puts(b"t-times\n");
    let mut failed = false;

    // --- times(): the split is real ---
    let mut t0 = [0u8; 32];
    let elapsed0 = lin::times(Some(&mut t0));
    let utime0 = read_i64(&t0, 0);
    let stime0 = read_i64(&t0, 8);

    let mut t1 = [0u8; 32];
    let mut elapsed1 = elapsed0;
    let utime1 = burn_until_grew(utime0, || {
        elapsed1 = lin::times(Some(&mut t1));
        read_i64(&t1, 0)
    });
    // stime after the same burn; for a pure user spin utime must outgrow it.
    let stime1 = read_i64(&t1, 8);

    lin::check_true(
        &mut failed,
        b"times() returns a non-negative elapsed clock",
        elapsed1 >= 0,
    );
    lin::check_true(
        &mut failed,
        b"the elapsed clock advances across a spin",
        elapsed1 > elapsed0,
    );
    lin::check_true(
        &mut failed,
        b"a user-mode spin raises tms_utime (the ring split is real)",
        utime1 > utime0,
    );
    lin::check_true(
        &mut failed,
        b"a user spin charges utime, not stime",
        (utime1 - utime0) > (stime1 - stime0),
    );
    lin::check_true(
        &mut failed,
        b"tms_utime never decreases",
        utime1 >= utime0 && stime1 >= stime0,
    );

    // A null buffer still answers the elapsed clock.
    let null_elapsed = lin::times(None);
    lin::check_true(
        &mut failed,
        b"times(NULL) answers the elapsed clock",
        null_elapsed >= elapsed0,
    );

    // --- getrusage(RUSAGE_SELF): the same CPU as a real timeval ---
    let mut r0 = [0u8; 144];
    lin::check_eq(
        &mut failed,
        b"getrusage(RUSAGE_SELF) succeeds",
        lin::getrusage(RUSAGE_SELF, &mut r0),
        0,
    );
    let ru0 = read_i64(&r0, 0) * 1_000_000 + read_i64(&r0, 8);

    let mut r1 = [0u8; 144];
    let ru1 = burn_until_grew(ru0, || {
        let _ = lin::getrusage(RUSAGE_SELF, &mut r1);
        read_i64(&r1, 0) * 1_000_000 + read_i64(&r1, 8)
    });
    lin::check_true(
        &mut failed,
        b"a user spin raises getrusage ru_utime (as a real timeval)",
        ru1 > ru0,
    );
    // tv_usec is microseconds, not a raw tick count masquerading as one.
    lin::check_true(
        &mut failed,
        b"ru_utime tv_usec is in [0, 1_000_000)",
        read_i64(&r1, 8) >= 0 && read_i64(&r1, 8) < 1_000_000,
    );

    // An unknown who is EINVAL.
    lin::check_eq(
        &mut failed,
        b"getrusage with an unknown who is EINVAL",
        lin::getrusage(7, &mut r1),
        lin::EINVAL,
    );

    // --- reaped-child aggregation: a child's CPU folds into the parent ---
    //
    // A fresh process has waited no children, so its child CPU is zero. Fork a
    // child that burns user CPU and exits; the reap that frees it folds its CPU
    // into this process's child totals, which both `getrusage(RUSAGE_CHILDREN)`
    // and `times()`'s `tms_cutime` then report - Linux accumulates on `wait`.
    let mut rc0 = [0u8; 144];
    lin::check_eq(
        &mut failed,
        b"getrusage(RUSAGE_CHILDREN) succeeds with no children waited yet",
        lin::getrusage(RUSAGE_CHILDREN, &mut rc0),
        0,
    );
    lin::check_true(
        &mut failed,
        b"and the untouched child buffer is zeroed, not uninitialised",
        rc0.iter().all(|&b| b == 0),
    );
    let cutime0 = read_i64(&rc0, 0) * 1_000_000 + read_i64(&rc0, 8);

    let pid = lin::fork();
    if pid == 0 {
        // The child: burn user CPU (each chunk charges the child's own fresh
        // `Mm`), then leave. The parent's child totals must then grow.
        let _ = burn_until_grew(0i64, || {
            let mut b = [0u8; 32];
            lin::times(Some(&mut b));
            read_i64(&b, 0)
        });
        lin::exit_group(0);
    }
    lin::check_true(
        &mut failed,
        b"fork of a CPU-burning child succeeded",
        pid > 0,
    );
    if pid > 0 {
        let mut status = 0i32;
        let w = lin::waitpid(pid as usize, &mut status, 0);
        lin::check_true(
            &mut failed,
            b"the parent waited the child",
            w == pid as isize,
        );
        // The fold lands on the reap the scheduler runs after the wait, so a
        // short bounded re-read (each pass burns a chunk, letting a tick fire)
        // observes it without depending on a fixed wait.
        let mut cutime1 = cutime0;
        for _ in 0..60 {
            burn_chunk();
            let mut b = [0u8; 144];
            if lin::getrusage(RUSAGE_CHILDREN, &mut b) == 0 {
                cutime1 = read_i64(&b, 0) * 1_000_000 + read_i64(&b, 8);
                if cutime1 > cutime0 {
                    break;
                }
            }
        }
        lin::check_true(
            &mut failed,
            b"a waited child's CPU folds into getrusage(RUSAGE_CHILDREN)",
            cutime1 > cutime0,
        );
        let mut tb = [0u8; 32];
        lin::times(Some(&mut tb));
        lin::check_true(
            &mut failed,
            b"and into times() tms_cutime",
            read_i64(&tb, 16) > 0,
        );
    }

    lin::done(failed)
}

#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    lin::die(b"t-times: panicked\n")
}
