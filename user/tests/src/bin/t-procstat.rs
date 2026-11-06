//! t-procstat - `/proc/stat` is counters, and a counter is not a stand-in.
//!
//! Every monitor reads this file before any other: `vmstat`, `top`, `sar` and
//! every exporter take the `cpu` line's ten columns as *cumulative tick
//! counters* and subtract two readings to get a rate; `btime` as the instant the
//! machine booted; `processes` as how many tasks it has made; `ctxt` as how
//! often it has switched. That is why a plausible-looking file is worse than an
//! absent one - the reader does arithmetic on it and reports a number nobody
//! checked.
//!
//! This kernel printed the machine's **uptime in seconds** as the `cpu` line's
//! user column (`cpu  42 0 0 0 0 0 0 0 0 0`), with `intr 0` and `ctxt 0` beneath
//! it and no `btime`, `processes`, `procs_running` or `procs_blocked` line at
//! all. A minute of uptime then read back as ten thousand times the CPU time the
//! machine had spent, and every monitor that wanted the boot instant or the task
//! count hit a missing line.
//!
//! Measured on a current host (`build/scratch/stat/s.c`, `§6.100`) so the claims
//! are Linux's: the `cpu` line has ten columns; `processes` counts **threads as
//! well as processes** (three `fork`s then three `pthread_create`s moved it `+3`
//! and `+3`); `ctxt` counts switches (`+12` for that run); `btime` is
//! `now - uptime` rounded to the second; and the file's other lines
//! (`procs_running`, `procs_blocked`, `softirq`, `intr`) are the counters the
//! kernel keeps.
//!
//! What this kernel does *not* keep is a per-vector interrupt count and a
//! separate irq/softirq time split, so the `intr` line is **absent** rather than
//! zero (a zero there would claim no interrupt was ever taken) and the seven
//! columns this kernel has no dimension for print zero - both stated in
//! `src/sched/cpustat.rs`'s header and named as gaps in `docs/ROADMAP.md`.

use mpos_user_tests::{Report, host_run};
use std::time::{Duration, Instant};

/// The `cpu` aggregate line and the `cpu0` line, both in Linux's column order.
#[derive(Clone, Copy, Debug, Default)]
struct CpuLine {
    user: u64,
    nice: u64,
    system: u64,
    idle: u64,
    iowait: u64,
    irq: u64,
    softirq: u64,
    steal: u64,
    guest: u64,
    guest_nice: u64,
}

impl CpuLine {
    fn parse(fields: &[&str]) -> Option<CpuLine> {
        let mut numbers = [0u64; 10];
        for (slot, text) in numbers.iter_mut().zip(fields.iter()) {
            *slot = text.parse().ok()?;
        }
        Some(CpuLine {
            user: numbers[0],
            nice: numbers[1],
            system: numbers[2],
            idle: numbers[3],
            iowait: numbers[4],
            irq: numbers[5],
            softirq: numbers[6],
            steal: numbers[7],
            guest: numbers[8],
            guest_nice: numbers[9],
        })
    }

    fn busy(self) -> u64 {
        self.user + self.system
    }
}

/// One reading of the file: the lines this pin means to check.
#[derive(Default)]
struct Stat {
    cpu: CpuLine,
    cpu0: Option<CpuLine>,
    btime: Option<u64>,
    processes: Option<u64>,
    procs_running: Option<u64>,
    procs_blocked: Option<u64>,
    ctxt: Option<u64>,
}

fn read_stat() -> Option<Stat> {
    let text = std::fs::read_to_string("/proc/stat").ok()?;
    let mut stat = Stat::default();
    for line in text.lines() {
        let mut fields = line.split_whitespace();
        let Some(key) = fields.next() else { continue };
        let rest: Vec<&str> = fields.collect();
        match key {
            "cpu" => stat.cpu = CpuLine::parse(&rest)?,
            "cpu0" => stat.cpu0 = Some(CpuLine::parse(&rest)?),
            "btime" => stat.btime = rest.first()?.parse().ok(),
            "processes" => stat.processes = rest.first()?.parse().ok(),
            "procs_running" => stat.procs_running = rest.first()?.parse().ok(),
            "procs_blocked" => stat.procs_blocked = rest.first()?.parse().ok(),
            "ctxt" => stat.ctxt = rest.first()?.parse().ok(),
            _ => {}
        }
    }
    Some(stat)
}

