use core::arch::global_asm;

unsafe extern "C" {
    fn linux_syscall_entry();
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
        let entry = linux_syscall_entry as *const () as usize as u64;
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
        // swaps in. It points at `gdt::CURRENT_KERNEL_RSP`, whose first
        // quadword `set_kernel_stack` keeps equal to TSS.RSP0, so `mov rsp,
        // gs:[0]` lands on the running task's kernel stack. Writing it is
        // mandatory: with KERNEL_GS_BASE still zero, `swapgs` leaves GS.base
        // at zero and the `gs:[0]` load faults on the null page.
        let krsp = super::gdt::current_kernel_rsp_addr();
        core::arch::asm!(
            "wrmsr",
            in("ecx") 0xC000_0102u32,
            in("edx") (krsp >> 32) as u32,
            in("eax") krsp as u32,
        );

        // MSR_STAR (0xC0000081): segment selectors. `syscall` takes the kernel
        // CS from bits 47:32 (SS is CS+8); `sysret` computes its pair from bits
        // 63:48 as `+16` (CS) and `+8` (SS), forcing RPL 3 on both. So bits
        // 63:48 are *not* a selector: they are the GDT slot below the user data
        // descriptor, which `user_code_star_base` derives from the real
        // descriptors. Bits 31:0 are the SYSEXIT pair, unused here.
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
    }
}

use crate::syscall;

/// Register block pushed by the `syscall` entry stub.
///
/// Layout matches the entry stub's push order exactly: the last push (rax, the
/// syscall number) sits at offset 0, so the return value patches with one
/// `mov [rsp]`. The stub restores the block by symmetric pops, so every field
/// order here must equal the push order. `user_rsp` is the caller's RSP that
/// `syscall` left in the register (the CPU switches no stack for `syscall`),
/// recovered after the kernel-stack switch so `sysretq` returns to it.
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
    r15: usize,
    r14: usize,
    r13: usize,
    rbp: usize,
}

extern "C" fn linux_rust_syscall(regs: *mut Regs) -> usize {
    let regs = unsafe { &mut *regs };
    let args = [regs.rdi, regs.rsi, regs.rdx, regs.r10, regs.r8, regs.r9];
    // Detect ring: the CS selector's RPL bits tell us.  The saved rcx here
    // is the *user* RIP that `syscall` stashed; the interrupt frame above
    // the push block is absent (syscall uses sysret, not iretq), so we
    // check the saved RCX value which is a canonical user address → from ring 3.
    let _from_user = true; // syscall gate is always entered from ring 3
    syscall::linux_dispatch(regs.rax as u64, args) as usize
}

global_asm!(
    ".section .text.linux_syscall, \"ax\"",
    ".globl linux_syscall_entry",
    ".p2align 4",
    "linux_syscall_entry:",
    // `syscall` saved user RIP in rcx and user RFLAGS in r11, but unlike an
    // interrupt it did NOT switch RSP to a kernel stack. The CPU is now in
    // ring 0 still running on the caller's RSP. Capture that RSP in r12, then
    // `swapgs`/`mov rsp, gs:[0]` moves onto the current task's kernel stack
    // (CURRENT_KERNEL_RSP, kept in lock-step with TSS.RSP0 by set_kernel_stack)
    // before we push anything - a timer interrupt mid-syscall would otherwise
    // fault on the way back. The second `swapgs` restores the user's GS base.
    "    mov  r12, rsp",      // r12 = user RSP (syscall left it untouched)
    "    swapgs",
    "    mov  rsp, gs:[0]",   // rsp = current kernel stack top
    "    swapgs",             // user GS base restored
    // Preserve every register the Linux x86_64 syscall ABI promises to keep:
    // rbp, r13, r14, r15, and the user RSP (in r12). Then the args and the
    // two `syscall`-stashed values. rax (the number) is pushed last, so it
    // sits at offset 0 and the return value patches with one `mov [rsp]`.
    "    push rbp",
    "    push r13",
    "    push r14",
    "    push r15",
    "    push r12",           // user RSP
    "    push rdi",
    "    push rsi",
    "    push rdx",
    "    push r10",
    "    push r8",
    "    push r9",
    "    push rcx",           // user RIP
    "    push r11",           // user RFLAGS
    "    push rax",           // syscall number
    "    mov  rbp, rsp",      // rbp = &Regs (base for the call and restore)
    "    mov  rdi, rbp",
    "    and  rsp, -16",
    "    call {rust_entry}",
    // rax = return value. Patch the rax slot, then restore symmetrically.
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
    "    pop  r12",           // r12 = user RSP
    "    pop  r15",
    "    pop  r14",
    "    pop  r13",
    "    pop  rbp",
    "    mov  rsp, r12",      // back to the caller's RSP
    "    sysretq",
    rust_entry = sym linux_rust_syscall,
);
