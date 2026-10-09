//! Suspending one task and resuming another.
//!
//! What gets saved is only what the ABI says a callee must preserve: `x19..x28`,
//! the frame pointer `x29`, the link register `x30`, and the floating-point
//! registers. Everything else is caller-saved, and therefore already dead at any
//! point where a task can be switched out.
//!
//! The *callee-saved* registers are saved on the task's own stack, exactly as
//! the x86_64 twin does and for the same reason: a suspended task's stack is the
//! only memory whose lifetime is already tied to the task, so a switch is one
//! `str sp` to the task's record and one `ldr sp` from the next one's. The
//! floating-point half cannot be done that way, because it is not something the
//! compiler ever spilled for us.
//!
//! # Why the FPU is saved eagerly here and lazily on x86_64
//!
//! This is the one place the two machines' switches are genuinely different
//! designs, and the difference is the machine's rather than a preference.
//!
//! x86_64 saves 512 bytes of x87/SSE state per task *lazily*: the switch sets
//! `CR0.TS` on the task it enters, and the `#NM` its first floating-point
//! instruction raises is what restores the image. A task that runs no
//! floating-point code never pays for the move.
//!
//! aarch64 has no `CR0.TS`. The nearest equivalent is turning the FP unit off in
//! `CPACR_EL1` and trapping every use - which is not a lazy save, it is
//! *trapping the instruction*, and the handler would have to emulate or
//! single-step it. So the choice here is between saving eagerly and trapping
//! correctly, and only one of those is a switch.
//!
//! There is a second reason, and it is the one that makes this mandatory rather
//! than merely simpler. The kernel is built for `aarch64-unknown-none`, whose
//! baseline includes NEON, so LLVM is entitled to keep *its own* values in
//! `q0..q31` - and it does, as scratch. A switch that left them alone would hand
//! the next task a register file belonging to the previous one, whatever those
//! registers were being used for. On x86_64 the `soft-float` build removes that
//! possibility for the kernel's own code; here there is nothing to remove it
//! with.
//!
//! # What is deliberately not here
//!
//! Three things the x86_64 twin has, and the reason each is absent rather than
//! written and left unused.
//!
//! * **A Rust-callable `save_live_fpu`/`restore_fpu`.** The save cannot be
//!   lifted out of [`switch`] and called from Rust, and that is a property of the
//!   machine rather than a preference: a *task's* floating-point registers are
//!   live at the moment it is suspended, and any Rust code in between would be
//!   entitled to use `v` registers as scratch - so a save done in Rust before
//!   the switch would save the wrapper's registers rather than the task's. The
//!   save therefore lives inside the one asm block that also swaps the stacks,
//!   and a second copy for callers that do not exist is exactly the kind of
//!   shape this port refuses.
//! * **`to_bytes`/`from_bytes`, and the FPU *ownership* protocol**
//!   (`fpu_trapped`, `claim_live_fpu`, `reset_live_fpu`). Ownership exists
//!   because x86_64 can leave a task's image in the registers without it being
//!   in the record; here the record is always current, so there is nothing to
//!   claim. The byte view is what a debugger reads, and that is stage 6's
//!   `PTRACE_GETFPREGS` - which is also where the ABI's `user_fpsimd_state`
//!   decides whether the two shapes are one.
//! * **The *user* half** - `x0..x30`/`sp`/`pc`/`pstate`, `fork`'s register file,
//!   and the `eret` that enters EL0. Those fields get their order and their
//!   meaning from the ABI, so they belong with it: writing them at stage 2 would
//!   have been a struct nothing could check against `user_pt_regs` and an entry
//!   sequence with no user address space to enter - the shape this port refuses.
//!   Stage 6a is where both arrived, and it is the first moment either *could*:
//!   `UserFrame` is `user_pt_regs` field for field, and the space `enter_user`
//!   enters is one stage 6a also builds (`paging::UserSpace`). `NOTES.md` §9.1's
//!   stage table carries that split, which is why this file's stage number is 2
//!   and the register file's is 6.

use core::arch::global_asm;
use core::sync::atomic::{AtomicUsize, Ordering};

use super::{halt, pl011};
use mpos_vm::PAGE_SIZE;

/// The 528 bytes a floating-point save moves: thirty-two 16-byte `q` registers,
/// then `FPCR` and `FPSR`.
///
/// `align(16)` is what `stp q0, q1, [x]` requires of the address, and it is why
/// this is the first field of [`Context`]: a struct that *begins* with a
/// 16-byte-aligned member is 16-byte-aligned itself, whatever the caller does
/// with it.
#[repr(C, align(16))]
#[derive(Debug)]
pub struct FpuState {
    bytes: [u8; FPU_BYTES],
}

/// What one task's floating-point state occupies. Not the Linux ABI's
/// `user_fpsimd_state` (which is the same 32 registers plus a `fpsr`/`fpcr` pair
/// in its own order) - this is what the *switch* moves, and stage 6 is where the
/// two are related.
pub const FPU_BYTES: usize = 528;

/// Where `FPCR` and `FPSR` sit inside the 528 bytes: after the thirty-two `q`
/// registers, and in that order - which is *not* the order the Linux signal
/// frame's `fpsimd_context` holds them in (`fpsr` then `fpcr`), so the two
/// shapes are related by an explicit conversion rather than by a copy. The
/// offsets are `mpos_context_switch`'s `str x2, [x0, #512]`/`#520`, spelled once
/// here so the signal frame does not have to re-derive them from assembly.
pub const FPCR_OFFSET: usize = 512;
pub const FPSR_OFFSET: usize = 520;

impl FpuState {
    /// The architectural reset state: every field zero.
    ///
    /// Unlike x86_64 this needs no exception, and that is worth saying because
    /// the twin's version writes two fields by hand. `FPCR`'s reset value is 0,
    /// which is round-to-nearest with every floating-point exception *disabled*
    /// (the enable bits are what request a trap, so zero is the permissive
    /// setting), and `FPSR`'s is 0 as well. There is no `MXCSR`-style register
    /// here whose zero is a trap on the first inexact result.
    pub const fn reset() -> FpuState {
        FpuState {
            bytes: [0u8; FPU_BYTES],
        }
    }

    /// The save area as the bytes a debugger reads: `q0..q31` then `FPCR`/`FPSR`,
    /// the same layout [`mpos_context_switch`] writes. `FpuOps::to_bytes` delegates
    /// here so the trait's bytes and the switch's are one shape, not two.
    pub fn to_bytes(&self) -> [u8; FPU_BYTES] {
        self.bytes
    }

    /// [`to_bytes`](Self::to_bytes)'s inverse: state the bytes describe, for a
    /// restored `FPCR`/`FPSR`/`q`-register file.
    pub fn from_bytes(bytes: &[u8; FPU_BYTES]) -> FpuState {
        FpuState { bytes: *bytes }
    }
}

impl Default for FpuState {
    fn default() -> FpuState {
        FpuState::reset()
    }
}

/// The suspended half of a task: its floating-point state and the stack pointer
/// its callee-saved registers were left at.
///
/// The integer registers themselves are on the task's own stack, which is what
/// `stack_pointer` names, and that is why a `Context` must never move while its
/// task is alive.
#[repr(C)]
#[derive(Debug)]
pub struct Context {
    pub fpu: FpuState,
    stack_pointer: usize,
}

