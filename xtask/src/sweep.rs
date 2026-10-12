//! The half of a test sweep that is not the machine.
//!
//! `cargo xtask test` boots two very different payloads: x86_64 packs the test
//! programs into a throwaway initramfs on the QEMU wiring in `machine.toml` and
//! the guest ends the machine with a debug exit, while aarch64 stages them on
//! its own ESP and disk and the run ends on `=== END`. That divergence is real
//! and stays where it is - the boot mechanism is the machine's.
//!
//! What is *not* the machine's is everything around it, and it was written twice
//! (`TEST-AND-BOOT-REFACTOR.md` §2, which measured the copy at ~90 lines plus
//! the verdict parser):
//!
//! * what `tests.toml` means - which programs a run boots, which it leaves out,
//!   and the refusal that keeps the file honest (a group nobody defined);
//! * the `=== RUN/PASS/FAIL/SUMMARY/END` protocol, read out of the log;
//! * the "staged / left out" line and the per-group list under it, which say in
//!   one glance whether what this run skipped is what it meant to skip.
//!
//! One rule that *looked* like it belonged here does not, and [`select`] records
//! the measurement: "a name `tests.toml` declares must be a program this arm
//! built" has two scopes, because the two arms do not build the same file.
//!
//! So this module is that half. The run itself is one body now - x86_64's
//! `selftest::run_tests` and the second machine's `selftest::run_disk_sweep`
//! both end up in a disk sweep the machine parameterises - so what an adapter
//! still keeps is what only its own arm can answer: which target to build,
//! which firmware to boot under, and when the run is over. Its *verdict
//! wording* stays there too, because the two arms make different claims - one
//! of them has a debug exit to check and the other asserts an exact count
//! instead - and a shared sentence would have to be vague enough to cover both.

use anyhow::{Result, bail};

use crate::config::{Machine, TestConfig};
use crate::utils;

/// The summary line the parse below counts on. The verdict lines around it are
/// deliberately shaped so a test's own output cannot forge a verdict: no test
/// prints `=== `.
const SUMMARY_MARKER: &str = "=== SUMMARY";

/// One built test program, however this machine carries it.
///
/// The first machine stages a `PathBuf` the initramfs is assembled from; the
/// second reads its binary into a `Vec<u8>` and lays it on an ext4 image. Both
/// are asked one question by everything in this module - the name `tests.toml`
/// keys them by - so the payload itself never has to be neutral.
pub trait TestProgram {
    fn name(&self) -> &str;
}

/// What the caller asked `test` to run: the groups named on the command line
/// (`--groups external,slow`) and whether every group was asked for
/// (`--all-groups`). Neither means the `default` list in `tests.toml`.
#[derive(Clone, Default)]
pub struct Selection {
    pub groups: Vec<String>,
    pub all: bool,
}

/// The programs this run boots, and the ones `tests.toml` leaves out of it.
#[derive(Debug)]
pub struct Plan<'a, T> {
    pub staged: Vec<&'a T>,
    pub skipped: Vec<&'a T>,
    /// How many programs the crate built, staged or not. The report quotes it,
    /// so a run that quietly boots less than it used to cannot read as a green
    /// one.
    pub built: usize,
}

/// The `tests.toml` this workspace ships.
///
/// Loaded here rather than by each arm so the file's path is one fact. A missing
/// file is an empty configuration, which is `TestConfig::load`'s own rule.
pub fn load_config() -> Result<TestConfig> {
    TestConfig::load(&utils::project_root()?.join(crate::paths::TESTS_CONFIG))
}

