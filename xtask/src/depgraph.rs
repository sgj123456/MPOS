//! The module graph, measured and ratcheted (PLAN.md §11.2 Step G).
//!
//! Phase 11 opens with a measurement rather than an opinion: "bad architecture"
//! is not a finding until something counts it, and §11.0's count is that the
//! kernel's module graph is one strongly connected component. This command is
//! the *same* extraction, run as a check rather than as an audit, so the number
//! can only move down. A ratchet, not a cliff: every rule below is recorded at
//! the value the tree has today, with the call sites that keep it non-zero
//! named beside it and the Phase-11 step that removes them. A new edge from a
//! layer that has none fails immediately; an existing one must be *reduced* to
//! change its constant, which makes each reduction a commit somebody can read
//! and a regression a failing gate.
//!
//! One deliberate difference from §11.0's script: that one matches text, so a
//! `crate::syscall` named inside a doc comment counts as an edge. It cost the
//! published audit three phantom pairs (`uaccess → syscall` and two others were
//! comment mentions, and `arch ↔ vfs` was a file the compiler never saw at all).
//! This one strips `//` comments before matching, so what it counts is who
//! *names* whom in code - which is what the ratchet has to be about, since a
//! rule that fires on prose is one people learn to work around.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

/// The number of mutual module pairs (a cycle of length two) the tree had when
/// this ratchet was last recorded: 2026-10-02, after Phase 11 Step C and the
/// `mm`/`vfs` seam. §11.0 measured 23 by text; the three removed there were
/// comment mentions, the twenty-first went when the exit-hook registry took
/// `sched → net` out, and the eighteenth when `mm` stopped naming `vfs::Inode`
/// for a frame-backed mapping (`vfs` still names `mm`, so only one direction of
/// that pair had to go — `ARCHITECTURE.md` V-13). Was 19 before the seam.
/// The seventeenth went in D.2a when the active-console number moved from
/// `vfs::vcmux` to the `term` leaf, so the one `console → vfs` edge
/// (`console.rs::active_grid` reading which console is on screen) became
/// `console → term` and the `vfs <-> console` pair collapsed to a one-way edge —
/// only one direction had to go. Was 18 before it.
///
/// **15 now**, on the HAL landing (S1): rerouting every `crate::arch::timer`/
/// `rtc`/`idle`/`halt` call above the machine layer through `crate::hal` took
/// `arch → console` and `arch → net` to zero (both modules still *name* `arch`,
/// so each pair collapsed to a one-way edge). No pair was added in exchange:
/// the platform tag is `hal`'s own type and `arch` implements the traits for
/// it, so `arch → hal` is the only arrow between the two. Was 17 before it.
/// **14 now**, after the HAL's second stage: rerouting the interrupt controller
/// (`pic::set_masked`, `lapic::init`, `idt::NET_IRQ_VECTOR`) through
/// `crate::hal::Interrupts` took `drivers → arch` to zero, leaving only the
/// one-way `arch → drivers`, so the `arch ↔ drivers` pair collapsed. Was 15
/// after S1, which itself was 17 before it.
const BASELINE_MUTUAL_PAIRS: usize = 14;

/// References to `crate::drivers::`/`crate::net::` from `src/vfs/mod.rs`.
///
/// **Zero, and it must stay zero.** This was 9: the `Inode::as_*` downcasts
/// (V-1) - one method per concrete device and socket type, six under
/// `crate::drivers::` and three under `crate::net::`, so the VFS could not be
/// compiled without naming every driver in the kernel and a seventh device type
/// meant editing the filesystem layer. Phase 11 B1 removed them one device at a
/// time (fbdev, evdev, uinput, AF_PACKET, AF_VSOCK, AF_UNIX, then the DRM card's
/// three node types) and each migration took its references with it: 9 → 8 → 7 →
/// 6 → 5 → 4 → 3 → 0. The question they answered is now asked *of* the node by
/// the layer that owns the type, through `dyn Inode::as_any`; the VFS never
/// hears the answer's name. A new reference here is a new leak of the same
/// shape, so the ratchet fails on the first one.
const BASELINE_VFS_DEVICE_REFERENCES: usize = 0;

