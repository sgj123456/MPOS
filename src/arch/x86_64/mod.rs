//! x86_64 machine-dependent code.
//!
//! Everything in here may touch x86 specifics; nothing outside `arch::` may.

pub(crate) mod context;
pub(crate) mod critical;
pub(crate) mod gdt;
pub(crate) mod idt;
pub(crate) mod lapic;
pub(crate) mod paging;
pub(crate) mod pic;
pub(crate) mod rtc;
pub(crate) mod syscall;
pub(crate) mod timer;

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
///
/// The RTC read happens here too, before interrupts are on: it is plain port
/// I/O with no dependency on anything but the CPU, and anchoring wall time
/// before the first tick keeps `clock_gettime` coherent from the start.
pub fn init_interrupts() {
    gdt::init();
    idt::init();
    pic::remap();
    timer::init();
    rtc::init();
    syscall::init();
}

/// Enable SSE/AVX for user processes.
///
/// The kernel itself is built with `soft-float` so it never touches XMM/YMM
/// registers, but user binaries (musl, coreutils) expect SSE to work.  We set
/// CR4.OSFXSR and CR4.OSXMMEXCPT so that SSE instructions do not #UD in ring 3.
pub fn enable_user_sse() {
    unsafe {
        let mut cr4: u64;
        core::arch::asm!("mov {}, cr4", out(reg) cr4);
        cr4 |= 1 << 9; // OSFXSR — enables SSE
        cr4 |= 1 << 10; // OSXMMEXCPT — enables unmasked SSE exceptions
        core::arch::asm!("mov cr4, {}", in(reg) cr4);
    }
}

