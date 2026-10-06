//! Dispatch to the machine-dependent module for the target architecture.
//!
//! This module is the machine layer's *facade*: the submodules below it are
//! private, and the only things reachable from here are the facts the wiring in
//! `main.rs` needs before a HAL call can exist - where the linker put the image,
//! the one call that turns the entry register into a running machine, and the
//! machine's own wire for the lines that must reach the host before the console
//! service exists - plus, on x86_64, whether a breakpoint trap came back and
//! the register-file table the M1 check asserts against the ABI's. Everything
//! else the kernel does with the machine goes through `crate::hal`, and
//! `cargo xtask depgraph` fails on any module but `arch`/`hal`/`main`/`lib`
//! naming this one (`PLAN.md` §11.4).
//!
//! Keeping the facade this small is the point: a new caller cannot reach
//! `arch::aarch64::pl011` by accident, because it is not public. That is
//! defence in depth behind the ratchet, not a replacement for it - the ratchet
//! catches a `crate::arch::` reference, and this catches the *name*.
//!
//! # Two machines, one facade
//!
//! The port (`PLAN.md` §9.1) adds a body rather than a branch: `arch/aarch64/`
//! is to aarch64 what `arch/x86_64/` is to x86_64, and the `#[cfg]`s here are
//! the *seam* - which of the two bodies exists for this build - not the machine
//! code itself. Every item is gated to the architecture whose module defines
//! it, so neither machine is compiled with the other's names in scope.

#[cfg(target_arch = "x86_64")]
pub(crate) mod x86_64;

/// The aarch64 machine body. Stages 1a-1e of `PLAN.md` §9.1 land in here:
/// PL011 and the entry first, then the exception vectors and GIC, then the
/// page tables and the loader's arm.
#[cfg(target_arch = "aarch64")]
pub(crate) mod aarch64;

/// Where the linker put each part of the kernel image, as virtual addresses,
/// on aarch64. The linker script spells the section symbols name for name like
/// the x86_64 one, so this is the same item with a different body.
#[cfg(target_arch = "aarch64")]
pub use aarch64::kernel_sections;

/// Where the linker put each part of the kernel image, as virtual addresses.
///
/// The plan in `mm::vm` says what permissions each section gets; only the
/// machine half knows their names. The boot path installs the tables from this
/// before an allocator exists, which is why it is reached directly rather than
/// through the HAL.
#[cfg(target_arch = "x86_64")]
pub use x86_64::kernel_sections;

/// Run the machine from the entry stub's hand-off register. Never returns.
///
/// The aarch64 counterpart of `_start`'s body on x86_64: the assembly stub has
/// done the one thing Rust cannot - put a stack under the frame pointer - and
/// this is where the port's own boot sequence begins.
#[cfg(target_arch = "aarch64")]
pub use aarch64::boot;

/// Run the machine from the handoff record the loader left in the entry
/// register. Never returns.
///
/// The same seam as the aarch64 export above, with the same signature and the
/// same three callbacks, which is what makes the kernel's `_start` one piece of
/// architecture-independent code: it registers the kernel services and hands the
/// machine this pointer set, and which body runs is the build's business.
#[cfg(target_arch = "x86_64")]
pub use x86_64::boot;

/// Put a formatted line on the machine's boot console, through the machine's own
/// writer rather than `crate::console`.
///
/// This is what the entry banner, the milestone checks and the panic path use, so
/// a line reaches the wire at any point in boot - before the console service is
/// brought up, and in a panic where its state may not be trustworthy. The service
/// itself (`mpos::println!`) came to this machine at stage 4a and is exercised by
/// the `milestone` `boot` runs; this low-level writer stays for the early and
/// unwinding paths.
#[cfg(target_arch = "aarch64")]
pub use aarch64::print_args;

/// Put a formatted line on the machine's wire, without the console service.
///
/// The first machine's answer to the same question, so the wiring layer's banner
/// and the panic path are one piece of code: a line that must reach the host at
/// any point in boot, including before `crate::console` exists and in a panic
/// where its state cannot be trusted. What differs between the two is only the
/// UART and its newline rule, and both are the machine's own business.
#[cfg(target_arch = "x86_64")]
pub use x86_64::print_args;

/// Stop the core for good. The panic path's last act, and the bring-up halt.
#[cfg(target_arch = "aarch64")]
pub use aarch64::halt;

/// Arm the console's interrupt and let the console queue into it. See
/// `aarch64::enable_console_interrupts`: the PL011 is this machine's own
/// console rather than a driver above it, so the wiring asks here rather than
/// naming `drivers::serial` as the x86_64 arm does.
#[cfg(target_arch = "aarch64")]
pub use aarch64::enable_console_interrupts;

/// Hand the console over to its own interrupts, on the first machine.
///
/// The same call the second machine exports under the same name, and deliberately
/// the same name: what the wiring layer is asking - "the console may use its
/// transmitter's interrupt from here" - has one answer per machine and no
/// architecture in the question. Before this the wiring named `drivers::serial`
/// directly, which was the last thing outside `arch` that had to know which
/// UART this machine has.
#[cfg(target_arch = "x86_64")]
pub use x86_64::enable_console_interrupts;

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
