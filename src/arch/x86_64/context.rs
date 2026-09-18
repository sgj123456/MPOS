//! Suspending one task and resuming another.
//!
//! What gets saved is only what the ABI says a callee must preserve: the six
//! callee-saved registers and the flags. Everything else is either caller-saved,
//! and therefore already dead at any point where a task can be switched out, or
//! it belongs to the interrupted task's own trap frame, still sitting on its
//! stack where `iretq` will find it when that task comes back.
//!
//! Two properties of the target keep this as small as it is. `disable-redzone`
//! means an interrupt cannot land on data a leaf function parked below its stack
//! pointer. And SSE is off with `soft-float`, so there is no XMM state to carry.

use core::arch::global_asm;

/// The suspended half of a task: one word, the stack pointer its registers were
/// left at.
///
/// The registers themselves are on the task's own stack and this points at them,
/// which is why a `Context` must never move while its task is alive.
#[derive(Debug)]
#[repr(transparent)]
pub struct Context {
    stack_pointer: usize,
}

impl Context {
    /// A context that has never been switched out of. The boot task starts here;
    /// its first switch away fills in the real value.
    pub const fn empty() -> Context {
        Context { stack_pointer: 0 }
    }
}

/// Registers [`switch`] pushes, not counting the return address the `call` that
/// reached it already pushed.
const SAVED_REGISTERS: usize = 7;

/// How much room below `stack_top` [`bootstrap`] needs.
pub const BOOTSTRAP_BYTES: usize = (SAVED_REGISTERS + 1) * size_of::<usize>();

/// Build the state a task starts in, so that the first [`switch`] to it lands in
/// the trampoline instead of in the middle of somebody else's work.
///
/// `entry` is called with `arg` in the first argument register, on a stack whose
/// top is `stack_top`. `interruptible` decides whether it runs with interrupts
/// on, which for a fresh task is the only thing that decides it: there is no
/// saved state and no interrupt to return to.
///
/// # Safety
///
/// `stack_top` must be the top of a 16-byte-aligned block that the caller owns,
/// that nothing else is using, and that has [`BOOTSTRAP_BYTES`] free below it.
pub unsafe fn bootstrap(
    stack_top: usize,
    entry: extern "C" fn(usize) -> !,
    arg: usize,
    interruptible: bool,
) -> Context {
    const RFLAGS_RESERVED: usize = 1 << 1;
    const RFLAGS_INTERRUPT_FLAG: usize = 1 << 9;

    // Both casts go through a function pointer first. A function *item* is
    // zero-sized, so casting one straight to an integer is a different operation
    // from taking its address and easy to get wrong.
    let trampoline: unsafe extern "C" fn() = mpos_task_trampoline;
    let entry = entry as usize;

    let rflags = RFLAGS_RESERVED
        | if interruptible {
            RFLAGS_INTERRUPT_FLAG
        } else {
            0
        };
    // Ordered the way `switch` pops them, which is the reverse of the way it
    // pushes them: flags first because they were pushed last.
    let words = [
        rflags,
        0,     // r15
        0,     // r14
        0,     // r13
        entry, // r12, which the trampoline calls
        0,     // rbp
        arg,   // rbx, which the trampoline passes along
        trampoline as usize,
    ];

    let base = stack_top - BOOTSTRAP_BYTES;
    let slot = base as *mut usize;
    for (index, word) in words.iter().enumerate() {
        unsafe { slot.add(index).write_volatile(*word) };
    }
    Context {
        stack_pointer: base,
    }
}

unsafe extern "C" {
    fn mpos_context_switch(from: *mut Context, to: *const Context);
    fn mpos_task_trampoline();
}

