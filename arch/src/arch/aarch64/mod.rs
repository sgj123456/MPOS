//! aarch64 machine-dependent code.
//!
//! Everything in here may touch aarch64 specifics; nothing outside `arch::` may.
//!
//! This is the second machine (`NOTES.md` §9.1). Stage 1a landed the build
//! plumbing and a body that links; 1b the PL011 and the entry; 1c the exception
//! vectors, a GICv2 and the generic timer; 1d a page-table builder for the
//! `-kernel` bring-up; 1e replaced that bring-up with the real boot path, so a
//! UEFI loader reads the kernel off an ESP and branches here in the higher half;
//! **1f is the kernel taking ownership of its own address space** - it builds
//! its page tables from `mm::vm::kernel_mappings` out of frames the allocator
//! hands it, instead of running on the loader's; **2 is two kernel tasks
//! suspending and resuming each other** (`context.rs`); **5a is the machine
//! reading its own description of itself** - the firmware's device tree, out of
//! the UEFI configuration table by way of the loader, held up against the
//! constants the machine module is compiled with (`checks::check_devices`); and
//! **5b is that description becoming actionable** - the device window those
//! devices live in moved out of the low half and into the kernel's, and the
//! machine's virtio transports are read back through it, so the tree now leads
//! somewhere a driver can go (`check_virtio`); **6a is the machine running
//! code below itself** - a user address space of its own in `TTBR0_EL1`, the
//! frame a trap from EL0 leaves behind, the `eret` that enters it, and the
//! vector entry that takes its `svc` (`check_el0`); and **6b is that address
//! space being taken apart and put back together** - permissions changed in
//! place, mappings handed back, a walk that names what a process owns, a frame
//! shared until one of two processes writes it, and the tables given back when
//! the space goes (`check_user_memory`).
//!
//! What is still absent, and where it lands: everything above the machine layer
//! - `sched` (3), `console`/`vfs` (4), the drivers (5) and the rest of the Linux
//! ABI (6, of which 6a is the entry half and 6b the address-space half). Those
//! reach this machine through `crate::hal`, which it implements trait by trait as
//! the stages land - which is why `hal` itself is still x86_64-only (`src/lib.rs`'s
//! ledger). Nothing above `arch` un-gates before `hal` compiles, and `hal` is
//! two things away now: the `Paging` methods that need a running process, and a
//! `WallClock`. That is measured rather than asserted - §9.1 records the error
//! counts under its stage table, and 6b is what made it two rather than three.

pub(crate) mod checks;
pub(crate) mod context;
pub(crate) mod critical;
pub(crate) mod gic;
/// Where a BAR the firmware handed out is mapped - the aarch64 half of
/// `hal::map_mmio_window`. It has a window to allocate from because PCIe is
/// here now (`NOTES.md` §9.1's gap): the ECAM walk and the AHCI driver both ask
/// for addresses no device tree names.
pub(crate) mod mmio;
pub(crate) mod paging;
pub(crate) mod pl011;
pub(crate) mod pl031;
pub(crate) mod signal;
pub(crate) mod timer;
pub(crate) mod vectors;

use core::arch::global_asm;
use core::fmt;

use mpos_bootinfo::BootInfo;
use mpos_bootinfo::Handoff;
use mpos_vm::KernelSections;
use mpos_vm::PAGE_SIZE;

/// Where the linker script put each part of the kernel image, as virtual
/// addresses.
///
/// The aarch64 half of the seam, and the same shape as `arch/x86_64`'s: the
/// plan in `mm::vm` says what permissions each section gets, and only here does
/// anything know their names. The linker script spells these symbols
/// deliberately name-for-name like the x86_64 one so the two bodies stay
/// comparable.
///
/// The addresses that come back are the *running* ones, not the linked ones. The
/// script has no base address - the image is linked at zero and made a static
/// PIE - so every reference the linker emitted for one of these symbols is a
/// self-relative fixup, and the loader applied them against `KERNEL_VIRT_BASE`
/// before it branched here. That is what makes `text` begin at
/// `BootInfo::kernel_virt_base` plus an offset rather than at the offset alone,
/// and it is what `mm::vm::kernel_mappings` needs: it is handed these ranges and
/// works out each one's physical address from the record.
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
        // Data runs to the end of the image: `.dynamic` and `.bss` follow it
        // with no gap, and all three must be writable.
        data: (&raw const __data_start as usize)..(&raw const kernel_end as usize),
    }
}

