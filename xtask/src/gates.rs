//! The target gate as a rule rather than a habit (NOTES.md §11.2 Step F).
//!
//! `src/lib.rs` decides which modules the host build compiles, and that
//! decision is a per-module judgement: a module belongs on the host when its
//! logic touches no hardware, no port I/O, no virtio ring and no real clock -
//! and, the half that keeps being forgotten, *its dependencies must be
//! host-compiled too, transitively*. Nothing in the compiler objects when that
//! judgement is applied in a way that contradicts itself, and that is the
//! failure this file exists to catch: a `#[cfg(test)]` module written inside a
//! file that only a `target_os = "none"` build ever reaches. The annotation
//! says "run me on the host", the gate says the host never sees the file, and
//! the test has therefore never run. `src/net/socket.rs` carried three of those
//! (ARCHITECTURE.md §9 V-11) until Step F settled it.
//!
//! The check is deliberately textual and crude - a `cfg` is matched, not
//! evaluated - because that is what makes it cheap enough to run on every
//! `cargo xtask test`. Its own tests build a two-file tree of both shapes, so a
//! checker that has stopped being able to tell "gated" from "ungated" fails
//! loudly rather than reporting a clean tree (the lesson `t-ioctl`'s wrong
//! `RNDADDENTROPY` constant taught: a check that cannot tell a credit from a
//! drain agrees with itself and pins nothing).

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// A test module that can never run: the file it lives in is reachable only
/// through a module gated `target_os = "none"`, so no host build compiles it.
#[derive(Debug)]
pub struct DeadTest {
    /// The file, relative to the workspace root where that is possible.
    pub file: PathBuf,
    /// 1-based line of the `#[cfg(...)]` attribute that claims the host.
    pub line: usize,
    /// The module path whose gate makes it dead, e.g. `crate::net`.
    pub gated_by: String,
}

/// Every `#[cfg(test)]` in the tree that a `target_os = "none"` gate hides from
/// the host build. Empty is the healthy answer.
pub fn dead_host_tests(root: &Path) -> Result<Vec<DeadTest>> {
    let entry = root.join("src/lib.rs");
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    visit(&entry, false, String::from("crate"), &mut out, &mut seen)?;
    Ok(out)
}

/// Walks the module tree the way the compiler resolves it - `mod x;` in
/// `dir/mod.rs` is `dir/x.rs` or `dir/x/mod.rs` - carrying the gate state down
/// so a child of a machine-only module is machine-only whatever its own
/// attribute says.
fn visit(
    file: &Path,
    machine_only: bool,
    module_path: String,
    out: &mut Vec<DeadTest>,
    seen: &mut HashSet<PathBuf>,
) -> Result<()> {
    // A `#[path]` include can name the same file twice; and a module reached
    // twice would report its dead tests twice.
    if !seen.insert(file.to_path_buf()) {
        return Ok(());
    }
    let text =
        fs::read_to_string(file).with_context(|| format!("Failed to read {}", file.display()))?;

    if machine_only {
        for (line, attribute) in cfg_attributes(&text) {
            if attribute_mentions(&attribute, "test") {
                out.push(DeadTest {
                    file: file.to_path_buf(),
                    line,
                    gated_by: module_path.clone(),
                });
            }
        }
    }

    for (name, gate) in module_declarations(&text) {
        let Some(child) = resolve_module(file, &name) else {
            // An inline `mod x { … }`, or a declaration this walk cannot
            // resolve; either way there is no separate file to inspect.
            continue;
        };
        let child_machine_only = machine_only || gate_is_machine_only(&gate);
        visit(
            &child,
            child_machine_only,
            format!("{module_path}::{name}"),
            out,
            seen,
        )?;
    }
    Ok(())
}

/// The `mod name;` declarations in `text`, each with the `#[cfg(...)]`
/// attribute immediately above it (`""` when there is none).
///
/// "Immediately above" allows blank lines, doc comments and further attributes
/// between the two, and is cleared by anything else - which is what stops the
/// gate on one declaration from being read as the gate on the next.
fn module_declarations(text: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut pending: Option<String> = None;
    for line in logical_lines(text) {
        let trimmed = line.trim();
        if let Some(attribute) = as_attribute(&line) {
            if attribute.starts_with("#[cfg(") {
                pending = Some(attribute);
            }
            continue;
        }
        if trimmed.is_empty() || trimmed.starts_with("//") {
            continue;
        }
        match mod_declaration(trimmed) {
            Some(name) => out.push((name, pending.take().unwrap_or_default())),
            None => pending = None,
        }
    }
    out
}