/// Whether `name` belongs to `machine`'s *default* sweep: ungrouped, or named
/// in a group the plain run adds.
///
/// Lives here because it is the same question `select` answers when it fills a
/// `Plan`, and one reader of `tests.toml` decides membership - pinned against
/// that selection by [`tests::in_default_follows_the_group_membership_the_selection_uses`].
/// Its caller today is §10.5's documentation of intent: the instrument's two
/// programs stay in the default list on both machines *because* they are exact
/// counts rather than timings, and this is the question an arm could ask if a
/// future harness ever needed "would a plain sweep stage this name" without a
/// built-program list to select from.
pub fn in_default(machine: Machine, config: &TestConfig, name: &str) -> bool {
    let selected = config.selection(&[], false);
    // A machine's own gap group is not a reason-group: a name the second
    // machine cannot serve yet is still in that machine's *plain* run for the
    // first arm's purposes, but for this arm it never stages - so "is this
    // program in my default sweep" is exactly `runs`, unchanged.
    config.runs(name, &selected, machine)
}

/// Choose what this run boots, refusing a group nobody defined.
///
/// The refusal is the point rather than the politeness: a group name nobody
/// defined would otherwise show up only as a smaller number in a summary line -
/// which is the one failure a sweep must not have.
///
/// The three lines printed here are byte-identical on both machines on purpose:
/// they are statements about `tests.toml`, and a reader comparing the two arms'
/// output has to be able to diff them.
///
/// # What is deliberately *not* here
///
/// The "a declared name must be a program this arm builds" check. It looks like
/// one rule and is two, measured: the first machine's arm builds both suites, so
/// every name in the file has to be answerable there; the second's builds only
/// the std-side suite (153 programs against the first's 170 today - §15.11 moves
/// the rest across a batch at a time), so the names in
/// `external`/`slow`/`diagnostic` that belong to the bare-metal conformance
/// suite legitimately have no program on that machine. Sharing one scope would
/// either refuse every aarch64 sweep or stop checking the first machine
/// altogether, so each arm keeps the question it can answer - and its own
/// wording for it.
pub fn select<'a, T: TestProgram>(
    machine: Machine,
    config: &TestConfig,
    built: &'a [T],
    selection: &Selection,
) -> Result<Plan<'a, T>> {
    let unknown = config.unknown(&selection.groups);
    if !unknown.is_empty() {
        let known = config.group_names(machine);
        bail!(
            "unknown test group(s) {}; tests.toml defines {}",
            unknown.join(", "),
            if known.is_empty() {
                "none".to_string()
            } else {
                known.join(", ")
            }
        );
    }

    let selected = config.selection(&selection.groups, selection.all);
    // The reason-groups (`external`, `slow`, `diagnostic`) are x86_64's
    // vocabulary: they name programs that need *that* machine's harness - the
    // internet through its QEMU wiring, a host listener on its 127.0.0.1 - or
    // measure against its clock. On the second machine the same-named program is
    // a different instrument (`t-netlat` measures this kernel's own loopback
    // fabric, not the internet), and §13.5's contract for this arm is that it
    // runs **every program it built** except the ones its own group names as
    // gaps. So here a *reason* never leaves a program out; only this machine's
    // own gap list does. A machine-named group keeps meaning "not me" everywhere
    // else, unchanged.
    //
    // The one exception is a program that is not a check at all. A measurement
    // instrument (`t-forkcost`, `t-netlat`) is a different instrument on each
    // machine, so running it on both is right. A diagnostic (`t-sh`) is the
    // *same* thing everywhere - an interactive shell exec'd onto the console to
    // be read by hand - and §15.34's rule says what a boot may assert is
    // bounded by what the kernel itself produces, which an interactive child's
    // state letter is not. So `diagnostic` leaves a program out on both arms;
    // `external` and `slow` stay x86_64-harness words.
    let hand_read = |name: &str| {
        config
            .groups_of(name, Machine::X86_64)
            .contains(&"diagnostic")
    };
    let staged: Vec<&'a T> = if machine == Machine::X86_64 {
        built
            .iter()
            .filter(|test| config.runs(test.name(), &selected, machine))
            .collect()
    } else {
        built
            .iter()
            .filter(|test| {
                !hand_read(test.name())
                    && !config
                        .groups_of(test.name(), machine)
                        .iter()
                        .any(|group| config.owns_a_machine(group))
            })
            .collect()
    };
    let skipped: Vec<&'a T> = built
        .iter()
        .filter(|test| {
            !staged
                .iter()
                .any(|staged_test| staged_test.name() == test.name())
        })
        .collect();
    println!(
        "user tests: {} staged, {} left out by tests.toml",
        staged.len(),
        skipped.len()
    );
    // Printed group by group rather than as a list of names, because the reason
    // is the group: a reader has to be able to see at a glance whether the tests
    // this run left out are the ones it meant to.
    for group in config.group_names(machine) {
        let left: Vec<&str> = skipped
            .iter()
            .filter(|test| config.groups_of(test.name(), machine).contains(&group))
            .map(|test| test.name())
            .collect();
        if !left.is_empty() {
            println!("  {group:11} {}", left.join(", "));
        }
    }
    println!();

    Ok(Plan {
        staged,
        skipped,
        built: built.len(),
    })
}

