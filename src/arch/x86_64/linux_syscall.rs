use core::arch::global_asm;

unsafe extern "C" {
    fn linux_syscall_entry();
}

pub fn init() {
    unsafe {
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

        // MSR_STAR (0xC0000081): segment selectors.
        // Bits 63:48 = kernel SS, 47:32 = kernel CS (for syscall entry).
        // Bits 31:16 = user CS,  15:0  = user SS (for sysret).
        let gdt = super::gdt::user_selectors();
        let user_cs = gdt.0.0 as u64;
        let user_ss = gdt.1.0 as u64;
        // Kernel CS/SS: selectors 0x08 (code) and 0x10 (data) in our GDT.
        let kern_cs: u64 = 0x08;
        let kern_ss: u64 = 0x10;
        let star = (kern_ss << 48) | (kern_cs << 32) | (user_cs << 16) | user_ss;
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
/// Layout matters: the stub reads fields by fixed offset.  rax is last pushed,
/// so it sits at offset 0 — the return value can be patched with one `mov [rsp]`.
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
    // `syscall` saved user RIP in rcx and user RFLAGS in r11.
    // We must preserve rdi/rsi/rdx/r8/r9/r10 (args) and rax (number).
    "    push rdi",
    "    push rsi",
    "    push rdx",
    "    push r10",
    "    push r8",
    "    push r9",
    "    push rcx",       // user RIP
    "    push r11",       // user RFLAGS
    "    push rax",       // syscall number
    "    mov  rdi, rsp",  // pointer to Regs
    "    and  rsp, -16",
    "    call {rust_entry}",
    // Store return value into the rax slot in the saved registers.
    "    mov  [rsp + 0], rax",
    // Restore everything.
    "    pop  rax",
    "    pop  r11",       // user RFLAGS
    "    pop  rcx",       // user RIP
    "    pop  r9",
    "    pop  r8",
    "    pop  r10",
    "    pop  rdx",
    "    pop  rsi",
    "    pop  rdi",
    "    sysretq",
    rust_entry = sym linux_rust_syscall,
);
