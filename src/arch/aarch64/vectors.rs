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
//!
//! What is saved *additionally* for a trap from EL0 - and why. The rule above
//! is the ABI's rule for a call, and it holds only for code that agreed to be
//! called. A program did not: it is interrupted wherever it stands, so an
//! interrupt or a system call that lands on it has to give back everything it
//! was using, the *caller-saved* registers included. The integer half is
//! already there - the sync stub saves all thirty-one - but the floating-point
//! half was not, and on this machine that is a real loss rather than a
//! tidiness: this kernel is built for `aarch64-unknown-none`, whose baseline
//! includes NEON, so LLVM keeps its own values in `q0..q31`. A program's SIMD
//! state was therefore destroyed by its own `svc`, and by every timer tick that
//! landed on it. Both EL0 stubs now carry the 528 bytes (`q0..q31`, `FPCR`,
//! `FPSR` - the same shape `mpos_context_switch` moves) in their own frame.
//! The kernel's entries do not pay for it, because for EL1 the call rule *is*
//! the truth: `irq` preserves `x19..x28` itself, and its caller-saved registers
//! are dead by definition.
//!
//! Saving it on the trap's own stack, rather than in the task's record, is what
//! makes it survive a blocking call: a task parked in `read` is switched out and
//! back while the trap is still open, and the context switch writes the *live*
//! registers (the kernel's scratch) into the record as it goes past.

use core::arch::global_asm;

use core::fmt::Write;

use super::context::{self, UserFrame};
use super::{gic, halt, pl011, timer};

/// The `ESR_EL1` exception class for a `svc` taken from AArch64.
///
/// `ESR_EL1.EC`, the top six bits, and the only field whose meaning does not
/// depend on the class itself - which is why it is what a handler reads first.
const EC_SVC_FROM_AARCH64: u64 = 0x15;
/// `ESR_EL1.EC` for a Data Abort taken from a lower exception level (EL0), and an
/// Instruction (prefetch) Abort from EL0 - the two page faults a running program
/// takes. `WnR`, **bit 6** of a Data Abort's ISS, says which way the access went:
/// 1 is a write. (Bit 7 is RES0 there - AArch64's ESR does not put `WnR` where
/// AArch32's `DFSR` does, and reading bit 7 answers "read" for every fault this
/// kernel takes. Measured: the write fault that killed the aarch64 test sweep is
/// `ESR 0x9200004f` - `DFSC` `0xF`, a level-3 *permission* fault on a page the
/// tables call `r--u` and `is_cow_page` calls copy-on-write, so the access can
/// only have been a write, and bit 6 is set while bit 7 is clear.)
const EC_DATA_ABORT_EL0: u64 = 0x24;
const EC_INSTR_ABORT_EL0: u64 = 0x20;

/// `exit_group`'s number on this machine, which is not the number the x86_64
/// table gives it (231, against 94 here).
///
/// Spelled out rather than taken from `crate::syscall::linux::nr` because that
/// table *is* x86_64's - the ABI's numbers are per-machine and this machine's
/// do not exist yet. It is the one call this stage's gate answers, and it
/// answers it because a program that has finished has to be able to say so.
const NR_EXIT_GROUP: u64 = 94;

/// Where the interrupted floating-point/SIMD state sits inside the EL0 *sync*
/// frame: immediately after the [`UserFrame`] the gate is handed, so the
/// register offsets `context.rs` asserts - and every reader of a `UserFrame` -
/// are unchanged by it being there.
///
/// The stubs below spell these numbers as assembly literals, which an assembler
/// cannot check against a Rust layout, so the check is here (the same shape as
/// `UserFrame`'s own offset asserts): the failure mode is a program's registers
/// read from the middle of a saved `q` register, which reads as a corrupt
/// syscall argument rather than as a layout bug.
pub const TRAP_FPU_OFFSET: usize = size_of::<UserFrame>();

/// The bytes one floating-point save moves: thirty-two 16-byte `q` registers,
/// then `FPCR` and `FPSR`, in the order `context.rs`'s `FpuState` holds them.
pub const TRAP_FPU_BYTES: usize = 528;

/// The EL0 sync frame: the `UserFrame`, then the floating-point state.
pub const TRAP_FRAME_BYTES: usize = TRAP_FPU_OFFSET + TRAP_FPU_BYTES;