/// Whether this core would take an interrupt right now.
///
/// The one honest test for "may this context sleep": a task with interrupts on
/// can be woken by a device, while an interrupt handler - which runs with them
/// off, however it got there - cannot, and must never try to park.
pub fn interrupts_enabled() -> bool {
    x86_64::instructions::interrupts::are_enabled()
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

/// The x86_64 machine, as the HAL sees it: this module implements the machine
/// traits for `crate::hal::Current`, the platform tag `hal` owns.
///
/// The binding lives here rather than in `hal` because its bodies name this
/// module's `timer`/`rtc`/`idt` - a new architecture adds its own impls in its
/// own module and edits no shared file (`PLAN.md` §9.1). Everything below is a
/// one-line static dispatch, so `crate::hal::ticks()` is exactly the
/// `timer::ticks()` it replaced once monomorphised.
impl crate::hal::Clock for crate::hal::Current {
    const TICK_HZ: u32 = timer::TICK_HZ;

    #[inline]
    fn ticks() -> u64 {
        timer::ticks()
    }

    #[inline]
    fn since_boot_nanos() -> u64 {
        timer::since_boot_nanos()
    }

    #[inline]
    fn busy_wait_ms(ms: u64) {
        timer::busy_wait_ms(ms)
    }
}

impl crate::hal::WallClock for crate::hal::Current {
    #[inline]
    fn boot_epoch() -> u64 {
        rtc::boot_epoch()
    }

    #[inline]
    fn set_boot_epoch(epoch: u64) {
        rtc::set_boot_epoch(epoch)
    }

    #[inline]
    fn write_epoch(epoch: u64) -> bool {
        rtc::write_epoch(epoch)
    }
}

impl crate::hal::Cpu for crate::hal::Current {
    #[inline]
    fn interrupts_enabled() -> bool {
        interrupts_enabled()
    }

    #[inline]
    fn enable_interrupts() {
        idt::enable()
    }

    #[inline]
    fn halt() -> ! {
        halt()
    }

    #[inline]
    fn idle() {
        idle()
    }

    #[inline]
    fn enable_user_simd() {
        enable_user_sse()
    }

    #[inline]
    fn init_interrupts() {
        init_interrupts()
    }
}

impl crate::hal::Interrupts for crate::hal::Current {
    #[inline]
    fn unmask(line: crate::hal::Line) {
        pic::set_masked(pic_line(line), false)
    }

    #[inline]
    fn mask(line: crate::hal::Line) {
        pic::set_masked(pic_line(line), true)
    }

    #[inline]
    fn eoi(line: crate::hal::Line) {
        pic::end_of_interrupt(pic_line(line))
    }
}

/// x86_64 has an MSI controller - the local APIC page a PCI device's MSI-X table
/// is mapped through, and the vector such an entry carries. This is the optional
/// [`crate::hal::Msi`] capability, not a baseline concern, because a machine
/// whose devices are wired to a GIC has neither.
impl crate::hal::Msi for crate::hal::Current {
    #[inline]
    fn init_message_signalled() {
        lapic::init()
    }

    #[inline]
    fn net_msi_vector() -> u8 {
        idt::NET_IRQ_VECTOR
    }
}

/// The 8259 input a named line is wired to on this machine.
fn pic_line(line: crate::hal::Line) -> u8 {
    match line {
        crate::hal::Line::Timer => 0,
        crate::hal::Line::Serial => 4,
    }
}

impl crate::hal::Paging for crate::hal::Current {
    type Space = paging::AddressSpace;

    unsafe fn install(
        info: &crate::boot::BootInfo,
        sections: &crate::mm::vm::KernelSections,
    ) -> usize {
        // SAFETY: the caller's contract, passed through unchanged.
        unsafe { paging::install(info, sections) }
    }

    #[inline]
    fn map_mmio(virt: usize, phys: usize, len: usize) {
        paging::map_mmio(virt, phys, len)
    }

    #[inline]
    fn translate(virt: usize) -> Option<(usize, crate::mm::MapFlags)> {
        paging::translate(virt)
    }

    #[inline]
    fn user_accessible(start: usize, len: usize, write: bool) -> bool {
        paging::user_accessible(start, len, write)
    }

    #[inline]
    fn is_cow_page(virt: usize) -> bool {
        paging::is_cow_page(virt)
    }

    #[inline]
    fn write_user_u32(root: usize, at: usize, value: u32) -> bool {
        paging::write_user_u32(root, at, value)
    }

    #[inline]
    fn active_root() -> usize {
        paging::active_root()
    }

    #[inline]
    fn kernel_root() -> usize {
        paging::kernel_root()
    }

    #[inline]
    fn switch_to_root(root: usize) {
        paging::switch_to_root(root)
    }

    #[inline]
    fn switch_to_kernel() {
        paging::switch_to_kernel()
    }

    #[inline]
    fn phys_for_virt_of(root: usize, virt: usize) -> Option<usize> {
        paging::phys_for_virt_of(root, virt)
    }

    #[inline]
    fn user_phys_for_virt_of(root: usize, virt: usize) -> Option<usize> {
        paging::user_phys_for_virt_of(root, virt)
    }
}

impl crate::hal::Contexts for crate::hal::Current {
    type Context = context::Context;
    type Fpu = context::FpuState;
    type UserFrame = context::UserContext;

    const FPU_BYTES: usize = context::FPU_BYTES;

    #[inline]
    fn set_kernel_stack(top: usize) {
        gdt::set_kernel_stack(top)
    }

    #[inline]
    unsafe fn bootstrap(
        stack_top: usize,
        entry: extern "C" fn(usize) -> !,
        slot: usize,
        interruptible: bool,
    ) -> Self::Context {
        // SAFETY: the caller's contract, passed through unchanged.
        unsafe { context::bootstrap(stack_top, entry, slot, interruptible) }
    }

    #[inline]
    unsafe fn switch(from: *mut Self::Context, to: *const Self::Context) {
        // SAFETY: as above.
        unsafe { context::switch(from, to) }
    }

    #[inline]
    unsafe fn resume_user(frame: *const Self::UserFrame) -> ! {
        // SAFETY: as above.
        unsafe { context::resume_user(frame) }
    }

    #[inline]
    unsafe fn enter_user(entry: usize, user_rsp: usize) -> ! {
        // SAFETY: as above.
        unsafe { context::enter_user(entry, user_rsp) }
    }
}

/// x86_64's lazy-FPU ownership: `CR0.TS` traps the first FP instruction and the
/// `#NM` handler saves the previous task there, so the scheduler queries whether
/// the registers are live and hands ownership across a switch. The optional
/// [`crate::hal::FpuTrap`] capability - a machine that saves FP eagerly inside
/// the switch has no trap to query.
impl crate::hal::FpuTrap for crate::hal::Current {
    #[inline]
    fn fpu_trapped() -> bool {
        context::fpu_trapped()
    }

    #[inline]
    unsafe fn save_live_fpu(state: &mut Self::Fpu) {
        // SAFETY: as above.
        unsafe { context::save_live_fpu(state) }
    }

    #[inline]
    unsafe fn restore_fpu(state: &Self::Fpu) {
        // SAFETY: as above.
        unsafe { context::restore_fpu(state) }
    }

    #[inline]
    unsafe fn claim_live_fpu() {
        // SAFETY: as above.
        unsafe { context::claim_live_fpu() }
    }

    #[inline]
    unsafe fn reset_live_fpu() {
        // SAFETY: as above.
        unsafe { context::reset_live_fpu() }
    }
}

/// The `cs`/`ss` a ring-3 frame carries, read off the GDT - the optional
/// [`crate::hal::Segments`] capability, which exists only on a machine that has
/// segment selectors at all.
impl crate::hal::Segments for crate::hal::Current {
    fn user_segments() -> (usize, usize) {
        let (code, data) = gdt::user_selectors();
        (code.0 as usize, data.0 as usize)
    }
}

/// The console sink: the 16550 COM1 wire, reached through `drivers::serial`.
/// The polled `write_bytes` and the `interrupt_driven` question live here; the
/// queue-and-park ring is the [`crate::hal::SerialRing`] capability below.
impl crate::hal::Console for crate::hal::Current {
    #[inline]
    fn init() {
        crate::drivers::serial::init()
    }

    #[inline]
    fn write_bytes(bytes: &[u8]) {
        crate::drivers::serial::write_bytes(bytes)
    }

    #[inline]
    fn interrupt_driven() -> bool {
        crate::drivers::serial::interrupt_driven()
    }
}

/// x86_64's 16550 has an interrupting transmitter and a ring the console parks
/// on, so it implements the optional [`crate::hal::SerialRing`] over
/// `drivers::serial`'s ring. A polled machine (aarch64 at stage 4a) is not a
/// `SerialRing`; `crate::console` then never queues.
impl crate::hal::SerialRing for crate::hal::Current {
    #[inline]
    fn tx_space() -> usize {
        crate::drivers::serial::tx_space()
    }

    #[inline]
    fn tx_submit(bytes: &[u8]) -> usize {
        crate::drivers::serial::tx_submit(bytes)
    }

    #[inline]
    fn tx_wait_now() {
        crate::drivers::serial::tx_wait_now()
    }

    #[inline]
    fn tx_stop_waiting() {
        crate::drivers::serial::tx_stop_waiting()
    }
}

impl crate::hal::Platform for crate::hal::Current {}
