//! t-uptime-shape - `/proc/uptime` is two zero-padded columns, and its second
//! column is the idle counter this kernel actually keeps.
//!
//! Linux renders each column as `%lu.%02lu`: a fraction of four centiseconds is
//! `04`, never `4`. This kernel rendered
//! `format!("{}.{} 0.0\n", secs, centis)`, where
//! `centis = (ticks % 250) * 100 / 250` walks every value `0..=99` (each for
//! two or three ticks). Nine of those hundred values are one digit, so about
//! 9% of reads came out as `S.1` .. `S.9`. A caller parsing that with `atof` -
//! `t-starttime`'s `uptime_ms`, and any C program that reads the first field -
//! hears `S + centis/10` instead of Linux's `S + centis/100`: an error of
//! `90 * centis` milliseconds on that one read. §6.126 has the arithmetic:
//! `t-starttime`'s `150 ticks inside Some((1860, 1860)) ms` is exactly
//! `1500 ms + 360 ms` (`centis = 4`, `90 * 4 = 360`) on both short endpoints of
//! its bracket; §6.108's older `150 ticks against 1320 ms` is the same bug with
//! `centis = 2` (`180 ms`). Both had been diagnosed as "TCG contention" or
//! "fork latency" - the error was in the file all along.
//!
//! The second column was a literal `0.0`, which claims a number this kernel
//! does keep: `sched::cpustat` charges idle ticks from the same tick interrupt,
//! and `/proc/stat` already prints them. A zero there is `AGENTS.md` §3's
//! "returning success for something not implemented" in a file's clothing.
//!
//! Measured on the host before this kernel was touched
//! (`build/scratch/uptime/{shape,probe}.c`, 7.2.14): 1200 consecutive reads
//! never produced a one-digit fraction, the idle column grows across a one
//! second sleep (304616.14 -> 304630.73), and it can exceed uptime because
//! Linux sums idle across CPUs.
//!
//! The shape is the counterfactual here, not a timing window: the idle column's
//! literal `0.0` is one digit on *every* read, and the first column emits a
//! one-digit fraction on nine of every hundred centisecond values. A loop that
//! spans a whole centisecond cycle therefore cannot avoid either tell - which
//! is why this pin is deterministically red on the old kernel while
//! `t-starttime`, reading the file only a handful of times, was a 9% flake that
//! two phases mistook for scheduler load.

use mpos_user_tests::Report;
use std::time::{Duration, Instant};

/// A reading of the two columns, kept as the raw words so the fraction's
/// *width* can be asserted as well as its value.
fn columns() -> Option<(String, String)> {
    let text = std::fs::read_to_string("/proc/uptime").ok()?;
    let mut words = text.split_whitespace();
    let up = words.next()?.to_owned();
    let idle = words.next()?.to_owned();
    if words.next().is_some() {
        return None;
    }
    Some((up, idle))
}

fn fraction_digits(word: &str) -> Option<usize> {
    Some(word.split_once('.')?.1.len())
}

/// The `atof` a C reader uses: `S.4` and `S.04` are deliberately *not* the
/// same number here, which is the bug this pins.
fn millis(word: &str) -> Option<u64> {
    Some((word.parse::<f64>().ok()? * 1000.0) as u64)
}

fn main() {
    let mut r = Report::new();

    let first = columns();
    r.check("the uptime file has exactly two columns", first.is_some());

    // Shape. The loop must span a *whole centisecond cycle*, not merely many
    // reads: a tight 3000-read burst can finish inside one tick on an emulated
    // machine and never see a `centis < 10` value, which is exactly how the
    // claim passed on the broken kernel during the first measurement. Batching
    // the reads across 1.3 s of sleeps covers all 100 centisecond values from
    // any starting point, so the old `S.1`..`S.9` rendering cannot hide.
    const BATCHES: usize = 130;
    const PER_BATCH: usize = 20;
    const READS: usize = BATCHES * PER_BATCH;
    let mut seen = 0usize;
    let mut up_width_ok = true;
    let mut idle_width_ok = true;
    let mut backwards = false;
    let mut last_up = 0u64;
    for _ in 0..BATCHES {
        for _ in 0..PER_BATCH {
            let Some((up, idle)) = columns() else {
                continue;
            };
            seen += 1;
            up_width_ok &= fraction_digits(&up) == Some(2);
            idle_width_ok &= fraction_digits(&idle) == Some(2);
            let Some(now) = millis(&up) else {
                continue;
            };
            if now + 1 < last_up {
                backwards = true;
            }
            last_up = now;
        }
        std::thread::sleep(Duration::from_millis(10));
    }

    r.check(
        &format!("the shape loop actually read the file ({seen} of {READS} reads)"),
        seen > READS / 2,
    );
    r.check(
        &format!("every first-column fraction has exactly two digits ({seen} reads)"),
        up_width_ok,
    );
    r.check(
        &format!("every idle-column fraction has exactly two digits ({seen} reads)"),
        idle_width_ok,
    );
    // A `0.00` idle reading is legitimate early in a boot: what is *not*
    // legitimate is the old literal `0.0`, whose one-digit fraction the
    // `idle_width_ok` claim above already rejects. The growth claim below
    // pins that the column is a counter rather than a constant.
    r.check(
        &format!("the first column never goes backwards ({seen} reads)"),
        !backwards,
    );

    // Movement: the second column is a counter, not a constant. Both host
    // Linux and this kernel charge idle ticks while the machine has nothing to
    // run, so the grown value must be strictly larger after a sleep.
    let idle_a = columns().and_then(|(_, idle)| millis(&idle));
    std::thread::sleep(Duration::from_millis(200));
    let idle_b = columns().and_then(|(_, idle)| millis(&idle));
    r.check(
        &format!(
            "the idle column grows while the machine has nothing to run ({idle_a:?} -> {idle_b:?})"
        ),
        matches!((idle_a, idle_b), (Some(a), Some(b)) if b > a),
    );

    // A bracket, not two absolute numbers: the first column's own delta across
    // a measured sleep must land inside that sleep, with room for a loaded
    // TCG lane's scheduling. The old rendering's `90 * centis` ms step lands
    // outside it. `Instant` and uptime are both real time, so a slow lane
    // stretches both sides of the bracket together.
    let mut bracket_bad = 0usize;
    for _ in 0..40 {
        let up_a = columns().and_then(|(up, _)| millis(&up));
        let began = Instant::now();
        std::thread::sleep(Duration::from_millis(5));
        let up_b = columns().and_then(|(up, _)| millis(&up));
        let elapsed = began.elapsed().as_millis() as u64;
        match (up_a, up_b) {
            (Some(a), Some(b)) if b >= a => {
                let delta = b - a;
                if delta > elapsed + 300 || delta + 300 < elapsed {
                    bracket_bad += 1;
                }
            }
            _ => bracket_bad += 1,
        }
    }
    r.check(
        &format!(
            "the first column's delta stays inside its own sleep bracket ({bracket_bad} of 40 outside)"
        ),
        bracket_bad == 0,
    );

    r.finish();
}