impl Context {
    /// A context that has never been switched out of. The boot task starts here;
    /// its first switch away fills in the real value.
    pub const fn empty() -> Context {
        Context {
            fpu: FpuState::reset(),
            stack_pointer: 0,
        }
    }
}

/// The offset `mpos_context_switch` reads the stack pointer from - the end of
/// the `FpuState` that starts the struct.
const CONTEXT_STACK_POINTER: usize = size_of::<FpuState>();

/// The switch's `str sp, [x0, #528]` and its `ldp/stp` offsets are written into
/// the assembly as literals, which an assembler cannot check against a Rust
/// layout. This is the check: change either type and the build stops here rather
/// than at the first switch, where the symptom would be a stack pointer read
/// from the middle of a saved `q` register.
const _: () = {
    assert!(
        CONTEXT_STACK_POINTER == 528,
        "the context switch's stack-pointer offset moved"
    );
    assert!(
        FPU_BYTES == 528,
        "the context switch's floating-point save area moved"
    );
    assert!(
        FPCR_OFFSET == 512 && FPSR_OFFSET == 520 && FPSR_OFFSET + 8 <= FPU_BYTES,
        "the context switch's control/status register offsets moved"
    );
    assert!(
        BOOTSTRAP_BYTES == 96,
        "the context switch's integer frame moved"
    );
};

/// Registers [`switch`] pushes, not counting the return address the `bl` that
/// reached it already put in `x30`.
const SAVED_REGISTERS: usize = 12;

/// How much room below `stack_top` [`bootstrap`] needs.
pub const BOOTSTRAP_BYTES: usize = SAVED_REGISTERS * size_of::<usize>();

/// Build the state a task starts in, so that the first [`switch`] to it lands in
/// the trampoline instead of in the middle of somebody else's work.
///
/// `entry` is called with `slot` in its first argument register, on a stack whose
/// top is `stack_top`. `interruptible` decides whether it runs with interrupts
/// on, and on this machine that is a decision the *trampoline* makes rather than
/// the switch: `DAIF` is per-core state that no task owns, so it cannot be part
/// of a context without a task being able to take the whole machine's interrupt
/// mask with it.
///
/// # Safety
///
/// `stack_top` must be the top of a 16-byte-aligned block that the caller owns,
/// that nothing else is using, and that has [`BOOTSTRAP_BYTES`] free below it.
pub unsafe fn bootstrap(
    stack_top: usize,
    entry: extern "C" fn(usize) -> !,
    slot: usize,
    interruptible: bool,
) -> Context {
    // Both casts go through a function pointer first. A function *item* is
    // zero-sized, so casting one straight to an integer is a different operation
    // from taking its address and easy to get wrong.
    let trampoline: unsafe extern "C" fn() = mpos_task_trampoline;
    let entry = entry as usize;

    // Ordered the way `switch` restores them, which is ascending offsets: the
    // three the trampoline reads come first, so the frame reads as "what the
    // trampoline needs, then twelve zeroes for the registers it does not".
    //
    // `x19` carries the argument and `x20` the entry point because both are
    // callee-saved: the trampoline is reached by a `ret`, which is not a call
    // the compiler knows about, so the registers it reads cannot be ones any
    // code in between was entitled to use as scratch.
    let words: [usize; SAVED_REGISTERS] = [
        slot,                   // x19, which the trampoline passes along
        entry,                  // x20, which the trampoline branches to
        interruptible as usize, // x21, which the trampoline reads
        0,                      // x22
        0,                      // x23
        0,                      // x24
        0,                      // x25
        0,                      // x26
        0,                      // x27
        0,                      // x28
        0,                      // x29, the frame pointer: a fresh task has no frame
        trampoline as usize,    // x30, which `ret` jumps to
    ];

    let base = stack_top - BOOTSTRAP_BYTES;
    let at = base as *mut usize;
    for (index, word) in words.iter().enumerate() {
        unsafe { at.add(index).write_volatile(*word) };
    }
    Context {
        fpu: FpuState::reset(),
        stack_pointer: base,
    }
}

// ---------------------------------------------------------------------------
// The user half
// ---------------------------------------------------------------------------

/// The register state a trap from EL0 leaves behind: `x0`..`x30`, the user's
/// own stack, the program counter and the saved status word.
///
/// The order is the ABI's rather than this file's choice, and that is the whole
/// reason the plan moved this struct out of stage 2. AArch64 Linux reports a
/// task's registers as `struct user_pt_regs`, which is exactly this - `regs[31]`
/// then `sp`, `pc`, `pstate` - so the frame the vector saves *is* the frame
/// `ptrace`, a signal's `sigcontext` and `/proc/<pid>/syscall` are written in,
/// and the conversion into the ABI's field order is an identity rather than a
/// translation. Which field a register belongs in is the one thing that differs
/// between one machine's frame and another's, and it is the one thing that had
/// nothing to be checked against until there was an ABI to check it against.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct UserFrame {
    /// `x0`..`x30`, in order. `x30` is the link register, which a trap saves
    /// like any other register - the kernel's own return address is not in it.
    pub x: [u64; 31],
    /// `sp`: the *user's* stack, `SP_EL0`. Never the kernel's.
    pub sp: u64,
    /// `pc`: `ELR_EL1`, which on a trap is the instruction that trapped -
    /// *not* the one after it, so a syscall gate has to step over its own `svc`.
    pub pc: u64,
    /// `pstate`: `SPSR_EL1`, the status word `eret` restores.
    pub pstate: u64,
}

/// The offsets `__vec_el0_sync` stores at, as literals in its assembly. An
/// assembler cannot check those against a Rust layout, so the check is here -
/// the same shape as `CONTEXT_STACK_POINTER` above and for the same reason: the
/// failure mode is a trap frame read from the middle of the wrong register,
/// which looks like a corrupt syscall argument rather than like a layout bug.
const _: () = {
    assert!(
        core::mem::offset_of!(UserFrame, x) == 0,
        "the trap frame's registers moved"
    );
    assert!(
        core::mem::offset_of!(UserFrame, sp) == 248,
        "the trap frame's stack pointer moved"
    );
    assert!(
        core::mem::offset_of!(UserFrame, pc) == 256,
        "the trap frame's program counter moved"
    );
    assert!(
        core::mem::offset_of!(UserFrame, pstate) == 264,
        "the trap frame's status word moved"
    );
    assert!(
        size_of::<UserFrame>() == 272,
        "the trap frame's size is not the 34 words the vector reserves"
    );
};

/// The width of the architecture's register file, as Linux reports it: `struct
/// user_pt_regs` is [`UserFrame`] field for field - 31 `x` registers, `sp`,
/// `pc` and `pstate`.
///
/// This is what `hal::Contexts::ABI_REG_COUNT` answers with, and it is why the
/// conversion into the ABI's order is an identity on this machine: the trap
/// frame the vector saves already *is* the structure, so writing it out is a
/// copy and reading one back is a copy, with no slot left over and none
/// invented. The size assertion above is what keeps the two in step.
pub const ABI_REG_COUNT: usize = 34;