/// Leave ring 0 for ring 3, at `entry` with the stack at `user_rsp`, and never
/// come back to this call: the only ways out are the syscall gate and the
/// interrupt handlers, which return to user mode through their own `iretq`.
///
/// The frame is built by hand with ordinary pushes. `push` walks the stack
/// *down*, so the word pushed first ends up highest and is the one `iretq`
/// pops last - which makes the push order the exact reverse of the pop order.
/// `iretq` pops `RIP, CS, RFLAGS, RSP, SS`, so the pushes have to run
/// `SS, RSP, RFLAGS, CS, RIP`, and getting any pair of those the wrong way
/// round leaves two correct values in each other's slots. `cli` first, because
/// the frame and the jump must be atomic with respect to the clock - a tick in
/// between would run the timer handler on a stack whose top is a half-built
/// frame - and the `iretq` itself restores IF through the flags image it
/// consumes.
///
/// The flags image `enter_user` hands to ring 3. `IF` is set so the program
/// gets preempted on the next tick rather than monopolising the core, and bit 1
/// is the always-one bit `iretq` insists on.
pub const USER_RFLAGS: usize = (1 << 1) | (1 << 9);

/// # Safety
///
/// The active address space must map `entry` executable and `user_rsp`
/// read-write for ring 3. The caller must also never need its kernel stack
/// for anything but traps from here on: entering user mode abandons this call
/// frame permanently.
pub unsafe fn enter_user(entry: usize, user_rsp: usize) -> ! {
    let (code, data) = crate::arch::gdt::user_selectors();

    // Push in `iretq`'s pop order reversed, one operand per push so the
    // sequence below reads as that order rather than as a list to be
    // mentally inverted. `ss` goes first and `rip` last; the two
    // selectors and the flags image each land in the slot the instruction
    // will read them from.
    unsafe {
        core::arch::asm!(
            "cli",
            "push {ss}",
            "push {rsp}",
            "push {rflags}",
            "push {cs}",
            "push {rip}",
            "iretq",
            ss = in(reg) data.0 as usize,
            rsp = in(reg) user_rsp,
            rflags = in(reg) USER_RFLAGS,
            cs = in(reg) code.0 as usize,
            rip = in(reg) entry,
            options(noreturn),
        );
    }
}

/// Suspend the caller and resume whichever task `to` describes.
///
/// This returns twice: once here when the caller is eventually resumed, which
/// may be arbitrarily later and with other tasks having run in between. Any `&mut`
/// the caller holds therefore has to be dead before the call, or the task that
/// runs next will take a second `&mut` to the same data.
///
/// Interrupts are off on return, whatever they were on the way in. Re-enabling
/// them is the job of whoever enabled them in the first place - a critical
/// section's release, or the `iretq` of the interrupt that caused the preemption.
///
/// Switching to the context that is already running is a no-op rather than a
/// fault: the save and the restore cancel out.
///
/// # Safety
///
/// Both pointers must address a live `Context` that will not move for as long as
/// its task exists, and `to` must describe a task that is suspended or freshly
/// bootstrapped rather than one already running.
pub unsafe fn switch(from: *mut Context, to: *const Context) {
    unsafe { mpos_context_switch(from, to) }
}

/// The ring-3 machine state a process resumes with.
///
/// A forked child does not re-enter its program at the entry point: it comes
/// back from the very `fork` call its parent made, one instruction past the
/// `syscall`. The kernel therefore has to hand it the parent's register file
/// exactly as the gate left it, with a single difference - `rax` is zero, the
/// value that tells a program it is the child. This struct *is* that register
/// file, ordered so the assembly in [`resume_user`] can load it by offset.
///
/// `rcx` and `r11` are deliberately absent. `syscall` itself overwrites both,
/// the ABI declares them clobbered, and `iretq` takes their roles over: RIP and
/// RFLAGS are fields of the interrupt frame instead.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UserContext {
    pub rip: usize,
    pub rsp: usize,
    pub rflags: usize,
    pub rax: usize,
    pub rbx: usize,
    pub rbp: usize,
    pub cs: usize,
    pub ss: usize,
    pub r12: usize,
    pub r13: usize,
    pub r14: usize,
    pub r15: usize,
    pub rdi: usize,
    pub rsi: usize,
    pub rdx: usize,
    pub r8: usize,
    pub r9: usize,
    pub r10: usize,
}

