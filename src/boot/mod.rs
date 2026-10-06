//! Machine-independent view of the hardware the kernel was handed at boot.
//!
//! This module deliberately does not name any bootloader type. `boot::handoff`
//! translates what `mpos-boot`, the EFI loader, left behind into these
//! structures; everything else in the kernel only ever sees what is declared
//! here. That is the seam future ports (riscv64 via OpenSBI+DTB, aarch64) plug
//! into.

// The kernel build is the only one that needs this, and the host cannot
// provide the loader's record - but the layout it declares is a promise to
// another crate, so `test` keeps it compiled where the tests can pin it.
//
// Both machines keep it compiled too, and that is the point of the record: it
// carries plain data and no page-table root, no GDT, no EFI table, so the one
// record in `mpos-bootinfo` serves the x86_64 loader and the aarch64 one stage
// 1e adds. The `target_arch` half of the gate is therefore a list of the
// machines that have a loader, not a property of the record.
#[cfg(all(
    any(target_os = "none", test),
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
pub mod handoff;

/// The consoles this boot writes its kernel log to, read off the command line
/// the way Linux reads `console=`. Not target-gated: walking a command line is
/// pure, and the policy it yields is exactly the kind of thing a host test
/// should be able to argue with.
pub mod console_list;

// The record itself, and the kernel's view of it, are the `mpos-bootinfo` leaf
// now - re-exported under the names they have always had, so every
// `crate::boot::` caller is unchanged. They moved out because the machine layer
// names both: `hal::Paging::install` takes a `BootInfo` and the aarch64 entry
// takes a `Handoff`, and a machine crate cannot depend on the kernel
// (`PLAN.md` §14 S2a). What stays here is what the kernel *does* with them:
// translating the record (`handoff`), reading the command line, and the console
// list.
pub use mpos_bootinfo::{BootInfo, MAX_MEM_REGIONS, MemKind, MemRegion};

/// The command line the bootloader was given, kept where the filesystems can
/// read it: `/proc/cmdline` is how the initramfs's init learns `root=`.
static CMDLINE: critical_section::Mutex<core::cell::RefCell<&'static str>> =
    critical_section::Mutex::new(core::cell::RefCell::new(""));

/// Record the boot command line. Called once, from the boot task.
pub fn set_cmdline(line: &'static str) {
    critical_section::with(|cs| *CMDLINE.borrow(cs).borrow_mut() = line);
}

/// The boot command line, as the bootloader was given it.
pub fn cmdline() -> &'static str {
    critical_section::with(|cs| *CMDLINE.borrow(cs).borrow())
}

/// Whether the command line carries `word`, as a whole word.
///
/// The one spelling of that question, because the callers are two boots that must
/// agree about it: `mpos.selftest` decides whether a machine runs the milestone
/// checks, and a boot that read the line differently from the loader's own
/// grammar (a prefix match, say, or the line's first word only) would be a boot
/// whose behaviour depended on which machine it was - the class of difference
/// Phase 15's unification exists to remove. The words the kernel reads for its own
/// configuration (`loglevel=`, `console=`) go through their own parsers, which
/// name the prefix they are looking for.
pub fn cmdline_has(word: &str) -> bool {
    cmdline().split_whitespace().any(|arg| arg == word)
}
