//! The CPU class in sysfs: the four lists that say which CPUs the machine runs.
//!
//! crashpad - the crash handler inside chromium - opens
//! `/sys/devices/system/cpu/online` before it can size its thread list, and a
//! tree without it makes the handler give up and guess. Linux's shape, measured
//! on a current host first: all four attributes are `0444`, each holds a CPU
//! list (`0-15` on that 16-way machine), and `offline` is empty.
//!
//! This kernel runs one CPU, so the list has the single entry `0`. The claims
//! below therefore split: the *shape* (a parseable list, the `0444` word) is
//! checked on both machines, and the *value* only on the guest, because the
//! host this test may run on has however many CPUs it has.

use mpos_user_tests::Report;

use std::fs;

const CPU: &str = "/sys/devices/system/cpu";

/// A Linux CPU list: a comma-separated run of single CPUs (`3`) and ranges
/// (`0-15`), with no spaces, and the trailing newline sysfs writes.
fn parse_cpu_list(text: &str) -> Option<Vec<u32>> {
    let body = text.strip_suffix('\n')?;
    if body.is_empty() {
        return Some(Vec::new());
    }
    let mut cpus = Vec::new();
    for part in body.split(',') {
        match part.split_once('-') {
            Some((first, last)) => {
                let first: u32 = first.parse().ok()?;
                let last: u32 = last.parse().ok()?;
                if first > last {
                    return None;
                }
                cpus.extend(first..=last);
            }
            None => cpus.push(part.parse().ok()?),
        }
    }
    Some(cpus)
}

fn read(path: &str) -> Option<String> {
    fs::read_to_string(path).ok()
}

fn mode_of(path: &str) -> Option<u32> {
    use std::os::unix::fs::PermissionsExt;
    fs::metadata(path)
        .ok()
        .map(|m| m.permissions().mode() & 0o7777)
}

fn main() {
    let mut report = Report::new();
    let host = mpos_user_tests::host_run();

    // The class directory, and its four lists.
    report.check(
        "the CPU class is a directory",
        fs::metadata(CPU).map(|m| m.is_dir()).unwrap_or(false),
    );
    for name in ["online", "present", "possible", "offline"] {
        let path = format!("{CPU}/{name}");
        let body = read(&path);
        report.check(
            &format!("{name} is there and parses as a CPU list"),
            body.as_deref().and_then(parse_cpu_list).is_some(),
        );
        report.check(
            &format!("{name} carries a read-only attribute's word"),
            mode_of(&path) == Some(0o444),
        );
    }

    // The value is this machine's: one CPU, and nothing offline, because the
    // scheduler is uniprocessor and the one CPU is the one running.
    let online = read(&format!("{CPU}/online")).and_then(|t| parse_cpu_list(&t));
    let present = read(&format!("{CPU}/present")).and_then(|t| parse_cpu_list(&t));
    let possible = read(&format!("{CPU}/possible")).and_then(|t| parse_cpu_list(&t));
    let offline = read(&format!("{CPU}/offline")).and_then(|t| parse_cpu_list(&t));
    if host {
        // Whatever this machine has, the lists must agree with each other.
        report.check(
            "online is a non-empty list of CPUs",
            online.as_ref().is_some_and(|cpus| !cpus.is_empty()),
        );
        report.check(
            "every online CPU is also present and possible",
            online.as_ref().is_some_and(|cpus| {
                cpus.iter().all(|cpu| {
                    present.as_ref().is_some_and(|p| p.contains(cpu))
                        && possible.as_ref().is_some_and(|p| p.contains(cpu))
                })
            }),
        );
        println!("note: the values checked here are this host's; the guest's are one CPU");
    } else {
        report.check(
            "online names the one CPU this machine runs",
            online == Some(vec![0]),
        );
        report.check("present names it too", present == Some(vec![0]));
        report.check("possible names it too", possible == Some(vec![0]));
        report.check("nothing is offline", offline == Some(Vec::new()));
        // The other door that answers the same question: `/proc/stat` prints a
        // `cpu0` line, and the two files are the same fact read twice.
        let stat = read("/proc/stat").unwrap_or_default();
        report.check(
            "/proc/stat agrees there is a cpu0",
            stat.lines().any(|line| line.starts_with("cpu0 ")),
        );
    }

    report.finish();
}
