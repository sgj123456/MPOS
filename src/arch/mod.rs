//! Dispatch to the machine-dependent module for the target architecture.
//!
//! Each supported architecture gets a submodule here exporting the same surface
//! (`init_interrupts`, `halt`, `idle`, `context`, `paging`, `timer`,
//! `interrupts`, ...), and nothing outside this directory ever names an
//! architecture-specific module. The per-arch names differ where the hardware
//! does (`idt` vs `trap` for the descriptor table), so both are re-exported
//! under the uniform names: `interrupts` for trap entry, `timer` for the clock.
//! The aarch64 entry is added when its modules are written rather than now:
//! declaring a module whose file does not exist yet breaks rustfmt's module
//! resolution for the whole crate.

#[cfg(target_arch = "x86_64")]
pub mod x86_64;

#[cfg(target_arch = "x86_64")]
pub use x86_64::idt as interrupts;
#[cfg(target_arch = "x86_64")]
pub use x86_64::*;
