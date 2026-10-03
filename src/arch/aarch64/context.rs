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
//!   meaning from the ABI, so they belong with it (stage 6): writing them now
//!   would be a struct nothing can check against `user_regs_struct` and an
//!   entry sequence with no user address space to enter. `PLAN.md` §9.1's stage
//!   table carries that split, which is why this file's stage number is 2 and
//!   the register file's is 6.

use core::arch::global_asm;
use core::sync::atomic::{AtomicUsize, Ordering};

use super::{halt, pl011};
use crate::mm::{self, PAGE_SIZE};

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

unsafe extern "C" {
    fn mpos_context_switch(from: *mut Context, to: *const Context);
    fn mpos_task_trampoline();
}

global_asm!(
    // x0: the context being suspended (written). x1: the one being resumed.
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
    let stack = mm::alloc_frames(STACK_ORDER)
        .expect("out of frames for the boot check's second task stack");
    let top = mm::phys_to_virt(stack) + (PAGE_SIZE << STACK_ORDER);

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
