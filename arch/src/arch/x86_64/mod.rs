//! x86_64 machine-dependent code.
//!
//! Everything in here may touch x86 specifics; nothing outside `arch::` may.

pub(crate) mod context;
pub(crate) mod critical;
pub(crate) mod gdt;
pub(crate) mod idt;
pub(crate) mod lapic;
pub(crate) mod mmio;
pub(crate) mod paging;
pub(crate) mod pic;
pub(crate) mod qemu;
pub(crate) mod rtc;
pub(crate) mod serial;
pub(crate) mod signal;
pub(crate) mod syscall;
pub(crate) mod timer;

use mpos_bootinfo::BootInfo;
use mpos_bootinfo::Handoff;
use mpos_vm::KernelSections;
use mpos_vm::PAGE_SIZE;

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

/// Put a formatted line on this machine's wire, without the console service.
///
/// The x86_64 twin of `aarch64::print_args`, and deliberately byte-for-byte what
/// the kernel console would have written on this machine: COM1, polled, with no
/// `\n`-to-`\r\n` expansion, because this machine's console path does not do one
/// either (the second machine's PL011 does, which is the whole of the
/// difference). Both its callers are paths the console service must not be
/// trusted on - the machine's own `boot`, which prints before `crate::console`
/// exists, and the panic path, where its state may already be corrupt.
///
/// [`serial::init`] must have run. The machine's `boot` reaches it through the
/// `milestone` callback the wiring layer hands down, so the sink is live by the
/// time anything prints here.
pub fn print_args(args: core::fmt::Arguments<'_>) {
    use core::fmt::Write;

    /// A sink over the raw port. The port's registers are the state, so there is
    /// nothing for an instance to carry.
    struct Wire;

    impl Write for Wire {
        fn write_str(&mut self, text: &str) -> core::fmt::Result {
            serial::write_bytes(text.as_bytes());
            Ok(())
        }
    }

    let _ = Wire.write_fmt(args);
}

/// Run the machine from the record `mpos-boot` left in `RDI`. Never returns.
///
/// The x86_64 twin of `aarch64::boot`, and the same three callbacks for the same
/// reason: bringing the subsystems up, probing the machine's devices and loading
/// the first program all sit *above* this crate and cannot be named from it
/// (`AGENTS.md` §4, downward-only), so the wiring layer hands pointers down and
/// this function only ever invokes them.
///
/// The order is the dependencies and nothing else, and it is the second
/// machine's order: physical memory before the page tables, because the tables
/// are made of frames; interrupts configured before the tables are installed but
/// enabled too, so the build cannot be interrupted halfway. The console service
/// comes up once the record has been read, because the command line in that
/// record is what decides where the kernel log goes - and a record that fails its
/// magic check is still reported, because the panic path writes through
/// [`print_args`] rather than through the service.
///
/// # Safety
///
/// The caller is the loader's jump to `_start`, with `RDI` holding the record it
/// wrote and every address that record names mapped for as long as the kernel
/// runs. Nothing chooses this call from Rust, and the pointer is read before it
/// can be validated - [`crate::hal::kernel::boot_info`] is what checks the magic,
/// *after* the read - so the validity of `handoff` is the caller's contract and
/// not something this function can establish.
pub unsafe fn boot(
    handoff: *const Handoff,
    milestone: fn(&BootInfo),
    devices: fn(&BootInfo),
    init: fn(&BootInfo) -> !,
) -> ! {
    // SSE/AVX for user processes, before anything else touches CR0/CR4.
    enable_user_sse();

    // SAFETY: the loader's contract - `handoff` points at the record it wrote,
    // still mapped at the address it put it, and every physical range that record
    // names is reachable through the direct map it chose.
    let info = unsafe { crate::hal::kernel::boot_info(handoff) };
    crate::hal::kernel::set_cmdline(info.cmdline);

    // The console service, which lives above this crate: the wiring layer hands
    // the pointer down. After the record, because the command line that decides
    // *where* the kernel log goes is in it; before physical memory, because the
    // handful of lines between here and there are reported through the machine's
    // own writer and must not be the only thing on the wire.
    milestone(&info);

    // SAFETY: the same contract, restated where the allocator takes it up.
    let stats = unsafe { crate::hal::kernel::start_memory(&info) };
    print_args(format_args!(
        "pmm: {} zones, {} frames ({} MiB)\n",
        stats.zones,
        stats.total,
        stats.total * PAGE_SIZE / (1024 * 1024),
    ));

    init_interrupts();
    crate::hal::enable_interrupts();

    let sections = kernel_sections();
    // SAFETY: the frame allocator is up, and everything the kernel is executing
    // from, reading and writing at this moment - the image's own sections and the
    // direct map over the record's regions - is in the tables this builds.
    let frames = unsafe { crate::hal::install(&info, &sections) };
    print_args(format_args!(
        "image: text {} KiB, rodata {} KiB, data {} KiB\n",
        (sections.text.end - sections.text.start) / 1024,
        (sections.rodata.end - sections.rodata.start) / 1024,
        (sections.data.end - sections.data.start) / 1024,
    ));
    print_args(format_args!(
        "page tables: {} frames ({} KiB)\n",
        frames,
        frames * PAGE_SIZE / 1024
    ));

    // The machine's own devices, now that the kernel's tables reach them. On this
    // machine that is the PCI bus, which the wiring layer enumerates; a machine
    // whose firmware describes its devices instead walks its tree in this slot.
    devices(&info);

    // And the kernel's own boot, which never returns.
    init(&info)
}

