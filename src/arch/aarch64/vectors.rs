//! The exception vector table, and what each entry does.
//!
//! `VBAR_EL1` points at a 2 KiB-aligned table of sixteen entries, 128 bytes
//! each: four exception *kinds* (synchronous, IRQ, FIQ, SError) for each of
//! four sources (current EL on SP0, current EL on SPx, lower EL in AArch64,
//! lower EL in AArch32). The CPU picks the entry itself, so which one is taken
//! is a fact about where the exception came from rather than something the
//! handler decides - which is why the table has an entry for a case this kernel
//! cannot currently produce (lower EL) instead of leaving it empty.
//!
//! Each entry is a branch to a stub outside the table. The table's entries are
//! 128 bytes apart and an entry that saved registers inline would be one
//! register away from writing into its neighbour; keeping the stubs out of the
//! table is what makes that impossible rather than unlikely.
//!
//! What is saved. The IRQ stub saves `x0..x18`, `x29` and `x30`: exactly the
//! registers a Rust callee may clobber, so preserving those is preserving the
//! interrupted code's architecturally-visible state. `x19..x28` are
//! callee-saved and the handler restores them itself. `PC` and `PSTATE` need no
//! saving at all - the CPU put them in `ELR_EL1` and `SPSR_EL1` on entry, and
//! `eret` restores them - and they cannot be clobbered in between, because
//! taking any exception from EL1 masks IRQs, so nothing here nests.
//!
//! The same reason is why `SPSR_EL1` is not read: the condition flags the
//! interrupted code left are already in it, and `eret` is what puts them back.

use core::arch::global_asm;

use core::fmt::Write;

use super::{gic, halt, pl011, timer};