/// `SPSR_EL1` for a return to EL0: mode `EL0t`, every interrupt open.
///
/// Zero is not a shortcut for "don't care". `SPSR_EL1`'s mode field is `M[3:0]`,
/// `0b0000` is EL0 running on `SP_EL0`, and the `D`/`A`/`I`/`F` bits are the
/// ones that *mask* - so "EL0t with nothing masked" is the whole register, and
/// writing anything else would either trap at a level this kernel has no code
/// for or leave the program running with the clock off.
pub const PSTATE_EL0T: u64 = 0;

/// `SPSR_EL1` for a return to EL1 on `SP_EL1`: `EL1h`.
///
/// The `h` is the half that matters: `0b0100` would be `EL1t`, which returns to
/// EL1 still using `SP_EL0` - and `SP_EL0` at that moment holds the *user's*
/// stack, so the kernel would push its frame onto the program it just left.
///
/// Substituted into both of [`leave_for_kernel`]'s `eret` blocks as an inline-asm
/// `const` operand. A `const` operand is the immediate itself, not a register, so
/// naming it here costs the block no scratch register and cannot disturb the
/// fixed registers those blocks carry their resume state in.
const PSTATE_EL1H: u64 = 0b0101;

/// Where an EL0 program that asks to leave is resumed, and the kernel stack it
/// is resumed on.
///
/// Both are set by [`enter_frame`] and by [`enter_probe`] - each in its own way
/// (see `enter_probe`) - and read by the trap gate's `exit` arm, which reaches
/// them through [`leave_for_kernel`]. They are statics rather than arguments
/// because the two ends are reached through the CPU's exception machinery: the
/// gate runs on the vector with a stack the caller never sees, so there is no
/// calling convention to carry anything in.
static ON_EXIT: AtomicUsize = AtomicUsize::new(0);
static KERNEL_SP: AtomicUsize = AtomicUsize::new(0);

/// Leave the user program for the kernel: never returns to EL0, and how it
/// reaches the kernel depends on who dropped to EL0.
///
/// This is the `exit` arm of the trap gate, and it is an `eret` like every other
/// way out of a trap - what it changes is *where* the `eret` goes. `SPSR_EL1`'s
/// mode field says `EL1h`, so the same instruction that would have resumed the
/// program resumes the kernel, on the `SP_EL1` the trap landed on once the
/// trap's own frame has been given back.
///
/// A real `exit` does a third thing this cannot: it picks another task to run,
/// and gives the one that exited away. This machine has one EL0 program and no
/// task table to hold it yet, so "the program is finished" is the whole of what
/// leaving means here, and the task table is stage 6's next piece rather than
/// something this pretends to have.
///
/// The resume pair the two statics carry names either a function that never
/// returns (`enter_frame`'s `on_exit`) or, for a probe, nothing at all - see
/// below. Both readings are single-use; whichever armed them erases its half.
///
/// Which of the two it is, and where the probe's half lives, is decided by the
/// side that owns the reservation: `enter_probe`'s prologue stamps `[ret, sp]`
/// sixteen bytes below the `sp` it drops from and publishes that address in
/// `mpos_probe_slot[0]`, never a distance recomputed in Rust. Measured: an
/// earlier version did recompute it, was eight bytes out, and aimed the `eret`
/// at the saved `sp` instead of the resume address - the first EL0 program to
/// ask to leave came back as an instruction permission fault (`EC 0x21`,
/// `IFSC 0b001111`) inside the very stack it had been running on.
///
/// # Safety
///
/// Only from the trap gate, on a machine that reached it through
/// [`enter_user`] - it resumes the function that call recorded and restores the
/// stack that call recorded.
pub unsafe fn leave_for_kernel(code: u64) -> ! {
    let sp = KERNEL_SP.load(Ordering::Relaxed);
    let target = ON_EXIT.load(Ordering::Relaxed);
    // Both readings are single-use: whoever armed them is the frame this `eret`
    // leaves, so nothing else may find them again. (A later EL0 program that
    // asks to leave after its own `enter_frame` re-arms them; one that somehow
    // reached EL0 without doing so hits the assert at the bottom.)
    ON_EXIT.store(0, Ordering::Relaxed);
    KERNEL_SP.store(0, Ordering::Relaxed);
    // `enter_frame` always arms both statics, so a zero `KERNEL_SP` can only
    // mean the machine reached EL0 through `enter_probe`: that is this branch's
    // whole discriminator, and the empty `ON_EXIT` beside it is deliberate -
    // there is no handler to call, because the probe's way back is a return into
    // its own caller rather than a call. The return itself is
    // [`mpos_probe_return`], aimed through `elr_el1` and handed the save area's
    // address in `x19`: it reloads the two words the pair got stamped from and
    // `ret`s, which lands on the instruction after `enter_probe`'s drop with the
    // exit code already in `x0` - the ordinary ABI return this function's caller
    // was compiled for. Naming the stub from Rust is the one thing this
    // higher-half link allows that the reverse does not: an `adrp`/`:lo12:`
    // pair against a Rust static from `global_asm!` is an ABS64 relocation it
    // refuses, which is why the area travels in the `.bss` slot instead.
    if sp == 0 {
        // SAFETY: only `enter_probe`'s prologue writes the slot, and a zero
        // and a zero means no probe ever stamped one - which the assert reports
        // rather than a return into nothing.
        let area: usize =
            unsafe { core::ptr::read_volatile(core::ptr::addr_of!(mpos_probe_slot).cast()) };
        assert!(
            area != 0,
            "an EL0 program left for the kernel and the drop that started it left no way back: \
             `enter_probe` did not publish a save area"
        );
        let stub: unsafe extern "C" fn() = mpos_probe_return;
        // SAFETY: `area` is the 16-byte region `enter_probe` reserved below its
        // own `sp`, live until the stub's `ret` gives that frame back - which is
        // what this `eret` makes happen. Writing `x19` from inside the block is
        // legal because the block never comes back: there is no frame left whose
        // callee-saved registers the compiler could ask for. The `eret` restores
        // `PSTATE` from `EL1h`, whose `D`/`A`/`I`/`F` bits are all clear, so the
        // interrupt mask the drop went under is lifted by the return itself.
        unsafe {
            core::arch::asm!(
                "mov x19, x8",
                "ldr x8, [x8, #8]",
                "mov sp, x8",
                "msr elr_el1, x9",
                "mov x9, {pstate}",
                "msr spsr_el1, x9",
                "mov x0, x10",
                "eret",
                in("x8") area,
                in("x9") stub as usize,
                in("x10") code,
                pstate = const PSTATE_EL1H,
                options(noreturn),
            );
        }
    }
    assert!(
        sp != 0 && target != 0,
        "an EL0 program left for the kernel with no entry to return to: \
         `enter_user` was not the way it was started"
    );
    // SAFETY: both values came from `enter_user`, which the machine reached EL0
    // through; `EL1h` is the level this is already running at, so the `eret`
    // neither changes exception level nor touches `SP_EL1` after the move above.
    // The words are written by hand rather than through named operands because
    // neither half of this pair survives a named-operand build: `mov sp, Xn` is
    // the encoding no register operand can name (`{sp}` binds to the stack
    // pointer, so a copy built from an operand folds away), and an `msr` of a
    // system register cannot take an operand at all. Fixed registers carry the
    // resume state instead: `x8` = the kernel stack, `x9` = the resume address,
    // `x10` = the exit code.
    unsafe {
        core::arch::asm!(
            "mov sp, x8",
            "msr elr_el1, x9",
            "mov x9, {pstate}",
            "msr spsr_el1, x9",
            "mov x0, x10",
            "eret",
            in("x8") sp,
            in("x9") target,
            in("x10") code,
            pstate = const PSTATE_EL1H,
            options(noreturn),
        );
    }
}

