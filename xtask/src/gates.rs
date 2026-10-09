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
}