/// Put a formatted line on the machine's boot console.
///
/// The console mux `mpos::println!` reaches is x86_64's until stage 4, so this
/// is the one door the entry and the panic path use. It is a bypass of the log
/// ring on purpose: both callers run before (or instead of) anything the ring
/// could carry.
pub fn print_args(args: fmt::Arguments) {
    use fmt::Write;
    let _ = pl011::Writer.write_fmt(args);
}

/// Run the machine from the record `mpos-boot` left in `x0`. Never returns.
///
/// The register is the whole interface and the record is checked rather than
/// trusted: nothing in the machine enforces that `x0` holds what this function
/// was compiled to expect, and [`crate::boot::handoff::collect`] asserts the
/// magic before any field is read.
///
/// By the time this runs the machine is already in the kernel's higher half -
/// the loader built page tables enough to get here - but it is still *the
/// loader's* address space. The middle of this function is where that stops: the
/// frame allocator comes up out of the record's own memory map, and then the
/// kernel builds tables from `mm::vm::kernel_mappings` and switches onto them.
///
/// The order is the dependencies and nothing else. The frame allocator is before
/// the tables because the tables are made of frames. Interrupts are up before the
/// tables but not *enabled* until after them, so the build cannot be interrupted
/// halfway - and after them so that the console and the clock are configured
/// once, on the machine that will actually run.
///
/// `milestone` is a function the caller (the `main` boot wiring) hands in, run
/// once the PL011 is up so the machine's own log has reached the wire. It exists
/// because `crate::console` is above `arch` and `arch` may not name it
/// (`AGENTS.md` §4, downward-only) - yet the console service is exactly what
/// stage 4a un-gated for this machine, and a milestone check for it belongs in
/// the boot wiring, not the machine layer. So `main` supplies a function that
/// writes through `crate::console`, and `arch` only ever invokes the pointer:
/// the console name stays on the `main` side of the seam. It is handed the
/// record because the command line in it decides *where* the kernel log goes
/// (`console=`), and the first machine's counterpart reads the same two facts.
///
/// # Safety
///
/// The caller is the loader's branch to `_start`, with `x0` holding the record
/// it wrote and every address that record names mapped for as long as the kernel
/// runs. Nothing chooses this call from Rust, and the pointer is read before it
/// can be validated, because the magic check inside `collect` happens *after*
/// the read: its validity is the caller's contract and not something this
/// function can establish.
/// `milestone` and `devices` are pointers [`main`] handed down, and the reason
/// they are pointers is the shape of this seam: the console service and the
/// device drivers sit *above* the machine layer, so `arch` may call them but
/// must not name them. `init` is the same again - the handoff that loads the
/// first program.
pub unsafe fn boot(
    handoff: *const Handoff,
    milestone: fn(&BootInfo),
    devices: fn(&BootInfo),
    init: fn(&BootInfo) -> !,
) -> ! {
    pl011::init();

    // SAFETY: the loader's contract - `handoff` points at the record it wrote,
    // still mapped at the address it put it, and every physical range that
    // record names is reachable through the direct map it chose.
    let info = unsafe { crate::hal::kernel::boot_info(handoff) };
    banner(handoff as usize, &info);

    // The clock this machine's kernel wants is the EL1 physical timer, and the
    // vector table and GIC are configured for EL1 as well; a machine that
    // started this image at EL2 would fault or, worse, arm a timer nothing
    // ever reads. Which level the entry arrives at is the firmware's decision
    // (`NOTES.md` §9.1's risk list), so it is checked rather than assumed - and
    // the check is here, before the machine is configured, rather than in the
    // banner's printout alone.
    let level = current_el();
    if level != 1 {
        print_args(format_args!(
            "aarch64: entered at EL{level}, and this stage's clock and vectors are \
             EL1's; stopping rather than configuring the wrong machine\n"
        ));
        halt();
    }

    // The console service (`crate::console`, un-gated for this machine at stage
    // 4a) has never been on the wire yet - every line above this was the machine's
    // own `pl011::Writer`. `main`'s `milestone` writes one through the service and
    // reports it; `arch` only calls the pointer, so the console name stays in the
    // wiring layer that may hold it.
    milestone(&info);

    // Stage 5a: the machine's own description of itself, held up against the
    // constants the rest of this file is compiled with. Before the allocator
    // and before the kernel's own tables, because it needs neither - the tree
    // is in the loader's direct map, which is the address space the kernel is
    // still running in.
    checks::check_devices(&info);

    // SAFETY: the record's contract, unchanged - every region it names is live
    // memory the kernel may hand out, and the loader's copy of it is the only
    // description of this machine there is.
    let stats = unsafe { crate::hal::kernel::start_memory(&info) };
    print_args(format_args!(
        "pmm: {} zones, {} frames ({} MiB of {} MiB usable)\n",
        stats.zones,
        stats.total,
        stats.total * PAGE_SIZE / (1024 * 1024),
        info.allocatable_bytes() / (1024 * 1024),
    ));

    init_interrupts();

    let sections = kernel_sections();
    // SAFETY: the frame allocator is up, and everything the kernel is executing
    // from, reading and writing at this moment - the image's sections and the
    // direct map over the record's regions - is in the tables this builds.
    let frames = unsafe { paging::install(&info, &sections) };
    checks::check_paging(&info, &sections, frames);

    // The machine's transports, opened and driven. This is a pointer `main`
    // handed down and not a call this module makes: *which windows exist* is
    // the machine's to answer - the tree said, and these tables now reach them
    // - but opening one as a virtio device, mounting a filesystem over it and
    // registering it with the block layer is driver, filesystem and block work,
    // which sits above the machine layer and cannot be named from it.
    // `milestone` above travels the same way and for the same reason.
    devices(&info);

    checks::run();

    // And the kernel's own boot, which never returns - the same fall-through the
    // first machine's `boot` has had all along. It used to be stashed in a pair
    // of statics and run from inside `el0_returned`, because the EL0 probe was
    // entered through `enter_frame` and the only way back into an EL1 frame was
    // the trap gate's `eret`; `context::enter_probe` returns now, so the handoff
    // is an ordinary call. What travels is this function's *own* `info` local:
    // the record's raw pointer is dead (`boot_info` read it before
    // `paging::install` swapped in the kernel's tables, and the loader's identity
    // address is not in them), but the `BootInfo` value copied into this frame
    // holds slices into the direct map, which the switch keeps, and this frame
    // never returns - `boot` is `-> !` - so the reference lives as long as the
    // machine does.
    init(&info)
}

