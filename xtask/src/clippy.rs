//! Lint counts, measured and ratcheted.
//!
//! `AGENTS.md` §2 lists `cargo clippy` clean among the commands a change runs,
//! and until 2026-11-13 **nothing ran it**: `grep -rn clippy xtask/src/*.rs`
//! answered nothing, so the convention was a sentence with no instrument behind
//! it. It was also red - `cargo clippy --target x86_64-unknown-none` exited
//! 101 on `clippy::never_loop` in `src/net/socket.rs`, in a `loop` whose body
//! always returned, wrapt around a wait by a fix that only meant to bound it.
//! The same shape as the two findings that preceded it: the `fuzz/` workspace no
//! gate built (`docs/notes/` §1.4) and a `docs/uapi` inventory no gate compared.
//!
//! Why a ratchet rather than a clean baseline: the tree carries **162**
//! diagnostics on `x86_64-unknown-none` and **168** on `aarch64-unknown-none`
//! (plus 273 in the user suite, 389 in the host `--lib` half - 336 of those
//! `dead_code`, because on the host a `target_os = "none"` caller does not
//! exist), and `cargo clippy --fix` was measured to leave the tree byte for
//! byte identical (9080 ms, `--allow-dirty --allow-staged`). Removing them is
//! a round of work with its own measurements; making sure the *next* one never
//! arrives is this file. So each arm's diagnostics are counted per lint and
//! recorded here: a new lint fails immediately, a count that grows fails
//! immediately, and a count that falls prints the line telling its finder to
//! lower the constant - which is what makes each improvement a commit somebody
//! can read and a regression a failing gate.
//!
//! The parsing is deliberately textual and crude - no `serde_json` in this
//! crate's dependencies, and inventing one for a lint count is the wrong trade.
//! A line is a diagnostic when it carries `"reason":"compiler-message"`; the
//! level and the lint code are then read out of that line's fixed field order.
//! It was validated against a real JSON parse over all six captured arms
//! (2026-11-13): zero mismatches, zero false positives, and never more than one
//! diagnostic per line.

use std::collections::BTreeMap;
use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result, bail};

/// One measured arm: a package, a target it is built for, and what the tree
/// answered the last time somebody recorded it.
pub struct Arm {
    /// What the gate prints, package first: `mpos (x86_64-unknown-none)`.
    pub name: &'static str,
    /// The cargo arguments, without the `clippy` subcommand or
    /// `--message-format=json`, which [`measure`] appends for every arm.
    pub args: &'static [&'static str],
    /// The recorded count per lint: `(lint, diagnostics)`, sorted by lint. A
    /// lint absent from this list has never been seen in this arm, and seeing
    /// one now fails (rather than warning) because that is the whole point of a
    /// ratchet written after the fact.
    pub baseline: &'static [(&'static str, usize)],
}

impl Arm {
    /// The recorded total, which the gate prints beside the measured one.
    pub fn recorded(&self) -> usize {
        self.baseline.iter().map(|(_, count)| count).sum()
    }
}

