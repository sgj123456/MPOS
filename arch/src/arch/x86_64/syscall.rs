//! The one `syscall` gate: how ring 3 turns registers into a syscall and
//! gets the result back in `rax`.
//!
//! One instruction entry serves both number spaces, and the number space is
//! the only distinction the gate knows about: mpos's own syscall numbers,
//! and the Linux x86_64 numbers a musl binary speaks. The two cannot share
//! the raw `rax` - mpos's 0-16 collide with Linux's read/write/open - so
//! user code tags mpos numbers with "mpos" in the top 32 bits (see
//! user/lib/sys.rs), and Linux numbers ride raw. The tag is stripped before
//! anything downstream, dispatch included, sees the number.
//!
//! The stub is raw asm rather than the `x86-interrupt` ABI, for two reasons:
//! that ABI exposes none of the caller's registers, and `syscall` did not
//! switch stacks in the first place. The stub captures the caller's RSP,
//! moves onto the current task's kernel stack through `swapgs`, builds the
//! register block the dispatch wants, calls Rust, stores the result back
//! over the saved `rax`, and `sysretq` restores the rest itself.
//!
//! What it must not do is borrow a register: the Linux ABI leaves rbp, rbx,
//! r12-r15 and RSP to user code, so the stack switch parks the caller's RSP in
//! the `swapgs` scratch instead of in r12 - see [`super::gdt`]'s
//! `EntryScratch`. A libc is entitled to keep a live value in r12 across a
//! `syscall`, and one that does gets that value back unchanged.
//!
//! FMASK clears IF on the way in, so the gate runs with interrupts off until
//! the handler yields or sleeps - the same contract the clock handler keeps:
//! a task that switches away inside the gate resumes with interrupts off and
//! re-enables them by unwinding through the critical section that suspended
//! it, or at `sysretq`.

use core::arch::global_asm;

unsafe extern "C" {
    fn syscall_entry();
}

pub fn init() {
    unsafe {
        // EFER (0xC0000080): enable SCE (System Call Extensions, bit 0)
        // so the `syscall` instruction is not a #UD.
        let mut lo: u32;
        let mut hi: u32;
        core::arch::asm!(
            "rdmsr",
            in("ecx") 0xC000_0080u32,
            out("eax") lo,
            out("edx") hi,
        );
        lo |= 1; // SCE bit
        core::arch::asm!(
            "wrmsr",
            in("ecx") 0xC000_0080u32,
            in("eax") lo,
            in("edx") hi,
        );

        // MSR_LSTAR (0xC0000082): kernel entry for `syscall` instruction.
        let entry = syscall_entry as *const () as usize as u64;
        core::arch::asm!(
            "wrmsr",
            in("ecx") 0xC000_0082u32,
            in("edx") (entry >> 32) as u32,
            in("eax") entry as u32,
        );

        // MSR_FMASK (0xC0000084): mask interrupts on syscall entry (IF=0).
        // Bit 9 = IF.
        core::arch::asm!(
            "wrmsr",
            in("ecx") 0xC000_0084u32,
            in("edx") 0u32,
            in("eax") (1u32 << 9),
        );

        // MSR_KERNEL_GS_BASE (0xC0000102): what `swapgs` installs as GS.base on
        // the way in. The entry stub runs before it has a stack, so the only
        // way to find one is through an MSR - and this is the one `swapgs`
        // swaps in. It points at the entry scratch, whose first quadword
        // `set_kernel_stack` keeps equal to TSS.RSP0, so `mov rsp, gs:[0]`
        // lands on the running task's kernel stack. Writing it is mandatory:
        // with KERNEL_GS_BASE still zero, `swapgs` leaves GS.base at zero and
        // the `gs:[0]` load faults on the null page.
        let krsp = super::gdt::entry_scratch_addr();
        core::arch::asm!(
            "wrmsr",
            in("ecx") 0xC000_0102u32,
            in("edx") (krsp >> 32) as u32,
            in("eax") krsp as u32,
        );

        // MSR_STAR (0xC0000081): segment selectors. `syscall` takes the kernel
        // CS from bits 47:32 (SS is CS+8); `sysret` computes its pair from bits
        // 63:48 as `+16` (CS) and `+8` (SS). The SDM says both arrive with
        // RPL 3 forced, but this machine's host CPU applies it to neither
        // reliably, so `user_code_star_base` bakes the RPL bits into the base
        // itself - see the XSA-132 note there. Bits 31:0 are the SYSEXIT
        // pair, unused here.
        let user_base = super::gdt::user_code_star_base();
        // Kernel CS/SS: `syscall` loads the kernel CS from these bits and
        // takes the kernel SS from CS + 8, so the table's data descriptor
        // must sit directly above its code descriptor.
        let (kern_code, kern_data) = super::gdt::kernel_selectors();
        debug_assert_eq!(
            kern_data.0,
            kern_code.0 + 8,
            "syscall derives the kernel SS from the CS, one slot up"
        );
        let kern_cs = kern_code.0 as u64;
        let star = (user_base << 48) | (kern_cs << 32);
        core::arch::asm!(
            "wrmsr",
            in("ecx") 0xC000_0081u32,
            in("edx") (star >> 32) as u32,
            in("eax") star as u32,
        );

        // `sysret` is the *other* way back to ring 3, and unlike `enter_user`'s
        // `iretq` it builds no frame anyone can inspect - it derives both
        // selectors from what was just written here. The SDM says it forces
        // RPL 3 on each (`CS.Selector ← STAR[63:48]+16; CS.Selector ←
        // CS.Selector OR 3`, likewise SS), but the host this boots on does not
        // honor that for SS: the first log of this bug shows a user task
        // running with SS 0x18 from exactly this derivation. Hence the base
        // carries the RPL bits pre-applied, and the check below is on the
        // *raw* sums - without the OR - so a CPU that skips the OR still
        // loads ring-3 selectors.
        let (user_code, user_data) = super::gdt::user_selectors();
        let derived_cs = (user_base + 16) | 3;
        let derived_ss = (user_base + 8) | 3;
        debug_assert_eq!(
            derived_cs, user_code.0 as u64,
            "sysret would load CS {:#x}, not the user code selector",
            derived_cs
        );
        debug_assert_eq!(
            derived_ss, user_data.0 as u64,
            "sysret would load SS {:#x}, not the user data selector",
            derived_ss
        );
        // The invariant that matters on this host: the sums must carry RPL 3
        // before any `OR 3`. `| 3` on the base is what guarantees it, and this
        // assert fails if anyone "simplifies" that away.
        debug_assert_eq!(
            (user_base + 8) & 0b11,
            0b11,
            "sysret's raw SS sum must already carry RPL 3 - some CPUs skip the OR"
        );
        debug_assert_eq!(
            (user_base + 16) & 0b11,
            0b11,
            "sysret's raw CS sum must already carry RPL 3 - some CPUs skip the OR"
        );
    }
}