/// The `#[cfg(...)]` attributes in `text` with their 1-based line numbers.
fn cfg_attributes(text: &str) -> Vec<(usize, String)> {
    let mut out = Vec::new();
    for (line, number) in numbered_logical_lines(text) {
        if let Some(attribute) = as_attribute(&line)
            && attribute.starts_with("#[cfg(")
        {
            out.push((number, attribute));
        }
    }
    out
}

/// The lines of `text` with each attribute reassembled into one, so an
/// attribute written across several lines is still one string.
fn logical_lines(text: &str) -> Vec<String> {
    numbered_logical_lines(text)
        .into_iter()
        .map(|(line, _)| line)
        .collect()
}

fn numbered_logical_lines(text: &str) -> Vec<(String, usize)> {
    let lines: Vec<&str> = text.lines().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let start = i + 1;
        if !lines[i].trim_start().starts_with("#[") {
            out.push((lines[i].to_owned(), start));
            i += 1;
            continue;
        }
        let mut attribute = lines[i].trim().to_owned();
        while !balanced(&attribute) && i + 1 < lines.len() {
            i += 1;
            attribute.push(' ');
            attribute.push_str(lines[i].trim());
        }
        out.push((attribute, start));
        i += 1;
    }
    out
}

/// `Some(name)` for a `mod name;` declaration (with or without a visibility),
/// `None` for everything else - including an inline `mod name { … }`, which has
/// no file of its own to walk into.
fn mod_declaration(line: &str) -> Option<String> {
    let line = line.trim_end_matches(';').trim();
    let line = line
        .strip_prefix("pub(crate) ")
        .or_else(|| line.strip_prefix("pub "))
        .unwrap_or(line);
    let name = line.strip_prefix("mod ")?;
    if !name.is_empty() && name.chars().all(|c| c.is_alphanumeric() || c == '_') {
        Some(name.to_owned())
    } else {
        None
    }
}

/// `file`'s directory plus the module `name`'s path, in the two shapes Rust
/// accepts.
fn resolve_module(file: &Path, name: &str) -> Option<PathBuf> {
    let dir = file.parent()?;
    let base = match file.file_name()?.to_str()? {
        "mod.rs" | "lib.rs" | "main.rs" => dir.to_path_buf(),
        _ => dir.join(file.file_stem()?),
    };
    let flat = base.join(format!("{name}.rs"));
    if flat.exists() {
        return Some(flat);
    }
    let nested = base.join(name).join("mod.rs");
    if nested.exists() {
        return Some(nested);
    }
    None
}

/// Whether a declaration's attribute puts the module in the host build at all.
///
/// Crude on purpose: `#[cfg(all(any(target_os = "none", test), …))]` (the boot
/// handoff's own gate) names the host by way of `test`, so it is *not*
/// machine-only, while `#[cfg(target_os = "none")]` and
/// `#[cfg(all(target_os = "none", target_arch = "x86_64"))]` are.
fn gate_is_machine_only(attribute: &str) -> bool {
    if attribute.is_empty() {
        return false;
    }
    let squeezed = attribute.replace(' ', "");
    squeezed.contains("target_os=\"none\"")
        && !squeezed.contains("not(target_os=\"none\")")
        && !attribute_mentions(attribute, "test")
}

/// Whether `attribute` names `word` as a whole token, so that `test` matches
/// `#[cfg(test)]` and `#[cfg(all(test, …))]` but not a name it happens to be
/// inside.
fn attribute_mentions(attribute: &str, word: &str) -> bool {
    attribute
        .split(|c: char| !(c.is_alphanumeric() || c == '_'))
        .any(|token| token == word)
}

/// Whether every `[` and `(` opened in `attribute` has been closed - i.e.
/// whether the attribute is complete or continues on the next line.
fn balanced(attribute: &str) -> bool {
    let opens = attribute.matches(['[', '(']).count();
    let closes = attribute.matches([']', ')']).count();
    opens <= closes
}