/// `/proc/uptime`'s first field: seconds since boot, with two decimals.
fn uptime_seconds() -> Option<f64> {
    let text = std::fs::read_to_string("/proc/uptime").ok()?;
    text.split_whitespace().next()?.parse().ok()
}

fn now_epoch() -> Option<i64> {
    let mut now: libc::time_t = 0;
    if unsafe { libc::time(&mut now) } < 0 {
        return None;
    }
    Some(now as i64)
}

/// Burn user CPU for `micros` of wall time.
fn burn(micros: u64) {
    let start = Instant::now();
    let mut chunk = 0u64;
    while (Instant::now() - start).as_micros() < micros as u128 {
        for i in 0..10_000u64 {
            chunk = chunk.wrapping_add(i.wrapping_mul(2654435761));
        }
    }
    std::hint::black_box(chunk);
}

fn the_cpu_line_is_ten_columns_of_counters(r: &mut Report) {
    let Some(stat) = read_stat() else {
        r.check("the machine has a /proc/stat", false);
        return;
    };
    // Linux prints all ten, in this order, zero or not - a parser indexes them.
    let cpu = stat.cpu;
    let ten = [
        cpu.user,
        cpu.nice,
        cpu.system,
        cpu.idle,
        cpu.iowait,
        cpu.irq,
        cpu.softirq,
        cpu.steal,
        cpu.guest,
        cpu.guest_nice,
    ];
    r.check(
        &format!("the cpu line carries ten columns ({ten:?})"),
        ten.len() == 10,
    );
}

fn the_columns_are_the_time_the_machine_spent(r: &mut Report) {
    let Some(before) = read_stat() else {
        r.check("the machine has a /proc/stat", false);
        return;
    };
    burn(300_000);
    let Some(after) = read_stat() else {
        r.check("the machine still has a /proc/stat", false);
        return;
    };
    // A tick counter that is charged per mode moves while the mode is worked in.
    // The old field was the uptime in *seconds*, so half a second of burning
    // moved it by zero and the number it printed had nothing to do with CPU.
    r.check(
        &format!(
            "user and system grow while this process burns CPU ({} -> {})",
            before.cpu.busy(),
            after.cpu.busy()
        ),
        after.cpu.busy() > before.cpu.busy(),
    );
    // Idle is the column that is *not* charged while something runs - and it
    // does have to move when nothing does, which a hardcoded zero never could.
    let idle_before = after.cpu.idle;
    std::thread::sleep(Duration::from_millis(300));
    let Some(slept) = read_stat() else {
        r.check("the machine still has a /proc/stat", false);
        return;
    };
    r.check(
        &format!(
            "and the idle column grows while the machine has nothing to run ({} -> {})",
            idle_before, slept.cpu.idle
        ),
        slept.cpu.idle > idle_before,
    );
}

fn the_second_line_of_the_file_is_the_boot_instant(r: &mut Report) {
    let Some(stat) = read_stat() else {
        r.check("the machine has a /proc/stat", false);
        return;
    };
    let (Some(btime), Some(now), Some(uptime)) = (stat.btime, now_epoch(), uptime_seconds()) else {
        r.check(
            "the file names the boot instant (btime is a line of its own)",
            false,
        );
        return;
    };
    // Linux's btime is `now - uptime`, rounded to the second: measured on the
    // host as `btime=1793942878` with `now=1793998793` and `uptime=55915.27`,
    // a difference of one. Two seconds of slack covers the reads themselves.
    let derived = (now as f64 - uptime).round() as i64;
    let apart = (btime as i64 - derived).abs();
    r.check(
        &format!("btime is now minus uptime ({btime} against {derived}, {apart} s apart)"),
        apart <= 2,
    );
}