// The program the check runs at EL0.
//
// It is the smallest thing that can be wrong in all the ways this stage is
// about, and every instruction is load-bearing:
//
// * `mov x20` puts the answer page's address in a *callee-saved* register, so
//   the store at the end is what proves the whole register file survived the
//   round trip rather than just the argument registers.
// * the first `str` writes the program's own value before it has made any call,
//   which is the page's first word and the one no kernel code ever touches.
// * the `svc` with `0x1234` is a number no system call has, and the `str` after
//   it is what proves the trap *returned*: if the gate never came back, nothing
//   would be written to the second word.
// * `x0` is read back from that store by the kernel, so the second word is the
//   gate's answer as the program received it - not as the kernel remembers
//   writing it.
// * `exit_group` ends it, because a program that finishes has to be able to say
//   so, and because it is the one call this stage's gate answers.
//
// It is copied into a frame of its own rather than mapped where it is linked.
// The kernel's text is EL1-only and read-only, and mapping *that* page into a
// user space would hand a program the kernel's code; a loader copies bytes into
// a fresh frame, and there is no reason for a check to do anything else.
//
// It has a section of its own because it is *data*: bytes the kernel never
// executes where they are linked, which is why the W^X plan need not give them
// execute permission. The section is entered with `.pushsection` and left with
// `.popsection`, and that is load-bearing rather than tidiness. Every
// `global_asm!` in this crate is emitted into one assembly unit, and a block
// that names no section lands in whatever directive was last in effect - so a
// block that *changes* the section without changing it back moves every block
// assembled after it into its section, silently and with no diagnostic. Measured:
// with `.section` here and no restore, `context.rs`'s three code blocks
// (`mpos_enter_frame`, `mpos_context_switch` with `mpos_task_trampoline`, and
// `mpos_context_round_trip`) were assembled into `.rodata.el0_program`, the
// kernel therefore mapped them read-only, and the first call to one - stage 2's
// context check - was an instruction abort whose fault address was inside the
// image. `linker.ld` now asserts the placement so the next such leak fails the
// link instead of the boot.
global_asm!(
    ".pushsection .rodata.el0_program, \"a\"",
    ".balign 4",
    ".globl __aarch64_el0_program",
    "__aarch64_el0_program:",
    "    mov  x20, #{answer}",
    "    movz x0, #0xaaaa",
    "    str  x0, [x20]",
    "    movz x8, #0x1234",
    "    svc  #0",
    "    str  x0, [x20, #8]",
    "    movz x8, #{exit_group}",
    "    movz x0, #7",
    "    svc  #0",
    "1:  b 1b",
    ".globl __aarch64_el0_program_end",
    "__aarch64_el0_program_end:",
    ".popsection",
    answer = const checks::ANSWER_VA as u64,
    exit_group = const 94u64,
);