/// The arms, one entry per thing that is compiled.
///
/// The four `mpos_*` arms are the same kernel crate under the cfg sets the
/// machines and the host actually build it with - a lint can be reachable on
/// one and not another (measured: `dead_code` is 21 on x86_64 and 30 on
/// aarch64, and `src/syscall/linux/abi.rs` alone answers 252 on the host), so
/// each is recorded separately rather than once for "the kernel".
///
/// `--release` for the kernel arms is what the gate already builds, and the
/// counts are identical in the other profile (measured 2026-11-13: 162/162 and
/// 168/168), so this is the profile whose artefacts the gate shares.
pub const ARMS: &[Arm] = &[
    Arm {
        name: "mpos (x86_64-unknown-none)",
        args: &["--release", "--target", "x86_64-unknown-none"],
        baseline: &[
            ("clippy::arc_with_non_send_sync", 1),
            ("clippy::chunks_exact_to_as_chunks", 3),
            ("clippy::clone_on_copy", 1),
            ("clippy::collapsible_if", 13),
            ("clippy::doc_lazy_continuation", 36),
            ("clippy::empty_line_after_doc_comments", 4),
            ("clippy::empty_line_after_outer_attr", 2),
            ("clippy::let_and_return", 2),
            ("clippy::manual_div_ceil", 4),
            ("clippy::manual_is_multiple_of", 1),
            ("clippy::manual_isolate_lowest_one", 1),
            ("clippy::manual_range_contains", 2),
            ("clippy::manual_saturating_arithmetic", 1),
            ("clippy::match_single_binding", 1),
            ("clippy::missing_safety_doc", 5),
            ("clippy::needless_borrow", 3),
            ("clippy::needless_range_loop", 4),
            ("clippy::needless_return", 1),
            ("clippy::new_ret_no_self", 2),
            ("clippy::new_without_default", 2),
            ("clippy::question_mark", 3),
            ("clippy::result_unit_err", 1),
            ("clippy::single_match", 3),
            ("clippy::slow_vector_initialization", 2),
            ("clippy::tabs_in_doc_comments", 4),
            ("clippy::too_many_arguments", 8),
            ("clippy::type_complexity", 9),
            ("clippy::unnecessary_cast", 4),
            ("clippy::unnecessary_sort_by", 1),
            ("clippy::unnecessary_unwrap", 1),
            ("clippy::useless_format", 2),
            ("dead_code", 21),
            ("dropping_references", 1),
            ("private_interfaces", 4),
            ("unused_assignments", 1),
            ("unused_imports", 2),
            ("unused_mut", 3),
            ("unused_parens", 1),
            ("unused_variables", 2),
        ],
    },
    Arm {
        name: "mpos (aarch64-unknown-none)",
        args: &["--release", "--target", "aarch64-unknown-none"],
        baseline: &[
            ("clippy::arc_with_non_send_sync", 1),
            ("clippy::chunks_exact_to_as_chunks", 3),
            ("clippy::clone_on_copy", 1),
            ("clippy::collapsible_if", 13),
            ("clippy::doc_lazy_continuation", 36),
            ("clippy::empty_line_after_doc_comments", 4),
            ("clippy::empty_line_after_outer_attr", 2),
            ("clippy::let_and_return", 2),
            ("clippy::manual_div_ceil", 5),
            ("clippy::manual_is_multiple_of", 1),
            ("clippy::manual_isolate_lowest_one", 1),
            ("clippy::manual_range_contains", 2),
            ("clippy::manual_saturating_arithmetic", 1),
            ("clippy::match_single_binding", 1),
            ("clippy::missing_safety_doc", 1),
            ("clippy::needless_borrow", 3),
            ("clippy::needless_range_loop", 4),
            ("clippy::needless_return", 1),
            ("clippy::new_ret_no_self", 2),
            ("clippy::new_without_default", 2),
            ("clippy::question_mark", 3),
            ("clippy::result_unit_err", 1),
            ("clippy::single_match", 3),
            ("clippy::slow_vector_initialization", 2),
            ("clippy::tabs_in_doc_comments", 4),
            ("clippy::too_many_arguments", 8),
            ("clippy::type_complexity", 9),
            ("clippy::unnecessary_cast", 4),
            ("clippy::unnecessary_sort_by", 1),
            ("clippy::unnecessary_unwrap", 1),
            ("clippy::useless_format", 2),
            ("dead_code", 30),
            ("dropping_references", 1),
            ("private_interfaces", 4),
            ("unused_assignments", 1),
            ("unused_imports", 2),
            ("unused_mut", 3),
            ("unused_parens", 1),
            ("unused_variables", 2),
        ],
    },
    Arm {
        // The host half of the same crate. It needs `--lib` rather than a plain
        // `--target` invocation: a bare host `cargo clippy` also builds
        // `src/main.rs` and `src/board/`, which are `target_os = "none"`-only
        // and fail `E0432: unresolved import mpos::boot::handoff` (measured).
        // Its 336 `dead_code` diagnostics are the host's own shape - a caller
        // behind `target_os = "none"` does not exist here - not a debt.
        name: "mpos --lib (x86_64-unknown-linux-gnu)",
        args: &[
            "-p",
            "mpos",
            "--lib",
            "--target",
            "x86_64-unknown-linux-gnu",
        ],
        baseline: &[
            ("clippy::chunks_exact_to_as_chunks", 3),
            ("clippy::collapsible_if", 2),
            ("clippy::doc_lazy_continuation", 13),
            ("clippy::let_and_return", 1),
            ("clippy::manual_div_ceil", 1),
            ("clippy::manual_is_multiple_of", 1),
            ("clippy::manual_isolate_lowest_one", 1),
            ("clippy::manual_range_contains", 2),
            ("clippy::match_single_binding", 1),
            ("clippy::needless_borrow", 2),
            ("clippy::needless_range_loop", 1),
            ("clippy::needless_return", 1),
            ("clippy::new_ret_no_self", 1),
            ("clippy::new_without_default", 2),
            ("clippy::result_unit_err", 1),
            ("clippy::tabs_in_doc_comments", 4),
            ("clippy::too_many_arguments", 3),
            ("clippy::type_complexity", 3),
            ("clippy::unnecessary_cast", 1),
            ("clippy::unnecessary_unwrap", 1),
            ("dead_code", 336),
            ("unused_imports", 1),
            ("unused_must_use", 1),
            ("unused_mut", 1),
            ("unused_parens", 1),
            ("unused_variables", 4),
        ],
    },
    Arm {
        // The ABI suite, which is the arm that carried the one *error*: a
        // `fn push(&self) -> &mut Sqe` handed a caller a mutable borrow of
        // memory it only held shared, in `t-io-uring`'s hand-built ring. Fixed
        // by making the ring `&mut` where it mutates, which is what the claim
        // was always doing.
        name: "mpos-user-tests (x86_64-unknown-linux-musl)",
        args: &[
            "--release",
            "-p",
            "mpos-user-tests",
            "--target",
            "x86_64-unknown-linux-musl",
        ],
        baseline: &[
            ("E0133", 2),
            ("clippy::byte_char_slices", 3),
            ("clippy::chunks_exact_to_as_chunks", 1),
            ("clippy::collapsible_if", 5),
            ("clippy::doc_lazy_continuation", 45),
            ("clippy::empty_loop", 5),
            ("clippy::identity_op", 2),
            ("clippy::if_same_then_else", 1),
            ("clippy::int_plus_one", 1),
            ("clippy::iter_nth_zero", 1),
            ("clippy::manual_c_str_literals", 10),
            ("clippy::manual_dangling_ptr", 20),
            ("clippy::manual_is_multiple_of", 1),
            ("clippy::manual_ok_err", 3),
            ("clippy::manual_range_contains", 8),
            ("clippy::manual_unwrap_or", 1),
            ("clippy::manual_unwrap_or_default", 1),
            ("clippy::map_clone", 1),
            ("clippy::needless_borrows_for_generic_args", 9),
            ("clippy::needless_range_loop", 3),
            ("clippy::ptr_eq", 4),
            ("clippy::redundant_closure", 1),
            ("clippy::redundant_pattern_matching", 2),
            ("clippy::single_match", 1),
            ("clippy::unnecessary_cast", 61),
            ("clippy::unnecessary_lazy_evaluations", 1),
            ("clippy::unnecessary_map_or", 7),
            ("clippy::unnecessary_mut_passed", 3),
            ("clippy::unusual_byte_groupings", 1),
            ("clippy::useless_conversion", 3),
            ("clippy::useless_format", 1),
            ("clippy::useless_vec", 1),
            ("clippy::while_let_loop", 1),
            ("clippy::zero_ptr", 5),
            ("dead_code", 9),
            ("deprecated", 1),
            ("function_casts_as_integer", 5),
            ("unreachable_code", 13),
            ("unused_assignments", 3),
            ("unused_imports", 4),
            ("unused_must_use", 1),
            ("unused_mut", 6),
            ("unused_unsafe", 14),
            ("unused_variables", 2),
        ],
    },
    Arm {
        name: "mpos-xtask (x86_64-unknown-linux-gnu)",
        args: &["-p", "mpos-xtask", "--target", "x86_64-unknown-linux-gnu"],
        baseline: &[
            ("clippy::too_many_arguments", 2),
            ("clippy::unnecessary_map_or", 1),
        ],
    },
    Arm {
        // The loader has its own manifest and its own target: it is a member of
        // the root workspace, but the `-unknown-uefi` triple only exists in
        // `boot/.cargo/config.toml`, so the arm runs from that manifest.
        name: "mpos-boot (aarch64-unknown-uefi)",
        args: &[
            "--manifest-path",
            "boot/Cargo.toml",
            "--target",
            "aarch64-unknown-uefi",
        ],
        baseline: &[("clippy::too_many_arguments", 1)],
    },
];