/// Register block pushed by the `syscall` entry stub.
///
/// Layout matches the entry stub's push order exactly: the last push (rax, the
/// syscall number) sits at offset 0, so the return value patches with one
/// `mov [rsp]`. Every field order here must equal the push order, because the
/// stub reads the frame back by offset where it cannot pop. `user_rsp` is the
/// caller's RSP that `syscall` left in the register (the CPU switches no stack
/// for `syscall`), parked and recovered by the stub itself - `sysretq` takes
/// the return address from rcx and the flags from r11 and nothing at all for
/// the stack. `r12` sits where the stub pushed it: a saved caller value, not
/// scratch.
///
/// The frame is one word longer than this struct's *fields*: the stub pushes a
/// second copy of `rax` between the caller's RSP and the caller's r12, so that
/// the nine `pop`s in its epilogue consume whole words without ever landing on
/// the two values recovered by offset. That filler is not a register and so is
/// spelled as a padding field here - leaving it out would shift `r12` and
/// everything above it one word down in this struct relative to the frame, and
/// a `fork` reconstructing a child from the block would hand it an `r12` that
/// held the caller's rdi.
#[repr(C)]
struct Regs {
    rax: usize,
    r11: usize,
    rcx: usize,
    r9: usize,
    r8: usize,
    r10: usize,
    rdx: usize,
    rsi: usize,
    rdi: usize,
    user_rsp: usize,
    /// The stub's filler word. Never meaningful: it is a second copy of `rax`,
    /// pushed purely to hold the frame open one word so the offsets line up.
    _filler: usize,
    r12: usize,
    r15: usize,
    r14: usize,
    r13: usize,
    rbx: usize,
    rbp: usize,
}

/// The gate's frame, as `hal`'s seam hands it to the ABI layer.
///
/// x86_64's gate frame is the entry stub's saved block ([`Regs`]), not the task's
/// user frame - which is exactly why `hal::Gate` is a set of functions over an
/// opaque pointer: the two machines' gate frames are different objects, and this
/// is the only place that difference lives.
impl crate::hal::Gate for crate::hal::Current {
    unsafe fn user_ip(frame: *mut ()) -> u64 {
        // SAFETY: the caller's contract - the pointer is the frame
        // `rust_syscall` was handed.
        let regs = unsafe { &*(frame as *const Regs) };
        // `syscall` parked the caller's return address in rcx, which is where
        // `sysretq` takes it from again.
        regs.rcx as u64
    }