unsafe extern "C" {
    fn mpos_context_switch(from: *mut Context, to: *const Context);
    fn mpos_task_trampoline();
    fn mpos_enter_frame(frame: *const UserFrame) -> !;
    fn mpos_probe_return();
}

core::arch::global_asm!(
    ".section .bss.probe, \"aw\", @nobits",
    ".globl mpos_probe_slot",
    ".balign 8",
    "mpos_probe_slot:",
    ".skip 32",
);

unsafe extern "C" {
    /// The probing frame's save area: `[slot_addr]` written by `enter_probe`'s
    /// prologue, and `[ret, sp]` at that address - the resume pair the gate's
    /// exit arm hands to [`mpos_probe_return`] (see [`leave_for_kernel`]), which
    /// reloads them and returns. A global symbol in `.bss` because the asm names
    /// it directly
    /// (`adrp`/`:lo12:` against a Rust static is an ABS64 relocation the
    /// higher-half link refuses; a named object in the image resolves like any
    /// other symbol). 32 bytes so every access lands inside one page either way.
    static mpos_probe_slot: [usize; 4];
}

global_asm!(
    // Come back into the kernel frame that dropped to EL0 for a milestone probe.
    // The gate's `exit` arm got here by moving `sp` and `eret`-ing - the only way
    // out of a trap - and what it asks for is an ordinary return: put the caller's
    // callee-saved registers, `x30` and `sp` back the way the ABI says a callee
    // must, leave `x0` (the exit code the gate put there) alone, and `ret`.
    // Control lands on the instruction after the drop, which is this function's
    // own return into the boot path.
    //
    // `x19` arrives holding the save area both because it is the one register no
    // inline-asm operand can name and because a reference from `global_asm!` to
    // anything the Rust side owns is an ABS64 relocation this higher-half PIE
    // link refuses; so it is also the one reloaded last, from the pair's own third
    // word.
    //
    // The section is named for the reason `mpos_enter_frame` above names it.
    ".section .text.context, \"ax\"",
    ".globl mpos_probe_return",
    ".p2align 4",
    "mpos_probe_return:",
    "    ldp x9, x8, [x19]",
    "    ldp x20, x21, [x19, #32]",
    "    ldp x22, x23, [x19, #48]",
    "    ldp x24, x25, [x19, #64]",
    "    ldp x26, x27, [x19, #80]",
    "    ldp x28, x29, [x19, #96]",
    "    ldr x30, [x19, #16]",
    "    ldp q8, q9, [x19, #112]",
    "    ldp q10, q11, [x19, #144]",
    "    ldp q12, q13, [x19, #176]",
    "    ldp q14, q15, [x19, #208]",
    "    ldr x10, [x19, #24]",
    "    mov x19, x10",
    "    mov sp, x8",
    "    br x9",
);

/// Drop to EL0 on `frame`, let the program run to its `exit_group`, and come
/// back to the caller with the code it passed - the milestone-probe twin of
/// [`enter_frame`], which never returns because a task's `resume` has no caller
/// frame to return to.
///
/// The way back is the trap gate's ordinary `exit` arm, aimed through the pair
/// this stamps (see [`leave_for_kernel`]): the gate erets at EL1 into
/// [`mpos_probe_return`], which reloads those two words and `ret`s, landing on
/// the instruction after this function's `bl` with the exit code already in
/// `x0` - an ABI return, indistinguishable from the drop having been an ordinary
/// call. Nothing sleeps or allocates between the stamp and the landing (the
/// scheduler does not exist yet at boot), and the window is interrupt-masked for
/// the same reason the pre-spawn handoff is: a tick landing mid-probe would find
/// a frame the dispatcher cannot name. The mask come off with the `eret` itself,
/// whose `SPSR_EL1` is `EL1h` with `D`/`A`/`I`/`F` all clear.
///
/// # Safety
///
/// [`enter_frame`]'s contract verbatim - `frame`'s `pc`/`sp` mapped in the active
/// space, `SPSel` selecting `SP_EL1` - plus one rule: only the boot path may call
/// this, single-core, before any other context could probe.
pub unsafe fn enter_probe(frame: &UserFrame) -> u64 {
    let mut status: u64;
    // SAFETY: the block stamps the resume pair - this frame's return address and
    // the `sp` that goes with it - into 16 bytes below the drop, publishes that
    // area's *own* address in `mpos_probe_slot[0]` (the one value the gate's
    // `exit` arm needs that nothing on that side can compute; see
    // `leave_for_kernel` for the eight-byte disagreement the version that did
    // compute it was), drops to EL0 through the production restore, and declares
    // every register that restore overwrites except the ones used as operands.
    //
    // The gate learns this was a probe - rather than a task's `resume` - from
    // `KERNEL_SP` being zero, which only this path ever stores; `ON_EXIT` is left
    // alone because there is no handler to call.
    KERNEL_SP.store(0, Ordering::Relaxed);
    unsafe {
        core::arch::asm!(
            // The save area, 240 bytes below the drop: where to resume, what to
            // give `sp` back, and every callee-saved register this frame owes its
            // caller - integer *and* floating-point.
            //
            // The resume word is the address of the instruction *after* the `bl`,
            // taken by `adr` rather than by any arithmetic: the landing has to be
            // inside this block so that the rest of it - and then this function's
            // own epilogue - still runs. Measured the other way: resuming at
            // this function's return address instead handed the caller back a
            // stack pointer 80 bytes lower than the one it was compiled for, and
            // every `[sp, #imm]` in the caller afterwards wrote into its own
            // locals instead.
            //
            // The registers are not defensive decoration either.
            // [`mpos_enter_frame`] reloads all of `x0..x30` from the frame, so a
            // drop that saved nothing hands the boot path back a caller whose own
            // `x20` has been zeroed - measured: the instruction after the landing
            // read `[x20, #0x8a8]` and faulted at `0x906`. `v8..v15` are
            // callee-saved too, and no epilogue covers them: LLVM never sees the
            // drop return, so nothing it might have spilled is put back.
            "sub sp, sp, #240",
            "mov x9, sp",
            "adr x8, 2f",
            "str x8, [x9]",
            "add x8, x9, #240",
            "str x8, [x9, #8]",
            "str x30, [x9, #16]",
            "stp x19, x20, [x9, #24]",
            "stp x21, x22, [x9, #40]",
            "stp x23, x24, [x9, #56]",
            "stp x25, x26, [x9, #72]",
            "stp x27, x28, [x9, #88]",
            "str x29, [x9, #104]",
            "stp q8, q9, [x9, #112]",
            "stp q10, q11, [x9, #144]",
            "stp q12, q13, [x9, #176]",
            "stp q14, q15, [x9, #208]",
            "adrp x8, mpos_probe_slot",
            "add x8, x8, :lo12:mpos_probe_slot",
            "str x9, [x8]",
            "msr daifset, #2",
            "bl {enter}",
            // The landing. The program's `exit_group` comes back here by `eret`
            // through [`mpos_probe_return`], which has already put `sp` and the
            // callee-saved registers back - so what is left for the block to do
            // is lift the interrupt mask it went under, and let this function's
            // own epilogue return to `check_el0` the ordinary way.
            "2:",
            "msr daifclr, #2",
            enter = sym mpos_enter_frame,
            inout("x0") frame as *const UserFrame => status,
            lateout("x2") _,
            lateout("x3") _,
            lateout("x4") _,
            lateout("x5") _,
            lateout("x6") _,
            lateout("x7") _,
            lateout("x8") _,
            lateout("x9") _,
            lateout("x10") _,
            lateout("x11") _,
            lateout("x12") _,
            lateout("x13") _,
            lateout("x14") _,
            lateout("x15") _,
            lateout("x16") _,
            lateout("x17") _,
            lateout("x18") _,
            // `x19` is reserved for LLVM's small-code-model register and `x29`
            // is the frame pointer: neither may be named as an operand, and
            // neither carries anything across the restore (both are reloaded
            // from the frame, and the return path does not come back through
            // here at all).
            lateout("x21") _,
            lateout("x22") _,
            lateout("x23") _,
            lateout("x24") _,
            lateout("x25") _,
            lateout("x26") _,
            lateout("x27") _,
            lateout("x28") _,
            out("x30") _,
            options(nostack),
        );
    }
    // Unreachable in fact - the drop comes back through `mpos_probe_return`
    // straight to this function's caller - but reachable in the type: Rust has
    // no way to say "the return I do not perform". What it must not do is leave
    // the consumed pair behind, which `leave_for_kernel` already erased on the
    // way out, so there is nothing here to tidy.
    status
}

