//! The kernel image: the machine's entry point, and the wiring for the boot.
//!
//! The binary is deliberately thin and arch-dispatched. What must run on the
//! machine is the whole of it, and "the machine" is not one machine any more:
//! the boot entry, the milestone checks and the panic handler are written
//! against the ABI of a particular CPU - `_start` receives its record in `RDI`
//! on x86_64 and in `x0` on aarch64, and the two panics print through different
//! consoles. Everything reusable lives in the `mpos` library, which the host
//! can unit-test without a machine at all.
//!
//! So this file holds no body of its own: it names the one that belongs to the
//! target the build is for, exactly as `src/arch/mod.rs` does for the library.
//! A machine that has no body yet fails to link rather than silently building
//! an image whose entry point cannot run - the same loud failure `PLAN.md`
//! §9.1 asks for.

#![no_std]
#![no_main]

extern crate alloc;

#[cfg(target_arch = "x86_64")]
#[path = "main/x86_64.rs"]
mod body;

#[cfg(target_arch = "aarch64")]
#[path = "main/aarch64.rs"]
mod body;

#[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
compile_error!(
    "no kernel entry for this target: add src/main/<arch>.rs and a `#[path]` arm here, \
     and see PLAN.md §9.1 for what a port needs"
);
