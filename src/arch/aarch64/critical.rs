//! Single-core critical sections, on the `DAIF` mask.
//!
//! The primitive is the same one x86_64 uses and for the same reason: this
//! kernel is uniprocessor, so disabling interrupts **is** mutual exclusion, and
//! a spin lock held across a preemption by the same core would be a deadlock.
//! What differs is the instruction - `msr daifset, #2` masks IRQ where `cli`
//! does, and reading `DAIF` back says whether they were already masked, which
//! is the value `restore-state-bool` has to carry.
//!
//! The bit positions matter and are easy to get wrong: in the `DAIF` *system
//! register* the interrupt mask is bit 7, but in the `daifset`/`daifclr`
//! immediate a 4-bit field is used where I is bit 1, so the immediate is `#2`
//! and the read-back test is against `1 << 7`. Both are spelled out rather than
//! derived from one another.

struct ArchCriticalSection;

unsafe impl critical_section::Impl for ArchCriticalSection {
    unsafe fn acquire() -> bool {
        let daif: u64;
        // `DAIF` must be read *before* the mask is set, or a nested section
        // would record "interrupts were off" and the outermost one would never
        // unmask them.
        unsafe {
            core::arch::asm!("mrs {}, daif", out(reg) daif, options(nomem, nostack));
            core::arch::asm!("msr daifset, #2", options(nomem, nostack));
        }
        // I (bit 7) clear on entry means IRQ were enabled.
        daif & (1 << 7) == 0
    }

    unsafe fn release(restore_state: bool) {
        if restore_state {
            unsafe {
                core::arch::asm!("msr daifclr, #2", options(nomem, nostack));
            }
        }
    }
}

critical_section::set_impl!(ArchCriticalSection);