global_asm!(
    // Restore the whole register file from a `UserFrame` and `eret` to EL0. The
    // offsets are the same ones `__vec_el1_sync`'s sibling saves to (the compile-
    // time asserts in `UserFrame`'s definition fix them), so this is the exact
    // inverse of the trap's save: a frame the vector wrote is a frame this reads
    // back, and a drift between the two would show up here, not as a corrupt
    // register on some later resume.
    //
    // The section is *named* rather than left to the assembler, and that is not
    // decoration. One `global_asm!` per crate is emitted into one assembly unit,
    // so a block that names no section lands wherever the last `.section`
    // directive left it - and this file's neighbours include one that switches to
    // a *data* section (`mod.rs`'s EL0 program). Measured: these routines were
    // once assembled into `.rodata.el0_program` that way, and since the kernel
    // maps `.rodata` read-only the first call to `mpos_context_round_trip` was an
    // instruction abort with the fault address inside the image. Code that names
    // its own section cannot be moved by the order its block happens to be
    // emitted in; `linker.ld` asserts the result.
    ".section .text.context, \"ax\"",
    ".globl mpos_enter_frame",
    ".p2align 4",
    "mpos_enter_frame:",
    // `x1` holds the frame pointer throughout; `x0` is scratch for the three
    // system-register loads and then takes its own restored value last.
    "    mov x1, x0",
    "    ldr x0, [x1, #248]",
    "    msr sp_el0, x0",
    "    ldr x0, [x1, #256]",
    "    msr elr_el1, x0",
    "    ldr x0, [x1, #264]",
    "    msr spsr_el1, x0",
    // `x2..x29` in pairs (offset `8*i`), then `x30` alone, then `x0` and `x1`
    // last so the base register stays live until nothing more reads it.
    "    ldp x2, x3,  [x1, #16]",
    "    ldp x4, x5,  [x1, #32]",
    "    ldp x6, x7,  [x1, #48]",
    "    ldp x8, x9,  [x1, #64]",
    "    ldp x10, x11, [x1, #80]",
    "    ldp x12, x13, [x1, #96]",
    "    ldp x14, x15, [x1, #112]",
    "    ldp x16, x17, [x1, #128]",
    "    ldp x18, x19, [x1, #144]",
    "    ldp x20, x21, [x1, #160]",
    "    ldp x22, x23, [x1, #176]",
    "    ldp x24, x25, [x1, #192]",
    "    ldp x26, x27, [x1, #208]",
    "    ldp x28, x29, [x1, #224]",
    "    ldr x30, [x1, #240]",
    "    ldr x0,  [x1, #0]",
    "    ldr x1,  [x1, #8]",
    "    isb",
    "    eret",
);

/// Resume `frame` at EL0 and come back to `on_exit` when the program asks to
/// leave - the frame-precise twin of [`enter_user`], which starts from an entry
/// and a stack instead of a saved register file.
///
/// `enter_user` and `hal::Contexts::resume_user`/`enter_user` are all this one
/// operation: write the trap gate's two statics, then hand the CPU to a register
/// file. It never returns in the ordinary sense; the only way out is the trap
/// gate's `exit` arm `eret`-ing to `on_exit` at EL1, exactly as [`enter_user`].
///
/// # Safety
///
/// `frame`'s `pc`/`sp` must be mapped in the space `TTBR0_EL1` currently names,
/// that space must already be active, and `SPSel` must select `SP_EL1` so the
/// next trap from EL0 lands on a real kernel stack (the boot sets this once).
pub unsafe fn enter_frame(frame: &UserFrame, on_exit: extern "C" fn(u64) -> !) -> ! {
    let sp: usize;
    // SAFETY: reading `sp` has no side effects.
    unsafe { core::arch::asm!("mov {}, sp", out(reg) sp, options(nomem, nostack)) };
    KERNEL_SP.store(sp, Ordering::Relaxed);
    ON_EXIT.store(on_exit as usize, Ordering::Relaxed);
    // SAFETY: the caller's contract on the frame, passed to the restore verbatim.
    unsafe { mpos_enter_frame(frame) }
}

/// Nothing to do: on AArch64 the privilege-transition stack *is* the current
/// `sp`, which the context switch already put on this task's kernel stack.
///
/// x86_64's `TSS.sp0` is a separate field the CPU swaps in when a trap crosses
/// a privilege level, so the scheduler must arm it before entering a task at
/// ring 3 - hence the shared `Contexts::set_kernel_stack` step and this slot in
/// it. AArch64 has no such spare field here: the kernel runs at `EL1h`
/// (`SPSel` = 1), so `sp` *is* `SP_EL1`, and a trap from EL0 lands on whatever
/// `SP_EL1` already holds. `switch` writes it directly (`mov sp, x2` restores
/// the incoming task's saved kernel `sp`), so by the time the scheduler drops
/// that task to EL0 the trap stack is already the task's own. Arming it a
/// second time is not just redundant - the dedicated `MSR SP_EL1, Xt` is
/// architecturally *undefined* while `SPSel` selects `SP_EL1` (the form can only
/// address the non-selected stack), so the write would fault with `EC 0x00`.
/// Measured: exactly this, on the first switch into the idle task once `/init`
/// had exited - which is the first time the park path ran the step at all.
///
/// # Safety
///
/// No preconditions remain; `top` is the scheduler's per-task kernel-stack top,
/// kept only so this satisfies the shared trait. It is not used.
pub unsafe fn set_kernel_stack(_top: usize) {}