unsafe extern "C" {
    static __aarch64_el0_program: u8;
    static __aarch64_el0_program_end: u8;
}

/// Bring the machine's interrupts up: the vector table, then the controller,
/// then the clock.
///
/// The order is the x86_64 `init_interrupts`'s, and for the same reason: the
/// vectors must exist before anything can raise one, and the controller must
/// know about the timer's line before the timer is armed to raise it. Unmasking
/// the CPU is the caller's separate step, because a boot path brings the machine
/// up and *then* opens the door.
pub fn init_interrupts() {
    vectors::init();
    gic::init();
    timer::init();
    print_args(format_args!(
        "interrupts: GICv2 (distributor {:#x}), vector table at {:#x}, timer on INTID {}\n",
        gic::DIST,
        vectors::table(),
        gic::TIMER_INTID
    ));
    // The GIC's and the timer's writes must have reached the devices before
    // this core can take an interrupt at all; without the barrier the window is
    // one instruction wide, which is exactly the kind of gap that works on a
    // fast model and fails on a slow one.
    unsafe {
        core::arch::asm!("dsb sy", options(nomem, nostack));
    }
}

/// Whether this core would take an interrupt right now.
pub fn interrupts_enabled() -> bool {
    vectors::interrupts_enabled()
}

/// Let the console's UART interrupt, and let the console queue into it.
///
/// The aarch64 counterpart of x86_64's `drivers::serial::{enable_rx,enable_tx}`
/// pair, and the reason this is one call rather than two: on that machine the
/// 16550 is a driver above the machine layer and its two directions are two
/// functions, while here the PL011 *is* the machine's console and both halves
/// are its (`pl011::enable_interrupts`). What the two machines share is the
/// queue the transmitter drains and the point in the boot at which the console
/// starts using it, not the names either machine calls them by.
///
/// Called once, from the wiring layer, once a typed byte has somewhere to go:
/// the handler hands one to the console's tty, so arming earlier would take an
/// interrupt into a subsystem that does not exist yet.
pub fn enable_console_interrupts() {
    pl011::enable_interrupts();
    crate::hal::tx_ring::enable();
}

/// Let the core take interrupts.
pub fn enable_interrupts() {
    vectors::enable_interrupts()
}

/// Say who this is, what it was handed, and get out of the way.
///
/// Deliberately the same shape as the x86_64 banner (`src/board/report.rs`,
/// the shared one), so
/// the two machines' first lines are comparable rather than merely both
/// present - and so that a loader which relocated the image to the wrong base,
/// or handed over the wrong memory map, shows up as a wrong number here rather
/// than as a fault somewhere later. Every line below comes out of the record
/// rather than out of a constant, which is the whole difference from stage 1d's
/// banner: that one could only say what this file believed, and this one says
/// what the loader did.
fn banner(record: usize, info: &BootInfo) {
    use fmt::Write;

    let sections = kernel_sections();
    let image = sections.data.end - sections.text.start;

    let mut out = pl011::Writer;
    let _ = writeln!(out, "mpos {} (aarch64) booting", env!("CARGO_PKG_VERSION"));
    // The address is fixed for this one machine and named as such; the device
    // tree is what stage 5 reads instead, and it comes from the UEFI
    // configuration table.
    let _ = writeln!(out, "console: PL011 at {:#x}", pl011::BASE);
    let _ = writeln!(
        out,
        "image: text {} KiB, rodata {} KiB, data {} KiB ({} KiB total)",
        (sections.text.end - sections.text.start) / 1024,
        (sections.rodata.end - sections.rodata.start) / 1024,
        (sections.data.end - sections.data.start) / 1024,
        image / 1024,
    );
    let _ = writeln!(
        out,
        "handoff: {} {}, record at {record:#x}, image phys {:#x} -> virt {:#x}, \
         direct map at {:#x}",
        info.bootloader_name,
        info.bootloader_version,
        info.kernel_phys_base,
        info.kernel_virt_base,
        info.hhdm_offset,
    );
    let _ = writeln!(
        out,
        "memory: {} regions, {} MiB the allocator may use",
        info.region_count,
        info.allocatable_bytes() / (1024 * 1024),
    );
    // Recorded before it is printed, and recorded *here* because this is the one
    // place the loader's command line is in hand. `/proc/cmdline` is where a
    // userspace init reads it (Alpine's own reads `root=` through this same
    // file), and the handoff reads its own instrument flag back out of it - the
    // `mpos.selftest` word that says "this boot is a test", the same word the
    // x86_64 boot gates its milestone checks on. Until this call the machine's
    // command line was reachable nowhere but this print.
    crate::hal::kernel::set_cmdline(info.cmdline);
    // The command line is the one field whose bytes the loader had to read out
    // of a file, so printing it is how a wrong one - an empty file, a truncated
    // copy - is visible at boot rather than at the first program that parses it.
    let _ = writeln!(out, "cmdline: {:?}", info.cmdline);
    let _ = writeln!(out, "exception level: EL{}", current_el());
    let _ = writeln!(
        out,
        "aarch64: loaded by mpos-boot, building the kernel's own tables (stages 1a-1f)"
    );
}