global_asm!(
    r#"
    .section .text.vectors, "ax"
    .balign 2048
    .globl __aarch64_vectors
__aarch64_vectors:
    .macro ventry target
        .balign 0x80
        b \target
    .endm

    /* Current EL, SP0 - unused: this kernel runs on SPx, and entry above EL0
       never selects the zero stack. */
    ventry __vec_sync
    ventry __vec_irq
    ventry __vec_unexpected
    ventry __vec_unexpected

    /* Current EL, SPx - where the clock lands. */
    ventry __vec_sync
    ventry __vec_irq
    ventry __vec_unexpected
    ventry __vec_unexpected

    /* Lower EL, AArch64 - a user task's faults, once there are user tasks. */
    ventry __vec_sync
    ventry __vec_irq
    ventry __vec_unexpected
    ventry __vec_unexpected

    /* Lower EL, AArch32 - never, on a machine that runs no 32-bit code. */
    ventry __vec_sync
    ventry __vec_irq
    ventry __vec_unexpected
    ventry __vec_unexpected

    .text
    .balign 16
__vec_sync:
    b {synchronous}
__vec_unexpected:
    b {unexpected}
__vec_irq:
    sub sp, sp, #176
    stp x0, x1, [sp, #0]
    stp x2, x3, [sp, #16]
    stp x4, x5, [sp, #32]
    stp x6, x7, [sp, #48]
    stp x8, x9, [sp, #64]
    stp x10, x11, [sp, #80]
    stp x12, x13, [sp, #96]
    stp x14, x15, [sp, #112]
    stp x16, x17, [sp, #128]
    str x18, [sp, #144]
    stp x29, x30, [sp, #152]
    bl {irq}
    ldp x0, x1, [sp, #0]
    ldp x2, x3, [sp, #16]
    ldp x4, x5, [sp, #32]
    ldp x6, x7, [sp, #48]
    ldp x8, x9, [sp, #64]
    ldp x10, x11, [sp, #80]
    ldp x12, x13, [sp, #96]
    ldp x14, x15, [sp, #112]
    ldp x16, x17, [sp, #128]
    ldr x18, [sp, #144]
    ldp x29, x30, [sp, #152]
    add sp, sp, #176
    eret
    "#,
    synchronous = sym synchronous,
    irq = sym irq,
    unexpected = sym unexpected,
);

/// Point `VBAR_EL1` at the table.
pub fn init() {
    // SAFETY: `VBAR_EL1` takes the table's address, whose low eleven bits it
    // ignores; the table is 2048-byte aligned by the `.balign` above, and `isb`
    // is what makes the new value visible before any exception can be taken.
    unsafe {
        core::arch::asm!("msr vbar_el1, {}", in(reg) table(), options(nomem, nostack));
        core::arch::asm!("isb", options(nomem, nostack));
    }
}

/// Where the table is, which is what `VBAR_EL1` holds.
pub fn table() -> u64 {
    unsafe extern "C" {
        static __aarch64_vectors: u8;
    }
    &raw const __aarch64_vectors as u64
}

/// An interrupt: ask the controller which one, dispatch, retire it.
///
/// The EOI happens after the handler's work rather than before it, because the
/// timer's line stays asserted until it has been re-armed; retiring first would
/// let the controller deliver the same interrupt again the moment it is
/// unmasked.
extern "C" fn irq() {
    let intid = gic::acknowledge();
    if intid == gic::TIMER_INTID {
        timer::tick();
    }
    if intid != gic::SPURIOUS {
        gic::end_of_interrupt(intid);
    }
}

fn system_register(name: &str) -> u64 {
    let value: u64;
    // SAFETY: each name below is a read-only EL1 system register; reading one
    // has no side effects. Spelled as a match rather than four copies of the
    // same asm so the `mrs` operand is a literal, which the assembler requires.
    unsafe {
        match name {
            "esr" => core::arch::asm!("mrs {}, ESR_EL1", out(reg) value, options(nomem, nostack)),
            "elr" => core::arch::asm!("mrs {}, ELR_EL1", out(reg) value, options(nomem, nostack)),
            "far" => core::arch::asm!("mrs {}, FAR_EL1", out(reg) value, options(nomem, nostack)),
            "spsr" => core::arch::asm!("mrs {}, SPSR_EL1", out(reg) value, options(nomem, nostack)),
            _ => unreachable!(),
        }
    }
    value
}

/// A synchronous exception this kernel does not handle.
///
/// There is nothing to return to: by the time one of these arrives the fault is
/// a fact about the machine (a bad page table, an unaligned access) or about a
/// bug, and the machine has no way back. It reports the three registers that
/// say *what* and *where* and stops, rather than letting the CPU take a second
/// fault and disappear without a word.
extern "C" fn synchronous() -> ! {
    let esr = system_register("esr");
    let elr = system_register("elr");
    let far = system_register("far");
    let mut out = pl011::Writer;
    let _ = writeln!(out, "\nEXCEPTION: synchronous");
    // The class is the top six bits, and it is the only field whose meaning is
    // fixed across every cause; the rest depends on what the class says.
    let _ = writeln!(out, "  ESR_EL1  {esr:#018x} (EC {:#04x})", esr >> 26);
    let _ = writeln!(out, "  ELR_EL1  {elr:#018x}");
    let _ = writeln!(out, "  FAR_EL1  {far:#018x}");
    halt()
}

/// FIQ or SError. Neither is enabled on this machine, so either one means an
/// interrupt was raised that nothing here asked for.
extern "C" fn unexpected() -> ! {
    let esr = system_register("esr");
    let elr = system_register("elr");
    let spsr = system_register("spsr");
    let mut out = pl011::Writer;
    let _ = writeln!(out, "\nEXCEPTION: FIQ or SError (neither is enabled)");
    let _ = writeln!(out, "  ESR_EL1  {esr:#018x} (EC {:#04x})", esr >> 26);
    let _ = writeln!(out, "  ELR_EL1  {elr:#018x}");
    let _ = writeln!(out, "  SPSR_EL1 {spsr:#018x}");
    halt()
}

/// Whether this core would take an interrupt right now: the `I` bit of `DAIF`,
/// clear when IRQs are enabled.
pub fn interrupts_enabled() -> bool {
    let daif: u64;
    // SAFETY: `DAIF` is a readable system register; reading it has no side
    // effects. Bit 7 is `I`.
    unsafe {
        core::arch::asm!("mrs {}, daif", out(reg) daif, options(nomem, nostack));
    }
    daif & (1 << 7) == 0
}

/// Let the core take interrupts.
pub fn enable_interrupts() {
    // SAFETY: `daifclr` with the 4-bit field `#2` clears `I` (bit 1 of the
    // field), which is the interrupt mask this kernel wants open.
    unsafe {
        core::arch::asm!("msr daifclr, #2", options(nomem, nostack));
    }
}
