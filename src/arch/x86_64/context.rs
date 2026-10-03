//! Suspending one task and resuming another.
//!
//! What gets saved is only what the ABI says a callee must preserve: the six
//! callee-saved registers and the flags. Everything else is either caller-saved,
//! and therefore already dead at any point where a task can be switched out, or
//! it belongs to the interrupted task's own trap frame, still sitting on its
//! stack where `iretq` will find it when that task comes back.
//!
//! Two properties of the target keep the *integer* half as small as it is.
//! `disable-redzone` means an interrupt cannot land on data a leaf function
//! parked below its stack pointer, and the kernel is built `soft-float`, so its
//! own use of the FPU is nonexistent and needs no saving.
//!
//! What the soft-float build does **not** remove is the *user's* floating-point
//! state. Two ring-3 programs share one physical x87/XMM register file, and the
//! switch is the only place anything can arrange for each to keep its own - the
//! kernel never touches those registers, so a syscall does not disturb them, but
//! it does *preempt*, and the program that runs next has no reason to leave the
//! registers as it found them. That state is [`FpuState`], carried in the first
//! 512 bytes of every [`Context`], and handled *lazily*: `mpos_context_switch`
//! sets `CR0.TS` on the task it enters instead of loading anything, so a task
//! that never runs a floating-point instruction never pays for a 512-byte move
//! it would not read, and the `#NM` its first such instruction provokes is what
//! restores its own image. The outgoing half stays eager and is done in Rust -
//! [`Context`]'s caller writes the leaving task's registers out before the
//! switch, because that is the last moment at which they are uncontroversially
//! its own.
//!
//! Leaving it out is not a missing optimisation, it is a wrong answer that shows
//! up as memory corruption. musl's `__init_tp` builds `{td, td}` in `xmm0`, makes
//! the `set_tid_address` syscall with it live, and only then stores it - over the
//! `prev` and `next` links of the main thread's control block. Any preemption in
//! that window (any other program's `memcpy`, which musl writes with `movups`)
//! hands the store whatever that program left in the register: with zeros, `fish`
//! ends up with an unlinked thread ring, `pthread_key_delete` walks off its end,
//! reads a pointer out of the process's own program headers, and stores into the
//! read-only first page of its image.

use core::arch::global_asm;

use x86_64::registers::control::{Cr0, Cr0Flags};

/// The 512 bytes `fxsave` and `fxrstor` move: the x87 control/status/tag words,
/// the eight x87 registers, `MXCSR` and the sixteen XMM registers.
///
/// `align(16)` is the instruction's requirement, not a preference - `fxsave`
/// raises `#GP` on a misaligned operand - and it is why this is the first field
/// of [`Context`]: a struct that *begins* with a 16-byte-aligned member is
/// 16-byte-aligned itself, whatever the caller does with it.
///
/// Only x87 and SSE are covered. AVX's upper halves would need `xsave` and a
/// CPUID check, and no program that runs here is built to use them: the guest
/// CPU has no AVX, and musl's string routines - which are what would reach for
/// it - are baseline x86-64.
#[repr(C, align(16))]
#[derive(Debug)]
pub struct FpuState {
    bytes: [u8; FPU_BYTES],
}

/// How many bytes one task's floating-point state occupies - what `fxsave`,
/// `fxrstor`, `PTRACE_GETFPREGS` and `PTRACE_SETFPREGS` all agree on.
pub const FPU_BYTES: usize = 512;

impl FpuState {
    /// The architectural reset state: the state a CPU is in after `fninit` and
    /// `ldmxcsr(0x1f80)`.
    ///
    /// Two fields have to be *written* rather than left zero, and both are load
    /// bearing. `MXCSR` at 0 would unmask every SSE exception, so the first
    /// inexact result a program computed would raise `#XM` - with no handler for
    /// it, and a kernel that has never needed one. And the x87 tag word has to
    /// say "all eight registers empty", not "all eight hold zero".
    ///
    /// A task that runs user code must therefore never start from a zeroed
    /// block; [`Context::empty`] and [`bootstrap`] are the two doors in, and
    /// both go through here.
    pub const fn reset() -> FpuState {
        let mut bytes = [0u8; 512];
        bytes[0] = 0x7f; // x87 control word 0x037f: round to nearest, 64-bit
        bytes[1] = 0x03;
        bytes[4] = 0xff; // abridged tag word: all x87 registers empty
        bytes[24] = 0x80; // MXCSR 0x1f80: all exceptions masked, round to nearest
        bytes[25] = 0x1f;
        FpuState { bytes }
    }

