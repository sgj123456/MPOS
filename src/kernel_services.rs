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
use crate::{boot, console, drivers, mm, net, sched, syscall, vfs};

/// The kernel this boot is running.
struct Services;

impl Kernel for Services {
    fn started(&self) -> bool {
        sched::started()
    }

    fn current(&self) -> usize {
        sched::current().0
    }

    fn current_comm(&self) -> [u8; 16] {
        sched::current_comm_bytes()
    }

    fn log_line(&self, args: core::fmt::Arguments<'_>) {
        // A `Vec` because the ring takes slices: the line is formatted once,
        // then filed. This runs on the fault path, where the allocator is the
        // one thing that can still answer - a fault handler that could not
        // allocate would have to print nothing at all.
        use core::fmt::Write as _;
        let mut line = alloc::string::String::new();
        let _ = line.write_fmt(args);
        console::log_bytes(line.as_bytes());
    }

    fn report_user_fault(&self, addr: usize, ip: usize, sp: usize, error: usize) {
        // Linux's `show_signal_msg`, in its measured shape
        // (`docs/notes/` §6.73): the program first, the thread in brackets,
        // then `at`/`ip`/`sp` in hex and `error` as the machine's fault word.
        //
        // The `in <module>[<offset>,<base>+<len>]` clause is Linux's own way of
        // saying *which* file the faulting instruction came from, and it names
        // the module holding the *ip* - not the one holding the faulting
        // address, which is usually a different mapping and, when the address
        // is a wild pointer, no mapping at all. A fault whose ip sits in an
        // anonymous executable page gets no clause on Linux either, so an
        // unmapped ip is the same absence rather than a second format.
        let (comm, tid) = fault_identity();
        let line = match sched::mapping_span_at(ip) {
            Some((name, start, end)) => alloc::format!(
                "{comm}[{tid}]: segfault at {addr:x} ip {ip:016x} sp {sp:016x} error {error:x} \
                 in {name}[{:x},{start:x}+{:x}]",
                ip.wrapping_sub(start),
                end.saturating_sub(start)
            ),
            None => alloc::format!(
                "{comm}[{tid}]: segfault at {addr:x} ip {ip:016x} sp {sp:016x} error {error:x}"
            ),
        };
        // The newline is part of the *ring* line, not decoration: the ring
        // is a byte stream, and a line filed without it is glued to whatever
        // the next writer files - which is how a `traps:` line came to carry
        // a `segfault` line\'s address (`docs/notes/` §6.73).
        self.log_line(format_args!("{line}\n"));
        console::print_args(format_args!("{line}\n"));
    }

    fn report_user_trap(&self, ip: usize, sp: usize, error: usize) {
        // The `traps:` shape, which carries no `at <address>` because the
        // machine did not name one: a ring-3 `hlt` or a `#GP` is a trap the
        // process took, not an address it touched. Printing a borrowed address
        // beside it would report a page that had nothing to do with the fault.
        let (comm, tid) = fault_identity();
        let line = alloc::format!(
            "traps: {comm}[{tid}] general protection fault ip:{ip:x} sp:{sp:x} error:{error:x}"
        );
        // The newline is part of the *ring* line, not decoration: the ring
        // is a byte stream, and a line filed without it is glued to whatever
        // the next writer files - which is how a `traps:` line came to carry
        // a `segfault` line\'s address (`docs/notes/` §6.73).
        self.log_line(format_args!("{line}\n"));
        console::print_args(format_args!("{line}\n"));
    }

    fn current_stack_range(&self) -> Option<(usize, usize)> {
        sched::current_stack_range()
    }

    fn resolve_cow_fault(&self, virt: usize) -> bool {
        sched::resolve_cow_fault(virt)
    }

    fn vma_prot_at(&self, virt: usize) -> Option<mpos_vm::MapFlags> {
        sched::vma_prot_at(virt)
    }

    fn vma_kind_at(&self, virt: usize) -> Option<&'static str> {
        sched::vma_kind_at(virt)
    }

    fn mapping_name_at(&self, addr: usize) -> Option<alloc::string::String> {
        sched::mapping_name_at(addr)
    }

    fn resolve_anon_fault(&self, virt: usize, write: bool) -> crate::hal::kernel::FaultAnswer {
        sched::resolve_anon_fault(virt, write)
    }

    fn resolve_stack_growth(&self, addr: usize) -> bool {
        sched::resolve_stack_growth(addr)
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

    fn die_of_fatal_signal(&self, sig: u8) -> ! {
        sched::die_of_fatal_signal(sig)
    }

    fn out_of_memory(&self) -> ! {
        sched::out_of_memory()
    }

    fn preempt(&self) {
        sched::preempt()
    }

    fn preempt_at(&self, ip: usize) {
        sched::preempt_at(ip)
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

    fn service_blk(&self) {
        // The machine's own completion path: an MSI controller's single mailbox
        // (the first machine's PCI device), or the per-device line table the
        // MMIO transport registers against (the second machine's windows).
        #[cfg(mpos_msi)]
        drivers::virtio::blk_irq::complete_irq();
        #[cfg(not(mpos_msi))]
        drivers::virtio::mmio::complete_irq();
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

/// The program name and the thread id a fault report puts in front of its line.
///
/// The pair is Linux's `comm[tid]`, and both halves are already the kernel's own
/// answers: `current_comm_bytes` is what `PR_GET_NAME` and `/proc/<pid>/comm`
/// serve, and `current_pid_number` is the thread id `gettid(2)` answers - the
/// number of the *thread* that took the fault, not of its group, because a
/// thread is what dies.
///
/// The name is truncated at the first NUL (`comm` is 16 bytes, NUL-padded) and
/// anything that is not printable becomes `?`: a `comm` may hold any bytes a
/// program wrote with `PR_SET_NAME`, and a report that one of them could steer
/// - a newline, an escape sequence - would corrupt the very line an operator is
/// reading.
fn fault_identity() -> (alloc::string::String, u32) {
    let raw = sched::current_comm_bytes();
    let mut name = alloc::string::String::new();
    for &b in raw.iter() {
        if b == 0 {
            break;
        }
        if (0x20..0x7f).contains(&b) {
            name.push(b as char);
        } else {
            name.push('?');
        }
    }
    if name.is_empty() {
        name.push('?');
    }
    (name, sched::current_pid_number())
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
