//! The `int 0x80` gate: how a task's registers become a syscall dispatch and
//! the result becomes a value in `rax`.
//!
//! A raw asm stub rather than the `x86-interrupt` ABI, for one reason: that ABI
//! gives the handler no access to the interrupted task's registers, and the
//! whole point of this gate is that `rax` and friends *are* the call. The stub
//! builds the register block the dispatch wants, calls Rust, stores the result
//! back over the saved `rax`, and `iretq` restores the rest itself.
//!
//! The gate runs with interrupts off until the handler yields or sleeps, which
//! is the same contract the clock handler keeps: a task that switches away
//! inside the gate resumes with interrupts off and re-enables them by unwinding
//! through the critical section that suspended it, or at `iretq`.

use core::arch::global_asm;

use x86_64::VirtAddr;
use x86_64::structures::idt::InterruptDescriptorTable;

use crate::syscall;

/// Where the gate lives in the IDT. 0x80 is free: the PIC owns 0x20..0x30.
pub const VECTOR: u8 = 0x80;

/// The register block the stub pushes. Field order is push order: the last
/// push sits at the lowest address, so `rax` - pushed last, so the return
/// value can be stored back with one `mov [rsp]` - comes first.
#[repr(C)]
struct Regs {
    rax: usize,
    rcx: usize,
    rdx: usize,
    rdi: usize,
    rsi: usize,
    r8: usize,
    r9: usize,
    r10: usize,
    r11: usize,
    rbp: usize,
}

unsafe extern "C" {
    fn mpos_syscall_entry();
}

pub fn register(table: &mut InterruptDescriptorTable) {
    unsafe {
        table[VECTOR]
            .set_handler_addr(VirtAddr::new(
                mpos_syscall_entry as *const () as usize as u64,
            ))
            .set_privilege_level(x86_64::PrivilegeLevel::Ring3);
    }
}

extern "C" fn mpos_rust_syscall(regs: *mut Regs) -> usize {
    let regs = unsafe { &mut *regs };
    let args = [regs.rdi, regs.rsi, regs.rdx, regs.r10, regs.r8, regs.r9];
    // The stub leaves `regs` at the first pushed register, and the interrupt
    // frame the `iretq` will consume sits directly above the ten pushed words.
    // Its second word is the interrupted code's CS - same offset whether the
    // caller came from ring 0 or ring 3 - and its low bits decide how much a
    // pointer argument can be trusted.
    let cs = unsafe { (core::ptr::from_mut(regs) as *const u64).add(11).read() } as usize;
    let ret = syscall::dispatch(regs.rax as u64, args, cs & 3 == 3) as usize;
    // TEMPORARY: catch the frame the iretq will consume being corrupted.
    // Only a frame going back to ring 3 is checked in full: the pair that
    // `iretq` will load has to be the user code and data selectors, or the
    // return faults. A frame going back to ring 0 only has to carry the
    // kernel CS - SS is not consulted in 64-bit mode, and the CPU may well
    // have pushed whatever the last ring-3 return left in it.
    let base = core::ptr::from_mut(regs) as *const u64;
    let (rip, cs2, rfl, rsp, ss) = unsafe {
        (
            base.add(10).read(),
            base.add(11).read(),
            base.add(12).read(),
            base.add(13).read(),
            base.add(14).read(),
        )
    };
    let (user_code, user_data) = crate::arch::gdt::user_selectors();
    let (kern_code, _) = crate::arch::gdt::kernel_selectors();
    let going_to_user = cs2 & 3 == 3;
    let intact = if going_to_user {
        cs2 == user_code.0 as u64 && ss == user_data.0 as u64
    } else {
        cs2 == kern_code.0 as u64
    };
    if !intact {
        crate::println!(
            "FRAME-CORRUPT nr={} rip={:#x} cs={:#x} rfl={:#x} rsp={:#x} ss={:#x} task={}",
            regs.rax,
            rip,
            cs2,
            rfl,
            rsp,
            ss,
            crate::sched::current().0
        );
    }
    ret
}

global_asm!(
    ".section .text.mpos_syscall, \"ax\"",
    ".globl mpos_syscall_entry",
    "mpos_syscall_entry:",
    // Callee-saved registers stay live: Rust preserves them per the ABI, and
    // `rbp` is only borrowed as the block pointer, then restored.
    "    push rbp",
    "    push r11",
    "    push r10",
    "    push r9",
    "    push r8",
    "    push rsi",
    "    push rdi",
    "    push rdx",
    "    push rcx",
    "    push rax",
    "    mov  rbp, rsp",
    "    mov  rdi, rbp",
    "    and  rsp, -16",
    "    call {rust_entry}",
    "    mov  rsp, rbp",
    "    mov  [rsp], rax",
    "    pop  rax",
    "    pop  rcx",
    "    pop  rdx",
    "    pop  rdi",
    "    pop  rsi",
    "    pop  r8",
    "    pop  r9",
    "    pop  r10",
    "    pop  r11",
    "    pop  rbp",
    "    iretq",
    rust_entry = sym mpos_rust_syscall,
);
