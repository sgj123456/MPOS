//! The one implementation of the machine layer's hook table.
//!
//! `hal::kernel` is the seam's other direction. `arch/**` has to ask the kernel
//! whether a faulting page is one to populate, where a `svc` goes, what a line of
//! console input means, and it asks through a trait *this* crate implements - so
//! the machine layer never names `sched`, `syscall` or `mm` (`NOTES.md` §14.2).
//!
//! The table is **one** table. It used to be written twice, once in each
//! machine's boot wiring (`src/board/<arch>.rs`, then `src/main/<arch>.rs`), and
//! that duplication is what made it look
//! machine-shaped: the two impls were the same twenty-four methods with the same
//! bodies, and the only difference between them - `device_not_available` - is not
//! an architecture but a *capability*. `mpos_fpu_trap` is the capability that
//! gates the `#NM` trap, and the trait already gates the method on it, so the
//! machine that faults on the first floating-point instruction fills it in and
//! the machine that saves floating point eagerly never sees it - from one impl.
//! A machine that needs a different answer here is a machine with a different
//! *kernel*, which is the thing this seam exists to say is impossible.
//!
//! Registered by the kernel's entry (`src/main.rs::_start`) before anything that
//! can take a trap is armed on either machine: before the IDT is loaded on
//! x86_64, before the vector table on aarch64.

use crate::hal::kernel::{self, Kernel, MemoryStartup};
use crate::{boot, console, mm, net, sched, syscall, vfs};

/// The kernel this boot is running.
struct Services;

impl Kernel for Services {
    fn started(&self) -> bool {
        sched::started()
    }

    fn current(&self) -> usize {
        sched::current().0
    }

    fn current_stack_range(&self) -> Option<(usize, usize)> {
        sched::current_stack_range()
    }

    fn resolve_cow_fault(&self, virt: usize) -> bool {
        sched::resolve_cow_fault(virt)
    }

    fn resolve_anon_fault(&self, virt: usize, write: bool) -> bool {
        sched::resolve_anon_fault(virt, write)
    }

    fn resolve_stack_growth(&self, addr: usize, sp: usize) -> bool {
        sched::resolve_stack_growth(addr, sp)
    }

    fn kernel_lazy_accessible(&self, virt: usize, write: bool) -> bool {
        sched::kernel_lazy_accessible(virt, write)
    }

    fn send_signal(&self, sig: u8) -> bool {
        sched::send_signal(sched::current(), sig)
    }

    fn disposition_is_default(&self, sig: u8) -> bool {
        sched::current_disposition(sig).is_default()
    }

    fn exit(&self, code: isize) -> ! {
        sched::exit(code)
    }

    fn preempt(&self) {
        sched::preempt()
    }

    fn charge_cpu(&self, in_user: bool) {
        sched::charge_cpu(in_user)
    }

    /// The console service, which is where a `println!` on this kernel already
    /// lands - so a machine's fault report reads the same as any other line, and
    /// reaches whichever console the boot has pointed at.
    fn print_line(&self, args: core::fmt::Arguments<'_>) {
        console::print_args(args)
    }

    fn force_serial(&self) {
        console::force_serial()
    }

    fn route_console_input(&self, byte: u8) {
        vfs::vcmux::route_input(byte)
    }

    fn service_network(&self) {
        net::irq_service()
    }

    fn set_cmdline(&self, cmdline: &'static str) {
        boot::set_cmdline(cmdline)
    }

    unsafe fn boot_info(&self, handoff: *const mpos_bootinfo::Handoff) -> mpos_bootinfo::BootInfo {
        // SAFETY: the caller's contract, passed through unchanged.
        unsafe { boot::handoff::collect(handoff) }
    }

    unsafe fn start_memory(&self, info: &mpos_bootinfo::BootInfo) -> MemoryStartup {
        // SAFETY: the caller's contract, passed through unchanged.
        let stats = unsafe { mm::init(info) };
        MemoryStartup {
            zones: stats.zones,
            total: stats.total,
        }
    }

    fn alloc_frames(&self, order: usize) -> Option<usize> {
        mm::alloc_frames(order)
    }

    fn free_frames(&self, phys: usize, order: usize) {
        mm::free_frames(phys, order)
    }

    fn frames_free(&self) -> usize {
        mm::frame_stats().free
    }

    fn wake(&self, task: usize) {
        // The scheduler answers whether the task was actually parked; the hook
        // table's contract is "make it runnable", and the one caller - the
        // console's transmit ring - has nothing to do with the answer. Dropped
        // here rather than widened into the trait, which would be a scheduler
        // fact leaking through a seam that exists to hide them.
        let _ = sched::wake(sched::TaskId(task));
    }

    /// The `#NM` trap, on a machine that has one: the FPU is left trapped and
    /// faults on the first floating-point instruction, so the handler has to be
    /// able to hand it over. Gated on `mpos_fpu_trap` like the hook itself - the
    /// machine that has no such trap declares no such capability, so the method
    /// does not exist for it rather than being a stub it must fake
    /// (`NOTES.md` §14.2).
    #[cfg(mpos_fpu_trap)]
    fn device_not_available(&self) {
        sched::device_not_available()
    }

    unsafe fn dispatch_syscall(
        &self,
        nr: u64,
        args: [usize; 6],
        from_user: bool,
        frame: *mut (),
    ) -> isize {
        // SAFETY: the caller's contract, passed through unchanged: `frame` is
        // null or the gate's own frame for the calling task.
        unsafe { syscall::linux_dispatch(nr, args, from_user, frame) }
    }
}

/// The one value [`register`] hands over.
///
/// A `static` rather than a temporary because the hook table stores a
/// `&'static dyn Kernel`: the table outlives the entry function that registers
/// it, and the value it points at has to live exactly as long as the kernel.
static SERVICES: Services = Services;

/// Register the hook table, once, from the machine's entry.
pub fn register() {
    kernel::register(&SERVICES);
}