global_asm!(
    // x0: the context being suspended (written). x1: the one being resumed.
    // The section is named for the reason `mpos_enter_frame` above names it.
    ".section .text.context, \"ax\"",
    ".globl mpos_context_switch",
    ".p2align 4",
    "mpos_context_switch:",
    // The floating-point half goes into the outgoing context's own record. It
    // cannot go on the stack with the integer half because `Context`'s layout is
    // what `ContextOps::fpu` hands out, and the two halves have to be findable
    // by different callers: the record by Rust, the stack by the switch alone.
    //
    // x2 and x3 are the only registers this whole function clobbers that it does
    // not restore, and they are scratch on both sides of the exchange. Everything
    // from x4 to x18 is left exactly as it was found, which is what lets
    // `round_trip` below compare a pattern across the call.
    "    stp q0,  q1,  [x0, #0]",
    "    stp q2,  q3,  [x0, #32]",
    "    stp q4,  q5,  [x0, #64]",
    "    stp q6,  q7,  [x0, #96]",
    "    stp q8,  q9,  [x0, #128]",
    "    stp q10, q11, [x0, #160]",
    "    stp q12, q13, [x0, #192]",
    "    stp q14, q15, [x0, #224]",
    "    stp q16, q17, [x0, #256]",
    "    stp q18, q19, [x0, #288]",
    "    stp q20, q21, [x0, #320]",
    "    stp q22, q23, [x0, #352]",
    "    stp q24, q25, [x0, #384]",
    "    stp q26, q27, [x0, #416]",
    "    stp q28, q29, [x0, #448]",
    "    stp q30, q31, [x0, #480]",
    "    mrs x2, fpcr",
    "    mrs x3, fpsr",
    "    str x2, [x0, #512]",
    "    str x3, [x0, #520]",
    // The integer half goes on this task's own stack, below the frame this
    // function was called with, so that a `ret` at the end of it unwinds
    // correctly on the way back in. 96 bytes keeps `sp` 16-byte aligned, which
    // every one of these is required to be.
    "    sub sp, sp, #96",
    "    stp x19, x20, [sp, #0]",
    "    stp x21, x22, [sp, #16]",
    "    stp x23, x24, [sp, #32]",
    "    stp x25, x26, [sp, #48]",
    "    stp x27, x28, [sp, #64]",
    "    stp x29, x30, [sp, #80]",
    // Through x2, because `str sp, [x0, #imm]` does not exist: the encoding's
    // `Rt` field means `XZR` at 31, never the stack pointer, so moving `sp` into
    // a general register first is the only spelling of this.
    "    mov x2, sp",
    "    str x2, [x0, #528]",
    // And the incoming one comes back the same way. `x1` stays live throughout:
    // it is the only handle on the struct being restored, and the two scratch
    // registers above are gone by now.
    "    ldr x2, [x1, #528]",
    "    mov sp, x2",
    "    ldp x19, x20, [sp, #0]",
    "    ldp x21, x22, [sp, #16]",
    "    ldp x23, x24, [sp, #32]",
    "    ldp x25, x26, [sp, #48]",
    "    ldp x27, x28, [sp, #64]",
    "    ldp x29, x30, [sp, #80]",
    "    add sp, sp, #96",
    "    ldp q0,  q1,  [x1, #0]",
    "    ldp q2,  q3,  [x1, #32]",
    "    ldp q4,  q5,  [x1, #64]",
    "    ldp q6,  q7,  [x1, #96]",
    "    ldp q8,  q9,  [x1, #128]",
    "    ldp q10, q11, [x1, #160]",
    "    ldp q12, q13, [x1, #192]",
    "    ldp q14, q15, [x1, #224]",
    "    ldp q16, q17, [x1, #256]",
    "    ldp q18, q19, [x1, #288]",
    "    ldp q20, q21, [x1, #320]",
    "    ldp q22, q23, [x1, #352]",
    "    ldp q24, q25, [x1, #384]",
    "    ldp q26, q27, [x1, #416]",
    "    ldp q28, q29, [x1, #448]",
    "    ldp q30, q31, [x1, #480]",
    "    ldr x2, [x1, #512]",
    "    ldr x3, [x1, #520]",
    "    msr fpcr, x2",
    "    msr fpsr, x3",
    "    ret",
    // Where a bootstrapped task lands. `switch` restored `x19`, `x20`, `x21` and
    // `x30` from the frame `bootstrap` wrote and then `ret`-ed here, so this runs
    // with the task's own stack, not the switcher's.
    ".globl mpos_task_trampoline",
    ".p2align 4",
    "mpos_task_trampoline:",
    // The only place `interruptible` is honoured, and it is not a formality: a
    // fresh task has no saved state to return to, so nothing else could decide
    // it. `daifclr #2` clears `I`.
    "    cbz x21, 1f",
    "    msr daifclr, #2",
    "1:  mov x0, x19",
    "    blr x20",
    // The entry is `-> !`, so reaching here is a bug in whatever was bootstrapped
    // rather than a state to recover from.
    "    brk #1",
);

/// Suspend the caller and resume whichever task `to` describes.
///
/// This returns twice: once here when the caller is eventually resumed, which
/// may be arbitrarily later and with other tasks having run in between. Any
/// `&mut` the caller holds therefore has to be dead before the call, or the task
/// that runs next will take a second `&mut` to the same data.
///
/// Interrupts are left exactly as they were found - unlike the x86_64 twin,
/// which returns with them off. `DAIF` is per-core and the switch does not touch
/// it, so the discipline is the caller's: a scheduler switches inside a critical
/// section, and a switch out of an interrupt handler happens with `I` already
/// masked by the exception.
///
/// The FPU is saved and restored *eagerly*, on every switch, and there is no
/// flag a caller may set to skip it - see the module header for why.
///
/// # Safety
///
/// Both pointers must address a live `Context` that will not move for as long as
/// its task exists, and `to` must describe a task that is suspended or freshly
/// bootstrapped rather than one already running.
pub unsafe fn switch(from: *mut Context, to: *const Context) {
    unsafe { mpos_context_switch(from, to) }
}

unsafe extern "C" {
    /// Stamp the callee-saved registers and `v31`, hand over, and answer how
    /// many of them came back wrong.
    fn mpos_context_round_trip(from: *mut Context, to: *const Context, pattern: u64) -> u64;
}

/// One task's turn: stamp the callee-saved registers and `v31`, hand over to
/// `to`, and answer how many of them came back wrong.
///
/// The stamp and the check are one asm routine with nothing in between, and that
/// is the whole point rather than a style. A stamp, a Rust call, and a check
/// would be checking the compiler's register allocator: `x19..x28` are exactly
/// where Rust keeps the values that have to survive a call, so it would rewrite
/// them between the two and the answer would say nothing about the switch. The
/// routine therefore also *saves and restores* those registers itself, which is
/// what makes it a well-behaved callee despite deliberately trashing them - and
/// it reloads its pattern from its own frame rather than trusting a register to
/// have survived, because the one thing this check must not do is assume a
/// register came back.
///
/// # Safety
///
/// As [`switch`]: `from` and `to` must be live contexts, and `to` must not
/// already be running.
unsafe fn round_trip(from: *mut Context, to: *const Context, pattern: u64) -> u64 {
    unsafe { mpos_context_round_trip(from, to, pattern) }
}