/// Hand the console over to its own interrupts.
///
/// The x86_64 twin of `aarch64::enable_console_interrupts`, named the same way
/// for the same reason: this is the point at which the console stops waiting for
/// the wire and starts queueing into a transmitter that interrupts. Two things
/// have to be true together and neither machine's wiring should have to know
/// which - the receiver has to hand its bytes to the mux (armed by the wiring)
/// before the first keystroke arrives, and the transmitter's interrupt has to be
/// what drains the queue (`serial`'s IRQ4 is shared, and `tx_ring::enable` is
/// what lets the console take the queueing path at all).
///
/// Called once, from the wiring layer, after the mux is up.
pub fn enable_console_interrupts() {
    serial::enable_rx();
    serial::enable_tx();
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
/// own module and edits no shared file (`NOTES.md` §9.1). Everything below is a
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

    #[inline]
    fn set_thread_pointer(base: usize) {
        // `IA32_FS_BASE` (0xC000_0100): the base `%fs`-relative accesses resolve
        // against. The two halves go in separately because `wrmsr` reads them from
        // `eax`/`edx`; this is the same register the scheduler reinstalled on every
        // dispatch, moved out of `sched` because the register name is the machine's.
        unsafe {
            core::arch::asm!(
                "wrmsr",
                in("ecx") 0xC000_0100u32,
                in("eax") base as u32,
                in("edx") (base >> 32) as u32,
                options(nostack, preserves_flags),
            );
        }
    }

    #[inline]
    fn thread_pointer() -> usize {
        let lo: u32;
        let hi: u32;
        unsafe {
            core::arch::asm!(
                "rdmsr",
                in("ecx") 0xC000_0100u32,
                out("eax") lo,
                out("edx") hi,
                options(nostack, preserves_flags),
            );
        }
        ((hi as u64) << 32 | lo as u64) as usize
    }

    #[inline]
    fn stack_pointer() -> usize {
        let value: usize;
        // SAFETY: reading `rsp` has no side effects; `nomem` is what lets this
        // be called from a dying kernel that can no longer trust memory.
        unsafe { core::arch::asm!("mov {}, rsp", out(reg) value, options(nomem, nostack)) };
        value
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

    unsafe fn install(info: &mpos_bootinfo::BootInfo, sections: &mpos_vm::KernelSections) -> usize {
        // SAFETY: the caller's contract, passed through unchanged.
        unsafe { paging::install(info, sections) }
    }

    #[inline]
    fn map_mmio(virt: usize, phys: usize, len: usize) {
        paging::map_mmio(virt, phys, len)
    }

    #[inline]
    fn map_mmio_window(phys: usize, len: usize) -> Option<usize> {
        // A PCIe BAR has no fixed address, so this machine bump-allocates one
        // kernel-half mapping per window (`self::mmio`); `None` means the
        // window is full, which is the honest refusal rather than a bogus
        // address.
        mmio::window(phys, len)
    }

    #[inline]
    fn translate(virt: usize) -> Option<(usize, mpos_vm::MapFlags)> {
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
    const ABI_REG_COUNT: usize = context::ABI_REG_COUNT;

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

/// The console sink: the 16550 COM1 wire, reached through `drivers::serial`.
/// The polled `write_bytes` and the `interrupt_driven` question live here; the
/// queue-and-park ring is the [`crate::hal::SerialRing`] capability below.
impl crate::hal::Console for crate::hal::Current {
    #[inline]
    fn init() {
        crate::arch::x86_64::serial::init()
    }

    #[inline]
    fn write_bytes(bytes: &[u8]) {
        crate::arch::x86_64::serial::write_bytes(bytes)
    }

    #[inline]
    fn interrupt_driven() -> bool {
        crate::arch::x86_64::serial::interrupt_driven()
    }
}

/// x86_64's 16550 has an interrupting transmitter and a ring the console parks
/// on, so it implements the optional [`crate::hal::SerialRing`] over
/// `drivers::serial`'s ring: three operations on one I/O port. The queue itself
/// is shared with the second machine (`crate::hal::tx_ring`), whose PL011
/// answers the same three questions through different registers; see that
/// module for why the decisions live together rather than per machine.
impl crate::hal::SerialRing for crate::hal::Current {
    #[inline]
    fn tx_space() -> usize {
        crate::arch::x86_64::serial::tx_space()
    }

    #[inline]
    fn tx_submit(bytes: &[u8]) -> usize {
        crate::arch::x86_64::serial::tx_submit(bytes)
    }

    #[inline]
    fn tx_wait_now() {
        crate::arch::x86_64::serial::tx_wait_now()
    }

    #[inline]
    fn tx_stop_waiting() {
        crate::arch::x86_64::serial::tx_stop_waiting()
    }
}

/// The machine's entropy source: `RDRAND`, when the CPU answers it.
///
/// A stretch the instruction refuses is left as it was rather than filled with a
/// guess, and a CPU without it folds in nothing - the clock mix in `crate::rand`
/// is the floor, not a claim of entropy. The rng *device* is deliberately not
/// here: `virtio-rng` is a driver the kernel enumerates, not an answer this
/// machine gives, and `crate::rand` folds it in itself (`NOTES.md` §14.3). That
/// split is also why this trait can stay a machine trait.
impl crate::hal::Entropy for crate::hal::Current {
    fn mix_into(seed: &mut [u8; 32]) {
        if let Some(rdrand) = x86_64::instructions::random::RdRand::new() {
            for chunk in seed.chunks_mut(8) {
                if let Some(value) = rdrand.get_u64() {
                    chunk.copy_from_slice(&value.to_le_bytes());
                }
            }
        }
    }
}

impl crate::hal::Platform for crate::hal::Current {}