    unsafe fn set_exec_return(frame: *mut (), entry: usize, sp: usize) {
        // SAFETY: as above.
        let regs = unsafe { &mut *(frame as *mut Regs) };
        // `sysretq` restores RIP from rcx and RSP from the value the stub parked,
        // so this is what makes `execve` "not return": the very next instruction
        // is the new program's entry, on its own stack. The result slot is left
        // alone deliberately - the gate writes the dispatch's result (which is
        // zero here) into it after the handler returns, and `rax` doubles as
        // `orig_rax` for a tracer, so pre-zeroing it would change what a
        // `PTRACE_EVENT_EXEC` stop reports.
        regs.rcx = entry;
        regs.user_rsp = sp;
    }

    unsafe fn user_context(frame: *mut (), rax: usize) -> Option<crate::hal::UserFrame> {
        // SAFETY: the caller's contract - the pointer is the frame
        // `rust_syscall` was handed, or null for a call the kernel made on its
        // own behalf.
        if frame.is_null() {
            return None;
        }
        let regs = unsafe { &*(frame as *const Regs) };
        let (code, data) = super::gdt::user_selectors();
        Some(crate::arch::x86_64::context::UserContext {
            // `syscall` parked the return address in rcx and the caller's flags
            // in r11; those two are what an `iretq` needs to reproduce the
            // state the instruction left behind.
            rip: regs.rcx,
            rsp: regs.user_rsp,
            rflags: regs.r11,
            rax,
            rbx: regs.rbx,
            rbp: regs.rbp,
            cs: code.0 as usize,
            ss: data.0 as usize,
            r12: regs.r12,
            r13: regs.r13,
            r14: regs.r14,
            r15: regs.r15,
            rdi: regs.rdi,
            rsi: regs.rsi,
            rdx: regs.rdx,
            r8: regs.r8,
            r9: regs.r9,
            r10: regs.r10,
        })
    }

    unsafe fn capture_user_regs(frame: *mut (), rax: usize, tls: usize, out: &mut [u64]) {
        use super::context::uregs::*;
        // SAFETY: the caller's contract.
        let regs = unsafe { &*(frame as *const Regs) };
        debug_assert_eq!(out.len(), COUNT, "the ABI's register file is 27 words");
        let (code, data) = super::gdt::user_selectors();
        out[R15] = regs.r15 as u64;
        out[R14] = regs.r14 as u64;
        out[R13] = regs.r13 as u64;
        out[R12] = regs.r12 as u64;
        out[RBP] = regs.rbp as u64;
        out[RBX] = regs.rbx as u64;
        // The two slots `syscall` clobbers are reported twice: under their own
        // names, and as the `rip`/`eflags` a tracer reads them through.
        out[R11] = regs.r11 as u64;
        out[R10] = regs.r10 as u64;
        out[R9] = regs.r9 as u64;
        out[R8] = regs.r8 as u64;
        out[RAX] = rax as u64;
        out[RCX] = regs.rcx as u64;
        out[RDX] = regs.rdx as u64;
        out[RSI] = regs.rsi as u64;
        out[RDI] = regs.rdi as u64;
        out[ORIG_RAX] = regs.rax as u64;
        out[RIP] = regs.rcx as u64;
        out[CS] = code.0 as u64;
        out[EFLAGS] = regs.r11 as u64;
        out[RSP] = regs.user_rsp as u64;
        out[SS] = data.0 as u64;
        // The thread-local base is a CPU register, not a frame word: the gate
        // reinstalls it from the task on every switch, so the task is where it
        // is read from - and where a `PTRACE_SETREGS` puts it back.
        out[FS_BASE] = tls as u64;
        // The segment registers and the user GS base are left at the zero the
        // caller's buffer starts at: long mode ignores DS, ES, FS and GS - the
        // contents select nothing - and nothing here ever sets a user GS base.
        // FS *is* meaningful, and it is not a segment at all but the
        // thread-local base above; a tracer reading this structure sees the
        // base in `FS_BASE` and a selector it may ignore in `FS`.
    }