unsafe extern "C" {
    /// Never returns: the `iretq` at its end leaves this call frame for good.
    fn mpos_resume_user(state: *const UserContext) -> !;
}

/// Enter ring 3 with the register file `state` describes - the child half of
/// `fork`.
///
/// Like [`enter_user`], this call frame is abandoned: the only ways out of the
/// new process are the syscall gate and the interrupt handlers, which return to
/// user mode through their own frames.
///
/// # Safety
///
/// `state` must describe a live user image: `rip` executable in the active
/// address space, `rsp` writable by ring 3, and `cs`/`ss` the user selectors.
/// The caller must not need its kernel stack for anything but traps afterwards.
pub unsafe fn resume_user(state: *const UserContext) -> ! {
    unsafe { mpos_resume_user(state) }
}

global_asm!(
    // rdi: the `UserContext` to enter ring 3 with.
    ".globl mpos_resume_user",
    ".p2align 4",
    "mpos_resume_user:",
    // Build the `iretq` frame first, in the order the instruction pops it:
    // SS, RSP, RFLAGS, CS, RIP. `push` walks the stack down, so the first of
    // these lands highest and is popped last, which is what makes the pop
    // order come out right. Reading each word straight out of the struct keeps
    // the register file working from a single pointer, with no scratch
    // register borrowed from the state being restored.
    //
    // This order is the load-bearing part and the easy thing to get wrong:
    // [enter_user] builds the same frame with operand pushes, and a pair of
    // them swapped there put RFLAGS where SS belonged and killed the first
    // ring-3 entry with a `#GP` naming 0x202 - the flags image - as the
    // offending selector. Both builders have to agree with this sequence.
    "    cli",
    "    push qword ptr [rdi + 56]", // ss
    "    push qword ptr [rdi + 8]",  // rsp
    "    push qword ptr [rdi + 16]", // rflags
    "    push qword ptr [rdi + 48]", // cs
    "    push qword ptr [rdi + 0]",  // rip
    // Then the general registers. rdi is loaded last because until that line
    // it is the only handle on the struct.
    "    mov rax, [rdi + 24]",
    "    mov rbx, [rdi + 32]",
    "    mov rbp, [rdi + 40]",
    "    mov r12, [rdi + 64]",
    "    mov r13, [rdi + 72]",
    "    mov r14, [rdi + 80]",
    "    mov r15, [rdi + 88]",
    "    mov rsi, [rdi + 104]",
    "    mov rdx, [rdi + 112]",
    "    mov r8,  [rdi + 120]",
    "    mov r9,  [rdi + 128]",
    "    mov r10, [rdi + 136]",
    "    mov rdi, [rdi + 96]",
    "    iretq",
);

global_asm!(
    // rdi: where to leave the caller's stack pointer.
    // rsi: where the next task's stack pointer was left.
    //
    // The `cli` is what makes the choice above safe. Saving the flags after
    // disabling interrupts means a task always resumes with them off, so nothing
    // can be delivered between picking a task and arriving in it.
    ".globl mpos_context_switch",
    ".p2align 4",
    "mpos_context_switch:",
    "    cli",
    "    push rbx",
    "    push rbp",
    "    push r12",
    "    push r13",
    "    push r14",
    "    push r15",
    "    pushfq",
    "    mov [rdi], rsp",
    "    mov rsp, [rsi]",
    "    popfq",
    "    pop r15",
    "    pop r14",
    "    pop r13",
    "    pop r12",
    "    pop rbp",
    "    pop rbx",
    "    ret",
    // Where a task's stack first lands. The pops above have already put the
    // entry point in r12 and the argument in rbx.
    ".globl mpos_task_trampoline",
    ".p2align 4",
    "mpos_task_trampoline:",
    "    mov rdi, rbx",
    // The `ret` that got us here left the stack wherever `bootstrap` put it.
    // Realign rather than constrain every stack top to make this line work.
    "    and rsp, -16",
    "    call r12",
    // `entry` is `-> !`. If it ever returns there is nothing left to run.
    "    ud2",
);