/// The layers below `syscall` that must not name it (AGENTS.md §4's first hard
/// rule). `arch` and `main` are deliberately absent: the `syscall` gate's whole
/// job is to enter the dispatcher, and `main` is the wiring.
///
/// Recorded 2026-10-02, at Step F. Every non-zero entry names the call sites
/// that keep it there and the step that removes it.
const BASELINE_LOWER_LAYER_TO_SYSCALL: &[(&str, usize)] = &[
    // `sound.rs`'s one `use crate::syscall::linux::abi::{…}`: the device-ioctl
    // ABI that belongs with the device (V-5's remainder, Phase 11 B2).
    ("drivers", 1),
    // Nothing, and it must stay nothing: the errno table's move to `crate::err`
    // is what emptied this edge, and `uaccess` replaced the rest.
    ("net", 0),
    ("blk", 0),
    ("pci", 0),
    ("rand", 0),
    ("uevent", 0),
    ("vfs", 0),
    // `sched/runtime.rs`'s `crate::syscall::linux::{abi, nr}`, `MMAP_BASE` and
    // `DEFAULT_UMASK` (plus `sched/mod.rs`'s umask default): constants and
    // tables the scheduler's exec path reads out of the ABI layer. Phase 11 B/C
    // - the umask and the mapping base are scheduler state, and the `nr`
    // dependency is the exec/binfmt edge Step C decides about.
    ("sched", 10),
];

/// Where `sched` stands after Step C's registry, recorded beside the rule above
/// because that rule is about a layer naming another and not only about cycles:
/// `sched → net` is **gone** (0 above, which is what removed the `net ↔ sched`
/// pair), and `sched → drivers` is down from 4 to the two `close_at_exit` calls
/// left on the *execve* `FD_CLOEXEC` path. That path is a different event from
/// process exit with a different pair of subsystems to tell, and Step C
/// deliberately did not abstract the two into one registry (`PLAN.md` §11.2 Step
/// C: "the exec path decides per edge"). So the `drivers ↔ sched` pair stands
/// until that edge is decided on its own terms.

/// The measured graph: module-level edges, weighted by how many references
/// carry them.
pub struct Graph {
    edges: BTreeMap<(String, String), usize>,
}

impl Graph {
    /// How many `crate::<to>` references `<from>`'s files carry.
    pub fn weight(&self, from: &str, to: &str) -> usize {
        self.edges
            .get(&(from.to_owned(), to.to_owned()))
            .copied()
            .unwrap_or(0)
    }

    /// Every pair of modules that reference each other, `a < b`, once each.
    pub fn mutual_pairs(&self) -> Vec<(String, String)> {
        self.edges
            .keys()
            .filter(|(from, to)| from < to && self.edges.contains_key(&(to.clone(), from.clone())))
            .cloned()
            .collect()
    }

    /// The edges, for printing.
    pub fn edges(&self) -> impl Iterator<Item = (&String, &String, &usize)> {
        self.edges
            .iter()
            .map(|((from, to), weight)| (from, to, weight))
    }
}

/// Everything the ratchet looks at, measured once.
pub struct Report {
    pub graph: Graph,
    /// References to the device and network modules from `src/vfs/mod.rs`.
    pub vfs_device_references: usize,
}

impl Report {
    /// Every rule this run breaks, as sentences. Empty is the healthy answer.
    pub fn violations(&self) -> Vec<String> {
        let mut out = Vec::new();

        for (module, baseline) in BASELINE_LOWER_LAYER_TO_SYSCALL {
            let measured = self.graph.weight(module, "syscall");
            if measured > *baseline {
                out.push(format!(
                    "{module} names crate::syscall:: {measured} times, up from the recorded {baseline}"
                ));
            }
        }

        if self.vfs_device_references > BASELINE_VFS_DEVICE_REFERENCES {
            out.push(format!(
                "src/vfs/mod.rs names crate::drivers::/crate::net:: {} times, up from the recorded {BASELINE_VFS_DEVICE_REFERENCES}",
                self.vfs_device_references
            ));
        }

        let pairs = self.graph.mutual_pairs().len();
        if pairs > BASELINE_MUTUAL_PAIRS {
            out.push(format!(
                "the module graph has {pairs} mutual pairs, up from the recorded {BASELINE_MUTUAL_PAIRS}"
            ));
        }

        out
    }
}

/// Measures the tree under `root`'s `src/`.
pub fn report(root: &Path) -> Result<Report> {
    let src = root.join("src");
    let mut files = Vec::new();
    collect_sources(&src, &mut files)?;
    files.sort();

    let mut edges: BTreeMap<(String, String), usize> = BTreeMap::new();
    for file in &files {
        let Some(owner) = top_level_module(&src, file) else {
            continue;
        };
        let text = fs::read_to_string(file)
            .with_context(|| format!("Failed to read {}", file.display()))?;
        for reference in crate_references(&strip_comments(&text)) {
            if reference == owner || reference == "print" || reference == "println" {
                continue;
            }
            *edges.entry((owner.clone(), reference)).or_insert(0) += 1;
        }
    }

    let vfs_mod = fs::read_to_string(src.join("vfs/mod.rs")).context("src/vfs/mod.rs")?;
    let vfs_mod = strip_comments(&vfs_mod);
    let vfs_device_references = crate_references(&vfs_mod)
        .into_iter()
        .filter(|name| name == "drivers" || name == "net")
        .count();

    Ok(Report {
        graph: Graph { edges },
        vfs_device_references,
    })
}