/// The on-exit target for a bare `resume`: there is no task table or scheduler to
/// return an exiting EL0 program to until stage 3, so leaving means the machine
/// stops. A real resume hands the frame's task back to the scheduler, which is the
/// named stage-3 half of this path rather than something to fake here.
extern "C" fn resume_halt(_code: u64) -> ! {
    super::halt()
}

/// The floating-point half a switch moves, and where to find it. [`Context`]'s
/// `fpu` field is exactly [`crate::hal::ContextOps`]'s `Fpu`, so `fpu`/`fpu_mut`
/// are field access and `empty` is the boot context.
impl crate::hal::ContextOps for Context {
    type Fpu = FpuState;

    fn empty() -> Self {
        Context::empty()
    }

    fn fpu(&self) -> &Self::Fpu {
        &self.fpu
    }

    fn fpu_mut(&mut self) -> &mut Self::Fpu {
        &mut self.fpu
    }
}

/// The FPU save area as the bytes a debugger reads. This machine has no lazy FPU
/// (`Contexts`' trap protocol is x86-only), but every machine with floating point
/// has state to reset and a byte view to move, and that is what [`FpuOps`] asks.
impl crate::hal::FpuOps for FpuState {
    fn reset() -> Self {
        FpuState::reset()
    }

    fn to_bytes(&self) -> [u8; crate::hal::FPU_BYTES] {
        self.bytes
    }

    fn from_bytes(bytes: &[u8; crate::hal::FPU_BYTES]) -> Self {
        FpuState { bytes: *bytes }
    }
}

/// The ring-3 register state. Every method here is the field the name says, and
/// `resume` is [`enter_frame`] under the trap gate's halt exit. The ABI marshaling
/// (`to_user_regs`/`from_user_regs`) writes Linux's aarch64 `user_pt_regs` order -
/// `x0..x30`, `sp`, `pc`, `pstate` - and its field order is pinned by stage 6's
/// `ptrace`, the first caller that can check it against the ABI; there is no slot
/// for `tls` in that struct (the thread pointer is `TPIDR_EL0`, read separately),
/// so it is named and dropped rather than invented.
impl crate::hal::UserFrameOps for UserFrame {
    fn forked(&self, child_stack: usize) -> Self {
        let mut child = *self;
        if child_stack != 0 {
            child.sp = child_stack as u64;
        }
        child
    }

    fn set_result(&mut self, value: isize) {
        self.x[0] = value as u64;
    }

    fn ip(&self) -> usize {
        self.pc as usize
    }

    fn set_ip(&mut self, ip: usize) {
        self.pc = ip as u64;
    }

    fn sp(&self) -> usize {
        self.sp as usize
    }

    fn set_sp(&mut self, sp: usize) {
        self.sp = sp as u64;
    }

    fn to_user_regs(&self, tls: usize, out: &mut [u64]) {
        let _ = tls;
        let n = out.len().min(31);
        out[..n].copy_from_slice(&self.x[..n]);
        if out.len() > 31 {
            out[31] = self.sp;
        }
        if out.len() > 32 {
            out[32] = self.pc;
        }
        if out.len() > 33 {
            out[33] = self.pstate;
        }
    }

    fn from_user_regs(regs: &[u64]) -> Self {
        let mut frame = UserFrame {
            x: [0; 31],
            sp: 0,
            pc: 0,
            pstate: 0,
        };
        let n = regs.len().min(31);
        frame.x[..n].copy_from_slice(&regs[..n]);
        if regs.len() > 31 {
            frame.sp = regs[31];
        }
        if regs.len() > 32 {
            frame.pc = regs[32];
        }
        if regs.len() > 33 {
            frame.pstate = regs[33];
        }
        frame
    }

    unsafe fn resume(&self) -> ! {
        // SAFETY: the caller (the scheduler) built this frame from a task's own
        // record; `enter_frame`'s contract is that `pc`/`sp` are mapped in the
        // active space and `SPSel` selects the kernel stack - both stage 6a's boot
        // established and the scheduler maintains from stage 3.
        unsafe { enter_frame(self, resume_halt) }
    }
}

global_asm!(
    // x0: the context being suspended. x1: the one being resumed. x2: the
    // pattern. Answers in x0 the number of registers that did not come back.
    //
    // The first version of this lived in a Rust `asm!` block and was wrong twice
    // over, in ways worth keeping. It stamped `x19..x28` without declaring them,
    // so the release build - which had `boot_check`'s frame base in one of them -
    // faulted three turns later on an address of 0x0d. And the declaration it
    // needed cannot be written from Rust at all: `x19` is reserved as an inline
    // asm operand. A `global_asm!` routine has neither problem, because it is
    // the function the ABI describes rather than a block inside one.
    // The section is named for the reason `mpos_enter_frame` above names it.
    ".section .text.context, \"ax\"",
    ".globl mpos_context_round_trip",
    ".p2align 4",
    "mpos_context_round_trip:",
    "    stp x29, x30, [sp, #-128]!",
    "    mov x29, sp",
    "    stp x19, x20, [sp, #16]",
    "    stp x21, x22, [sp, #32]",
    "    stp x23, x24, [sp, #48]",
    "    stp x25, x26, [sp, #64]",
    "    stp x27, x28, [sp, #80]",
    // The pattern goes in this task's own frame, not a register: the switch is
    // entitled to clobber every caller-saved register and the check below runs
    // after it. `x0` and `x1` are still the switcher's arguments, left alone.
    "    str x2, [sp, #96]",
    "    mov x19, x2",
    "    mov x20, x2",
    "    mov x21, x2",
    "    mov x22, x2",
    "    mov x23, x2",
    "    mov x24, x2",
    "    mov x25, x2",
    "    mov x26, x2",
    "    mov x27, x2",
    "    mov x28, x2",
    "    dup v31.2d, x2",
    "    bl {switch}",
    // Resumed. `sp` is this task's again - the switch restored it - so the frame
    // written above is where it was left, and so is the pattern inside it.
    "    ldr x9, [sp, #96]",
    "    mov x10, #0",
    "    cmp x19, x9",
    "    cinc x10, x10, ne",
    "    cmp x20, x9",
    "    cinc x10, x10, ne",
    "    cmp x21, x9",
    "    cinc x10, x10, ne",
    "    cmp x22, x9",
    "    cinc x10, x10, ne",
    "    cmp x23, x9",
    "    cinc x10, x10, ne",
    "    cmp x24, x9",
    "    cinc x10, x10, ne",
    "    cmp x25, x9",
    "    cinc x10, x10, ne",
    "    cmp x26, x9",
    "    cinc x10, x10, ne",
    "    cmp x27, x9",
    "    cinc x10, x10, ne",
    "    cmp x28, x9",
    "    cinc x10, x10, ne",
    // The floating-point half, through a general register because there is no
    // integer/vector compare. One lane is enough: the stamp wrote all of them
    // and the switch moves the whole register, so a second lane is the same
    // answer twice.
    "    umov x11, v31.d[0]",
    "    cmp x11, x9",
    "    cinc x10, x10, ne",
    "    mov x0, x10",
    "    ldp x19, x20, [sp, #16]",
    "    ldp x21, x22, [sp, #32]",
    "    ldp x23, x24, [sp, #48]",
    "    ldp x25, x26, [sp, #64]",
    "    ldp x27, x28, [sp, #80]",
    "    ldp x29, x30, [sp], #128",
    "    ret",
    switch = sym mpos_context_switch,
);