/// Where the floating-point state sits inside the EL0 *interrupt* frame: after
/// the 176 bytes the kernel's own IRQ stub saves, which is the whole of the
/// difference between the two stubs.
pub const TRAP_IRQ_FPU_OFFSET: usize = 176;

/// The EL0 interrupt frame.
pub const TRAP_IRQ_FRAME_BYTES: usize = TRAP_IRQ_FPU_OFFSET + TRAP_FPU_BYTES;

const _: () = {
    assert!(
        TRAP_FPU_OFFSET == 272,
        "the EL0 sync stub's floating-point offset moved"
    );
    assert!(
        TRAP_FRAME_BYTES == 800,
        "the EL0 sync stub's frame size moved"
    );
    assert!(
        TRAP_IRQ_FPU_OFFSET == 176,
        "the EL0 IRQ stub's floating-point offset moved"
    );
    assert!(
        TRAP_IRQ_FRAME_BYTES == 704,
        "the EL0 IRQ stub's frame size moved"
    );
    assert!(
        TRAP_FPU_BYTES == 528,
        "the trap frame's floating-point save area moved"
    );
};

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

    /* Lower EL, AArch64 - where a user task's system call arrives, and where
       its faults do. It is the only entry in the table that has two different
       jobs, and the stub below is the one that tells them apart. Its IRQ entry
       is separate from the kernel's own because an interrupt that lands on a
       program has to give its floating-point registers back too. */
    ventry __vec_el0_sync
    ventry __vec_el0_irq
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

    /* An interrupt that lands on a *program*. It is the stub above plus the
       floating-point half, and it exists because the two differ in exactly
       that: the kernel's own code has no interest in the vector registers of
       the task it interrupted (a call may destroy the caller-saved ones, and
       the callee-saved ones are `irq`'s to keep), while a program's registers
       are its own across an interrupt whatever the ABI says about calls. The
       528 bytes are `q0..q31`, `FPCR` and `FPSR` in the same order
       `mpos_context_switch` saves them, at the offset [`TRAP_FPU_OFFSET`]
       names; the integer half above is the same 176 bytes the sibling stub
       uses, so the frame's first half is identical on both entries. */
    .balign 16