/// Prints the measured graph and its ratchet, and fails the command when a rule
/// has been broken.
pub fn run(root: &Path) -> Result<()> {
    let report = report(root)?;

    println!("Module graph (crate:: references, comment mentions excluded)");
    for (from, to, weight) in report.graph.edges() {
        println!("  {from:12} -> {to:12} {weight}");
    }

    let pairs = report.graph.mutual_pairs();
    println!();
    println!("Mutual pairs (a cycle of length two): {}", pairs.len());
    for (a, b) in &pairs {
        println!("  {a:12} <-> {b}");
    }

    println!();
    println!("Ratchet (recorded 2026-10-02)");
    for (module, baseline) in BASELINE_LOWER_LAYER_TO_SYSCALL {
        println!(
            "  lower layer -> syscall   {module:8} {}/{baseline}",
            report.graph.weight(module, "syscall")
        );
    }
    println!(
        "  vfs/mod.rs -> drivers|net  {}/{BASELINE_VFS_DEVICE_REFERENCES}",
        report.vfs_device_references
    );
    println!(
        "  mutual pairs               {}/{BASELINE_MUTUAL_PAIRS}",
        pairs.len()
    );

    let violations = report.violations();
    if violations.is_empty() {
        println!();
        println!("Within the ratchet: no edge has appeared and no count has grown.");
        return Ok(());
    }
    println!();
    for violation in &violations {
        eprintln!("depgraph: {violation}");
    }
    bail!(
        "{} rule(s) got worse. Either remove the edge - that is what Phase 11 is \
         for - or, if it is deliberate and argued in ARCHITECTURE.md §2/§3, raise \
         the constant next to the rule with the date and the reason.",
        violations.len()
    )
}

/// Every `.rs` under `dir`, recursively.
fn collect_sources(dir: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    let entries = fs::read_dir(dir).with_context(|| format!("Failed to read {}", dir.display()))?;
    for entry in entries {
        let path = entry?.path();
        if path.is_dir() {
            collect_sources(&path, out)?;
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            out.push(path);
        }
    }
    Ok(())
}

/// The top-level module a file belongs to: the first path component under
/// `src`, with the extension dropped for a file at the root (`src/console.rs`
/// is `console`; `src/net/socket.rs` is `net`, as is `src/net/mod.rs`).
fn top_level_module(src: &Path, file: &Path) -> Option<String> {
    let relative = file.strip_prefix(src).ok()?;
    let mut components = relative.components();
    let first = components
        .next()?
        .as_os_str()
        .to_string_lossy()
        .into_owned();
    match components.next() {
        Some(_) => Some(first),
        None => Some(first.trim_end_matches(".rs").to_owned()),
    }
}

/// The identifiers named by `crate::<name>` in `text`, in order.
fn crate_references(text: &str) -> Vec<String> {
    let bytes = text.as_bytes();
    let mut out = Vec::new();
    let needle = b"crate::";
    let mut i = 0;
    while i + needle.len() <= bytes.len() {
        if &bytes[i..i + needle.len()] != needle {
            i += 1;
            continue;
        }
        let mut j = i + needle.len();
        let start = j;
        while j < bytes.len() && (bytes[j].is_ascii_alphanumeric() || bytes[j] == b'_') {
            j += 1;
        }
        if j > start {
            let name = &text[start..j];
            if name.chars().next().is_some_and(|c| c.is_ascii_lowercase()) {
                out.push(name.to_owned());
            }
        }
        i = j.max(i + 1);
    }
    out
}

/// `text` with everything from a `//` to the end of the line removed.
///
/// The kernel has no block comments and no `//` inside a string literal that a
/// `crate::` reference could follow, so this is the whole of the comment
/// handling - and it is the difference between counting who names whom and
/// counting who writes about whom.
fn strip_comments(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for line in text.lines() {
        let code = match line.find("//") {
            Some(at) => &line[..at],
            None => line,
        };
        out.push_str(code);
        out.push('\n');
    }
    out
}