    unsafe fn restore_user_regs(frame: *mut (), regs: &[u64], tls: &mut usize) -> isize {
        use super::context::uregs::*;
        // SAFETY: the caller's contract.
        let frame = unsafe { &mut *(frame as *mut Regs) };
        debug_assert_eq!(regs.len(), COUNT, "the ABI's register file is 27 words");
        frame.r15 = regs[R15] as usize;
        frame.r14 = regs[R14] as usize;
        frame.r13 = regs[R13] as usize;
        frame.r12 = regs[R12] as usize;
        frame.rbp = regs[RBP] as usize;
        frame.rbx = regs[RBX] as usize;
        // `sysretq` restores the flags from r11 and the return address from
        // rcx, so the ABI's `eflags`/`rip` slots land there. The ABI's own
        // `r11`/`rcx` slots hold the same two values and are not read here -
        // a tracer's write reaches the CPU through the slots it reads, not
        // through both spellings.
        frame.r11 = regs[EFLAGS] as usize;
        frame.r10 = regs[R10] as usize;
        frame.r9 = regs[R9] as usize;
        frame.r8 = regs[R8] as usize;
        frame.rcx = regs[RIP] as usize;
        frame.rdx = regs[RDX] as usize;
        frame.rsi = regs[RSI] as usize;
        frame.rdi = regs[RDI] as usize;
        frame.user_rsp = regs[RSP] as usize;
        *tls = regs[FS_BASE] as usize;
        regs[RAX] as isize
    }

    unsafe fn syscall_args(frame: *mut ()) -> [usize; 6] {
        // SAFETY: the caller's contract.
        let regs = unsafe { &*(frame as *const Regs) };
        // The Linux x86_64 argument order, which is *not* the field order: the
        // fourth argument is in r10 because `syscall` clobbers rcx.
        [regs.rdi, regs.rsi, regs.rdx, regs.r10, regs.r8, regs.r9]
    }

    unsafe fn syscall_entry_result_slot(_frame: *mut (), nr: usize) -> usize {
        // The number, which is what this machine's `syscall` instruction left in
        // the result slot and what a tracer reads there. See the trait's note
        // for why the other machine cannot answer this way.
        nr
    }

    unsafe fn set_result(frame: *mut (), value: isize) {
        if frame.is_null() {
            return;
        }
        // SAFETY: the caller's contract.
        let frame = unsafe { &mut *(frame as *mut Regs) };
        // The slot `sysretq` returns in, and the one the stub pops the caller's
        // `rax` back out of. Writing it here as well as there is not a second
        // source of truth: both write the same call's result, and this is the
        // one that happens *before* a handler is entered.
        frame.rax = value as usize;
    }
}

extern "C" fn rust_syscall(regs: *mut Regs) -> usize {
    let regs = unsafe { &mut *regs };
    let args = [regs.rdi, regs.rsi, regs.rdx, regs.r10, regs.r8, regs.r9];
    // One number space - Linux x86_64 - serves every call. The gate is only
    // ever entered from ring 3 (a `syscall` at ring 0 would load a user CS
    // and fault), so every call here is from user.
    let regs_ptr = regs as *mut Regs as *mut ();
    // SAFETY: `regs_ptr` is this gate's own frame, just built by the stub.
    unsafe { crate::hal::kernel::dispatch_syscall(regs.rax as u64, args, true, regs_ptr) as usize }
}