/// The lints cargo reports without a code, under the name the report gives
/// them. Not used for filtering - it exists so the arithmetic in the
/// documentation above has a constant to point at.
pub const UNCODED: &str = "(uncoded)";

/// One diagnostic line, reduced to what the ratchet counts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnostic {
    /// `warning` or `error`; `help`/`note`/`help: …` children are dropped.
    pub level: String,
    /// `clippy::never_loop`, `dead_code`, an `E••••` rustc code, or
    /// [`UNCODED`] when the diagnostic carries no code at all.
    pub lint: String,
}

/// Every `warning`/`error` diagnostic in one arm's `--message-format=json`
/// stream, in the order cargo emitted them.
///
/// The scanner is crude on purpose (see the module header): a line is a
/// diagnostic iff it holds `"reason":"compiler-message"`, and then the level is
/// the value after the **last** `"level":"` on the line while the lint is the
/// value after the **first** `"code":{"code":"`. Last for the level because a
/// diagnostic's `children` and any nested diagnostic repeat the key, and the
/// outermost one is written last in the object cargo serialises; first for the
/// code because the outermost diagnostic is written first. Both orders were
/// checked against `serde_json` over 1,000 diagnostics from six arms, which is
/// the whole of what this parser may ever be asked.
pub fn parse_diagnostics(text: &str) -> Vec<Diagnostic> {
    let mut out = Vec::new();
    for line in text.lines() {
        if !line.contains("\"reason\":\"compiler-message\"") {
            continue;
        }
        // The level: what the last `"level":"` names.
        let Some(at) = line.rfind("\"level\":\"") else {
            continue;
        };
        let rest = &line[at + "\"level\":\"".len()..];
        let Some(end) = rest.find('"') else {
            continue;
        };
        let level = &rest[..end];
        if level != "warning" && level != "error" {
            continue;
        }
        // The lint: what the first `"code":{"code":"` names, or `(uncoded)`.
        let lint = match line.find("\"code\":{\"code\":\"") {
            Some(at) => {
                let rest = &line[at + "\"code\":{\"code\":\"".len()..];
                match rest.find('"') {
                    Some(end) => rest[..end].to_string(),
                    None => UNCODED.to_string(),
                }
            }
            None => UNCODED.to_string(),
        };
        out.push(Diagnostic {
            level: level.to_string(),
            lint,
        });
    }
    out
}