/// A two-module tree and the graph measured over it, for the tests below.
#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str, files: &[(&str, &str)]) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("mpos-xtask-depgraph-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        for (path, contents) in files {
            let path = dir.join(path);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, contents).unwrap();
        }
        // `report` reads `src/vfs/mod.rs` for its second rule, so every tree
        // needs one - empty ones are the ordinary case.
        let vfs = dir.join("src/vfs/mod.rs");
        if !vfs.exists() {
            fs::create_dir_all(vfs.parent().unwrap()).unwrap();
            fs::write(&vfs, "\n").unwrap();
        }
        dir
    }

    #[test]
    fn a_reference_in_a_doc_comment_is_not_an_edge() {
        // The distinction the ratchet depends on: `sched/signal.rs` talks about
        // `crate::syscall::linux::proc` in three doc comments and names it in
        // none, and a rule that fired on the prose would be a rule people learn
        // to write around.
        let dir = scratch(
            "comments",
            &[
                ("src/lib.rs", "pub mod sched;\n"),
                (
                    "src/sched/mod.rs",
                    "//! see crate::syscall::linux::proc\n\
                     /// and crate::syscall::DEFAULT_UMASK\n\
                     pub fn f() {}\n",
                ),
            ],
        );
        let report = report(&dir).unwrap();
        assert_eq!(report.graph.weight("sched", "syscall"), 0);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_mutual_pair_is_reported_once_and_a_one_way_edge_is_not() {
        let dir = scratch(
            "pairs",
            &[
                ("src/lib.rs", "pub mod a;\npub mod b;\npub mod c;\n"),
                ("src/a.rs", "use crate::b::X;\n"),
                ("src/b.rs", "use crate::a::Y;\n"),
                ("src/c.rs", "use crate::a::Y;\n"),
            ],
        );
        let report = report(&dir).unwrap();
        assert_eq!(report.graph.weight("a", "b"), 1);
        assert_eq!(report.graph.weight("b", "a"), 1);
        assert_eq!(report.graph.weight("c", "a"), 1, "c names a");
        assert_eq!(report.graph.weight("a", "c"), 0, "a does not name c");
        let pairs = report.graph.mutual_pairs();
        assert_eq!(pairs, vec![("a".to_owned(), "b".to_owned())]);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_new_upward_edge_breaks_the_ratchet() {
        // The counterfactual the ratchet needs: a service naming the ABI layer
        // it sits below, with no baseline to cover it, must fail.
        let dir = scratch(
            "upward",
            &[
                ("src/lib.rs", "pub mod uevent;\n"),
                ("src/uevent.rs", "use crate::syscall::linux::abi;\n"),
            ],
        );
        let report = report(&dir).unwrap();
        assert_eq!(report.graph.weight("uevent", "syscall"), 1);
        let violations = report.violations();
        assert!(
            violations.iter().any(|v| v.contains("uevent")),
            "{violations:?}"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_vfs_module_naming_a_device_breaks_the_ratchet() {
        // The rule is "at or below the recorded number", so the counterfactual
        // is one reference *more* than the record allows: at the baseline it
        // must be silent, and one past it must fail.
        let references = |n: usize| {
            (0..n)
                .map(|i| format!("use crate::drivers::drm::Type{i};\n"))
                .collect::<String>()
        };
        let at = BASELINE_VFS_DEVICE_REFERENCES;
        let dir = scratch(
            "vfs-device",
            &[
                ("src/lib.rs", "pub mod vfs;\n"),
                ("src/vfs/mod.rs", &references(at)),
            ],
        );
        let measured = report(&dir).unwrap();
        assert_eq!(measured.vfs_device_references, at);
        assert!(
            measured.violations().is_empty(),
            "{:?}",
            measured.violations()
        );
        let _ = fs::remove_dir_all(&dir);

        let dir = scratch(
            "vfs-device-over",
            &[
                ("src/lib.rs", "pub mod vfs;\n"),
                ("src/vfs/mod.rs", &references(at + 1)),
            ],
        );
        let measured = report(&dir).unwrap();
        assert_eq!(measured.vfs_device_references, at + 1);
        assert!(
            measured
                .violations()
                .iter()
                .any(|v| v.contains("src/vfs/mod.rs")),
            "{:?}",
            measured.violations()
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_measured_tree_holds_the_recorded_baselines() {
        let root = crate::utils::project_root().expect("the workspace root");
        let report = report(&root).expect("the module walk");
        let violations = report.violations();
        assert!(
            violations.is_empty(),
            "the module graph grew past its ratchet: {violations:#?}\n\
             Reduce the edge, or raise the constant next to the rule in \
             xtask/src/depgraph.rs with the date and the reason (PLAN.md §11.2 Step G)."
        );
    }
}
