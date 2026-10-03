//! Dispatch to the machine-dependent module for the target architecture.
//!
//! This module is the machine layer's *facade*: the submodules below it are
//! private, and the only things reachable from here are the two facts the
//! wiring in `main.rs` needs before a HAL call can exist - where the linker put
//! the image, and whether a breakpoint trap came back - plus the register-file
//! table the M1 check asserts against the ABI's. Everything else the kernel does
//! with the machine goes through `crate::hal`, and `cargo xtask depgraph` fails
//! on any module but `arch`/`hal`/`main`/`lib` naming this one
//! (`PLAN.md` §11.4).
//!
//! Keeping the facade this small is the point: a new caller cannot reach
//! `arch::x86_64::paging` by accident, because it is not public. That is
//! defence in depth behind the ratchet, not a replacement for it - the ratchet
//! catches a `crate::arch::` reference, and this catches the *name*.

#[cfg(target_arch = "x86_64")]
pub(crate) mod x86_64;

/// Where the linker put each part of the kernel image, as virtual addresses.
///
/// The plan in `mm::vm` says what permissions each section gets; only the
/// machine half knows their names. The boot path installs the tables from this
/// before an allocator exists, which is why it is reached directly rather than
/// through the HAL.
#[cfg(target_arch = "x86_64")]
pub use x86_64::kernel_sections;

/// How many `#BP` traps have been caught, for the M1 milestone check.
///
/// A milestone instrument rather than a machine capability: it counts, and
/// `main.rs` asserts on the count to prove the IDT is loaded and returning.
#[cfg(target_arch = "x86_64")]
pub use x86_64::idt::breakpoints_caught;

/// The Linux register-file field order, as this machine writes it out, for the
/// M1 check that it agrees with `syscall/linux/abi.rs` (see
/// `arch/x86_64/context.rs` for why the two tables are duplicated).
#[cfg(target_arch = "x86_64")]
pub use x86_64::context::{ABI_REG_COUNT, ABI_REG_INDEX};