global_asm!(
    ".section .text.syscall_entry, \"ax\"",
    ".globl syscall_entry",
    ".p2align 4",
    "syscall_entry:",
    // `syscall` saved the caller's RIP in rcx and RFLAGS in r11, but unlike an
    // interrupt it did NOT switch RSP to a kernel stack: the CPU is still in
    // ring 0, running on the caller's RSP. Park that RSP in the entry scratch
    // and move onto the current task's kernel stack (whose top lives in the
    // scratch's first word, kept in lock-step with TSS.RSP0 by
    // `set_kernel_stack`) before pushing anything - a timer interrupt
    // mid-syscall would otherwise fault on the way back.
    //
    // The switch needs one register it may overwrite, and it may not borrow
    // any of the caller's: r12 included, because it is callee-saved on both
    // the SysV and the Linux syscall ABI, and a libc keeps live values in it
    // across a `syscall`. So r12 is parked in the scratch's second word, read
    // back as the frame records it, and the *value* in r12 becomes the
    // caller's RSP - the one register the stub is free to reuse once its old
    // contents are somewhere safe. Store and reload both happen between the
    // two `swapgs`es, with interrupts still off (`syscall` enters with IF
    // clear), so nothing else can observe the slot.
    "    swapgs",
    "    mov  qword ptr gs:[8], r12", // park the caller's r12
    "    mov  r12, rsp",      // r12 = the caller's RSP
    "    mov  rsp, qword ptr gs:[0]", // rsp = current kernel stack top
    // Preserve every register the Linux x86_64 syscall ABI promises to keep:
    // rbp, rbx, r12-r15 (which this stub never touches), and the caller's RSP.
    // Then the args and the two `syscall`-stashed values. rax (the number) is
    // pushed last, so it sits at offset 0 and the return value patches with
    // one `mov [rsp]`. rbx is saved rather than merely left alone because
    // `fork` has to reconstruct the child's whole register file from this
    // frame, and a register the frame does not carry cannot be reconstructed.
    "    push rbp",
    "    push rbx",
    "    push r13",
    "    push r14",
    "    push r15",
    "    push qword ptr gs:[8]", // the caller's r12, read back through the kernel GS
    // One filler word, and it has to sit exactly here. The epilogue's nine
    // `pop`s restore nine registers and walk *up* from `rbp`, so whatever is
    // nine words below the top is where they stop; the two values they cannot
    // reach - the caller's RSP and r12 - have to be below that. Sixteen pushes
    // would put the caller's RSP exactly where the ninth pop lands, so the
    // frame carries seventeen. This is the extra one: a second copy of `rax`,
    // which nothing reads back, placed so that the block below it is the
    // register file `Regs` describes, word for word.
    "    push rax",
    "    push r12",           // the caller's RSP
    "    swapgs",             // user GS base restored
    "    push rdi",
    "    push rsi",
    "    push rdx",
    "    push r10",
    "    push r8",
    "    push r9",
    "    push rcx",           // user RIP
    "    push r11",           // user RFLAGS
    "    push rax",           // syscall number, at offset 0 so [rsp] patches
    "    mov  rbp, rsp",      // rbp = &Regs (base for the call and restore)
    "    mov  rdi, rbp",
    "    and  rsp, -16",
    "    call {rust_entry}",
    // rax = return value. Patch the rax slot, then pop back the half of the
    // frame the stub never wrote to: the six registers and the two
    // `syscall`-stashed values, leaving rsp on the slot that holds the
    // caller's RSP.
    "    mov  rsp, rbp",
    "    mov  [rsp], rax",
    "    pop  rax",           // return value
    "    pop  r11",           // user RFLAGS
    "    pop  rcx",           // user RIP
    "    pop  r9",
    "    pop  r8",
    "    pop  r10",
    "    pop  rdx",
    "    pop  rsi",
    "    pop  rdi",
    // What is left of the frame is read by offset rather than popped, because
    // the two words that carry the caller's RSP and r12 sit *below* the nine
    // that were popped, and a `pop` cannot reach down.
    //
    // Two facts about the baseline have to hold at once here, and getting
    // either wrong moves every offset by a fixed amount:
    //
    //   1. `mov rbp, rsp` runs AFTER the last push, so `rbp` points at the
    //      newest word - the syscall number - and not at the top of the task's
    //      kernel stack. The frame occupies `[rbp .. rbp + 0x80]`, with `rbp`
    //      itself being its lowest-addressed word. That is what makes the
    //      `mov [rsp], rax` above land on the number, patching the return
    //      value in place.
    //   2. The pops walk *up* from `rbp`. Nine of them consume `rbp+0x00`
    //      through `rbp+0x40` - the rax slot through the rdi slot - and leave
    //      `rsp` at `rbp + 0x48`, which is where the offsets below are
    //      measured from. `rbp+0x40` is genuinely `rdi` and not the filler:
    //      the filler sits at `rbp+0x58`, above the caller's RSP, because the
    //      nine popped words have to be the six argument/system registers and
    //      the three `syscall`-stashed ones, exactly as `Regs` lists them.
    //
    // From `rbp + 0x48` the frame reads, going up: the caller's RSP at `+0x00`
    // (the word the `push r12` put there), the filler at `+0x08` that nobody
    // needs back, the caller's r12 at `+0x10`, then r15/r14/r13, and rbx and
    // rbp closing the frame at `+0x30` and `+0x38`.
    "    mov  r12, [rsp + 0x10]", // the caller's r12, pushed from gs:[8]
    "    mov  r15, [rsp + 0x18]",
    "    mov  r14, [rsp + 0x20]",
    "    mov  r13, [rsp + 0x28]",
    // r13-r15 are pushed so a frame reader can see them, and reading them back
    // is what makes `rt_sigreturn` work: it rewrites them in the frame and
    // expects the rewrite to take effect - a handler that clobbers r13 would
    // otherwise return to code that had lost it.
    "    mov  rbx, [rsp + 0x30]", // the caller's rbx
    "    mov  rbp, [rsp + 0x38]", // the caller's rbp
    // The caller's RSP is read last, out of the frame's deepest word, because
    // that load is the one that retires the frame.
    "    mov  rsp, [rsp]",        // back to the caller's RSP
    "    sysretq",
    rust_entry = sym rust_syscall,
);
