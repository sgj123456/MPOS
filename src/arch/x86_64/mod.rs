//! x86_64 machine-dependent code.
//!
//! Everything in here may touch x86 specifics; nothing outside `arch::` may.

pub mod context;
pub mod critical;
pub mod gdt;
pub mod idt;
pub mod paging;
pub mod pci;
pub mod pic;
pub mod qemu;
pub mod serial;
pub mod syscall;
pub mod timer;

use crate::mm::vm::KernelSections;

/// Where the linker script put each part of the kernel image, as virtual
/// addresses. This is the x86_64 half of the seam: the plan in `mm::vm` says what
/// permissions each section gets, and only here does anything know their names.
pub fn kernel_sections() -> KernelSections {
    // These symbols are addresses, not objects - there is no `u8` behind them to
    // read, so take their address and never dereference it.
    unsafe extern "C" {
        static __text_start: u8;
        static __text_end: u8;
        static __rodata_start: u8;
        static __rodata_end: u8;
        static __data_start: u8;
        static kernel_end: u8;
    }

    KernelSections {
        text: (&raw const __text_start as usize)..(&raw const __text_end as usize),
        rodata: (&raw const __rodata_start as usize)..(&raw const __rodata_end as usize),
        // Data runs to the end of the image: .dynamic and .bss follow it with no
        // gap, and all three must be writable.
        data: (&raw const __data_start as usize)..(&raw const kernel_end as usize),
    }
}

/// Bring up the interrupt subsystem: descriptors first, then the clock, then
/// unmask. Order matters - the PIT must not fire before the IDT is loaded.
pub fn init_interrupts() {
    gdt::init();
    idt::init();
    pic::remap();
    timer::init();
}

pub fn halt() -> ! {
    loop {
        // `hlt` with interrupts disabled parks the core until reset, which is
        // what a panic path with nothing left to do should end in.
        unsafe { core::arch::asm!("cli", "hlt") };
    }
}

/// Wait for the next interrupt and return with interrupts off.
///
/// The scheduler's idle path, and the opposite of [`halt`]: this one expects to
/// be woken. `sti` and `hlt` have to be adjacent - `sti` blocks interrupts for
/// exactly one instruction, so halting straight after it cannot miss the
/// interrupt that was already pending. The `cli` is needed because that
/// interrupt's `iretq` restores the flags the `sti` set, so without it the
/// caller would get control back with interrupts on.
pub fn idle() {
    unsafe { core::arch::asm!("sti", "hlt", "cli") };
}