/// The `=== ` protocol, as both runners print it.
///
/// One parser for two runners, and it takes the stricter of the two rules each
/// arm used to carry: the summary is the **last** `=== SUMMARY` line (a log can
/// only be trusted for its final word on the run - and both runners print
/// exactly one, so the stricter rule costs nothing), and a failed program's name
/// is the first word after `=== FAIL ` (the runner appends `exit=N`, which is
/// not part of the name).
pub struct Verdict<'a> {
    pub passed: Vec<&'a str>,
    pub failed: Vec<&'a str>,
    /// The summary line, if the run reached the end of its programs.
    pub summary: Option<&'a str>,
    /// The runner could not set the machine up, so nothing the programs did is
    /// evidence about the kernel yet. The aarch64 runner is the one that prints
    /// this; the first machine's setup failing shows up as a missing summary.
    pub setup_failed: Option<&'a str>,
    /// The *kernel's* own suites, which ran before any program did.
    pub kunit: Option<Kunit>,
}

/// What the test kernel's suites found, as `mpos-kunit: N passed, M failed`.
///
/// A separate verdict from the sweep's, because it is a separate half of the
/// gate: the harness prints one line, the payload prints `=== ` lines, and the
/// two say different things. It is also what makes "the suites ran" a claim
/// rather than a hope - a suite that silently never ran leaves its cases out of
/// the count, and the count is asserted below rather than trusted (`src/board/
/// kunit.rs`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Kunit {
    pub passed: usize,
    pub failed: usize,
}

/// The harness's line, if the boot printed one.
///
/// Absent is a *failure* everywhere it is used, not a neutral answer: a test
/// kernel that does not report its suites has not told us they ran, and this
/// repository's rule for a gate is that it must not be able to pass quietly
/// with less than it claims.
pub fn kunit(log: &str) -> Option<Kunit> {
    log.lines()
        .rev()
        .find_map(|line| line.trim_end().strip_prefix("mpos-kunit: "))
        .and_then(|rest| {
            let mut words = rest.split_whitespace();
            let passed = words.next()?.parse().ok()?;
            // "passed," / "failed" - the middle word is the comma.
            words.next()?;
            let failed = words.next()?.parse().ok()?;
            words.next()?;
            Some(Kunit { passed, failed })
        })
}

pub fn verdict(log: &str) -> Verdict<'_> {
    let mut passed: Vec<&str> = Vec::new();
    let mut failed: Vec<&str> = Vec::new();
    let mut setup_failed: Option<&str> = None;
    for line in log.lines() {
        let line = line.trim_end();
        if let Some(name) = line.strip_prefix("=== PASS ") {
            passed.push(name.trim());
        } else if let Some(rest) = line.strip_prefix("=== FAIL ") {
            failed.push(rest.split_whitespace().next().unwrap_or(""));
        } else if setup_failed.is_none() && line.starts_with("=== SETUP_FAIL") {
            setup_failed = Some(line);
        }
    }

    let summary = log
        .lines()
        .rev()
        .find(|line| line.starts_with(SUMMARY_MARKER));

    Verdict {
        passed,
        failed,
        summary,
        setup_failed,
        kunit: kunit(log),
    }
}

