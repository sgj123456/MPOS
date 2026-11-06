//! t-loadavg - the load average is a measurement, and both doors report it.
//!
//! `/proc/loadavg` is four fields a program parses mechanically: the one-, five-
//! and fifteen-minute load averages as integers with two decimals, then
//! `runnable/total` and the most recent pid. `sysinfo(2)` carries the same three
//! averages as fixed-point `unsigned long`s. What nobody parses is the *shape*
//! of the arithmetic, and that is where a kernel can print a plausible file
//! without measuring anything: this one printed `0.00 0.00 0.00 1/n` on every
//! machine while `docs/uapi/03-flag-vocabularies.md` listed the file as served,
//! and left `sysinfo.loads[0..3]` zero in a buffer it had just cleared - the
//! statement that nothing has wanted the CPU since boot, which is the one thing
//! a load average exists to contradict.
//!
//! Measured on Linux 7.2.14 (`build/scratch/loadavg/p.c`): an idle host printed
//! `1.85 2.04 2.21 3/1312`, and forty reads 100 ms apart saw the averages change
//! only at **5-second** boundaries (`1.85 2.04 2.21` -> `1.94 2.05 2.22` ->
//! `2.27 2.12 2.24`) while the runnable/threads field changed between reads.
//! `sysinfo()` answered `loads = 127456 134624 145376`, and
//! `127456 / 65536 = 1.9449` - one fixed-point value, printed two ways. Four
//! spinners for six seconds moved the one-minute figure to `2.27` at the next
//! boundary and `sysinfo.loads[0]` to `148768` in the same instant, which is the
//! claim the two doors must always satisfy together.
//!
//! The claims below are therefore: the two doors report one value and it is not
//! the zero this kernel used to print; the runnable field counts the threads
//! that are actually ready; and the averages change at the boundary of a sample,
//! not on every read. The last one separates a sampled average from one
//! recomputed per read, which would jitter with the runnable count instead of
//! decaying.

use mpos_user_tests::Report;
use std::time::{Duration, Instant};

/// Linux's `SI_LOAD_SHIFT`: `sysinfo.loads` are the averages shifted up 5 bits
/// from the 11-bit fixed point the kernel keeps (`FIXED_1 = 1 << 11`).
const SI_LOAD_SHIFT: u32 = 16;

/// One parsing of `/proc/loadavg`.
struct LoadAvg {
    /// The three averages in hundredths, the unit the file prints.
    averages: [i64; 3],
    runnable: u64,
    total: u64,
}

fn parse_average(text: &str) -> Option<i64> {
    let (int, frac) = text.split_once('.')?;
    Some(int.parse::<i64>().ok()? * 100 + frac.parse::<i64>().ok()?)
}

fn read_loadavg() -> Option<LoadAvg> {
    let text = std::fs::read_to_string("/proc/loadavg").ok()?;
    let mut fields = text.split_whitespace();
    let one = parse_average(fields.next()?)?;
    let five = parse_average(fields.next()?)?;
    let fifteen = parse_average(fields.next()?)?;
    let (runnable, total) = fields.next()?.split_once('/')?;
    Some(LoadAvg {
        averages: [one, five, fifteen],
        runnable: runnable.parse().ok()?,
        total: total.parse().ok()?,
    })
}

fn sysinfo_loads() -> Option<[u64; 3]> {
    let mut info: libc::sysinfo = unsafe { core::mem::zeroed() };
    if unsafe { libc::sysinfo(&mut info) } != 0 {
        return None;
    }
    Some([
        info.loads[0] as u64,
        info.loads[1] as u64,
        info.loads[2] as u64,
    ])
}

/// A `sysinfo` fixed-point load in the file's hundredths.
fn fixed_to_hundredths(fixed: u64) -> i64 {
    ((fixed as i128 * 100) >> SI_LOAD_SHIFT) as i64
}

fn spin_for(seconds: u64) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(seconds);
        let mut chunk = 0u64;
        while Instant::now() < deadline {
            for i in 0..100_000u64 {
                chunk = chunk.wrapping_add(i.wrapping_mul(2654435761));
            }
        }
        std::hint::black_box(chunk);
        std::hint::black_box(std::time::Instant::now());
    })
}

fn the_two_doors_report_one_average(r: &mut Report) {
    // Read the file between two `sysinfo` calls: if a sampling boundary falls
    // between them the file matches one of the two, and if it does not it
    // matches both. Either way one of the two is the value the file printed.
    let before = sysinfo_loads();
    let file = read_loadavg();
    let after = sysinfo_loads();
    let (Some(before), Some(file), Some(after)) = (before, file, after) else {
        r.check("both doors answer", false);
        return;
    };
    let one = file.averages[0];
    let matches = |fixed: u64| (fixed_to_hundredths(fixed) - one).abs() <= 1;
    r.check(
        &format!(
            "sysinfo and /proc/loadavg report one load average ({one} against {} and {})",
            fixed_to_hundredths(before[0]),
            fixed_to_hundredths(after[0])
        ),
        matches(before[0]) || matches(after[0]),
    );
}

fn the_runnable_field_counts_ready_threads(r: &mut Report) {
    let spinners: Vec<_> = (0..3).map(|_| spin_for(7)).collect();
    // Let the spinners reach the ready queues and one sampling boundary pass -
    // the average is only recomputed every five seconds, so a read before the
    // boundary says what the machine looked like before them.
    std::thread::sleep(Duration::from_millis(5_600));

    let file = read_loadavg();
    let sysinfo = sysinfo_loads();
    let (Some(file), Some(sysinfo)) = (file, sysinfo) else {
        r.check("the machine answers both doors", false);
        return;
    };
    r.check(
        &format!(
            "the runnable field counts the threads that are ready ({}/{})",
            file.runnable, file.total
        ),
        file.runnable >= 3,
    );
    r.check(
        &format!(
            "one sample lifts the one-minute average off zero ({} hundredths)",
            file.averages[0]
        ),
        file.averages[0] > 5,
    );
    r.check(
        &format!(
            "and sysinfo carries the same lift (loads[0] = {}, which is {} hundredths)",
            sysinfo[0],
            fixed_to_hundredths(sysinfo[0])
        ),
        sysinfo[0] > 0 && (fixed_to_hundredths(sysinfo[0]) - file.averages[0]).abs() <= 2,
    );

    // The averages are sampled once per five seconds, so a one-second window
    // sees at most one boundary: two distinct triples, never a value that
    // follows the runnable count read by read.
    let mut seen: Vec<[i64; 3]> = Vec::new();
    let start = Instant::now();
    while start.elapsed() < Duration::from_millis(1_000) {
        if let Some(file) = read_loadavg() {
            if !seen.contains(&file.averages) {
                seen.push(file.averages);
            }
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    r.check(
        &format!(
            "the averages hold steady between samples ({} distinct triples in 20 reads)",
            seen.len()
        ),
        (1..=2).contains(&seen.len()),
    );

    for spinner in spinners {
        let _ = spinner.join();
    }
}

fn main() {
    let mut r = Report::new();
    the_two_doors_report_one_average(&mut r);
    the_runnable_field_counts_ready_threads(&mut r);
    r.finish();
}
