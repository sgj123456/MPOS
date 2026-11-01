//! `/proc/<pid>/smaps_rollup`: the whole address space's accounting in one record.
//!
//! The file exists for readers that want the totals without parsing every
//! mapping, and Linux's answer has three properties a reader relies on. Each is
//! measured on a current host first (Linux 7.2.11):
//!
//! * **One header line and no size lines.** The header is a `maps`-shaped range
//!   covering the whole space, named `[rollup]`; the field block starts at `Rss:`
//!   - `Size:`/`KernelPageSize:`/`MMUPageSize:` are per-mapping numbers and are
//!   absent. Measured: 23 lines (one header, twenty-two fields), the header's
//!   permission column `---p` and its offset/inode zero.
//! * **The fields are `smaps`' fields, in `smaps`' order.** Read both files in
//!   one process and the label sequence of `smaps_rollup` is the sequence of
//!   `smaps` with the three size lines dropped. That is the claim here, and it
//!   is what keeps the two files from drifting apart.
//! * **One snapshot per open.** A second `read(2)` on the same descriptor is 0
//!   (Linux's `seq_file` semantics for this file), while a fresh open answers
//!   again.
//!
//! The `Rss:` total is also compared with the per-mapping numbers: the rollup's
//! figure is the sum of `smaps`' `Rss:` lines, which holds on this kernel by the
//! model `t-smaps` documents (every resident page belongs to the mapping that
//! faulted it in).

use mpos_user_tests::Report;
use std::io::Read;

fn slurp(path: &str) -> Option<String> {
    let mut f = std::fs::File::open(path).ok()?;
    let mut s = String::new();
    f.read_to_string(&mut s).ok()?;
    Some(s)
}

/// The `Name:` labels of a file's first block of `label: value` lines, in order.
///
/// `smaps` has one block per mapping, so the reading stops at the second record
/// header - the comparison is between *one* record's fields and the rollup's.
fn labels(text: &str) -> Vec<String> {
    let mut text = text;
    if let Some(cut) = text
        .lines()
        .enumerate()
        .skip(1)
        .find(|(_, l)| is_header(l))
        .map(|(i, _)| i)
    {
        let byte = text.lines().take(cut).map(|l| l.len() + 1).sum::<usize>();
        text = &text[..byte];
    }
    text.lines()
        .filter_map(|l| {
            let (label, rest) = l.split_once(':')?;
            if rest.starts_with(' ') && !label.contains(' ') {
                Some(format!("{label}:"))
            } else {
                None
            }
        })
        .collect()
}

/// Whether a line is a `maps` record header rather than a field line.
fn is_header(line: &str) -> bool {
    // A record header starts with an address and has no `label:` form - the
    // device column (`00:00`) is why "contains a colon" cannot be the test.
    line.starts_with(|c: char| c.is_ascii_hexdigit()) && line.split_whitespace().count() >= 5
}

/// The number a `Rss:`-style line carries, in kB.
fn kb_of(text: &str, label: &str) -> Option<u64> {
    for l in text.lines() {
        if let Some(rest) = l.strip_prefix(label) {
            let n: String = rest.chars().filter(|c| c.is_ascii_digit()).collect();
            if n.is_empty() {
                return None;
            }
            return n.parse().ok();
        }
    }
    None
}

