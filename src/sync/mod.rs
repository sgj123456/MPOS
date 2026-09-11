//! Synchronization primitives.
//!
//! This kernel is uniprocessor. Disabling interrupts **is** the mutual
//! exclusion mechanism — there is no second core to contend with. The
//! universal pattern is `critical_section::Mutex<RefCell<T>>`: the mutex
//! disables interrupts on acquire and restores them on release; the
//! `RefCell` provides interior mutability without `unsafe`.
//!
//! `spin::Mutex` is deliberately never used: busy-waiting on a lock held
//! by a task this same core just preempted would deadlock.
//!
//! `spin::Once` is used in exactly two places (`arch::x86_64::gdt` and
//! `arch::x86_64::idt`) for one-time hardware initialization that must
//! happen exactly once.
//!
//! All lock acquisitions go through `critical_section::with`, which is
//! provided by `arch::x86_64::critical` on the bare-metal target and by
//! the standard library on the host test target.
//!
//! # TODO: higher-level synchronization
//!
//! When the kernel grows beyond a single core, consider adding:
//! - `SpinLock<T>` — interrupt-disabled spin (same semantics, explicit API)
//! - `Mutex<T>` — sleeping mutex (for preemption-safe critical sections)
//! - `RwLock<T>` — reader-writer lock (for read-heavy VFS paths)
//!
//! For now, no project-specific lock wrappers are needed.