    /// The address `fxsave`/`fxrstor` take, for the assembly to use.
    fn ptr_mut(&mut self) -> *mut u8 {
        self.bytes.as_mut_ptr()
    }

    /// The same address for the read-only half: `fxrstor` reads the image.
    fn ptr(&self) -> *const u8 {
        self.bytes.as_ptr()
    }

    /// The image as a plain array, for a tracer to hand to ring 3.
    pub fn to_bytes(&self) -> [u8; FPU_BYTES] {
        self.bytes
    }

    /// An image a tracer supplied. Not validated: the caller is a debugger
    /// writing the registers its tracee will resume with, and every bit pattern
    /// in the area is a state `fxrstor` will load.
    pub fn from_bytes(bytes: &[u8; FPU_BYTES]) -> FpuState {
        FpuState { bytes: *bytes }
    }
}

impl Default for FpuState {
    fn default() -> FpuState {
        FpuState::reset()
    }
}

/// Copy the *live* floating-point registers into `state`.
///
/// For the two places that need the running task's state rather than a saved
/// one: a `fork` child inherits its parent's, and so does a thread. Reading the
/// registers touches no XMM register, so this is safe to call from the kernel
/// without disturbing the value it is copying.
///
/// # Safety
///
/// The CPU must be able to execute `fxsave`: `CR4.OSFXSR` set (see
/// [`super::enable_user_sse`]) and `CR0.TS` and `CR0.EM` clear (nothing here
/// sets either).
pub unsafe fn save_live_fpu(state: &mut FpuState) {
    unsafe {
        core::arch::asm!(
            "fxsave [{ptr}]",
            ptr = in(reg) state.ptr_mut(),
            options(nostack, preserves_flags),
        );
    }
}

/// Put the *live* floating-point registers back to the architectural reset
/// state, so that they agree with an [`FpuState::reset`] block.
///
/// `execve` is the caller: the image it discards had whatever state it had, and
/// the next program must not be able to read it out of the registers - which is
/// what it would do if only the *saved* block were reset, because the registers
/// are the copy that is live until the next switch.
///
/// # Safety
///
/// Same requirement as [`save_live_fpu`], plus that restoring the register file
/// is what the caller wants: it discards the current contents.
pub unsafe fn reset_live_fpu() {
    static DEFAULT_MXCSR: u32 = 0x1f80;
    unsafe {
        core::arch::asm!(
            "fninit",
            "ldmxcsr [{mxcsr}]",
            mxcsr = in(reg) &raw const DEFAULT_MXCSR,
            options(nostack, preserves_flags),
        );
    }
}

/// Clear `CR0.TS`, so floating-point instructions execute instead of trapping.
///
/// Two callers, and both are saying the same thing - "the registers are the
/// running task's now": the `#NM` handler, immediately before it loads the
/// image it trapped for, and [`reset_live_fpu`]'s caller on `execve`, which is
/// about to make the registers hold a reset state nobody else can claim.
///
/// # Safety
///
/// Whoever calls this owns the registers from here until the next switch. If
/// another task's image is in them, this does not move it out - it makes the
/// next floating-point instruction read that task's state and then destroy it.
pub unsafe fn claim_live_fpu() {
    unsafe {
        Cr0::update(|flags| flags.remove(Cr0Flags::TASK_SWITCHED));
    }
}

/// Whether the CPU is currently refusing floating-point instructions.
pub fn fpu_trapped() -> bool {
    Cr0::read().contains(Cr0Flags::TASK_SWITCHED)
}

/// Load a saved image back into the hardware registers.
///
/// `CR0.TS` has to be clear already - `fxrstor` faults on it like any other
/// floating-point instruction - which is what [`claim_live_fpu`] is for. The
/// pair is the two halves of the deferred restore the `#NM` handler performs.
///
/// # Safety
///
/// Same requirement as [`save_live_fpu`], plus that the caller has actually
/// claimed the registers: loading an image on top of one another task still
/// owns loses that task's state.
pub unsafe fn restore_fpu(state: &FpuState) {
    unsafe {
        core::arch::asm!(
            "fxrstor [{ptr}]",
            ptr = in(reg) state.ptr(),
            options(nostack, preserves_flags),
        );
    }
}