/// Counts one arm's diagnostics per lint, sorted by lint name.
pub fn count_per_lint(diagnostics: &[Diagnostic]) -> BTreeMap<String, usize> {
    let mut counts = BTreeMap::new();
    for diagnostic in diagnostics {
        *counts.entry(diagnostic.lint.clone()).or_insert(0) += 1;
    }
    counts
}

/// One arm's measurement.
pub struct Measurement {
    /// A reference to the arm that produced it.
    pub arm: usize,
    /// The counts, per lint.
    pub counts: BTreeMap<String, usize>,
    /// Every `error`-level diagnostic's lint, in emission order - an error is
    /// not a ratchet question at all, so the gate refuses on sight.
    pub errors: Vec<String>,
}

impl Measurement {
    /// The measured total.
    pub fn measured(&self) -> usize {
        self.counts.values().sum()
    }
}

/// Counts `cargo clippy`'s output for one arm, without spawning anything.
pub fn measure_text(arm: &Arm, text: &str) -> Measurement {
    let diagnostics = parse_diagnostics(text);
    let errors = diagnostics
        .iter()
        .filter(|diagnostic| diagnostic.level == "error")
        .map(|diagnostic| diagnostic.lint.clone())
        .collect();
    Measurement {
        arm: ARMS
            .iter()
            .position(|candidate| std::ptr::eq(candidate, arm))
            .unwrap_or(0),
        counts: count_per_lint(&diagnostics),
        errors,
    }
}

