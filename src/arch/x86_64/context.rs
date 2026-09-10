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
/// The interrupt frame is built by hand with ordinary pushes rather than with
/// an `iretq`-shaped struct, so the five words land in the order the
/// instruction pops them. `cli` first, because the frame and the jump must be
/// atomic with respect to the clock - a tick in between would run the timer
/// handler on a stack whose top is a half-built frame - and the `iretq` itself
/// restores IF through the flags image it consumes.
///
/// # Safety
///
/// The active address space must map `entry` executable and `user_rsp`
/// read-write for ring 3. The caller must also never need its kernel stack
/// for anything but traps from here on: entering user mode abandons this call
/// frame permanently.
pub unsafe fn enter_user(entry: usize, user_rsp: usize) -> ! {
    const RFLAGS_RESERVED: usize = 1 << 1;
    const RFLAGS_INTERRUPT_FLAG: usize = 1 << 9;

    let (code, data) = crate::arch::gdt::user_selectors();
    unsafe {
        core::arch::asm!(
            "cli",
            "push {data}",
            "push {stack}",
            "push {flags}",
            "push {code}",
            "push {entry}",
            "iretq",
            data = in(reg) data.0 as usize,
            stack = in(reg) user_rsp,
            flags = in(reg) RFLAGS_RESERVED | RFLAGS_INTERRUPT_FLAG,
            code = in(reg) code.0 as usize,
            entry = in(reg) entry,
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