/// Which exception level this code is running at.
///
/// Read rather than assumed, and read *now* rather than when it matters: which
/// level the entry arrives at depends on the firmware and the `-cpu`, and a
/// paging or timer path that assumed EL1 would be wrong on a machine that
/// starts at EL2 (`NOTES.md` §9.1's risk list). Printing it here is the cheapest
/// way to keep that assumption from being made.
fn current_el() -> u64 {
    let current: u64;
    // SAFETY: `CurrentEL` is a read-only system register, readable at every
    // exception level; the read has no side effects.
    unsafe {
        core::arch::asm!("mrs {}, CurrentEL", out(reg) current, options(nomem, nostack));
    }
    (current >> 2) & 0b11
}

/// Stop the machine for good.
///
/// `wfi` parks the core until an interrupt. Before stage 1c's clock there was
/// none to wake it, so this was a reset-only stop; now it is the real idle -
/// which is the shape the scheduler's own loop will want, where a parked core
/// is woken by the tick rather than spinning on it. The panic path reaches here
/// with interrupts as they were, which is what makes a panic on a machine with
/// the clock running park rather than spin.
pub fn halt() -> ! {
    loop {
        unsafe { core::arch::asm!("wfi", options(nomem, nostack)) };
    }
}

// ---------------------------------------------------------------------------
// `crate::hal`: this machine implements the six capability concerns for
// `crate::hal::Current`. Everything here is a one-line static dispatch onto the
// module that actually holds the body - `timer`/`pl031`/`vectors`/`gic`/`paging`/
// `context` - so `crate::hal::ticks()` is exactly the `timer::ticks()` a caller
// above the seam wants, monomorphised, with no architecture named above `arch`
// (`AGENTS.md` §4, `NOTES.md` §9.1). x86_64's `mod.rs` is the shape template; the
// optional capabilities it has (`Msi`, `FpuTrap`, `SignalFrame`) are absent
// here because this machine has no MSI controller, no lazy-FPU trap, and no
// `rt_sigframe` - so they are *not implemented*, which is the whole point of
// splitting them out.
// ---------------------------------------------------------------------------

/// The line a caller masks/unmasks by name, as this machine's GIC INTID. The
/// timer is the non-secure EL1 physical PPI (30) and the serial is the PL011's
/// SPI (33); a driver never says which distributor bit.
fn gic_line(line: crate::hal::Line) -> u32 {
    match line {
        crate::hal::Line::Timer => gic::TIMER_INTID,
        crate::hal::Line::Serial => gic::SERIAL_INTID,
    }
}

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
        pl031::boot_epoch()
    }

    #[inline]
    fn set_boot_epoch(epoch: u64) {
        pl031::set_boot_epoch(epoch)
    }

    #[inline]
    fn write_epoch(epoch: u64) -> bool {
        pl031::write_epoch(epoch)
    }
}

impl crate::hal::Cpu for crate::hal::Current {
    #[inline]
    fn interrupts_enabled() -> bool {
        interrupts_enabled()
    }

    #[inline]
    fn enable_interrupts() {
        enable_interrupts()
    }

    #[inline]
    fn halt() -> ! {
        halt()
    }