/// Runs one arm and counts what it answered.
///
/// `--message-format=json` on stdout, and *only* stdout is parsed: cargo's own
/// `warning: …` summary lines are stderr in this format, so a stray one cannot
/// be mistaken for a diagnostic.
fn run_arm(root: &Path, arm: &Arm) -> Result<String> {
    let mut cmd = Command::new("cargo");
    cmd.arg("clippy")
        .args(arm.args)
        .arg("--message-format=json");
    cmd.current_dir(root);
    let output = cmd
        .output()
        .with_context(|| format!("Failed to run cargo clippy for {}", arm.name))?;
    let text = String::from_utf8_lossy(&output.stdout).into_owned();
    if !output.status.success() {
        // A crate that does not compile is not a lint count: say so with the
        // compiler's own words rather than as a ratchet violation.
        let stderr = String::from_utf8_lossy(&output.stderr);
        let tail: Vec<&str> = stderr.lines().rev().take(40).collect();
        let tail: Vec<&str> = tail.into_iter().rev().collect();
        bail!(
            "cargo clippy for {} failed to build:\n{}",
            arm.name,
            tail.join("\n")
        );
    }
    Ok(text)
}

/// Measures every arm.
pub fn measure(root: &Path) -> Result<Vec<Measurement>> {
    let mut measured = Vec::new();
    for (index, arm) in ARMS.iter().enumerate() {
        let text = run_arm(root, arm)?;
        let mut measurement = measure_text(arm, &text);
        measurement.arm = index;
        measured.push(measurement);
    }
    Ok(measured)
}

/// What the ratchet says about the measurements: the violations first, then the
/// counts that *fell*, which are not failures but are the finding a reader
/// needs to lower a constant (a constant left high is a ratchet that can no
/// longer catch a regression back up to it).
pub fn verdicts(measured: &[Measurement]) -> (Vec<String>, Vec<String>) {
    let mut violations = Vec::new();
    let mut improvements = Vec::new();

    for measurement in measured {
        let arm = &ARMS[measurement.arm];

        for lint in &measurement.errors {
            // Not a count: a denial is a build the tree must not have. The
            // arm that produced one is named so the fix has somewhere to go.
            violations.push(format!(
                "{}: {lint} is an ERROR, not a warning - fix it",
                arm.name
            ));
        }

        for (lint, measured_count) in &measurement.counts {
            match arm
                .baseline
                .iter()
                .find(|(recorded, _)| recorded == lint)
                .map(|(_, count)| *count)
            {
                Some(recorded) => {
                    if *measured_count > recorded {
                        violations.push(format!(
                            "{}: {lint} is at {measured_count}, up from the recorded {recorded}",
                            arm.name
                        ));
                    } else if *measured_count < recorded {
                        improvements.push(format!(
                            "{}: {lint} fell from {recorded} to {measured_count} - lower the constant",
                            arm.name
                        ));
                    }
                }
                None => violations.push(format!(
                    "{}: {lint} is at {measured_count}, and the tree has never had that lint - the ratchet opens at the recorded counts and a new lint fails immediately",
                    arm.name
                )),
            }
        }

        for (lint, recorded) in arm.baseline {
            if !measurement.counts.contains_key(*lint) {
                improvements.push(format!(
                    "{}: {lint} is gone (was {recorded}) - remove the row",
                    arm.name
                ));
            }
        }
    }

    (violations, improvements)
}

