//! aarch64 machine-dependent code.
//!
//! Everything in here may touch aarch64 specifics; nothing outside `arch::` may.
//!
//! This is the second machine (`PLAN.md` §9.1). Stage 1a landed the build
//! plumbing and a body that links; stage 1b is the one that *speaks* - a PL011
//! writer and the boot banner - which is what proves the toolchain, the linker
//! script, the gate list and QEMU's wiring all agree rather than each merely
//! compiling.
//!
//! What is still absent, and where it lands: the exception vectors and the GIC
//! (1c), the page tables (1d), the loader's arm (1e). Everything above the seam
//! reaches those through `crate::hal`, which this machine implements trait by
//! trait as its stages land - which is also why `hal` itself is still x86_64-only
//! (`src/lib.rs`'s ledger).

pub(crate) mod critical;
pub(crate) mod pl011;

use core::fmt;

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

/// Put a formatted line on the machine's boot console.
///
/// The console mux `mpos::println!` reaches is x86_64's until stage 4, so this
/// is the one door the entry and the panic path use. It is a bypass of the log
/// ring on purpose: both callers run before (or instead of) anything the ring
/// could carry.
pub fn print_args(args: fmt::Arguments) {
    use fmt::Write;
    let _ = pl011::Writer.write_fmt(args);
}

/// Run the machine from the entry stub's hand-off register. Never returns.
///
/// `x0` is the register the interface is defined in: once stage 1e's loader
/// exists it will hold the handoff record `boot/src/handoff.rs` writes, which is
/// the same record `main.rs` receives in `RDI` on x86_64. That the *register* is
/// the whole interface is the seam `src/boot/mod.rs` describes; which record it
/// carries is the stage.
///
/// On the `-kernel` bring-up path it carries zero, which is measured rather
/// than assumed: QEMU 11.1.1 with `-M virt` and a bare aarch64 ELF starts the
/// image with `x0 = 0` and `sp = 0` (QEMU's own `-d cpu` log and this banner
/// agree). A device tree is not handed over on this path - AAVMF will pass one
/// through the UEFI configuration table at stage 1e, and stage 5's parser reads
/// it from there. Nothing here depends on the register, which is why the port is
/// not weakened by the difference.
pub fn boot(handoff: usize) -> ! {
    pl011::init();
    banner(handoff);
    halt()
}

/// Say who this is and what it was given, then get out of the way.
///
/// Deliberately the same shape as the x86_64 banner (`src/main/x86_64.rs`), so
/// the two machines' first lines are comparable rather than merely both
/// present - and so that a broken linker script shows up as a wrong number here
/// rather than as a fault somewhere later.
fn banner(handoff: usize) {
    use fmt::Write;

    let sections = kernel_sections();
    let image = sections.data.end - sections.text.start;

    let mut out = pl011::Writer;
    let _ = writeln!(out, "mpos {} (aarch64) booting", env!("CARGO_PKG_VERSION"));
    // The address is fixed for this one machine and named as such; the device
    // tree is what stage 5 reads instead, and it comes from the UEFI
    // configuration table rather than from `x0` (see `boot`).
    let _ = writeln!(out, "console: PL011 at {:#x}", pl011::BASE);
    let _ = writeln!(
        out,
        "image: text {} KiB, rodata {} KiB, data {} KiB ({} KiB total)",
        (sections.text.end - sections.text.start) / 1024,
        (sections.rodata.end - sections.rodata.start) / 1024,
        (sections.data.end - sections.data.start) / 1024,
        image / 1024,
    );
    // Printed because it *is* the machine's contract with its loader, and this
    // stage is the one that establishes what the `-kernel` path actually puts
    // there rather than what it is supposed to.
    let _ = writeln!(out, "x0 at entry: {handoff:#018x}");
    let _ = writeln!(out, "exception level: EL{}", current_el());
    let _ = writeln!(
        out,
        "aarch64: stage 1b bring-up (MMU off, no interrupts); halting"
    );
}

/// Which exception level this code is running at.
///
/// Read rather than assumed, and read *now* rather than when it matters: which
/// level the entry arrives at depends on the firmware and the `-cpu`, and a
/// paging or timer path that assumed EL1 would be wrong on a machine that
/// starts at EL2 (`PLAN.md` §9.1's risk list). Printing it here is the cheapest
/// way to keep that assumption from being made.
fn current_el() -> u64 {
    let current: u64;
    // SAFETY: `CurrentEL` is a read-only system register, readable at every
    // exception level; the read has no side effects.
    unsafe {
        core::arch::asm!("mrs {}, CurrentEL", out(reg) current, options(nomem, nostack));
    }
    (current >> 2) & 0b11
}

/// Stop the machine for good.
///
/// `wfi` parks the core until an interrupt, and nothing here has unmasked one
/// yet, so this is a reset-only stop - which is what a bring-up halt and a
/// panic path both want. Stage 1c's clock is what makes it a real idle.
pub fn halt() -> ! {
    loop {
        unsafe { core::arch::asm!("wfi", options(nomem, nostack)) };
    }
}
