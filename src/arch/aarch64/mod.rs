//! aarch64 machine-dependent code.
//!
//! Everything in here may touch aarch64 specifics; nothing outside `arch::` may.
//!
//! This is the second machine (`PLAN.md` §9.1). Stage 1a lands the smallest
//! body that links and runs: the section symbols `mm::vm`'s mapping plan needs,
//! the critical section, and an entry that parks the core. The console arrives
//! with stage 1b - it is deliberately absent here rather than stubbed, because
//! a `boot` that prints nothing and a `boot` that cannot print look identical
//! from the outside and only one of them is this.
//!
//! What is *not* here yet, and where it lands: the PL011 writer and the banner
//! (1b), the exception vectors and GIC (1c), the page tables (1d), the loader's
//! arm (1e). Everything above the seam reaches these through `crate::hal`, which
//! this machine implements trait by trait as its stages land.

pub(crate) mod critical;

use crate::mm::vm::KernelSections;

/// Where the linker script put each part of the kernel image, as virtual
/// addresses.
///
/// The aarch64 half of the seam, and the same shape as `arch/x86_64`'s: the
/// plan in `mm::vm` says what permissions each section gets, and only here does
/// anything know their names. The linker script spells these symbols
/// deliberately name-for-name like the x86_64 one so the two bodies stay
/// comparable.
pub fn kernel_sections() -> KernelSections {
    // These symbols are addresses, not objects - there is no `u8` behind them to
    // read, so take their address and never dereference it.
    unsafe extern "C" {
        static __text_start: u8;
        static __text_end: u8;
        static __rodata_start: u8;
        static __rodata_end: u8;
        static __data_start: u8;
        static kernel_end: u8;
    }

    KernelSections {
        text: (&raw const __text_start as usize)..(&raw const __text_end as usize),
        rodata: (&raw const __rodata_start as usize)..(&raw const __rodata_end as usize),
        // Data runs to the end of the image: `.bss` and the boot stack follow it
        // with no gap, and all three must be writable.
        data: (&raw const __data_start as usize)..(&raw const kernel_end as usize),
    }
}

/// Run the machine from the entry stub's hand-off register. Never returns.
///
/// `x0` is where the firmware's pointer arrives: the device tree on the
/// `-kernel` bring-up path, and - once stage 1e's loader exists - the handoff
/// record `boot/src/handoff.rs` writes, which is the same record `main.rs`
/// receives in `RDI` on x86_64. That the *register* is the whole interface is
/// the seam `src/boot/mod.rs` describes; which record it carries is the stage.
pub fn boot(_handoff: usize) -> ! {
    halt()
}

/// Stop the machine for good.
///
/// `wfi` parks the core until an interrupt, and nothing here has unmasked one
/// yet, so this is a reset-only stop - which is what a bring-up halt and a
/// panic path both want.
pub fn halt() -> ! {
    loop {
        unsafe { core::arch::asm!("wfi", options(nomem, nostack)) };
    }
}