    #[inline]
    fn idle() {
        vectors::idle()
    }

    #[inline]
    fn enable_user_simd() {
        vectors::enable_user_simd()
    }

    #[inline]
    fn init_interrupts() {
        init_interrupts()
    }

    #[inline]
    fn set_thread_pointer(base: usize) {
        vectors::set_thread_pointer(base)
    }

    #[inline]
    fn thread_pointer() -> usize {
        vectors::thread_pointer()
    }

    #[inline]
    fn stack_pointer() -> usize {
        vectors::stack_pointer()
    }
}

impl crate::hal::Interrupts for crate::hal::Current {
    #[inline]
    fn unmask(line: crate::hal::Line) {
        gic::enable(gic_line(line))
    }

    #[inline]
    fn mask(line: crate::hal::Line) {
        gic::disable(gic_line(line))
    }

    #[inline]
    fn eoi(line: crate::hal::Line) {
        gic::end_of_interrupt(gic_line(line))
    }
}

impl crate::hal::Paging for crate::hal::Current {
    type Space = paging::UserSpace;

    unsafe fn install(info: &mpos_bootinfo::BootInfo, sections: &mpos_vm::KernelSections) -> usize {
        // SAFETY: the caller's contract, passed through unchanged.
        unsafe { paging::install(info, sections) };
        // SAFETY: the machine is now running on the tables `install` just built,
        // so reading the base registers reports the kernel's own root.
        unsafe { paging::adopt() };
        paging::kernel_root()
    }

    #[inline]
    fn map_mmio(virt: usize, phys: usize, len: usize) {
        paging::map_mmio(virt, phys, len)
    }

    #[inline]
    fn map_mmio_window(phys: usize, len: usize) -> Option<usize> {
        // Three answers now, and which one a caller gets is a fact about the
        // range rather than about the caller. Inside the window the boot mapped,
        // the range is already reachable at its offset into the kernel-half
        // alias, and that is still the answer - a device the tree describes keeps
        // its one address. Outside it, the range is a BAR or an ECAM bus, whose
        // address the firmware chose at enumeration time: it gets a mapping of
        // its own from `mmio::window`.
        //
        // The third answer comes first, and it is the one a *device* address can
        // need: `TCR_EL1`'s `IPS` bounds every physical address the walk may
        // output, so a range above this CPU's own physical address space has no
        // mapping to give. Writing one anyway is what an address-size abort at
        // the first read is made of - the machine stops on a leaf nothing could
        // translate instead of the driver being told there is no device there.
        // `None` is this contract's own word for "this machine cannot map that
        // range" (`hal::Paging::map_mmio_window`), the same answer a full window
        // gives, and the absence a driver is built to read rather than die of.
        if !paging::phys_reachable(phys, len) {
            return None;
        }
        paging::device_virt(phys, len).or_else(|| mmio::window(phys, len))
    }

    #[inline]
    fn translate(virt: usize) -> Option<(usize, mpos_vm::MapFlags)> {
        // The hardware walk also says whether the page is cached as memory or
        // device; the trait's question is the frame and the permissions, so the
        // third element is dropped here and kept only in `paging::translate`.
        paging::translate(virt).map(|(phys, flags, _)| (phys, flags))
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
        // SAFETY: the scheduler installs a space only after mapping it, which is
        // `paging::switch_to_root`'s contract of a complete low half.
        unsafe { paging::switch_to_root(root) };
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
    type UserFrame = context::UserFrame;

    const FPU_BYTES: usize = context::FPU_BYTES;
    const ABI_REG_COUNT: usize = context::ABI_REG_COUNT;

    #[inline]
    fn set_kernel_stack(top: usize) {
        // SAFETY: the scheduler points a task's own kernel stack here, which is
        // the value `context::set_kernel_stack`'s contract asks for.
        unsafe { context::set_kernel_stack(top) };
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
        // SAFETY: the scheduler built this frame from a task's own record; the
        // register-file entry it calls documents the rest.
        unsafe { <context::UserFrame as crate::hal::UserFrameOps>::resume(&*frame) }
    }

    #[inline]
    unsafe fn enter_user(entry: usize, user_rsp: usize) -> ! {
        // A fresh task's first frame: `entry` at `pc`, `user_rsp` as `SP_EL0`,
        // `pstate` 0 = EL0t with nothing masked, every register zero. It is then
        // handed to the same register-file entry a resume uses.
        let frame = context::UserFrame {
            x: [0; 31],
            sp: user_rsp as u64,
            pc: entry as u64,
            pstate: context::PSTATE_EL0T,
        };
        // SAFETY: the caller's `entry`/`user_rsp` are a mapped address and a
        // usable stack in the active space, which is what `resume` requires.
        unsafe { <context::UserFrame as crate::hal::UserFrameOps>::resume(&frame) }
    }
}

/// The console sink on this machine: the PL011 in the kernel-half device window.
///
/// [`Console`](crate::hal::Console)'s baseline half is the polled write below,
/// and it stays: it is what every line before the interrupt is armed goes out
/// on, and what the panic path uses afterwards. The machine's PL011 also has an
/// interrupting transmitter the GIC routes to INTID 33, so it is a
/// [`SerialRing`](crate::hal::SerialRing) too - see below.
impl crate::hal::Console for crate::hal::Current {
    #[inline]
    fn init() {
        pl011::init()
    }