/// The suspended half of a task: its floating-point state and the stack pointer
/// its integer registers were left at.
///
/// `fpu` comes first so that it starts 16 bytes into nothing - see [`FpuState`].
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
        fpu: FpuState::reset(),
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
/// The task entered resumes with `CR0.TS` set and its floating-point image *not*
/// loaded; the `#NM` handler is what finishes the job. Unlike the general
/// registers this is not a detail the caller may skip: nothing else on the way
/// in would restore the image, so a caller that bypasses this function to resume
/// a task has to arrange the restore itself.
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
    // The floating-point state is deliberately *not* moved here. It used to be
    // an unconditional `fxsave`/`fxrstor` pair on the 512 bytes at offset zero
    // of each `Context`, which cost every switch - including the overwhelming
    // majority between tasks that never touch an XMM register - the price of
    // two 512-byte memory operations.
    //
    // It is lazy now: the task being left has already had its registers written
    // out by `sched::Table::hand_over_fpu`, which runs in Rust while the
    // outgoing task is still the one on the CPU (so its live state is genuinely
    // its own), and the task being entered gets `CR0.TS` set below so its first
    // floating-point instruction traps into the `#NM` handler and restores its
    // own image then. A program that never uses the FPU pays one instruction.
    //
    // The `CR0.TS` write has to happen *here* rather than in the Rust handover:
    // between that call and this switch the kernel is still running on the
    // outgoing task's stack, and setting `TS` there would put the whole
    // scheduler in the window where a floating-point instruction - were there
    // one in the kernel, which `soft-float` says there is not - would fault
    // with the table lock held. On this side of `mov rsp` the only instructions
    // left are integer register moves and `ret`. `rax` is caller-saved, so
    // clobbering it is within the ABI of an `extern "C"` call.
    "    push rbx",
    "    push rbp",
    "    push r12",
    "    push r13",
    "    push r14",
    "    push r15",
    "    pushfq",
    "    mov [rdi + {sp}], rsp",
    "    mov rsp, [rsi + {sp}]",
    // `CR0.TS`: the incoming task's floating-point registers are *not* in the
    // hardware yet, so its first use of them must fault. Whoever claims them
    // clears the bit again in `claim_live_fpu`, which is the same place that
    // loads the image.
    "    mov rax, cr0",
    "    or rax, 8",
    "    mov cr0, rax",
    "    popfq",
    "    pop r15",
    "    pop r14",
    "    pop r13",
    "    pop r12",
    "    pop rbp",
    "    pop rbx",
    "    ret",
    sp = const CONTEXT_STACK_POINTER,
);