/// Measures every arm, prints the table, and fails the command when a rule has
/// been broken. The `cargo xtask clippy` answer.
pub fn run(root: &Path) -> Result<()> {
    let measured = measure(root)?;

    println!("Lint counts (cargo clippy, per arm, recorded 2026-11-13)");
    for measurement in &measured {
        let arm = &ARMS[measurement.arm];
        println!(
            "  {:44} {:4}/{:<4} ({} lint(s))",
            arm.name,
            measurement.measured(),
            arm.recorded(),
            measurement.counts.len()
        );
    }

    let (violations, improvements) = verdicts(&measured);
    if !improvements.is_empty() {
        println!();
        println!("Improved since the baseline:");
        for improvement in &improvements {
            println!("  {improvement}");
        }
    }
    if violations.is_empty() {
        println!();
        println!("Within the ratchet: no lint has appeared and no count has grown.");
        return Ok(());
    }
    println!();
    for violation in &violations {
        eprintln!("clippy: {violation}");
    }
    bail!(
        "{} lint count(s) got worse. Fix the diagnostic - that is the cheap \
         half - or, if the lint is wrong about this code and the reason is \
         worth a reader's time, allow it at the site with the reason on the \
         same line, which is the convention the 23 existing allows follow. A \
         grown-but-argued count lowers the baseline table in \
         `xtask/src/clippy.rs` in the same commit, with the measurement.",
        violations.len()
    )
}

/// The gate's step: measure, print one summary line per arm, and refuse any
/// violation.
///
/// Separate from [`run`] so the gate can keep its own output shape (one line
/// per step in stage 1's captured text) while `cargo xtask clippy` prints the
/// full table for a person.
pub fn gate(root: &Path) -> Result<String> {
    let measured = measure(root)?;
    let mut text = String::from("Lint: every crate, ratcheted against the recorded counts\n");
    for measurement in &measured {
        let arm = &ARMS[measurement.arm];
        text.push_str(&format!(
            "  {:44} {:4}/{:<4} recorded\n",
            arm.name,
            measurement.measured(),
            arm.recorded()
        ));
    }
    let (violations, improvements) = verdicts(&measured);
    for improvement in &improvements {
        text.push_str(&format!("  improved: {improvement}\n"));
    }
    if !violations.is_empty() {
        for violation in &violations {
            text.push_str(&format!("  clippy: {violation}\n"));
        }
        bail!(
            "the clippy ratchet failed ({} finding(s), above): fix the \
             diagnostic, allow it at the site with the reason, or lower the \
             baseline in `xtask/src/clippy.rs` with the measurement",
            violations.len()
        );
    }
    Ok(text)
}