    fn write_bytes(bytes: &[u8]) {
        // The polled wire expands a bare `\n` to `\r\n` - the same rule
        // `pl011::write_str` applies to text, spelled over arbitrary bytes because
        // a byte sink cannot assume its input is UTF-8. A kernel `println!`'s
        // trailing newline would otherwise print a staircase.
        for &byte in bytes {
            if byte == b'\n' {
                pl011::write_byte(b'\r');
            }
            pl011::write_byte(byte);
        }
    }

    #[inline]
    fn interrupt_driven() -> bool {
        // `tx_ring` armed by [`enable_console_interrupts`], after which the
        // console may queue rather than wait for the wire. Until then this is
        // the honest `false`, so the console takes the polled path and no task
        // ever parks on a queue nothing is draining.
        crate::hal::tx_ring::enabled()
    }
}

/// This machine's console may queue its bytes and let the UART's own interrupt
/// drain them - the optional [`crate::hal::SerialRing`] capability, which it is
/// because the PL011 has an interrupting transmitter and the GIC has the SPI to
/// carry it (`gic::SERIAL_INTID`, the tree's `interrupts = <0 1 4>`).
///
/// Every method is a forward to the queue. That is the point rather than an
/// indirection: the room there is, who sleeps for it and what a visit from the
/// transmitter does with it were written once, above both machines, so this
/// machine's answer differs from x86_64's only in which three lines reach its
/// UART.
impl crate::hal::SerialRing for crate::hal::Current {
    #[inline]
    fn tx_space() -> usize {
        crate::hal::tx_ring::space()
    }

    #[inline]
    fn tx_submit(bytes: &[u8]) -> usize {
        crate::hal::tx_ring::submit::<pl011::Pl011>(bytes)
    }

    #[inline]
    fn tx_wait_now() {
        crate::hal::tx_ring::wait_now();
    }

    #[inline]
    fn tx_stop_waiting() {
        crate::hal::tx_ring::stop_waiting();
    }
}

impl crate::hal::ConsoleInput for crate::hal::Current {
    #[inline]
    fn poll_input() -> Option<u8> {
        pl011::read_byte()
    }

    #[inline]
    fn inject_input(byte: u8) {
        pl011::inject_via_loopback(byte)
    }
}

/// This machine's own entropy: none.
///
/// There is no `RDRAND` on aarch64 and nothing else in the core to ask, so the
/// answer is empty rather than invented. What this machine is seeded from is its
/// MMIO virtio-rng, which the boot's `check_rng` installs before `rand::init`
/// runs - and which is *not* folded in here, because the device is a driver the
/// kernel owns rather than an answer the machine gives. `crate::rand` folds it
/// in after this returns (`NOTES.md` §14.3), so a machine with no rng window
/// still leaves the clock mix as the floor.
impl crate::hal::Entropy for crate::hal::Current {
    fn mix_into(_seed: &mut [u8; 32]) {}
}

/// The gate's frame, as `hal`'s seam hands it to the ABI layer.
///
/// aarch64's gate frame *is* the task's user frame - `__vec_el0_sync` saves the
/// trapped registers straight into it - so unlike x86_64 there is no second
/// object here, and every op is a copy of it with no register named. What the
/// two machines share is the *operations*, which is the whole point of the
/// seam.
impl crate::hal::Gate for crate::hal::Current {
    unsafe fn user_ip(frame: *mut ()) -> u64 {
        // SAFETY: the caller's contract - the pointer is the frame the EL0 sync
        // vector built for this task.
        let frame = unsafe { &*(frame as *const context::UserFrame) };
        crate::hal::UserFrameOps::ip(frame) as u64
    }

