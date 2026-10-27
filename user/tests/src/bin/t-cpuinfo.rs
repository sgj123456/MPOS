//! t-cpuinfo - `/proc/cpuinfo` has the shape of the **machine**, not of the
//! kernel, and a reader picks its parser from that shape.
//!
//! Linux on x86 reports `vendor_id`/`model name`; on ARM64 it reports
//! `CPU implementer`/`CPU part`/`Features` and no `model name` at all.
//! `fastfetch`'s `detectCPUOthers` has no row for `model name`, so an
//! x86-shaped file on the second machine answers "Unknown CPU" - the reason
//! this kernel's two renderings are separate (`vfs/procfs.rs`).
//!
//! The aarch64 fields are read from `MIDR_EL1`/`ID_AA64PFR0_EL1`/
//! `ID_AA64ISAR0_EL1` rather than invented, so this program checks the
//! *shape* and that the identification word is a real ARM implementer value -
//! it cannot know which core the host's QEMU was built as, and a pin that
//! hardcoded `part` would be a pin about the runner rather than the kernel.

use mpos_user_tests::Report;

fn cpuinfo() -> String {
    std::fs::read_to_string("/proc/cpuinfo").expect("/proc/cpuinfo is readable")
}

/// The value after `key`, trimmed - Linux pads the separator with tabs and the
/// two renderings differ in how many.
fn field<'a>(text: &'a str, key: &str) -> Option<&'a str> {
    text.lines().find_map(|line| {
        line.strip_prefix(key)?
            .trim_start()
            .strip_prefix(':')
            .map(str::trim)
    })
}

fn main() {
    println!("t-cpuinfo");
    let mut r = Report::new();
    let text = cpuinfo();

    r.check(
        "the first line names processor 0",
        text.lines().next() == Some("processor\t: 0"),
    );
    if cfg!(target_arch = "aarch64") {
        r.check(
            "an ARM64 file carries `CPU implementer`",
            field(&text, "CPU implementer").is_some(),
        );
        r.check(
            "the implementer is a real, non-zero ARM id",
            field(&text, "CPU implementer")
                .and_then(|v| u32::from_str_radix(v.trim_start_matches("0x"), 16).ok())
                .is_some_and(|v| v != 0),
        );
        r.check(
            "and `CPU part` names one",
            field(&text, "CPU part")
                .and_then(|v| u32::from_str_radix(v.trim_start_matches("0x"), 16).ok())
                .is_some_and(|v| v != 0),
        );
        r.check(
            "`CPU architecture: 8` is stated",
            field(&text, "CPU architecture") == Some("8"),
        );
        r.check(
            "the feature word is present",
            field(&text, "Features").is_some_and(|f| f.contains("fp")),
        );
        r.check(
            "there is no x86 `model name` to mislead a reader",
            field(&text, "model name").is_none(),
        );
    } else {
        r.check(
            "an x86 file carries `model name`",
            field(&text, "model name").is_some(),
        );
        r.check(
            "the name is a non-empty string",
            field(&text, "model name").is_some_and(|v| !v.is_empty()),
        );
        r.check(
            "there is no ARM `CPU implementer` to mislead a reader",
            field(&text, "CPU implementer").is_none(),
        );
    }

    r.finish();
}