fn the_task_and_switch_counters_count(r: &mut Report) {
    let Some(before) = read_stat() else {
        r.check("the machine has a /proc/stat", false);
        return;
    };
    let (Some(tasks_before), Some(switches_before)) = (before.processes, before.ctxt) else {
        r.check(
            "the file counts the tasks it made and the switches it took",
            false,
        );
        return;
    };
    // A `fork` is a task made (Linux counts threads in this figure too, but a
    // process is unambiguous on either machine).
    let child = unsafe { libc::fork() };
    if child == 0 {
        unsafe { libc::_exit(0) };
    }
    if child > 0 {
        let mut status = 0;
        unsafe { libc::waitpid(child, &mut status, 0) };
    }
    std::thread::sleep(Duration::from_millis(100));
    let Some(after) = read_stat() else {
        r.check("the machine still has a /proc/stat", false);
        return;
    };
    let (Some(tasks_after), Some(switches_after)) = (after.processes, after.ctxt) else {
        r.check(
            "the file counts the tasks it made and the switches it took",
            false,
        );
        return;
    };
    r.check(
        &format!("processes counts the task this run forked ({tasks_before} -> {tasks_after})"),
        tasks_after > tasks_before,
    );
    // A sleeping process is switched away from and back to, so the counter is
    // the machine's own: a value that is a constant zero claims the machine
    // never leaves a task, which no scheduler that sleeps can honour.
    r.check(
        &format!("ctxt counts the switches a sleep takes ({switches_before} -> {switches_after})"),
        switches_after > switches_before,
    );
}

fn the_runnable_and_blocked_counts_read(r: &mut Report) {
    // One spinner beside this thread: one of them is running, the other is
    // ready, so the count is at least two while both are on the CPU's list.
    let spinner = std::thread::spawn(|| {
        let deadline = Instant::now() + Duration::from_millis(700);
        let mut chunk = 0u64;
        while Instant::now() < deadline {
            for i in 0..10_000u64 {
                chunk = chunk.wrapping_add(i.wrapping_mul(2654435761));
            }
        }
        std::hint::black_box(chunk);
    });
    std::thread::sleep(Duration::from_millis(200));
    let mut peak = 0;
    for _ in 0..16 {
        if let Some(stat) = read_stat() {
            peak = peak.max(stat.procs_running.unwrap_or(0));
        }
    }
    let blocked = read_stat().and_then(|stat| stat.procs_blocked);
    spinner.join().unwrap();
    r.check(
        &format!("procs_running counts the threads that could run ({peak})"),
        peak >= 2,
    );
    // Nothing in this run holds a device wait, so nothing is blocked - and the
    // line has to be there to say so.
    r.check(
        &format!(
            "procs_blocked names the uninterruptible waiters ({})",
            blocked.map_or("absent".into(), |n| n.to_string())
        ),
        blocked == Some(0),
    );
}

fn the_one_cpu_is_the_whole_machine(r: &mut Report) {
    // A uniprocessor's aggregate and its `cpu0` line are the same counters.
    // Linux on many CPUs would not be, so this claim is the machine's own.
    if host_run() {
        return;
    }
    let Some(stat) = read_stat() else {
        r.check("the machine has a /proc/stat", false);
        return;
    };
    let Some(first) = stat.cpu0 else {
        r.check("the file has a cpu0 line beside the aggregate", false);
        return;
    };
    r.check(
        &format!(
            "the aggregate is cpu0, this being one CPU ({} against {})",
            stat.cpu.busy(),
            first.busy()
        ),
        stat.cpu.user == first.user
            && stat.cpu.system == first.system
            && stat.cpu.idle == first.idle,
    );
}

fn main() {
    let mut r = Report::new();
    the_cpu_line_is_ten_columns_of_counters(&mut r);
    the_columns_are_the_time_the_machine_spent(&mut r);
    the_second_line_of_the_file_is_the_boot_instant(&mut r);
    the_task_and_switch_counters_count(&mut r);
    the_runnable_and_blocked_counts_read(&mut r);
    the_one_cpu_is_the_whole_machine(&mut r);
    r.finish();
}
