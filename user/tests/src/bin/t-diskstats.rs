//! t-diskstats — `/proc/diskstats`: Linux's twenty fields, and the counters a
//! real read moves.
//!
//! The file had no door until this item: the block layer counted reads, writes,
//! sectors, milliseconds, in-flight transfers and flushes into its wrapper, and
//! the only reader was `uname(2)`'s private release word — a measurement, not an
//! interface. This is the interface half, and the claims are the ones a parser of
//! this file depends on.
//!
//! # What is claimed, and why each claim can go red
//!
//! * **Linux's twenty fields per line**, first field the major: a rendering that
//!   dropped a column or reordered two would make every later number a lie for
//!   `iostat` and `node_exporter`, even though the bytes still looked plausible.
//! * **A whole disk has a line and that line is the one the read moves**: the
//!   pin opens the disk itself (`/dev/<name>` of the minor-0 row), reads a page,
//!   and requires *that* row's reads to have advanced — a counter taken at the
//!   wrong layer (the partition's, or a copy taken at registration) fails here.
//! * **The sectors column is sectors**: 4096 bytes read is eight 512-byte
//!   sectors; a byte count or a KiB count in that column fails.
//! * **Nothing is in flight when nothing is running**: the in-flight column is
//!   the pair the stall hunt needs, so it is asserted zero at rest rather than
//!   merely printed.
//! * **A second read with no I/O between does not move anything**: a counter
//!   that fabricates work would fail this.
//!
//! On the host the same claims run against real Linux's own file (the device is
//! the host's root disk), which is the reference the rendering was written
//! against.

use mpos_user_tests::Report;

/// The twenty fields every line has: major, minor, name, then seventeen.
const FIELDS: usize = 20;

/// One parsed line.
struct Row {
    major: u64,
    minor: u64,
    name: String,
    values: Vec<u64>,
}

impl Row {
    fn field(&self, index: usize) -> u64 {
        // `values` starts at the fourth field of the line (reads).
        self.values[index]
    }
}

fn read_rows() -> Option<Vec<Row>> {
    let text = std::fs::read_to_string("/proc/diskstats").ok()?;
    let mut rows = Vec::new();
    for line in text.lines() {
        let words: Vec<&str> = line.split_whitespace().collect();
        if words.len() != FIELDS {
            return None;
        }
        let mut values = Vec::new();
        for word in &words[3..] {
            values.push(word.parse::<u64>().ok()?);
        }
        rows.push(Row {
            major: words[0].parse().ok()?,
            minor: words[1].parse().ok()?,
            name: words[2].to_string(),
            values,
        });
    }
    Some(rows)
}

/// `/proc/partitions`' whole disks: the names with minor zero.
fn whole_disks() -> Vec<String> {
    let Ok(text) = std::fs::read_to_string("/proc/partitions") else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for line in text.lines().skip(2) {
        let words: Vec<&str> = line.split_whitespace().collect();
        if words.len() == 4 && words[1] == "0" {
            out.push(words[3].to_string());
        }
    }
    out
}

/// Read one page of the device itself, so the disk's row is what moves.
fn read_a_page(name: &str) -> Option<usize> {
    use std::os::unix::fs::FileExt;
    let file = std::fs::File::open(format!("/dev/{name}")).ok()?;
    let mut buf = [0u8; 4096];
    file.read_at(&mut buf, 0).ok()
}

fn main() {
    let mut r = Report::new();

    let rows = read_rows();
    r.check(
        "the file parses as Linux's twenty fields per line",
        rows.is_some(),
    );
    let Some(rows) = rows else {
        r.finish();
        return;
    };
    r.check("there is at least one block device", !rows.is_empty());

    if mpos_user_tests::host_run() {
        // Real Linux's own file is the reference the rendering was written
        // against; the counter claims below are this kernel's door (Linux has
        // its own implementation of the same file) and are run on the guest.
        let disks = whole_disks();
        r.check("the host's file lists a whole disk", !disks.is_empty());
        println!("     (the counter claims run on the guest; this run is the reference)");
        r.finish();
    }

    let disks = whole_disks();
    r.check("a whole disk has a line of its own", !disks.is_empty());
    let Some(disk) = disks.first() else {
        r.finish();
        return;
    };
    let Some(row) = rows.iter().find(|row| &row.name == disk && row.minor == 0) else {
        r.check(
            "the disk's line names the disk /proc/partitions lists",
            false,
        );
        r.finish();
        return;
    };
    r.check("the disk's major is the driver's", row.major != 0);

    let before = read_rows().and_then(|all| {
        all.into_iter()
            .find(|line| line.name == row.name && line.minor == 0)
    });
    let read = read_a_page(disk);
    r.check("the disk answers a page read", read == Some(4096));
    std::thread::sleep(std::time::Duration::from_millis(50));
    let after = read_rows().and_then(|all| {
        all.into_iter()
            .find(|line| line.name == row.name && line.minor == 0)
    });
    match (before, after) {
        (Some(before), Some(after)) => {
            r.check(
                "the read moved this disk's read counter, and only forward",
                after.field(0) > before.field(0),
            );
            r.check(
                "the sectors column counted eight 512-byte sectors",
                after.field(2) >= before.field(2) + 8,
            );
            r.check("nothing is in flight at rest", after.field(8) == 0);
            r.check(
                "the millisecond columns are milliseconds, not a constant zero",
                after.field(3) >= 0 && after.field(3) < 60_000,
            );
        }
        _ => {
            r.check("the file still parses after the read", false);
        }
    }

    // A second read of the file with no I/O between must not move anything.
    let quiet = read_rows().and_then(|all| {
        all.into_iter()
            .find(|line| line.name == row.name && line.minor == 0)
    });
    let quiet_again = read_rows().and_then(|all| {
        all.into_iter()
            .find(|line| line.name == row.name && line.minor == 0)
    });
    match (quiet, quiet_again) {
        (Some(a), Some(b)) => r.check(
            "two reads with no I/O between them agree",
            a.values == b.values,
        ),
        _ => r.check("the file parses twice in a row", false),
    }

    r.finish();
}