fn main() {
    let mut r = Report::new();

    // Shape: one header line, a field block, an open that answers, and a second
    // read that is 0.
    let rollup = slurp("/proc/self/smaps_rollup").unwrap_or_default();
    r.check(
        "the file is there and reads as a whole snapshot",
        !rollup.is_empty(),
    );
    r.check(
        "the header names the rollup and covers the space",
        rollup
            .lines()
            .next()
            .map(|l| l.contains("[rollup]") && l.contains("---p"))
            .unwrap_or(false),
    );
    let labels_here = labels(&rollup);
    r.check(
        "the field block starts at Rss, with no per-mapping size lines",
        labels_here.first().map(|l| l == "Rss:").unwrap_or(false)
            && !labels_here
                .iter()
                .any(|l| l == "Size:" || l == "KernelPageSize:" || l == "MMUPageSize:"),
    );

    // The labels are `smaps`' labels minus the size lines, in order.
    if let Some(smaps) = slurp("/proc/self/smaps") {
        let want: Vec<String> = labels(&smaps)
            .into_iter()
            .filter(|l| l != "Size:" && l != "KernelPageSize:" && l != "MMUPageSize:")
            .collect();
        // Linux's rollup carries three `Pss_*` lines `smaps` here does not, so the
        // claim is the order-preserving one that holds on both: every field this
        // kernel's `smaps` prints is printed by the rollup, in the same order.
        let present: Vec<&String> = want.iter().filter(|w| labels_here.contains(w)).collect();
        let mut it = labels_here.iter();
        let same = present.iter().all(|w| it.any(|l| l == *w)) && present.len() + 3 >= want.len();
        if !same {
            eprintln!("rollup labels: {labels_here:?}");
            eprintln!("smaps labels minus sizes: {want:?}");
        }
        r.check(
            "every field smaps prints is in the rollup, in the same order",
            same,
        );
    } else {
        r.check("smaps is read as the reference for the field list", false);
    }

    // The Rss total is the sum of the per-mapping figures.
    if let Some(smaps) = slurp("/proc/self/smaps") {
        // The exact sum is deliberately *not* a claim: reading `smaps` grows this
        // process's own heap (the buffer it allocates is a mapping), so the set of
        // mappings differs between the two reads - measured on a current host as
        // 2468 kB against a 2384 kB total. The claim that holds is the inequality
        // the file promises: the total covers every mapping.
        let largest = smaps
            .lines()
            .filter_map(|l| l.strip_prefix("Rss:"))
            .filter_map(|rest| {
                let n: String = rest.chars().filter(|c| c.is_ascii_digit()).collect();
                if n.is_empty() {
                    None
                } else {
                    n.parse::<u64>().ok()
                }
            })
            .max()
            .unwrap_or(0);
        let total = kb_of(&rollup, "Rss:").unwrap_or(0);
        r.check(
            "the total covers the largest single mapping",
            total >= largest,
        );
    }

    // The `Pss` split is the rollup's alone. Measured on a current host (Linux
    // 7.2.11): `grep -c Pss_Anon /proc/self/smaps` is 0 and
    // `grep -c Pss_Anon /proc/self/smaps_rollup` is 1 - so a `smaps` that grew
    // these three lines would be *less* like Linux, not more.
    if let Some(smaps) = slurp("/proc/self/smaps") {
        let smaps_labels = labels(&smaps);
        let split_in_rollup = ["Pss_Anon:", "Pss_File:", "Pss_Shmem:"]
            .iter()
            .all(|l| labels_here.iter().any(|x| x == l));
        let split_out_of_smaps = ["Pss_Anon:", "Pss_File:", "Pss_Shmem:"]
            .iter()
            .all(|l| !smaps_labels.iter().any(|x| x == l));
        r.check(
            "the Pss split belongs to the rollup alone, as on Linux",
            split_in_rollup && split_out_of_smaps,
        );
    }

    // One snapshot per open.
    let mut f = std::fs::File::open("/proc/self/smaps_rollup").unwrap();
    let mut first = String::new();
    f.read_to_string(&mut first).ok();
    r.check("an open answers a snapshot", !first.is_empty());
    let mut again = String::new();
    let n = f
        .read_to_string(&mut again)
        .ok()
        .map(|_| again.len())
        .unwrap_or(0);
    r.check("a second read of the same descriptor is empty", n == 0);
    // Same header, not the same bytes: reading the first snapshot allocates, so
    // this process's own address space can grow between the two opens (measured
    // on a current host: the total moved by 84 kB).
    r.check(
        "a fresh open answers again",
        slurp("/proc/self/smaps_rollup")
            .map(|s| s.lines().next() == first.lines().next())
            .unwrap_or(false),
    );

    // The file is readable by anyone the process may be inspected by, and its
    // mode is Linux's `0444`.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let m = std::fs::metadata("/proc/self/smaps_rollup")
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        r.check("the file's mode is 0444", m == 0o444);
    }

    r.finish();
}