global_asm!(
    // Where a task's stack first lands. The pops in `mpos_context_switch` have
    // already put the entry point in r12 and the argument in rbx - the two
    // registers `bootstrap` wrote for the purpose. Separate from the switch
    // above only because a `global_asm!` template may not be resumed after a
    // named operand.
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

// The machine's answers to the HAL's context contract. Every body that has an
// inherent method to reach forwards to it as `Type::<name>`, deliberately
// *without* the trait in scope, so the path can only resolve to the inherent
// impl and cannot recurse into this same block.
impl crate::hal::ContextOps for Context {
    type Fpu = FpuState;

    #[inline]
    fn empty() -> Context {
        Context::empty()
    }

    #[inline]
    fn fpu(&self) -> &FpuState {
        &self.fpu
    }

    #[inline]
    fn fpu_mut(&mut self) -> &mut FpuState {
        &mut self.fpu
    }
}

impl crate::hal::FpuOps for FpuState {
    #[inline]
    fn reset() -> FpuState {
        FpuState::reset()
    }

    #[inline]
    fn to_bytes(&self) -> [u8; FPU_BYTES] {
        FpuState::to_bytes(self)
    }

    #[inline]
    fn from_bytes(bytes: &[u8; FPU_BYTES]) -> FpuState {
        FpuState::from_bytes(bytes)
    }
}

/// `RIP`/`RSP`/`CS`/`SS` in the Linux x86_64 `user_regs_struct`, and the whole
/// of the field order [`UserContext`] is written out in.
///
/// From the x86_64 Linux ABI, `struct user_regs_struct` - the twin table is
/// `crate::syscall::linux::abi::uregs`, which is where the ABI's own code reads
/// it. The duplication is deliberate and *checked*: `main.rs`'s milestone checks
/// assert this table against that one on every boot, so the two cannot drift
/// silently. They cannot share a definition because this module is bare-metal
/// only and `abi.rs` is host-compiled too - the gate is on the module, so a
/// shared constant would have to live on one side of it or the other.
mod uregs {
    pub const R15: usize = 0;
    pub const R14: usize = 1;
    pub const R13: usize = 2;
    pub const R12: usize = 3;
    pub const RBP: usize = 4;
    pub const RBX: usize = 5;
    pub const R10: usize = 7;
    pub const R9: usize = 8;
    pub const R8: usize = 9;
    pub const RAX: usize = 10;
    pub const RDX: usize = 12;
    pub const RSI: usize = 13;
    pub const RDI: usize = 14;
    pub const RIP: usize = 16;
    pub const CS: usize = 17;
    pub const EFLAGS: usize = 18;
    pub const RSP: usize = 19;
    pub const SS: usize = 20;
    pub const FS_BASE: usize = 21;
    pub const COUNT: usize = 27;
}

/// The field order this table and `abi::uregs` must agree on, for the boot check.
pub const ABI_REG_INDEX: [(&str, usize); 19] = [
    ("R15", uregs::R15),
    ("R14", uregs::R14),
    ("R13", uregs::R13),
    ("R12", uregs::R12),
    ("RBP", uregs::RBP),
    ("RBX", uregs::RBX),
    ("R10", uregs::R10),
    ("R9", uregs::R9),
    ("R8", uregs::R8),
    ("RAX", uregs::RAX),
    ("RDX", uregs::RDX),
    ("RSI", uregs::RSI),
    ("RDI", uregs::RDI),
    ("RIP", uregs::RIP),
    ("CS", uregs::CS),
    ("EFLAGS", uregs::EFLAGS),
    ("RSP", uregs::RSP),
    ("SS", uregs::SS),
    ("FS_BASE", uregs::FS_BASE),
];

/// The ABI's register count, from this side, for the same check.
pub const ABI_REG_COUNT: usize = uregs::COUNT;

impl crate::hal::UserFrameOps for UserContext {
    #[inline]
    fn forked(&self, child_stack: usize) -> UserContext {
        let mut child = *self;
        // A thread starts on the stack `clone` handed it; a process comes back
        // from the call on the stack its parent was using.
        if child_stack != 0 {
            child.rsp = child_stack;
        }
        child
    }

    #[inline]
    fn set_result(&mut self, value: isize) {
        self.rax = value as usize;
    }

    #[inline]
    fn ip(&self) -> usize {
        self.rip
    }

    #[inline]
    fn set_ip(&mut self, ip: usize) {
        self.rip = ip;
    }

    #[inline]
    fn sp(&self) -> usize {
        self.rsp
    }

    #[inline]
    fn set_sp(&mut self, sp: usize) {
        self.rsp = sp;
    }

    fn to_user_regs(&self, tls: usize, out: &mut [u64]) {
        use uregs::*;
        debug_assert_eq!(out.len(), COUNT, "the ABI's register file is 27 words");

        out[R15] = self.r15 as u64;
        out[R14] = self.r14 as u64;
        out[R13] = self.r13 as u64;
        out[R12] = self.r12 as u64;
        out[RBP] = self.rbp as u64;
        out[RBX] = self.rbx as u64;
        out[R10] = self.r10 as u64;
        out[R9] = self.r9 as u64;
        out[R8] = self.r8 as u64;
        out[RAX] = self.rax as u64;
        out[RDX] = self.rdx as u64;
        out[RSI] = self.rsi as u64;
        out[RDI] = self.rdi as u64;
        out[RIP] = self.rip as u64;
        out[CS] = self.cs as u64;
        out[EFLAGS] = self.rflags as u64;
        out[RSP] = self.rsp as u64;
        out[SS] = self.ss as u64;
        out[FS_BASE] = tls as u64;
    }

    fn from_user_regs(regs: &[u64]) -> UserContext {
        use uregs::*;
        // The slots this frame does not carry - `rcx`/`r11` (the two `syscall`
        // clobbers), `orig_rax`, the segment bases but `fs` - are read past
        // rather than stored: a task cannot be relying on them across a call.
        UserContext {
            rip: regs[RIP] as usize,
            rsp: regs[RSP] as usize,
            rflags: regs[EFLAGS] as usize,
            rax: regs[RAX] as usize,
            rbx: regs[RBX] as usize,
            rbp: regs[RBP] as usize,
            cs: regs[CS] as usize,
            ss: regs[SS] as usize,
            r12: regs[R12] as usize,
            r13: regs[R13] as usize,
            r14: regs[R14] as usize,
            r15: regs[R15] as usize,
            rdi: regs[RDI] as usize,
            rsi: regs[RSI] as usize,
            rdx: regs[RDX] as usize,
            r8: regs[R8] as usize,
            r9: regs[R9] as usize,
            r10: regs[R10] as usize,
        }
    }

    #[inline]
    unsafe fn resume(&self) -> ! {
        // SAFETY: the caller's contract, passed through unchanged.
        unsafe { resume_user(self) }
    }
}