/// `cargo fmt --check` for every workspace in the tree.
///
/// The root workspace covers the kernel, `arch/`, `boot/`, `xtask/`, `user/`
/// and `crates/*` - `boot` is a member, so its own manifest does not need a
/// second invocation. `fuzz/` is a **separate** workspace
/// (`fuzz/Cargo.toml`'s `[workspace]`), which is exactly why eleven unformatted
/// files sat in it for a month: `cargo fmt --check` at the root never read
/// them (`docs/notes/` §1.4). Both are asked, and the second one is the whole
/// reason this function is not a single command.
pub fn check_formatting(root: &Path) -> Result<String> {
    for (manifest, what) in [
        (None, "the root workspace"),
        (Some("fuzz/Cargo.toml"), "the fuzz workspace"),
    ] {
        let mut cmd = Command::new("cargo");
        cmd.arg("fmt");
        if let Some(manifest) = manifest {
            cmd.arg("--manifest-path").arg(manifest);
        }
        cmd.args(["--all", "--check"]).current_dir(root);
        let output = cmd
            .output()
            .with_context(|| format!("Failed to run cargo fmt --check for {what}"))?;
        if output.status.success() {
            continue;
        }
        // rustfmt prints its own diff on stderr and nothing on stdout, so both
        // are shown: the diff is what names the files, and a reader who only
        // got "formatting failed" would have to run the same command again.
        let mut text = String::new();
        text.push_str(&String::from_utf8_lossy(&output.stdout));
        text.push_str(&String::from_utf8_lossy(&output.stderr));
        let tail: Vec<&str> = text.lines().rev().take(40).collect();
        let tail: Vec<&str> = tail.into_iter().rev().collect();
        bail!(
            "cargo fmt --check for {what} failed:\n{}\n  \
             A formatting difference is `cargo fmt`'s own answer, so running it in \
             the same tree is the fix - this check exists because nothing ran it and \
             the two workspaces drifted apart (`docs/notes/` §1.4).",
            tail.join("\n")
        );
    }
    Ok("Formatting: both workspaces are rustfmt's own shape\n".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One line of cargo's JSON, the shape `--message-format=json` writes: the
    /// outer diagnostic carries `children`, each with its own `level` and a
    /// null `code`, and the outer `code` arrives first while its `level`
    /// arrives last.
    fn line(level: &str, code: Option<&str>) -> String {
        let code = match code {
            Some(code) => format!("\"code\":{{\"code\":\"{code}\",\"explanation\":null}}"),
            None => "\"code\":null".to_string(),
        };
        format!(
            "{{\"reason\":\"compiler-message\",\"package_id\":\"mpos 0.1.0\",\
             \"manifest_path\":\"/r/Cargo.toml\",\"target\":{{\"kind\":[\"lib\"]}},\
             \"message\":{{\"rendered\":\"{level}: something\\n\",\
             \"$message_type\":\"diagnostic\",\
             \"children\":[{{\"children\":[],\"code\":null,\"level\":\"help\",\
             \"message\":\"try this\",\"rendered\":null,\"spans\":[]}}],\
             \"level\":\"{level}\",\"message\":\"something\",\"spans\":[],{code}}}}}"
        )
    }

    #[test]
    fn a_warning_line_yields_its_lint() {
        let diagnostics = parse_diagnostics(&line("warning", Some("clippy::never_loop")));
        assert_eq!(
            diagnostics,
            vec![Diagnostic {
                level: "warning".to_string(),
                lint: "clippy::never_loop".to_string(),
            }]
        );
    }

    #[test]
    fn a_child_note_on_the_same_line_is_not_a_second_diagnostic() {
        // The `help` child repeats `"level":"help"` earlier in the line; the
        // level is read from the *last* occurrence, so the child cannot be
        // counted as a diagnostic of its own.
        let text = line("warning", Some("unused_mut"));
        assert_eq!(parse_diagnostics(&text).len(), 1);
    }

    #[test]
    fn a_line_with_no_reason_is_ignored() {
        // `{"reason":"build-finished"}` and the compiler-artifact lines share
        // every field but the reason, which is what the scanner matches on.
        let text = "\
            {\"reason\":\"compiler-artifact\",\"manifest_path\":\"/r/Cargo.toml\"}\n\
            {\"reason\":\"build-finished\",\"success\":true}\n\
            warning: this is a plain stderr line, not JSON\n";
        assert!(parse_diagnostics(text).is_empty());
    }

    #[test]
    fn the_level_decides_and_an_error_is_kept_apart() {
        let text = format!(
            "{}\n{}\n",
            line("warning", Some("dead_code")),
            line("error", Some("clippy::mut_from_ref"))
        );
        let measurement = measure_text(&ARMS[0], &text);
        assert_eq!(measurement.measured(), 2);
        assert_eq!(measurement.errors, vec!["clippy::mut_from_ref".to_string()]);
    }

    #[test]
    fn an_uncoded_warning_is_counted_under_its_own_name() {
        let text = line("warning", None);
        let diagnostics = parse_diagnostics(&text);
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].lint, UNCODED);
    }

    #[test]
    fn every_arm_has_a_sorted_unique_baseline() {
        // Two rows for one lint would make the table's total wrong and its
        // violation check find only the first one.
        for arm in ARMS {
            let mut lints: Vec<&str> = arm.baseline.iter().map(|(lint, _)| *lint).collect();
            let sorted = {
                let mut copy = lints.clone();
                copy.sort_unstable();
                copy
            };
            assert_eq!(lints, sorted, "{}'s baseline is not sorted", arm.name);
            lints.dedup();
            assert_eq!(
                lints.len(),
                arm.baseline.len(),
                "{} names a lint twice",
                arm.name
            );
            assert!(
                !arm.baseline.is_empty(),
                "{} has no baseline, so every diagnostic in it would be a new lint",
                arm.name
            );
        }
    }

    #[test]
    fn every_arm_names_its_target_and_no_arm_repeats_another() {
        for arm in ARMS {
            assert!(
                arm.args.iter().any(|arg| arg.starts_with("--target")),
                "{} does not name a target; a clippy arm without one builds for \
                 the config's pinned triple and the counts would describe another \
                 machine",
                arm.name
            );
        }
        for (index, arm) in ARMS.iter().enumerate() {
            for other in &ARMS[index + 1..] {
                assert_ne!(arm.name, other.name, "two arms share a name");
                assert_ne!(arm.args, other.args, "two arms share arguments");
            }
        }
    }

    /// The smallest recording arm, so a test that grows one count names a row
    /// that exists without depending on any larger arm's table.
    fn xtask_arm() -> usize {
        ARMS.iter()
            .position(|arm| arm.name == "mpos-xtask (x86_64-unknown-linux-gnu)")
            .expect("the xtask arm is recorded")
    }

    #[test]
    fn a_grown_count_and_a_new_lint_are_both_violations() {
        let mut counts = BTreeMap::new();
        counts.insert("clippy::too_many_arguments".to_string(), 5);
        counts.insert("clippy::brand_new".to_string(), 1);
        let measured = vec![Measurement {
            arm: xtask_arm(),
            counts,
            errors: Vec::new(),
        }];
        let (violations, _) = verdicts(&measured);
        assert_eq!(violations.len(), 2, "{violations:?}");
        // Sorted by lint, so the never-seen one is reported first - the order
        // of the map, not of the insert.
        assert!(
            violations[0].contains("never had that lint"),
            "{violations:?}"
        );
        assert!(
            violations[1].contains("up from the recorded 2"),
            "{violations:?}"
        );
    }

    #[test]
    fn a_falling_count_is_an_improvement_and_not_a_failure() {
        let mut counts = BTreeMap::new();
        counts.insert("clippy::too_many_arguments".to_string(), 1);
        counts.insert("clippy::unnecessary_map_or".to_string(), 1);
        let measured = vec![Measurement {
            arm: xtask_arm(),
            counts,
            errors: Vec::new(),
        }];
        let (violations, improvements) = verdicts(&measured);
        assert!(violations.is_empty(), "{violations:?}");
        assert_eq!(improvements.len(), 1);
        assert!(improvements[0].contains("fell from 2 to 1"));
    }
}
