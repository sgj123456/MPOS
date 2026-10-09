//! One body per machine, for the parts of the boot that are this machine's.
//!
//! The kernel's entry and its panic handler are architecture-independent and live
//! in `src/main.rs`. What is left over is what only a machine can answer - how
//! this one is brought up, which devices it has, and what its own acceptance
//! checks are - and that is one body per port, chosen here in exactly one place
//! and nowhere else. That is the same arrangement `arch/src/arch/mod.rs` uses for
//! the machine layer, and it is deliberate: a `#[cfg]` on every item inside one
//! file would be two files wearing one hat.
//!
//! The bodies live in the kernel crate rather than in `mpos-arch` because most of
//! what they do is *not* the machine's. The checks drive `vfs`, `blk`, `net` and
//! `drivers`, and the handoff loads the first program; all of that sits above the
//! machine layer, and a crate that must not depend on the kernel cannot name any
//! of it (`AGENTS.md` §3, downward-only). What the machine layer *does* own - the
//! entry, the descriptor tables, the page tables, the wire, the device
//! description - moved into `mpos-arch` with `boot`, which is where the three
//! callbacks below are invoked from.
//!
//! The half of a body that is *not* this machine's - the steps every port's
//! `init` runs, and the ordering contracts in them - is [`steps`], next to the
//! bodies that call it, so a body here holds only what would have to be answered
//! differently on a different machine (`NOTES.md` §14.9).

mod checks;
mod report;
mod steps;

#[cfg(target_arch = "x86_64")]
mod x86_64;

#[cfg(target_arch = "aarch64")]
mod aarch64;

/// Run this machine's kernel from the loader's record. Never returns.
///
/// # Safety
///
/// As `mpos::arch::boot`, which is what this hands the record to.
#[cfg(target_arch = "x86_64")]
pub use x86_64::start;

/// Run this machine's kernel from the loader's record. Never returns.
///
/// # Safety
///
/// As `mpos::arch::boot`, which is what this hands the record to.
#[cfg(target_arch = "aarch64")]
pub use aarch64::start;

/// What the panic handler prints about the context it died in.
///
/// One definition for both machines ([`report`]): the walk is a function of the
/// stack pointer, the kernel's text range and the scheduler's record of the
/// running task, and none of those is a machine's to answer differently. What a
/// machine *does* contribute is `hal::stack_pointer` - the register read - and
/// the floor its own traps recover to.
pub use report::panic_context;

/// A machine with no body fails to link rather than silently building an image
/// whose entry point cannot run - the same loud failure `NOTES.md` §9.1 asks for.
#[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
compile_error!(
    "no kernel body for this target: add src/board/<arch>.rs and a `mod` arm here, \
     and see NOTES.md §9.1 for what a port needs"
);