__vec_el0_irq:
    sub sp, sp, #704
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
    stp q0,  q1,  [sp, #176]
    stp q2,  q3,  [sp, #208]
    stp q4,  q5,  [sp, #240]
    stp q6,  q7,  [sp, #272]
    stp q8,  q9,  [sp, #304]
    stp q10, q11, [sp, #336]
    stp q12, q13, [sp, #368]
    stp q14, q15, [sp, #400]
    stp q16, q17, [sp, #432]
    stp q18, q19, [sp, #464]
    stp q20, q21, [sp, #496]
    stp q22, q23, [sp, #528]
    stp q24, q25, [sp, #560]
    stp q26, q27, [sp, #592]
    stp q28, q29, [sp, #624]
    stp q30, q31, [sp, #656]
    mrs x9, fpcr
    mrs x10, fpsr
    str x9, [sp, #688]
    str x10, [sp, #696]
    bl {irq}
    ldp q0,  q1,  [sp, #176]
    ldp q2,  q3,  [sp, #208]
    ldp q4,  q5,  [sp, #240]
    ldp q6,  q7,  [sp, #272]
    ldp q8,  q9,  [sp, #304]
    ldp q10, q11, [sp, #336]
    ldp q12, q13, [sp, #368]
    ldp q14, q15, [sp, #400]
    ldp q16, q17, [sp, #432]
    ldp q18, q19, [sp, #464]
    ldp q20, q21, [sp, #496]
    ldp q22, q23, [sp, #528]
    ldp q24, q25, [sp, #560]
    ldp q26, q27, [sp, #592]
    ldp q28, q29, [sp, #624]
    ldp q30, q31, [sp, #656]
    ldr x9, [sp, #688]
    ldr x10, [sp, #696]
    msr fpcr, x9
    msr fpsr, x10
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
    add sp, sp, #704
    eret

    /* A trap from EL0. Unlike the IRQ stub this one has to save *everything*,
       because the gate behind it can resume the program and a register it
       clobbered without putting back is a register the program loses. The 34
       words are `context::UserFrame` and nothing else: x0..x30, then the three
       status words the trap put in `SP_EL0`, `ELR_EL1` and `SPSR_EL1`, which
       are part of the same frame because a `fork`, a signal and `ptrace` all
       read them there.

       The gate answers whether to resume the program or to leave for the
       kernel, and the two exits are the two `eret`s: this one is the only place
       in the kernel where one `eret` can return to either exception level, and
       the choice is the gate's - `SPSR_EL1`'s mode field is what it writes. */
    .balign 16
__vec_el0_sync:
    sub sp, sp, #800
    stp x0, x1, [sp, #0]
    stp x2, x3, [sp, #16]
    stp x4, x5, [sp, #32]
    stp x6, x7, [sp, #48]
    stp x8, x9, [sp, #64]
    stp x10, x11, [sp, #80]
    stp x12, x13, [sp, #96]
    stp x14, x15, [sp, #112]
    stp x16, x17, [sp, #128]
    stp x18, x19, [sp, #144]
    stp x20, x21, [sp, #160]
    stp x22, x23, [sp, #176]
    stp x24, x25, [sp, #192]
    stp x26, x27, [sp, #208]
    stp x28, x29, [sp, #224]
    str x30, [sp, #240]
    mrs x9, sp_el0
    mrs x10, elr_el1
    mrs x11, spsr_el1
    stp x9, x10, [sp, #248]
    str x11, [sp, #264]

    /* And the floating-point/SIMD state the trap interrupts, in the 528 bytes
       after the `UserFrame` - the reason this stub reserves 800 and not 272.
       It is not optional on this machine the way it is on x86_64: this kernel
       is built for `aarch64-unknown-none`, whose baseline includes NEON, so
       LLVM keeps its own values in `q0..q31` and a `svc` that reached Rust
       without this would hand the program back the kernel's scratch. (`core`'s
       formatting and `memcpy` do it too, which is why building this crate
       alone with `-neon` does not help.) Saving it on the trap's own stack
       rather than in the task's record is what makes it survive a *blocking*
       call: the record is the one the context switch writes on its way past,
       and a task parked inside `read` is switched out and back while it
       waits. */
    stp q0,  q1,  [sp, #272]
    stp q2,  q3,  [sp, #304]
    stp q4,  q5,  [sp, #336]
    stp q6,  q7,  [sp, #368]
    stp q8,  q9,  [sp, #400]
    stp q10, q11, [sp, #432]
    stp q12, q13, [sp, #464]
    stp q14, q15, [sp, #496]
    stp q16, q17, [sp, #528]
    stp q18, q19, [sp, #560]
    stp q20, q21, [sp, #592]
    stp q22, q23, [sp, #624]
    stp q24, q25, [sp, #656]
    stp q26, q27, [sp, #688]
    stp q28, q29, [sp, #720]
    stp q30, q31, [sp, #752]
    mrs x9, fpcr
    mrs x10, fpsr
    str x9, [sp, #784]
    str x10, [sp, #792]

    mov x0, sp
    bl {el0_sync}
    cbnz x0, __vec_el0_leave

    /* The status words go back first, through a scratch register the restore
       chain below is about to overwrite from the frame - which is the only
       window in which they can be used, and the reason this block is here and
       not at the end. */
    ldr x9, [sp, #248]
    msr sp_el0, x9
    ldr x9, [sp, #256]
    msr elr_el1, x9
    ldr x9, [sp, #264]
    msr spsr_el1, x9

    /* The floating-point half, through `x9`/`x10` - which has to come *before*
       the register restore below, because that restore gives `x9` and `x10`
       back to the program and would otherwise hand it a `FPCR` in one of them.
       Measured: with the two blocks the other way round, six of the sixteen
       sweep programs took a wild pointer (`user fault at 0x19`, an `EFAULT` from
       `recvfrom`) because a rustix asm block had kept a live value in `x9`
       across the `svc`. */
    ldp q0,  q1,  [sp, #272]
    ldp q2,  q3,  [sp, #304]
    ldp q4,  q5,  [sp, #336]
    ldp q6,  q7,  [sp, #368]
    ldp q8,  q9,  [sp, #400]
    ldp q10, q11, [sp, #432]
    ldp q12, q13, [sp, #464]
    ldp q14, q15, [sp, #496]
    ldp q16, q17, [sp, #528]
    ldp q18, q19, [sp, #560]
    ldp q20, q21, [sp, #592]
    ldp q22, q23, [sp, #624]
    ldp q24, q25, [sp, #656]
    ldp q26, q27, [sp, #688]
    ldp q28, q29, [sp, #720]
    ldp q30, q31, [sp, #752]
    ldr x9, [sp, #784]
    ldr x10, [sp, #792]
    msr fpcr, x9
    msr fpsr, x10

    ldp x0, x1, [sp, #0]
    ldp x2, x3, [sp, #16]
    ldp x4, x5, [sp, #32]
    ldp x6, x7, [sp, #48]
    ldp x8, x9, [sp, #64]
    ldp x10, x11, [sp, #80]
    ldp x12, x13, [sp, #96]
    ldp x14, x15, [sp, #112]
    ldp x16, x17, [sp, #128]
    ldp x18, x19, [sp, #144]
    ldp x20, x21, [sp, #160]
    ldp x22, x23, [sp, #176]
    ldp x24, x25, [sp, #192]
    ldp x26, x27, [sp, #208]
    ldp x28, x29, [sp, #224]
    ldr x30, [sp, #240]
    add sp, sp, #800
    eret

    /* Not asking to come back: the gate has already set `SP_EL1`, `ELR_EL1` and
       `SPSR_EL1` for the kernel, and `x0` holds the exit status it wants
       delivered - so the frame below is dead, and touching it would put the
       program's registers back over a kernel that has stopped being one. */
__vec_el0_leave:
    eret
    "#,
    synchronous = sym synchronous,
    irq = sym irq,
    unexpected = sym unexpected,
    el0_sync = sym el0_sync,
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

/// A `svc` from EL0: the gate every system call on this machine comes through.
///
/// What this stage's gate answers is the kernel's own default and one call that
/// ends a program. A number nothing recognises is `-ENOSYS`, which is not a
/// placeholder: it is the same honest refusal the x86_64 dispatch's
/// `_ => ENOSYS` arm makes, and answering it *before* there is a table is right
/// for the reason that rule exists - a caller told "not here" goes down its own
/// fallback, while one told "done" stops asking.
///
/// `exit_group` it does answer, because a program that has finished has to be
/// able to say so, and because leaving is the one thing a refusal cannot
/// express. What that leaves undone is stage 6's next piece rather than
/// something this hides: choosing the next task, and giving the exiting one
/// away.
///
/// Answers zero to resume the program with the frame as the gate left it, and
/// non-zero to leave for the kernel - in which case the frame is dead, and the
/// value is the exit status [`context::leave_for_kernel`] has already arranged
/// to deliver.
extern "C" fn el0_sync(frame: *mut UserFrame) -> u64 {
    // SAFETY: the stub above passes the address of the frame it just saved, and
    // `context.rs`'s offset asserts are what make that frame a `UserFrame`.
    // Nothing else writes it while this runs, and the core is single.
    let frame = unsafe { &mut *frame };

    // The class is read first and everything else depends on it, because it is
    // the one field of `ESR_EL1` whose meaning does not depend on itself. A trap
    // from EL0 that is not an `svc` is a fault in the program - a bad address, a
    // bad instruction - and this kernel has no way to tell a program about one
    // yet, so it says so and stops rather than resuming into the same fault.
    let esr = system_register("esr");
    let class = esr >> 26;
    // A trap from EL0 that is not an `svc` is a fault in the program. x86's twin
    // `page_fault` answers the three faults that are not the program's error - a
    // copy-on-write write after a `fork`, a page of an anonymous mapping never yet
    // touched, and a push past the end of the mapped stack - by taking the page
    // from the allocator and letting the instruction retry. These run on aarch64
    // through the same arch-neutral `crate::sched` resolvers, so a real program
    // (a libc's larger stack, its thread arena, a `fork`'s COW page) can grow its
    // memory the way the 142 x86 musl tests already do. Any fault left after them
    // is the program's own error; Linux's answer is `SIGSEGV`, whose default action
    // is death - delivered here as the task exiting with `-11`, which is what the
    // aarch64 signal-frame path would end at anyway, so it is the honest answer,
    // not a stub that resumes into the same fault forever. Before `sched::started()`
    // (the bring-up EL0 probe) there is no task or VMA to resolve against, so those
    // traps stay fatal.
    if class != EC_SVC_FROM_AARCH64 {
        if crate::sched::started() {
            let addr = system_register("far") as usize;
            let is_write = match class {
                // `WnR` is bit 6 of the Data Abort ISS. Reading bit 7 made every
                // fault look like a read, which meant `resolve_cow_fault` was
                // never reached: a `fork`/`clone` child's first write to a page
                // it shares went to SIGSEGV instead of being split.
                EC_DATA_ABORT_EL0 => (esr >> 6) & 1 != 0,
                EC_INSTR_ABORT_EL0 => false,
                _ => false,
            };
            if class == EC_DATA_ABORT_EL0 || class == EC_INSTR_ABORT_EL0 {
                if is_write && crate::sched::resolve_cow_fault(addr) {
                    return 0; // The COW page split; the instruction retries.
                }
                if crate::sched::resolve_anon_fault(addr, is_write) {
                    return 0; // A demand-paged anon page now backs the address.
                }
                if crate::sched::resolve_stack_growth(addr, frame.sp as usize) {
                    return 0; // The stack grew over the faulting address.
                }
            }
            // Not resolvable: an unhandled SIGSEGV is the task's death. `sp` is
            // printed because a fault 16 bytes below it is a stack push and one
            // far from it is a wild pointer, and telling those apart from the
            // address alone is guesswork.
            crate::println!(
                "user fault at {addr:#x} (ESR {esr:#x}, pc {:#x}, sp {:#x})",
                frame.pc,
                frame.sp
            );
            crate::sched::exit(-11);
            // `exit` reschedules and does not return; the `return 0` below only
            // keeps the type checker happy if it ever did (retrying would refault).
            return 0;
        }
        let mut out = pl011::Writer;
        let _ = writeln!(out, "\nEXCEPTION: from EL0, and not a system call");
        let _ = writeln!(out, "  ESR_EL1  {esr:#018x} (EC {class:#04x})");
        let _ = writeln!(out, "  pc       {:#018x}", frame.pc);
        let _ = writeln!(out, "  pstate   {:#018x}", frame.pstate);
        let _ = writeln!(out, "  FAR_EL1  {:#018x}", system_register("far"));
        halt()
    }

    // `pc` is *not* moved past the `svc`, and that is the architecture's answer
    // rather than an omission: for a Supervisor Call, `ELR_EL1` already holds the
    // address of the instruction *after* it, so the frame the gate is handed
    // resumes where the program meant to carry on. Measured before it was
    // believed - the first run of this check advanced by four anyway, the trace
    // showed the trap reporting `pc` one instruction past the `svc`, and the
    // program's very next instruction was skipped as a result. A gate that
    // advanced *and* a machine that did not would run the `svc` twice.
    //
    // This is the one place the x86_64 twin is shaped differently rather than
    // merely spelled differently: there `rcx` holds the same "after" address and
    // the ABI's `SA_RESTART` rewind is what subtracts the two bytes back.

    // `x8` is the number and `x0`..`x5` the arguments on AArch64, where the
    // x86_64 this kernel was written for puts the number in `rax`.
    let number = frame.x[8];

    // Two contexts share this gate, and the difference is measured, not styled:
    // the bring-up EL0 probe (`check_el0`) enters EL0 straight from the boot code
    // *before* `sched::init`, so there is no current task; every real program
    // runs after the scheduler started and has one. `dispatch` opens on a
    // scheduler read (`current_is_seccomp_strict`), so before `sched::started()`
    // there is nothing for it to answer from - routing the probe would panic.
    // The probe is therefore served here inline: it answers `exit_group` (leaving
    // is the one thing a refusal cannot express, and the check's program ends on
    // it) and refuses every other number with `ENOSYS`. That is the same honest
    // answer the shared table's `_ => ENOSYS` gives, so `M6_EL0_OK` still reads
    // `-ENOSYS` back for its unassigned number.
    if !crate::sched::started() {
        if number == NR_EXIT_GROUP {
            let code = frame.x[0];
            // SAFETY: reached only from the EL0 vector, and only a machine that
            // got here through `context::enter_user` can take a lower-EL trap at
            // all.
            unsafe { context::leave_for_kernel(code) }
        }
        frame.x[0] = crate::err::ENOSYS as u64;
        return 0;
    }

    // A running task: defer to the kernel's own syscall table - the one dispatch
    // every machine shares. On aarch64 that answers the ported core handlers
    // (`read`/`write`/`openat`/`close`/`getpid`/`brk`/`mmap`/`exit_group`/...) and
    // every family this stage has not ported (signals, ptrace, sockets, device
    // ioctls, `execve`'s return-frame patch) falls to `dispatch`'s `_ => ENOSYS`.
    // The frame pointer travels because a few handlers take it, though the ones
    // that would *read* it as a register frame are gated off aarch64 until the
    // signal/ptrace stage lands; a handler reachable here never looks at it.
    let args = [
        frame.x[0] as usize,
        frame.x[1] as usize,
        frame.x[2] as usize,
        frame.x[3] as usize,
        frame.x[4] as usize,
        frame.x[5] as usize,
    ];
    // AArch64 returns a result in `x0` and an error as a negated errno, which is
    // already what the `err` crate's constants are - so this is the value
    // `dispatch` returned rather than a translation of it.
    // SAFETY: the frame is the one this vector just saved the trap into.
    frame.x[0] = unsafe {
        crate::syscall::linux_dispatch(number, args, true, core::ptr::from_mut(frame).cast::<()>())
    } as u64;
    0
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

/// Block the core from taking interrupts: set `DAIF.I`.
pub fn disable_interrupts() {
    // SAFETY: the inverse of `enable_interrupts`'s `daifclr, #2`.
    unsafe {
        core::arch::asm!("msr daifset, #2", options(nomem, nostack));
    }
}

/// Wait for the next interrupt and return with interrupts masked - the idle path
/// a scheduler parks an exhausted core on.
///
/// The mask on the way out is the whole contract: a task woken by this must find
/// interrupts off so it does not get preempted mid-resume, and `wfi` returning is
/// not itself a guarantee one is masked off after. This is `enable; wfi; disable`
/// where x86_64's is `sti; hlt; cli`.
pub fn idle() {
    enable_interrupts();
    // SAFETY: `wfi` parks the core until an interrupt (or an event); it has no
    // other side effect and is exactly what the boot's `halt` loop already uses.
    unsafe {
        core::arch::asm!("wfi", options(nomem, nostack));
    }
    disable_interrupts();
}

/// `CPACR_EL1`, read fresh - the register that decides who may use the FPU/SIMD.
pub fn cpacr() -> u64 {
    let value: u64;
    // SAFETY: `CPACR_EL1` is readable at EL1 with no side effects.
    unsafe {
        core::arch::asm!("mrs {}, cpacr_el1", out(reg) value, options(nomem, nostack));
    }
    value
}

/// Give EL0 access to the FPU/SIMD registers by setting `CPACR_EL1.FPEN` to
/// `0b11` (full access at EL1 and EL0), then an `isb` so the new permissions
/// cover the next instruction.
///
/// Without this an EL0 program's first FP instruction traps at EL1, and a user
/// program that uses `neon` - which is almost every optimized one - faults before
/// it can do anything. x86_64's twin sets `CR4.OSFXSR`/`OSXMMEXCPT` and clears
/// `CR0.TS`; the requirement ("userland expects the kernel to have turned the FPU
/// on") is the same, the register is not.
pub fn enable_user_simd() {
    const FPEN: u64 = 0b11 << 20;
    let value = cpacr() | FPEN;
    // SAFETY: writing `CPACR_EL1` back with only `FPEN` added is a plain system-
    // register write at EL1; the other fields (`ZEN`, `TTA`, ...) are preserved by
    // read-modify-write rather than clobbered. The `isb` after it is the
    // architecture's requirement for a control-register change to take effect on
    // the following instruction stream.
    unsafe {
        core::arch::asm!(
            "msr cpacr_el1, {value}",
            "isb",
            value = in(reg) value,
            options(nomem, nostack),
        );
    }
}

/// `TPIDR_EL0`, read fresh - the thread pointer an EL0 program's TLS accesses
/// resolve against, the aarch64 counterpart of x86_64's `%fs` base.
pub fn thread_pointer() -> usize {
    let value: usize;
    // SAFETY: `TPIDR_EL0` is readable at EL1 with no side effects; it is the
    // register `check_cpu`'s successor writes a sentinel into and reads back.
    unsafe {
        core::arch::asm!("mrs {}, tpidr_el0", out(reg) value, options(nomem, nostack));
    }
    value
}

/// Point the core's thread pointer at `base`, giving EL0 the base its TLS
/// relocations add to. x86_64's twin writes `IA32_FS_BASE`; the register name is
/// the machine's, which is why it reaches the scheduler as `hal::set_thread_pointer`
/// rather than as a raw `msr` in `sched`.
pub fn set_thread_pointer(base: usize) {
    // SAFETY: a plain system-register write at EL1; `TPIDR_EL0` has no
    // architectural side effect beyond becoming the value an `mrs` reads back.
    unsafe {
        core::arch::asm!("msr tpidr_el0, {base}", base = in(reg) base, options(nomem, nostack));
    }
}