/// §10.4's frame-rate readings, out of a sweep log.
///
/// `t-netstats` prints its steady-state line per run in the shape
/// `NETSTAT tx=<n> datagrams_per_sec=<rate> pool=<hits>:<misses>`; this keeps
/// the last word (a wire can wrap or repeat) and hands back every parsed
/// number. The instrument also prints `NETSTAT_FIRST` - the cold first-burst
/// window, kept for anyone who wants that number too - and the prefix check
/// below is anchored at the space so only the steady-state line parses. The
/// rate is *reported, not asserted* - t-pipecost's rule for any throughput
/// figure under TCG - and these readings exist so §10.4's before/after pair
/// lands beside the verdict automatically rather than in whoever-ran-the-gate's
/// notes. An empty answer means the instrument did not run, which the armed
/// report refuses only when the default list staged it.
pub fn netstat_readings(log: &str) -> Vec<(u64, u64, u64, u64)> {
    let mut latest: Option<(u64, u64, u64, u64)> = None;
    for line in log.lines() {
        let Some(rest) = line.trim_end().strip_prefix("NETSTAT ") else {
            continue;
        };
        let words: Vec<&str> = rest.split_whitespace().collect();
        let field = |key: &str| words.iter().find_map(|w| w.strip_prefix(key));
        let parse = |v: Option<&str>| v.and_then(|v| v.parse::<u64>().ok());
        let (Some(tx), Some(rate)) = (parse(field("tx=")), parse(field("datagrams_per_sec=")))
        else {
            continue;
        };
        // The pool pair arrives as one word, `<hits>:<misses>`; a line without
        // it is an older instrument's output and still counts for the rate.
        let (hits, misses) = match field("pool=").and_then(|p| p.split_once(':')) {
            Some((h, m)) => (h.parse().unwrap_or(0), m.parse().unwrap_or(0)),
            None => (0, 0),
        };
        latest = Some((tx, rate, hits, misses));
    }
    latest.into_iter().collect()
}

/// §10.5's disk-counter readings, out of a sweep log.
///
/// `t-diskbench` prints one line per pass in the shape
/// `DISKSTAT <disk> cold=<reads> warm=<reads>`; this keeps the last word for
/// each number (a QEMU wire can wrap or repeat a line, and the final word is
/// what the run ended with). An empty answer means no instrument ran - which is
/// *not* an error here: the caller armed the boot only when the default list
/// staged the program, so an un-armed run legitimately has nothing to read. The
/// armed report refuses silence.
pub fn diskstat_readings(log: &str) -> Vec<(&str, u64, u64)> {
    let mut out: Vec<(&str, u64, u64)> = Vec::new();
    for line in log.lines() {
        let Some(rest) = line.trim_end().strip_prefix("DISKSTAT ") else {
            continue;
        };
        let mut words = rest.split_whitespace();
        let Some(disk) = words.next() else { continue };
        let after_disk: Vec<&str> = words.collect();
        let Some(cold) = after_disk
            .iter()
            .find_map(|w| w.strip_prefix("cold="))
            .and_then(|v| v.parse::<u64>().ok())
        else {
            continue;
        };
        let Some(warm) = after_disk
            .iter()
            .find_map(|w| w.strip_prefix("warm="))
            .and_then(|v| v.parse::<u64>().ok())
        else {
            continue;
        };
        if let Some(existing) = out.iter_mut().find(|(d, _, _)| *d == disk) {
            *existing = (disk, cold, warm);
        } else {
            out.push((disk, cold, warm));
        }
    }
    out
}