/// `Some(attribute)` when the line is one, with the whole attribute
/// reassembled and trailing text dropped.
fn as_attribute(line: &str) -> Option<String> {
    let trimmed = line.trim();
    if !trimmed.starts_with("#[") {
        return None;
    }
    let mut depth = 0i32;
    for (at, c) in trimmed.char_indices() {
        match c {
            '[' => depth += 1,
            ']' => {
                depth -= 1;
                if depth == 0 {
                    return Some(trimmed[..=at].to_owned());
                }
            }
            _ => {}
        }
    }
    Some(trimmed.to_owned())
}

// ---------------------------------------------------------------------------
// The other rule this file carries: no test opening in the boot's own code
// ---------------------------------------------------------------------------

/// Where a test opening could grow back: the boot's own code, and only that.
///
/// Not a walk of the kernel: a boot-time `if testing { … }` can only be written
/// where the boot is written, which is the wiring layer's bodies, the harness
/// beside them and the machine layer's own aarch64 boot. A gate over the whole
/// tree would be a gate over prose - and this list is the reason it is a list:
/// three comments in it say `mpos.selftest` *on purpose*, to record what the
/// kernel stopped reading, so the walk skips comments and the rule is about code.
const BOOT_CODE: &[&str] = &[
    "src/board/flow.rs",
    "src/board/kunit.rs",
    "src/board/mod.rs",
    "src/board/report.rs",
    "src/board/steps.rs",
    "src/board/suites.rs",
    "src/drivers/virtio/probe.rs",
    "arch/src/arch/aarch64/mod.rs",
];

/// A test opening that has grown back into the boot's own code.
#[derive(Debug)]
pub struct TestOpening {
    /// The file, relative to the workspace root.
    pub file: String,
    /// 1-based line.
    pub line: usize,
    /// Which shape it is, as a sentence for the failure message.
    pub what: &'static str,
    /// The offending text, trimmed.
    pub code: String,
}

/// Every way the boot has grown a way to know it is being tested. Empty is the
/// healthy answer.
///
/// The three shapes are the three the refactor removed, each of which the
/// compiler cannot see because it is a *word* rather than a type
/// (`docs/NOTES.md` §15.19, §15.23):
///
/// - the kernel reading a test word off its own command line, which is what
///   `mpos.selftest` was;
/// - `feature = "selftest"` outside a `#[cfg]`, i.e. a check that survives into
///   the shipping kernel and decides something at run time. Inside a `#[cfg]` it
///   is the mechanism - the only place a test build may differ;
/// - an `M*` marker literal (`"M5_VIRTIO_OK"`), the vocabulary whose writers were
///   the checks. A check that comes back should speak in sentences; a reader
///   cannot tell a marker nothing parses from a mechanism.
pub fn test_openings(root: &Path) -> Result<Vec<TestOpening>> {
    let mut out = Vec::new();
    for relative in BOOT_CODE {
        let path = root.join(relative);
        let text =
            fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
        for (line, number) in numbered_logical_lines(&text) {
            if line.trim_start().starts_with("//") {
                continue;
            }
            let mut note = |what: &'static str, code: &str| {
                out.push(TestOpening {
                    file: (*relative).to_owned(),
                    line: number,
                    what,
                    code: code.trim().to_owned(),
                })
            };
            if line.contains("mpos.selftest") {
                note("reads a test word off the command line", &line);
            }
            if line.contains("feature = \"selftest\"") && as_attribute(&line).is_none() {
                note("asks the test feature at run time", &line);
            }
            for literal in string_literals(&line) {
                if is_marker_literal(literal) {
                    note("prints an `M*` marker", literal);
                }
            }
        }
    }
    Ok(out)
}

/// Every `"…"` on a line, by the crude rule this file's header argues for: the
/// quotes are matched, not parsed. An escaped quote inside a string can split one
/// literal into two, which costs nothing here - neither half can be `"M`+digit.
fn string_literals(line: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut rest = line;
    while let Some(open) = rest.find('"') {
        let after = &rest[open + 1..];
        let Some(close) = after.find('"') else { break };
        out.push(&after[..close]);
        rest = &after[close + 1..];
    }
    out
}

