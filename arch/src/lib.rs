//! The machine layer: the HAL and the two ports behind it.
//!
//! This crate is what used to be `src/hal/` and `src/arch/` in the kernel, and
//! it is a crate now for one reason: a module boundary inside `mpos` could not
//! stop the machine layer from reaching up into the kernel, and a crate
//! boundary can. Everything in here is written against `mpos-vm`,
//! `mpos-bootinfo`, `mpos-fdt`, `mpos-date` and `mpos-err` - leaves both
//! crates depend on - and against nothing else. There is no `crate::sched`,
//! no `crate::mm`, no `crate::println!`: those names are not in scope, so a
//! machine that reaches for one does not link rather than merely offending a
//! convention (`PLAN.md` §14).
//!
//! # The seam, in one paragraph
//!
//! [`hal`] is the *downward* face: capability traits ([`hal::Clock`],
//! [`hal::Paging`], [`hal::Console`], …) that a port implements for
//! [`hal::Current`], plus one forward per capability, so code above names no
//! architecture and pays no indirection - one implementation, no `dyn`, every
//! call monomorphised onto the machine's own body. [`hal::kernel`] is the
//! *upward* face: the questions a trap entry has to ask the kernel ("is this
//! page one to populate?", "charge this tick"), declared here and answered by
//! the kernel through one registered `&'static dyn Kernel`. The asymmetry is
//! deliberate and argued in that module (`PLAN.md` §14.2): the downward path is
//! hot and stays monomorphised, the upward path is a fault, a tick or a log
//! line and can afford one indirect call.
//!
//! [`arch`] is the two ports: `x86_64` and `aarch64`, selected by
//! `#[cfg(target_arch)]` in exactly one place (`crate::arch`'s module list).
//! A port implements the capability traits and owns everything that *is* the
//! machine - the descriptor tables, the exception vectors, the page-table
//! format, the UART, the register layout the Linux ABI speaks.
//!
//! # What the kernel sees
//!
//! `mpos` re-exports both modules under the names they already had
//! (`pub use mpos_arch::{arch, hal}`), so every `crate::hal::…` call site and
//! its surrounding commentary stayed true across the split - the same
//! treatment the leaf crates got. The facade is what keeps the rest of the
//! kernel from having to know this crate exists; what the split *bought* is
//! that the machine layer can no longer know the kernel does.

#![no_std]
#![cfg_attr(
    all(target_os = "none", target_arch = "x86_64"),
    feature(abi_x86_interrupt)
)]

extern crate alloc;

/// The two ports, behind one facade.
///
/// The submodules are private and the re-exports in `arch/mod.rs` are the whole
/// of what the wiring layer may name: where the linker put the image, which
/// machine's body exists for this build, and the one call that turns the
/// loader's handoff into a running machine.
#[cfg(target_os = "none")]
pub mod arch;
/// The machine layer as the *bootloader* sees it: the page tables it builds
/// before the jump, the wire its progress lines go out on, and the last
/// instruction.
///
/// Compiled for UEFI only, which is the one place this crate's gate on the
/// target's operating system is not `target_os = "none"`. The loader is a UEFI
/// application and the kernel is not, so this is the machine code a loader
/// build needs and `arch` and `hal` are what a kernel build needs; see the
/// module's own documentation for why the two must not be merged, and `PLAN.md`
/// §14.6 for the change to that gate when the kernel becomes a UEFI stub itself.
#[cfg(target_os = "uefi")]
pub mod early;
/// The hardware abstraction layer: the traits a machine implements, the
/// forwards everything above calls, and the kernel services the machine asks
/// back for. See the module's own documentation for the shape and for what it
/// deliberately does not abstract.
#[cfg(target_os = "none")]
pub mod hal;
