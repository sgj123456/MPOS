//! Single-core critical sections.
//!
//! This kernel is uniprocessor, so interrupts are the only source of
//! re-entrancy; disabling them is therefore a complete mutex. `spin::Mutex` is
//! deliberately never used in this tree - busy-waiting on a lock held by a task
//! this same core just preempted is a deadlock.
//!
//! `RawRestoreState` is `bool` (see the `restore-state-bool` feature): whether
//! interrupts were enabled on entry, so only the outermost section re-enables
//! them on exit.

struct ArchCriticalSection;

unsafe impl critical_section::Impl for ArchCriticalSection {
    unsafe fn acquire() -> bool {
        let rflags: u64;
        // RFLAGS must be read before `cli`, otherwise a nested section would
        // record "interrupts were off" and the outer one would never re-enable.
        unsafe {
            core::arch::asm!("pushfq", "pop {0}", "cli", out(reg) rflags);
        }
        rflags & (1 << 9) != 0
    }

    unsafe fn release(restore_state: bool) {
        if restore_state {
            unsafe {
                core::arch::asm!("sti");
            }
        }
    }
}

critical_section::set_impl!(ArchCriticalSection);