/// Whether `literal` has the milestone markers' shape: `M`, a digit, `_`, then
/// capitals and underscores - `M5_VIRTIO_OK`, `M8_OK`. `MPOS_SWITCH_ROOT_OK` (the
/// init script's own line, which is not the kernel's to police) is not one, and
/// neither is any word a boot line would begin with.
fn is_marker_literal(literal: &str) -> bool {
    let bytes = literal.as_bytes();
    bytes.len() > 3
        && bytes[0] == b'M'
        && bytes[1].is_ascii_digit()
        && bytes[2] == b'_'
        && bytes[3..]
            .iter()
            .all(|b| b.is_ascii_uppercase() || *b == b'_')
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A workspace of two files: one behind a `target_os = "none"` gate with a
    /// host test in it, one ungated with the same test. The checker has to
    /// report the first and only the first - a checker that cannot tell the two
    /// apart would pass on a tree that has the defect.
    fn scratch_tree(name: &str, gated_cfg: &str, child_cfg: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("mpos-xtask-gates-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("src/gated")).unwrap();
        fs::write(
            dir.join("src/lib.rs"),
            format!("{gated_cfg}\npub mod gated;\npub mod open;\n"),
        )
        .unwrap();
        fs::write(
            dir.join("src/gated/mod.rs"),
            format!("pub mod inner;\n\n{child_cfg}\nmod tests {{}}\n"),
        )
        .unwrap();
        fs::write(dir.join("src/gated/inner.rs"), "pub fn f() {}\n").unwrap();
        fs::write(dir.join("src/open.rs"), "#[cfg(test)]\nmod tests {}\n").unwrap();
        dir
    }

    #[test]
    fn a_host_test_behind_a_machine_gate_is_reported() {
        let dir = scratch_tree(
            "gated",
            "#[cfg(target_os = \"none\")]",
            "#[cfg(all(test, not(target_os = \"none\")))]",
        );
        let found = dead_host_tests(&dir).unwrap();
        assert_eq!(
            found.len(),
            1,
            "expected exactly the gated file's test: {found:?}"
        );
        assert!(found[0].file.ends_with("src/gated/mod.rs"), "{found:?}");
        assert_eq!(found[0].gated_by, "crate::gated");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_test_in_an_ungated_file_is_left_alone() {
        let dir = scratch_tree(
            "open",
            "#[cfg(target_os = \"none\")]",
            "#[cfg(all(test, not(target_os = \"none\")))]",
        );
        // The ungated sibling carries the same annotation and must not be
        // reported: only the machine-gated one is dead.
        let found = dead_host_tests(&dir).unwrap();
        assert!(
            !found.iter().any(|d| d.file.ends_with("src/open.rs")),
            "an ungated file's test is not dead: {found:?}"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_gate_that_names_the_host_by_way_of_test_is_not_machine_only() {
        // The boot handoff's own gate: `all(any(target_os = "none", test), …)`
        // is compiled for the host, so its tests are alive.
        let dir = scratch_tree(
            "boot",
            "#[cfg(all(any(target_os = \"none\", test), target_arch = \"x86_64\"))]",
            "#[cfg(test)]",
        );
        let found = dead_host_tests(&dir).unwrap();
        assert!(found.is_empty(), "{found:?}");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_tree_has_no_host_test_behind_a_machine_gate() {
        let root = crate::utils::project_root().expect("the workspace root");
        let dead = dead_host_tests(&root).expect("the module walk");
        assert!(
            dead.is_empty(),
            "a `#[cfg(test)]` module the host never compiles: {dead:#?}\n\
             Either host-compile the file, or move the pins to a program that runs \
             (NOTES.md §11.2 Step F)."
        );
    }

    /// The real tree's inventory holds the invariant its document states.
    ///
    /// The fixture test above proves the checker can *see* drift; this one is
    /// the claim about this repository, and it is the one that would have
    /// reddened on 2026-11-12 (three arms without rows, two rows without arms).
    /// Both are needed and neither substitutes for the other: a checker that
    /// only ever runs on a synthetic tree can agree with itself forever.
    #[test]
    fn the_syscall_inventory_covers_every_dispatch_arm() {
        let root = crate::utils::project_root().expect("the workspace root");
        let drift = syscall_inventory(&root).expect("the inventory read");
        assert!(
            drift.is_empty(),
            "the dispatch and docs/uapi/01-syscall-table.md disagree: {drift:#?}\n\
             A served call without a §1.0 row is the review finding that document \
             exists to make (docs/uapi/index.md)."
        );
    }

    /// The boot's own code, with every listed file present and empty, so a test
    /// can give one of them the shape it is about.
    fn scratch_boot_tree(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("mpos-xtask-boot-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        for file in BOOT_CODE {
            let path = dir.join(file);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, "fn f() {}\n").unwrap();
        }
        dir
    }

    /// All three shapes are reported, which is the half that keeps the gate from
    /// agreeing with itself: a checker that cannot tell an opening from ordinary
    /// code would pass on a tree that has one.
    #[test]
    fn every_shape_of_test_opening_is_reported() {
        let dir = scratch_boot_tree("shapes");
        fs::write(
            dir.join("src/board/flow.rs"),
            "fn f() {\n\
             \x20   let word = \"mpos.selftest\";\n\
             \x20   let testing = cfg!(feature = \"selftest\");\n\
             \x20   println!(\"M8_OK\");\n\
             }\n",
        )
        .unwrap();
        let found = test_openings(&dir).unwrap();
        assert_eq!(found.len(), 3, "{found:#?}");
        let _ = fs::remove_dir_all(&dir);
    }

    /// A comment may name all three, and the tree's own comments do - they are
    /// the record of what the kernel stopped doing. Prose is not an opening: a
    /// gate that fired on it would be switched off within a week.
    #[test]
    fn a_comment_that_names_the_vocabulary_is_not_an_opening() {
        let dir = scratch_boot_tree("prose");
        fs::write(
            dir.join("src/board/suites.rs"),
            "// M8_OK used to be printed here, and `mpos.selftest` was read here.\n\
             //! Not gated on `feature = \"selftest\"` any more.\n\
             fn f() {}\n",
        )
        .unwrap();
        let found = test_openings(&dir).unwrap();
        assert!(found.is_empty(), "{found:#?}");
        let _ = fs::remove_dir_all(&dir);
    }

    /// The vocabulary's shape, and the one line that only looks like it: the
    /// initramfs script's own `MPOS_SWITCH_ROOT_OK` is not the kernel's.
    #[test]
    fn only_the_marker_shape_counts() {
        assert!(is_marker_literal("M5_VIRTIO_OK"));
        assert!(is_marker_literal("M8_OK"));
        assert!(!is_marker_literal("MPOS_SWITCH_ROOT_OK"));
        assert!(!is_marker_literal("M5_"));
        assert!(!is_marker_literal("M5_virtio"));
        assert!(!is_marker_literal("virtio-gpu: /dev/fb0 is driven at"));
    }

    #[test]
    fn the_boot_has_no_test_opening() {
        let root = crate::utils::project_root().expect("the workspace root");
        let openings = test_openings(&root).expect("the boot code walk");
        assert!(
            openings.is_empty(),
            "a test opening in the boot's own code: {openings:#?}\n\
             The boot is told what to build by the build rather than by a word or \
             a run-time check (NOTES.md §15.19), and its log speaks in sentences \
             rather than `M*` markers (§15.24)."
        );
    }

    /// The inventory check finds both directions of drift, and only drift.
    ///
    /// Built as a two-file tree so the checker is exercised on a shape that
    /// *has* the defect - the point this file's header makes about the
    /// `RNDADDENTROPY` constant: a checker that cannot tell a correct tree from
    /// a broken one agrees with itself and pins nothing. The fixture carries one
    /// arm with no row and one row with no arm, and the check must report
    /// exactly those two.
    #[test]
    fn inventory_drift_is_reported_in_both_directions() {
        let dir = std::env::temp_dir().join(format!("mpos-xtask-inventory-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("docs/uapi")).unwrap();
        fs::create_dir_all(dir.join("src/syscall/linux")).unwrap();
        fs::write(
            dir.join("src/syscall/linux/mod.rs"),
            "match nr {\n    nr::READ => 1,\n    nr::WRITE => 2,\n    nr::GHOST => 3,\n}\n",
        )
        .unwrap();
        fs::write(
            dir.join("docs/uapi/01-syscall-table.md"),
            "### 1.0 Every served call\n\n| Syscall | Nr |\n|---|---|\n| `read` | 0 |\n| `write` | 1 |\n| `phantom` | 9 |\n\n### 1.1 Refused\n\n| `ignored` | 9 |\n",
        )
        .unwrap();
        let drift = syscall_inventory(&dir).unwrap();
        let said: Vec<&str> = drift.iter().map(|d| d.what.as_str()).collect();
        assert!(
            said.iter()
                .any(|w| w.contains("`ghost`") && w.contains("no §1.0 row")),
            "an arm with no row must be reported: {said:?}"
        );
        assert!(
            said.iter()
                .any(|w| w.contains("`phantom`") && w.contains("no dispatch arm")),
            "a row with no arm must be reported: {said:?}"
        );
        // §1.1's rows are not this count, and neither is a matched correct pair.
        assert!(!said.iter().any(|w| w.contains("`ignored`")), "{said:?}");
        assert!(!said.iter().any(|w| w.contains("`read`")), "{said:?}");
        assert_eq!(drift.len(), 2, "exactly the two defects: {said:?}");
    }
}

// ---------------------------------------------------------------------------
// The uapi inventory's own invariant
// ---------------------------------------------------------------------------

/// One way the syscall inventory and the dispatch disagree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InventoryDrift {
    /// The section of `docs/uapi/01-syscall-table.md` the row lives in.
    pub section: &'static str,
    /// What is wrong, in the words the gate prints.
    pub what: String,
}

/// Every syscall the dispatch has an arm for must have exactly one row in
/// §1.0 of `docs/uapi/01-syscall-table.md`, and every §1.0 row must name an arm.
///
/// This is the inventory's stated invariant - "what matters is *arms = rows*: a
/// syscall landing without a row here is the review finding" - and until this
/// function existed it was a sentence nobody re-ran. Re-measured 2026-11-12 it
/// was false in **both** directions: three arms had no row (`sync_file_range`,
/// `futex2_wake`, `futex2_wait`) and two rows named numbers that are not
/// syscalls at all (`futex_wake`/`futex_wait` at 454/455 - operation words,
/// listed again correctly under their real names beside them).
///
/// The check is textual and crude for the reason the rest of this file is: it
/// costs a file read, so it can run on every sweep, and a check that is not run
/// is a check that has already failed (the `RNDADDENTROPY` lesson in this file's
/// header - a checker that agrees with itself pins nothing).
///
/// Two exclusions, each stated rather than silent:
/// * a name numbered in the machine tables but deliberately unserved still has
///   its own row, which is why the comparison is against the *dispatch arms*
///   and not against the number tables - a number with a `_ => ENOSYS`
///   fall-through and no row is the finding;
/// * `### 1.1`/`### 1.2` rows are the refusals and the sub-gaps, and they are
///   not part of this count, so only `### 1.0`'s table is read.
pub fn syscall_inventory(root: &Path) -> Result<Vec<InventoryDrift>, String> {
    let doc = root.join("docs/uapi/01-syscall-table.md");
    let dispatch = root.join("src/syscall/linux/mod.rs");
    let text = fs::read_to_string(&doc).map_err(|e| format!("{}: {e}", doc.display()))?;
    let dispatch_text =
        fs::read_to_string(&dispatch).map_err(|e| format!("{}: {e}", dispatch.display()))?;

    // The arms: every `nr::NAME` the dispatch matches on.
    let mut arms: HashSet<String> = HashSet::new();
    for line in dispatch_text.lines() {
        let mut rest = line;
        while let Some(at) = rest.find("nr::") {
            rest = &rest[at + 4..];
            let name: String = rest
                .chars()
                .take_while(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || *c == '_')
                .collect();
            if !name.is_empty() {
                arms.insert(name.to_ascii_lowercase());
            }
        }
    }

    // §1.0's rows only: the section runs from its heading to the next one.
    let mut rows: HashSet<String> = HashSet::new();
    let mut in_section = false;
    for line in text.lines() {
        if line.starts_with("### ") || line.starts_with("## ") {
            in_section = line.trim_start_matches('#').trim_start().starts_with("1.0");
            continue;
        }
        if !in_section {
            continue;
        }
        let Some(rest) = line.strip_prefix("| `") else {
            continue;
        };
        let name: String = rest.chars().take_while(|c| *c != '`').collect();
        if !name.is_empty()
            && name
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
        {
            rows.insert(name);
        }
    }

    let mut drift = Vec::new();
    for arm in arms.difference(&rows) {
        drift.push(InventoryDrift {
            section: "1.0",
            what: format!("`{arm}` has a dispatch arm but no §1.0 row"),
        });
    }
    for row in rows.difference(&arms) {
        drift.push(InventoryDrift {
            section: "1.0",
            what: format!("`{row}` has a §1.0 row but no dispatch arm"),
        });
    }
    drift.sort_by(|a, b| a.what.cmp(&b.what));
    Ok(drift)
}
