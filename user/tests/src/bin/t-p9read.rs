//! t-p9read - what one 9P round trip costs, measured rather than asserted.
//!
//! Every message this client sends allocates a fresh answer buffer of the
//! negotiated `msize` (128 KiB, `PROPOSED_MSIZE` in `src/vfs/p9.rs` - QEMU's own
//! recommendation, since its server warns about anything at or below 8192).
//! A read of the 39-byte probe file therefore zeroes 128 KiB before the wire
//! even answers, and the same is true of a `walk`, an `open`, a `clunk`: the
//! buffer is the size the protocol allows, not the size the answer needs.
//!
//! This program is that figure's instrument. It reads the host share's probe
//! file `READS` times through the boot's own mount - one `Tread` per read, a
//! `seek` before each - and prints the totals and the nanoseconds per round
//! trip. The numbers are the claim `docs/notes/10-performance.md` §10.10
//! carries; the only thing it *asserts* is that every measured read came back
//! carrying the line the host laid, so a run that desynchronised the mount
//! cannot quietly report a cheap number.
//!
//! `assets/configs/tests.toml` keeps it in the `slow` group: it measures where
//! the other programs check, and its nanoseconds depend on how busy the host is.
//! `cargo xtask test --groups slow` is the run that takes it.

use mpos_user_tests::Report;
use rustix::fs::{CWD, Mode, OFlags, SeekFrom, openat, seek};
use rustix::io::read;
use rustix::time::{ClockId, clock_gettime};

/// The file `xtask` lays into every exported share (`xtask::utils::lay_p9_share`,
/// the same fixture `t-9p` pins).
const PROBE: &str = "/mnt/share/mpos-9p-probe";
/// The line it carries - `xtask::utils::P9_PROBE`.
const LINE: &[u8] = b"mpos reads from the host share over 9p\n";
/// Rounds trips measured. 20 000 of them is a fraction of a second of wire and
/// long enough that the per-request figure is not the start-up noise.
const READS: usize = 20_000;

fn now_ns() -> u128 {
    let ts = clock_gettime(ClockId::Monotonic);
    ts.tv_sec as u128 * 1_000_000_000 + ts.tv_nsec as u128
}

fn main() {
    println!("t-p9read");
    let mut r = Report::new();

    // The share is the sweep's own QEMU wiring, so a host run would measure the
    // host's absence: on the host `/mnt/share` is not a 9p mount this kernel
    // serves. Skip with the verdict printed; the payload boot answers here.
    if mpos_user_tests::host_run() {
        println!("skip: the 9p share exists only in the test boot");
        r.finish()
    }

    let fd = openat(CWD, PROBE, OFlags::RDONLY, Mode::empty());
    r.check("the boot's share serves its probe file", fd.is_ok());
    let Ok(fd) = fd else { r.finish() };
    let mut buf = [0u8; 64];

    // One warm read outside the clock: the first read after the mount pays for
    // whatever the server keeps per-open, and it is not the round trip's own
    // cost. What it asserts is the fixture, so the loop below measures the wire
    // rather than a mount that answers the wrong bytes quickly.
    let warm = read(&fd, &mut buf);
    r.check(
        "a first read answers the line the host laid",
        warm == Ok(LINE.len()) && &buf[..LINE.len()] == LINE,
    );

    let started = now_ns();
    let mut wrong = 0usize;
    for _ in 0..READS {
        // Back to the start of the file: the client sends nothing for an
        // offset, so each iteration is exactly one `Tread`.
        let _ = seek(&fd, SeekFrom::Start(0));
        match read(&fd, &mut buf) {
            Ok(n) if n == LINE.len() && &buf[..n] == LINE => {}
            _ => wrong += 1,
        }
    }
    let total_ns = now_ns() - started;
    let per_ns = total_ns / READS as u128;
    println!(
        "P9READ reads={READS} total_ms={} ns_per_read={per_ns}",
        total_ns / 1_000_000
    );
    println!(
        "p9read: {} MiB of answer buffers zeroed for {} B of file",
        READS as u64 * 131_072 / (1024 * 1024),
        READS as u64 * LINE.len() as u64
    );

    r.check("every measured read answered the recorded line", wrong == 0);
    r.finish()
}