/// How many hand-overs the boot check makes before it stops. Even, so that both
/// tasks take the same number of turns and the last one running is the one that
/// started.
const TURNS: usize = 6;

/// Turns taken, added to by whichever task is running.
///
/// Counted *up* rather than down, and that is the correction of a real bug in
/// the first version. Two tasks share this number, and the side that takes the
/// last turn does not stop there: it hands back to the other, which then takes
/// its own next turn and counts it. With a countdown the second side's
/// decrement underflowed - `0 - 1` in a `usize` is `usize::MAX` - so its loop
/// saw a huge number still to go, printed another letter, switched to a task
/// that had already left the loop, and parked the machine in `halt`. The
/// symptom was a log that stopped mid-alphabet with no fault, which reads like a
/// broken switch and was arithmetic.
static TAKEN: AtomicUsize = AtomicUsize::new(0);
/// The two contexts, as raw addresses, so that each task can find the other.
static PAIR: [AtomicUsize; 2] = [AtomicUsize::new(0), AtomicUsize::new(0)];
/// Registers that did not survive a hand-over, summed over every turn.
static MISMATCHES: AtomicUsize = AtomicUsize::new(0);

/// Stage 2's evidence: two kernel tasks alternate, and each one's registers are
/// its own when it comes back.
///
/// The check is the plan's own sentence for this stage - "a boot-time switch
/// between two kernel tasks" - made countable. Three claims, in the order they
/// matter:
///
/// * **Control actually changes hands.** The letters printed alternate `A B A B`,
///   so a switch that returned immediately would be visible rather than merely
///   not fatal.
/// * **Each task's registers are preserved across its own suspension.** Both
///   tasks stamp `x19..x28` and `v31` before every hand-over and check them
///   after, in one asm block, and the count of registers that came back wrong is
///   what this answers.
/// * **Both directions work.** Task A is the boot stack, never bootstrapped -
///   its context is filled in by its first switch *out* - and task B is built by
///   [`bootstrap`] and entered through the trampoline. A switch that only worked
///   for one of those shapes would pass half the turns and fail the others.
///
/// Answers how many turns this side took, or how many registers came back wrong
/// across every turn, so the caller can decide what to print.
pub fn boot_check() -> Result<usize, usize> {
    use core::fmt::Write;

    // Task B's stack, from the frame allocator - and specifically *not* a
    // `static [u8; N]`.
    //
    // That was the first version of this line and it faulted on the first store:
    // an immutable `static` with a zero initializer has no writer in Rust's type
    // system, so the compiler places it in `.rodata`, which this stage's page
    // tables have just made read-only. The symptom was a data abort with the
    // fault address *inside the kernel image*, which reads like a broken mapping
    // and was the linker being right. Stage 1d's page-table arena was an
    // immutable zeroed `static` too, and cost 128 KiB of `.rodata` for the same
    // reason (it was deleted with the `-kernel` path). Taking frames is both the
    // fix and the honest shape: it is how the scheduler will get a task's stack.
    //
    // Order 4 is sixteen pages, which is what a kernel task gets.
    const STACK_ORDER: usize = 4;
    let stack = crate::hal::kernel::alloc_frames(STACK_ORDER)
        .expect("out of frames for the boot check's second task stack");
    let top = crate::hal::phys_to_virt(stack) + (PAGE_SIZE << STACK_ORDER);

    let mut a = Context::empty();
    // SAFETY: `top` is the top of a 16-byte-aligned block in `.bss` that nothing
    // else uses, with far more than `BOOTSTRAP_BYTES` free below it.
    let b = unsafe { bootstrap(top, task_b, 0, false) };

    // Published as raw addresses because that is what a switch takes, and
    // because neither context may move: `a` lives on this frame for the length
    // of the check, `b` on this frame too, and both outlive every switch.
    PAIR[0].store(&mut a as *mut Context as usize, Ordering::Relaxed);
    PAIR[1].store(&b as *const Context as usize, Ordering::Relaxed);

    // Task A: this frame, on the boot stack. Its first switch out is what fills
    // in `a`'s stack pointer, which is why `Context::empty` is the right start
    // rather than a bootstrap of its own.
    while TAKEN.load(Ordering::Relaxed) < 2 * TURNS {
        let _ = pl011::Writer.write_str("A");
        let bad = unsafe { round_trip(&mut a, &b, A_PATTERN) };
        MISMATCHES.fetch_add(bad as usize, Ordering::Relaxed);
        TAKEN.fetch_add(1, Ordering::Relaxed);
    }

    let mismatches = MISMATCHES.load(Ordering::Relaxed);
    if mismatches == 0 {
        // The number of hand-overs actually taken - not a target, and not this
        // side's share of one. The two sides do not take the same number: the
        // side that resumes last takes one more turn than the side that had
        // already stopped. A check whose headline number is an assumption keeps
        // reading true after it has stopped being one.
        Ok(TAKEN.load(Ordering::Relaxed))
    } else {
        Err(mismatches)
    }
}

/// Task A's stamp. Any 64-bit pattern serves; two different ones make a
/// mis-restored register visible rather than merely suspicious.
const A_PATTERN: u64 = 0xa1a1_a1a1_a1a1_a1a1;
/// Task B's stamp.
const B_PATTERN: u64 = 0xb2b2_b2b2_b2b2_b2b2;

/// Task B: the bootstrapped half of the check.
///
/// It ends by handing back to task A rather than by returning, and that is the
/// check's shape rather than an oversight: `TAKEN` reaching the target means both
/// sides have taken `TURNS` turns, so the side that notices is the side that
/// must give control back to the frame that called [`boot_check`].
extern "C" fn task_b(_slot: usize) -> ! {
    use core::fmt::Write;

    // Task B suspends *into* its own context and resumes from task A's, which is
    // the mirror of what `boot_check`'s loop does - so the two pointers swap
    // roles here, and with them mutability.
    let a = PAIR[0].load(Ordering::Relaxed) as *const Context;
    let b = PAIR[1].load(Ordering::Relaxed) as *mut Context;
    while TAKEN.load(Ordering::Relaxed) < 2 * TURNS {
        let _ = pl011::Writer.write_str("B");
        // SAFETY: both addresses came from `boot_check`'s own frame, which is
        // still live - it is suspended, not gone - and neither context moves.
        let bad = unsafe { round_trip(b, a, B_PATTERN) };
        MISMATCHES.fetch_add(bad as usize, Ordering::Relaxed);
        TAKEN.fetch_add(1, Ordering::Relaxed);
    }
    // SAFETY: as above. This one never returns: task A's loop has already seen
    // `TAKEN` has already reached the target, so nothing switches back to B.
    unsafe { switch(b, a) };
    halt()
}