    unsafe fn set_exec_return(frame: *mut (), entry: usize, sp: usize) {
        // SAFETY: as above.
        let frame = unsafe { &mut *(frame as *mut context::UserFrame) };
        // `execve`'s result is never observed by its caller, so x0 is zeroed
        // rather than left holding the call's number - and `eret` then takes pc
        // and sp from here, which is what makes the call "not return".
        crate::hal::UserFrameOps::set_result(frame, 0);
        crate::hal::UserFrameOps::set_ip(frame, entry);
        crate::hal::UserFrameOps::set_sp(frame, sp);
    }

    unsafe fn user_context(frame: *mut (), rax: usize) -> Option<crate::hal::UserFrame> {
        // SAFETY: the caller's contract - the pointer is the frame the EL0 sync
        // vector built for this task, or null.
        if frame.is_null() {
            return None;
        }
        let mut user = unsafe { *(frame as *const context::UserFrame) };
        user.x[0] = rax as u64;
        Some(user)
    }

    unsafe fn capture_user_regs(frame: *mut (), rax: usize, tls: usize, out: &mut [u64]) {
        // SAFETY: as above.
        let mut user = unsafe { *(frame as *const context::UserFrame) };
        // `rax` belongs in the result slot, which is `x0` here, exactly as in
        // `user_context`.
        user.x[0] = rax as u64;
        crate::hal::UserFrameOps::to_user_regs(&user, tls, out);
    }

    unsafe fn restore_user_regs(frame: *mut (), regs: &[u64], tls: &mut usize) -> isize {
        let user: context::UserFrame = crate::hal::UserFrameOps::from_user_regs(regs);
        // SAFETY: as above.
        unsafe { *(frame as *mut context::UserFrame) = user };
        // The thread pointer is `TPIDR_EL0`, which is not a field of
        // `user_pt_regs` and so not a word of `regs` either: there is no value
        // for this machine to hand back, and the caller's `tls` is left as it
        // was. Nothing on this machine reads it back anyway - it has no tracer.
        let _ = tls;
        user.x[0] as isize
    }

    unsafe fn syscall_args(frame: *mut ()) -> [usize; 6] {
        // SAFETY: the caller's contract.
        let frame = unsafe { &*(frame as *const context::UserFrame) };
        // The EL0 sync vector reads the number from x8 and the arguments from
        // x0..x5, so the frame holds them in that order already.
        [
            frame.x[0] as usize,
            frame.x[1] as usize,
            frame.x[2] as usize,
            frame.x[3] as usize,
            frame.x[4] as usize,
            frame.x[5] as usize,
        ]
    }

    unsafe fn syscall_number(frame: *mut ()) -> u64 {
        // SAFETY: the caller's contract.
        let frame = unsafe { &*(frame as *const context::UserFrame) };
        // `x8`, where the EL0 sync vector reads the number from.
        frame.x[8]
    }

    unsafe fn set_syscall_number(frame: *mut (), nr: u64) {
        // SAFETY: the caller's contract.
        let frame = unsafe { &mut *(frame as *mut context::UserFrame) };
        frame.x[8] = nr;
    }

    unsafe fn syscall_entry_result_slot(frame: *mut (), _nr: usize) -> usize {
        if frame.is_null() {
            return 0;
        }
        // SAFETY: the caller's contract.
        let frame = unsafe { &*(frame as *const context::UserFrame) };
        // `x0`, unchanged: it is this call's first argument, and the number the
        // trait's `nr` names is in `x8` - a register this file already carries,
        // so there is nothing to inject. Writing `nr` here would replace a live
        // argument and the discharge below re-reads the arguments out of the
        // frame, so the tracee would make its call with the number as `arg0`.
        frame.x[0] as usize
    }

    unsafe fn set_result(frame: *mut (), value: isize) {
        if frame.is_null() {
            return;
        }
        // SAFETY: the caller's contract.
        let frame = unsafe { &mut *(frame as *mut context::UserFrame) };
        // `x0` is where this ABI returns a result *and* where a handler is
        // handed its signal number, which is why the write happens before one
        // is entered rather than in the gate's own return sequence.
        frame.x[0] = value as u64;
    }
}

impl crate::hal::Platform for crate::hal::Current {}