/// The boot disk's half of the same instrument: `DISKSTAT <disk> bootcold=N
/// bootblocks=M`, the pass `t-diskbench` makes over the root filesystem's disk
/// when it lays the fixture.
///
/// A second reader rather than a wider one, because the two lines answer
/// different questions about (usually) different disks and their keys mean
/// different things: `cold`/`warm` are the fixture *filesystem*'s passes on the
/// SATA disk, and `bootcold`/`bootblocks` are the calls and the blocks of the
/// pass that copied the fixture *file* off the disk the root lives on. One
/// reader that insisted on both would drop either line. The rules are the other
/// one's: the last word for a disk wins (a wire can wrap it), and a malformed
/// line is dropped rather than ending the report.
pub fn bootdisk_readings(log: &str) -> Vec<(&str, u64, u64)> {
    let mut out: Vec<(&str, u64, u64)> = Vec::new();
    for line in log.lines() {
        let Some(rest) = line.trim_end().strip_prefix("DISKSTAT ") else {
            continue;
        };
        let mut words = rest.split_whitespace();
        let Some(disk) = words.next() else { continue };
        let after_disk: Vec<&str> = words.collect();
        let Some(calls) = after_disk
            .iter()
            .find_map(|w| w.strip_prefix("bootcold="))
            .and_then(|v| v.parse::<u64>().ok())
        else {
            continue;
        };
        let Some(blocks) = after_disk
            .iter()
            .find_map(|w| w.strip_prefix("bootblocks="))
            .and_then(|v| v.parse::<u64>().ok())
        else {
            continue;
        };
        if let Some(existing) = out.iter_mut().find(|(d, _, _)| *d == disk) {
            *existing = (disk, calls, blocks);
        } else {
            out.push((disk, calls, blocks));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Clone, Debug)]
    struct Program(&'static str);

    impl TestProgram for Program {
        fn name(&self) -> &str {
            self.0
        }
    }

    const TOML: &str = r#"
default = []
external = ["t-dns", "t-udp"]
slow = ["t-forkcost"]
aarch64 = ["t-drm"]
"#;

    fn config(name: &str) -> TestConfig {
        // Through the same loader the sweep uses, so the fixture cannot drift
        // from the file the sweep reads. One directory per test, because the
        // suite runs in parallel and a shared path is a race - a test that goes
        // red one run in ten is the kind this repository refuses.
        let dir = std::env::temp_dir().join(format!("mpos-sweep-tests-{name}"));
        std::fs::create_dir_all(dir.join(crate::paths::TESTS_CONFIG).parent().unwrap()).unwrap();
        let path = dir.join(crate::paths::TESTS_CONFIG);
        std::fs::write(&path, TOML).unwrap();
        TestConfig::load(&path).unwrap()
    }

    /// The same loader over a file that also carries the `diagnostic` group -
    /// which the base fixture deliberately does not, so adding it there would
    /// move every other expectation in this module at once.
    fn config_with_diagnostic(name: &str) -> TestConfig {
        const WITH_DIAGNOSTIC: &str = r#"
default = []
external = ["t-dns", "t-udp"]
slow = ["t-forkcost"]
aarch64 = ["t-drm"]
diagnostic = ["t-sh"]
"#;
        let dir = std::env::temp_dir().join(format!("mpos-sweep-tests-{name}"));
        std::fs::create_dir_all(dir.join(crate::paths::TESTS_CONFIG).parent().unwrap()).unwrap();
        let path = dir.join(crate::paths::TESTS_CONFIG);
        std::fs::write(&path, WITH_DIAGNOSTIC).unwrap();
        TestConfig::load(&path).unwrap()
    }

    fn built() -> Vec<Program> {
        vec![
            Program("t-tcp"),
            Program("t-dns"),
            Program("t-udp"),
            Program("t-forkcost"),
            Program("t-drm"),
        ]
    }

    fn built_with_diagnostic() -> Vec<Program> {
        let mut tests = built();
        tests.push(Program("t-sh"));
        tests
    }

    /// A group nobody defined is refused by name rather than shrinking the run:
    /// the sweep must never answer "fewer tests" to a question it did not
    /// understand.
    #[test]
    fn an_undefined_group_is_refused_with_the_ones_that_exist() {
        let built = built();
        let selection = Selection {
            groups: vec!["nosuch".to_string()],
            all: false,
        };
        let error = select(
            Machine::X86_64,
            &config("undefined-group"),
            &built,
            &selection,
        )
        .expect_err("an unknown group must refuse the run")
        .to_string();
        assert!(error.contains("nosuch"), "{error}");
        assert!(error.contains("external"), "{error}");
    }

    /// `select` chooses from what the arm built and does *not* refuse a payload
    /// that carries only part of the file. The declared-name check lives in the
    /// arm because its scope is the arm's: the first machine builds both suites
    /// and every name in the file, the second builds only the std suite - so a
    /// shared scope would either refuse every aarch64 sweep or check nothing.
    /// Measured: with the check in here, the aarch64 sweep stopped on
    /// `t-sh, t-dns, t-rawicmp, t-tcp-close, t-fill-ramfs, t-forkcost, t-netlat,
    /// t-pipecost`, which are the bare-metal suite's membership and are not on
    /// that machine at all.
    #[test]
    fn a_payload_that_carries_only_part_of_the_file_still_selects() {
        let config = config("partial-payload");
        let built = vec![Program("t-tcp")];
        let plan = select(Machine::Aarch64, &config, &built, &Selection::default()).unwrap();
        assert_eq!(
            plan.staged.iter().map(|p| p.name()).collect::<Vec<_>>(),
            vec!["t-tcp"]
        );
        assert!(plan.skipped.is_empty());
    }

    /// `--groups` *replaces* the default list rather than adding to it, and a
    /// machine's own group is invisible to the other machine's sweep - which is
    /// the whole reason a gap is declared per machine instead of being deleted
    /// from the file.
    #[test]
    fn a_group_selects_its_own_programs_and_a_machines_group_is_its_own() {
        let config = config("group-selection");
        let built = built();

        let x86 = select(Machine::X86_64, &config, &built, &Selection::default()).unwrap();
        let staged: Vec<&str> = x86.staged.iter().map(|p| p.name()).collect();
        assert_eq!(
            staged,
            vec!["t-tcp", "t-drm"],
            "the ungrouped programs - and `t-drm` runs here even though the \
             other machine's group names it"
        );
        assert_eq!(x86.skipped.len(), 3);

        let slow = Selection {
            groups: vec!["slow".to_string()],
            all: false,
        };
        let slow = select(Machine::X86_64, &config, &built, &slow).unwrap();
        assert_eq!(
            slow.staged.iter().map(|p| p.name()).collect::<Vec<_>>(),
            vec!["t-tcp", "t-forkcost", "t-drm"],
            "`--groups` replaces the file's `default` list, not the ungrouped \
             tests: the named group's programs run *beside* the ones in no group"
        );

        let aarch64 = select(Machine::Aarch64, &config, &built, &Selection::default()).unwrap();
        assert_eq!(
            aarch64.staged.iter().map(|p| p.name()).collect::<Vec<_>>(),
            vec!["t-tcp", "t-dns", "t-udp", "t-forkcost"],
            "this machine's own group is one of its groups, so `t-drm` is left \
             out - and the *reason* groups are x86_64's vocabulary: they name \
             that harness's needs, while this arm stages every program it built \
             (§13.5), so `external` and `slow` leave nothing out here"
        );
        assert_eq!(aarch64.skipped.len(), 1);
    }

    /// The rule §13.5 wrote for this arm, measured on the day the std side first
    /// carried the instruments: moving `t-forkcost`, `t-netlat` and `t-pipecost`
    /// across made them *build* on aarch64, and the shared `runs()` still read
    /// their `slow` membership as an exclusion there - so three programs sat in
    /// the payload built, selected out, and run by neither arm's default sweep.
    /// A reason-group's membership is a statement about the x86_64 harness; the
    /// second machine's contract is its whole payload minus its own gap list.
    #[test]
    fn a_reason_group_never_leaves_a_built_program_out_of_the_second_machine() {
        let config = config("reason-groups-do-not-bind-aarch64");
        let built = built();
        let plan = select(Machine::Aarch64, &config, &built, &Selection::default()).unwrap();
        let staged: Vec<&str> = plan.staged.iter().map(|p| p.name()).collect();
        for program in ["t-dns", "t-udp", "t-forkcost"] {
            assert!(
                staged.contains(&program),
                "{program} is built for this machine but a reason-group hid it \
                 from the sweep"
            );
        }
        // And the first machine keeps its gate exactly as before: the same
        // programs are the ones a plain sweep leaves out there.
        let x86 = select(Machine::X86_64, &config, &built, &Selection::default()).unwrap();
        let left_out: Vec<&str> = x86.skipped.iter().map(|p| p.name()).collect();
        assert_eq!(left_out, vec!["t-dns", "t-udp", "t-forkcost"]);
    }

    /// `diagnostic` is not a harness word but a kind: what it names is not a
    /// check at all (`t-sh` execs an interactive shell onto the console to be
    /// read by hand), so the §13.5 contract - run everything else - does not
    /// cover it, and leaving it out on both arms' *default* sweeps is the
    /// honest reading of "everything that is a check of this machine". The
    /// second arm still has no way to ask for a reason-group by name: `main`
    /// refuses `--groups` there before it gets here, because its payload's
    /// selection vocabulary is the first machine's suites' membership.
    /// Measured on batch 43's day: with the reason-blind branch alone, the
    /// second machine ran `t-sh` in its default sweep while the first left it
    /// out of the very same one.
    #[test]
    fn a_diagnostic_is_left_out_on_both_machines_default_sweeps() {
        let config = config_with_diagnostic("diagnostic-on-both");
        let built = built_with_diagnostic();

        let aarch64 = select(Machine::Aarch64, &config, &built, &Selection::default()).unwrap();
        let staged: Vec<&str> = aarch64.staged.iter().map(|p| p.name()).collect();
        assert!(
            !staged.contains(&"t-sh"),
            "a diagnostic belongs to no machine's default sweep"
        );
        assert!(
            aarch64.skipped.iter().any(|p| p.name() == "t-sh"),
            "and it is reported as left out, never silently absent"
        );

        // Every *other* reason-group still binds neither arm the way §13.5 says.
        for program in ["t-dns", "t-forkcost"] {
            assert!(
                staged.contains(&program),
                "{program} is built for this machine and only `diagnostic` \
                 may leave a built program out here"
            );
        }

        let x86 = select(Machine::X86_64, &config, &built, &Selection::default()).unwrap();
        assert!(
            x86.skipped.iter().any(|p| p.name() == "t-sh"),
            "the first machine's selection is unchanged: a plain sweep \
             still leaves the diagnostic out"
        );
    }

    /// The parser reads what both runners print, and beats the two cheaper
    /// readings of the same log: a program that failed is not a program that
    /// passed, and `exit=N` is not part of its name.
    #[test]
    fn the_verdict_names_every_program_and_keeps_the_last_summary() {
        let log = "\
=== RUN t-a
=== PASS t-a
=== RUN t-b
=== FAIL t-b exit=7
=== SUMMARY pass=1 fail=1
=== SUMMARY pass=1 fail=1
";
        let verdict = verdict(log);
        assert_eq!(verdict.passed, vec!["t-a"]);
        assert_eq!(verdict.failed, vec!["t-b"]);
        assert_eq!(verdict.summary, Some("=== SUMMARY pass=1 fail=1"));
        assert!(verdict.setup_failed.is_none());
    }

    /// A run whose setup failed is refused before anything it printed is read as
    /// a verdict, which is the one line that is not about the programs.
    #[test]
    fn a_failed_setup_is_reported_as_such() {
        let verdict = verdict("=== SETUP_FAIL /tmp could not be made\n=== END\n");
        assert_eq!(
            verdict.setup_failed,
            Some("=== SETUP_FAIL /tmp could not be made")
        );
    }

    /// A log that stops early has no summary, which is how a run that never
    /// reached its last program reads.
    #[test]
    fn a_log_without_a_summary_is_a_run_that_did_not_end() {
        let verdict = verdict("=== RUN t-a\n=== PASS t-a\n");
        assert_eq!(verdict.passed, vec!["t-a"]);
        assert!(verdict.summary.is_none());
    }

    /// §10.5's reader: the instrument's line parses out of a log that carries
    /// everything else a sweep prints, the last word for a disk wins (a wire
    /// can wrap), and a malformed line is dropped rather than panicking the
    /// report - the harness must not die on one stray byte of a serial stream.
    #[test]
    fn the_diskstat_reader_keeps_the_last_word_per_disk() {
        let log = "\
=== RUN t-diskbench
DISKSTAT sda cold=16384 warm=0
some other noise cold=999999
=== PASS t-diskbench
DISKSTAT sda cold=16384 warm=7
DISKSTAT vda cold=2 warm=0
=== SUMMARY pass=2 fail=0
";
        let readings = diskstat_readings(log);
        assert_eq!(readings.len(), 2);
        assert_eq!(readings[0], ("sda", 16384, 7));
        assert_eq!(readings[1], ("vda", 2, 0));
        assert!(diskstat_readings("no instrument here").is_empty());
    }

    /// The boot-disk half of the same reader: its own keys, the same
    /// last-word-per-disk rule, and the same refusal to die on a stray byte. The
    /// two shapes must not answer for each other - a reader that accepted either
    /// line would report the fixture filesystem's numbers as the boot disk's.
    #[test]
    fn the_boot_disk_reader_keeps_the_last_word_per_disk() {
        let log = "\
=== RUN t-diskbench
DISKSTAT sda cold=16384 warm=0
DISKSTAT vda bootcold=1344 bootblocks=21504
=== PASS t-diskbench
DISKSTAT vda bootcold=1350 bootblocks=21504
=== SUMMARY pass=1 fail=0
";
        assert_eq!(bootdisk_readings(log), vec![("vda", 1350, 21504)]);
        assert!(bootdisk_readings("DISKSTAT sda cold=1 warm=0").is_empty());
        assert!(bootdisk_readings("no instrument here").is_empty());
    }

    /// §10.4's reader: the instrument's line parses out of a log that carries
    /// everything else a sweep prints, the last word wins, and a malformed or
    /// older-shape line is dropped rather than panicking the report - the same
    /// rule `diskstat_readings` follows, because one stray byte of a serial
    /// stream must never end a gate run.
    #[test]
    fn the_netstat_reader_keeps_the_last_word() {
        let log = "\
=== RUN t-netstats
NETSTAT_FIRST tx=256 datagrams_per_sec=7000
NETSTAT tx=512 datagrams_per_sec=8123 pool=0:256
=== PASS t-netstats
NETSTAT tx=512 datagrams_per_sec=9000 pool=41:3
=== SUMMARY pass=1 fail=0
";
        let readings = netstat_readings(log);
        assert_eq!(readings.len(), 1);
        assert_eq!(readings[0], (512, 9000, 41, 3));
        assert!(netstat_readings("no instrument here").is_empty());
    }

    /// The arming question is `tests.toml`'s, not the machine's: a name in no
    /// group is in both machines' default sweep, and a machine-named gap is not
    /// in *that* machine's. This is what keeps the disk-counter boot word and
    /// the staged instrument from ever disagreeing.
    #[test]
    fn in_default_follows_the_group_membership_the_selection_uses() {
        let config = config("in-default");
        // `t-dns` is in `external`, which a plain run does not add: not default.
        assert!(!in_default(Machine::X86_64, &config, "t-dns"));
        // `t-drm` is aarch64's own gap: out of *its* default, in x86_64's.
        assert!(!in_default(Machine::Aarch64, &config, "t-drm"));
        assert!(in_default(Machine::X86_64, &config, "t-drm"));
        // A name in no group at all is in both.
        assert!(in_default(Machine::X86_64, &config, "t-anything-else"));
        assert!(in_default(Machine::Aarch64, &config, "t-anything-else"));
    }
}
