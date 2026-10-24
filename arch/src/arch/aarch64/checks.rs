//! The aarch64 boot's milestone checks.
//!
//! These are the stage-by-stage evidence lines of `docs/notes/` §9.1: each one
//! runs once at boot, prints what it proved through the machine's own console,
//! and ends the machine by name (`halt`) when the claim fails - which is what
//! `xtask boot-aarch64` polls for. They live here rather than in `mod.rs`
//! because they are not the machine; they are the *test bench* the boot walks
//! over on the way to pid 1, and `mod.rs` is the seam the wiring layer calls.
//! Nothing outside this module names them, and nothing here is reached from
//! above `arch`: the only door is [`run`], called by [`super::boot`] at the
//! point each stage's prerequisites exist.
//!
//! What moved with the checks and why:
//!
//! * the EL0 probe program (`global_asm!`, its `.rodata.el0_program` section
//!   rule and the `__aarch64_el0_program` symbols) stays in `mod.rs` - see the
//!   note there about section directives leaking into every later assembly
//!   block in the crate (§13.16). It is machine code, not test scaffolding.
//! * the check pages' addresses (the `*_VA` constants) travel with the checks
//!   that map them; the probe's three stay beside the probe.
//! * the frame-allocator questions (`zeroed_frame`) travel with their only
//!   callers.

use core::fmt;

use mpos_bootinfo::BootInfo;
use mpos_fdt as fdt;
use mpos_vm::PAGE_SIZE;
use mpos_vm::{self as vm, KernelSections, MapFlags, Mapping};

use super::{context, gic, paging, pl011, pl031, timer, vectors};
use paging::Memory;

/// The one door `super::boot` calls for the stages that need nothing but a
/// running machine: every check whose prerequisites exist by the point in the
/// boot sequence where this is reached, in the order they were written. It runs
/// after the device step and the kernel's own tables, so each stage's evidence
/// line lands on the wire at the moment its claim becomes true. The two checks
/// that read the boot record - [`check_devices`] and [`check_paging`] - are
/// called by `boot` at their own moments, not from here.
pub(super) fn run() {
    check_timer();
    check_context();
    check_user_memory();
    check_paging_ops();
    check_wallclock();
    check_cpu();
    check_serial_rx();
    check_tls();
    check_el0();
}

/// Where the EL0 check's three pages go, in the low half - the half a process
/// owns, which until this stage mapped nothing at all.
///
/// Named rather than allocated from the bottom up, because there is no loader
/// here to choose an address and a program's first page is the one address the
/// kernel has to agree with it about. The null page is still not one of them:
/// 2 MiB is far enough above zero that a stray pointer is a fault.
const PROGRAM_VA: usize = 0x0020_0000;
const STACK_VA: usize = 0x0021_0000;
pub(super) const ANSWER_VA: usize = 0x0022_0000;

/// The frame the answer page is, so [`el0_returned`] can read it.
///
/// A static because the two halves of this check do not share a stack frame:
/// [`check_el0`] enters EL0 and the continuation runs on the trap gate's way
/// back, with nothing of the caller's to pass anything in.
static ANSWER_FRAME: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);

/// A fresh, zeroed frame from the frame allocator.
///
/// Zeroed for the reason the page tables' own frames are: a program that reads a
/// page it never wrote should read zeroes, and a frame straight from the
/// allocator holds whatever the last user left in it.
fn zeroed_frame() -> Option<usize> {
    let frame = crate::hal::kernel::alloc_frames(0)?;
    // SAFETY: the allocator handed this frame over and nothing else holds it,
    // and the direct map reaches it.
    unsafe { core::ptr::write_bytes(crate::hal::phys_to_virt(frame) as *mut u8, 0, PAGE_SIZE) };
    Some(frame)
}

/// Stage 5a's evidence: the machine reads its own description, and what it read
/// agrees with what this kernel has been compiled to expect.
///
/// Until this stage the addresses in `pl011`, `gic` and `paging` were constants
/// with a comment saying where they came from, and the loader mapped a device
/// window because there was no way to ask. The tree is the way to ask: the
/// firmware publishes it, the loader copies it out of the configuration table
/// before boot services go away, and this reads it.
///
/// What makes this a check rather than a printout is that every number is
/// compared against the constant the rest of the machine *already uses*: the
/// serial port's base, both of the interrupt controller's frames, the timer's
/// line, and the memory the record calls usable. A tree is not trusted to be
/// right - it is used to ask whether this kernel's idea of the machine is. That
/// is also the only test that can catch a tree walked *wrongly*: a reader that
/// answered a plausible address from a corrupt tree would put a device
/// somewhere this kernel does not reach, which is exactly what a disagreement
/// with the constants reports.
///
/// It runs before `mm::init`, and so before the kernel's own tables exist: the
/// tree is in the loader's direct map, which is the address space the kernel is
/// running in until `paging::install` returns.
pub(super) fn check_devices(info: &BootInfo) {
    use fmt::Write;

    let mut out = pl011::Writer;

    /// News this machine cannot act on: say which claim failed and stop. The
    /// marker is what `xtask boot-aarch64` polls for, so a check that fails
    /// ends the run rather than waiting out the deadline.
    macro_rules! fail {
        ($($arg:tt)*) => {{
            let _ = writeln!(out, "devices: {}", format_args!($($arg)*));
            super::halt();
        }};
    }

    let Some(bytes) = info.dtb else {
        fail!(
            "the record carries no device tree, and this machine has no other \
             description of itself"
        );
    };
    let tree = match fdt::Fdt::new(bytes) {
        Ok(tree) => tree,
        Err(error) => fail!("the device tree will not walk: {error:?}"),
    };
    let _ = writeln!(
        out,
        "devices: the firmware's tree, {} bytes at {:#x}",
        tree.total_size(),
        bytes.as_ptr() as usize,
    );

    // The tree's memory node against the loader's memory map: two descriptions
    // of the same RAM, written by two different parties, and a machine whose
    // tree disagrees with the map its loader built is one whose devices are not
    // where the tree says either.
    let memory = match tree.find(|node| node.string_property("device_type") == Some("memory")) {
        Ok(Some(node)) => node,
        Ok(None) => fail!("the tree describes no memory"),
        Err(error) => fail!("the tree will not walk: {error:?}"),
    };
    let Some((base, size)) = memory.reg().next() else {
        fail!("the memory node {} has no usable reg", memory.name());
    };
    let (base, size) = (base as usize, size as usize);
    for region in info
        .regions()
        .iter()
        .filter(|region| region.kind.is_allocatable())
    {
        if region.base < base || region.base + region.length > base + size {
            fail!(
                "the tree's memory is {base:#x}..{:#x}, and the record calls \
                 {:#x}..{:#x} usable",
                base + size,
                region.base,
                region.base + region.length,
            );
        }
    }
    let _ = writeln!(
        out,
        "devices: memory {base:#x}..{:#x} ({} MiB), every region the record calls \
         usable is inside it",
        base + size,
        size / (1024 * 1024),
    );

    // Three devices, each checked against the constant this kernel reaches it
    // at. The lookup is a macro rather than a closure because its failure path
    // writes to `out`: a closure would hold that borrow for as long as it
    // lived, and the checks between its calls have things to say too.
    macro_rules! found {
        ($what:expr, $compatible:expr) => {
            match tree.find_compatible($compatible) {
                Ok(Some(node)) => node,
                Ok(None) => fail!("the tree has no {} ({}) node", $what, $compatible),
                Err(error) => fail!("the tree will not walk: {error:?}"),
            }
        };
    }

    let serial = found!("serial port", "arm,pl011");
    let Some((serial_base, _)) = serial.reg().next() else {
        fail!("the serial node {} has no reg", serial.name());
    };
    if serial_base as usize != pl011::PHYS {
        fail!(
            "the tree puts the serial port at {serial_base:#x}, and this kernel \
             reaches it at {:#x}",
            pl011::PHYS
        );
    }

    // The interrupt controller is one `reg` of two windows, and the kernel has
    // two constants because the two frames do different things - the
    // distributor routes, the CPU interface acknowledges.
    let intc = found!("interrupt controller", "arm,cortex-a15-gic");
    let mut windows = intc.reg();
    let (Some((dist, _)), Some((cpu, _))) = (windows.next(), windows.next()) else {
        fail!(
            "the interrupt controller node {} has one reg window, and this machine \
             has a distributor and a CPU interface",
            intc.name()
        );
    };
    if dist as usize != gic::DIST_PHYS || cpu as usize != gic::CPU_PHYS {
        fail!(
            "the tree puts the GIC at {dist:#x}/{cpu:#x}, and this kernel reaches \
             it at {:#x}/{:#x}",
            gic::DIST_PHYS,
            gic::CPU_PHYS
        );
    }

    // The timer's lines, decoded the way the binding says: a triple per
    // interrupt - shared or per-CPU, the number in that space, and the trigger
    // - whose first cell decides the offset, since an SPI counts from 32 and a
    // PPI from 16. This kernel's clock is the EL1 physical timer, which is PPI
    // 14 and therefore INTID 30, and its sub-tick wake is the EL1 *virtual*
    // timer, PPI 11 and INTID 27; both sentences are what the check holds the
    // tree to - the node names all four timers the architecture has, and a
    // tree that stopped carrying either of the two this kernel drives would
    // leave a vector the GIC enables but nothing raises.
    let timer = found!("timer", "arm,armv8-timer");
    let cells = match intc.u32_property("#interrupt-cells") {
        Some(3) => 3usize,
        Some(other) => fail!(
            "the tree's interrupt controller takes {other} cells per interrupt, \
             and this kernel decodes the GIC's three"
        ),
        None => fail!("the tree's interrupt controller does not say how wide an interrupt is"),
    };
    // Read one entry at a time rather than collecting them first: this runs
    // before `mm::init`, so the kernel has no heap yet, and a check of the
    // machine's description should not need one.
    let mut interrupts = timer.cells("interrupts");
    if !interrupts.remaining().is_multiple_of(cells) {
        fail!(
            "the timer's interrupts are {} cells, not a whole number of {cells}-cell \
             entries",
            interrupts.remaining()
        );
    }
    let mut timer_line = None;
    let mut vtimer_line = None;
    while interrupts.remaining() >= cells {
        let (Some(kind), Some(number)) = (interrupts.next(), interrupts.next()) else {
            fail!("the timer's interrupts end in the middle of an entry");
        };
        // The rest of the entry is the trigger type and flags, which say how
        // the line is wired rather than which line it is.
        for _ in 2..cells {
            let _ = interrupts.next();
        }
        let intid = match kind {
            0 => number + 32,
            1 => number + 16,
            other => {
                fail!("the timer's interrupts carry kind {other}, not a shared or per-CPU line")
            }
        };
        if intid == gic::TIMER_INTID {
            timer_line = Some((kind, number));
        }
        if intid == gic::VTIMER_INTID {
            vtimer_line = Some((kind, number));
        }
    }
    let Some((kind, number)) = timer_line else {
        fail!(
            "the tree's timer is not on INTID {}, which is the line this kernel's \
             clock is armed on",
            gic::TIMER_INTID
        );
    };
    let Some((vkind, vnumber)) = vtimer_line else {
        fail!(
            "the tree's timer does not carry INTID {}, which is the line this \
             kernel's sub-tick wake is armed on",
            gic::VTIMER_INTID
        );
    };
    let _ = writeln!(
        out,
        "devices: PL011 at {serial_base:#x}, GIC at {dist:#x}/{cpu:#x}, timer on {} \
         {number} (INTID {}), wake on {} {vnumber} (INTID {})",
        if kind == 1 { "PPI" } else { "SPI" },
        gic::TIMER_INTID,
        if vkind == 1 { "PPI" } else { "SPI" },
        gic::VTIMER_INTID,
    );

    // And the transports the rest of stage 5 will drive, checked against the
    // range the kernel maps: a device outside it is one this kernel cannot
    // reach, whichever address the tree gives it, so the window is what the walk
    // below is bounded by.
    let (mut virtio, mut outside) = (0usize, None);
    let walked = tree.each(
        |node| node.is_compatible("virtio,mmio"),
        |node| {
            virtio += 1;
            for (address, size) in node.reg() {
                let (start, end) = (address as usize, (address + size) as usize);
                if start < paging::DEVICE_PHYS || end > paging::DEVICE_PHYS + paging::DEVICE_WINDOW
                {
                    outside = Some((node.name(), address, size));
                }
            }
            true
        },
    );
    if let Err(error) = walked {
        fail!("the tree will not walk: {error:?}");
    }
    if let Some((name, address, size)) = outside {
        fail!(
            "the tree's {name} is at {address:#x}..{:#x}, outside the \
             {:#x}..{:#x} device window this kernel maps",
            address + size,
            paging::DEVICE_PHYS,
            paging::DEVICE_PHYS + paging::DEVICE_WINDOW,
        );
    }
    let _ = writeln!(
        out,
        "devices: {virtio} virtio-mmio windows, all inside the {:#x}..{:#x} device window",
        paging::DEVICE_PHYS,
        paging::DEVICE_PHYS + paging::DEVICE_WINDOW,
    );
}

/// Stage 2's evidence: two kernel tasks alternate, and each one's registers are
/// its own when it comes back.
///
/// It runs last, with the clock already ticking, and that is deliberate rather
/// than convenient: a context switch is the one operation that has to be correct
/// when it is interrupted halfway through by the very machinery it is part of.
/// The letters [`context::boot_check`] prints alternate `ABABAB`, so a switch
/// that returned immediately is visible in the log rather than merely
/// survivable; the count it answers is registers that did not come back.
fn check_context() {
    use fmt::Write;

    let mut out = pl011::Writer;
    let _ = writeln!(out, "\ncontexts: two kernel tasks alternating");
    match context::boot_check() {
        Ok(hand_overs) => {
            let _ = writeln!(
                out,
                "context: {hand_overs} hand-overs, both directions, every \
                 callee-saved register and v31 back where its task left it",
            );
        }
        Err(bad) => {
            let _ = writeln!(out, "context: {bad} registers did not survive a hand-over");
            super::halt();
        }
    }
}

/// Stage 1f's evidence: the machine is running on the kernel's own tables, and
/// they say what the plan says.
///
/// The strongest form of the claim is available here and is the one made: every
/// mapping [`vm::kernel_mappings`] names is walked *page by page* and each page
/// is asked to translate to the frame and the permissions the plan gave it. That
/// is what makes this a check of the plan rather than of this file's opinion of
/// it - the same function the x86_64 `install` consumes is the one being held up
/// against the hardware.
///
/// Three claims are not in the plan and are checked by name: the device window
/// (which the plan does not describe - it is `paging::DEVICE_VIRT`), the null
/// page, and the low half being *empty*. That last one is the difference between
/// this stage and the one before it, and stage 5b made it stronger: the last
/// thing that lived down there, the device window itself, moved into the kernel
/// half, so the claim is now about a quarter of the address space rather than
/// about one hash-mapped pair of pages.
pub(super) fn check_paging(info: &BootInfo, sections: &KernelSections, frames: usize) {
    use fmt::Write;

    // SAFETY: the machine is running on the tables `install` just built - that is
    // what reaching this function means - and it never stops running on them.
    unsafe { paging::adopt() };
    let (ttbr0, ttbr1) = paging::roots();
    let mut out = pl011::Writer;
    let _ = writeln!(
        out,
        "page tables: {frames} frames ({} KiB), TTBR0 {ttbr0:#x}, TTBR1 {ttbr1:#x} \
         (the kernel's own, read back from the registers)",
        frames * PAGE_SIZE / 1024,
    );

    // `TCR_EL1`'s `IPS` is a *limit* rather than a detail: it bounds every
    // physical address the walk may output, and an unset field is 32 bits rather
    // than "no limit". A descriptor above it is not a refused write but a fault
    // at the first *read* - the PCIe ECAM at `0x40_1000_0000` is where that cost
    // a boot, as an address-size abort (`ESR_EL1` `0x96000003`) whose address was
    // the first byte of the dynamic MMIO window.
    //
    // The check reads the register back and compares it with what this CPU says
    // it implements, which is the pair that catches it: the code that wrote the
    // field *omitted* it, and every physical address the kernel had reached
    // before the ECAM - RAM below 4 GiB, a device window at `0x0800_0000` - fits
    // in 32 bits, so the omission was invisible rather than loud.
    let tcr = paging::tcr_el1();
    let ips = paging::ips_in(tcr);
    let want = paging::programmed_ips();
    if ips != want {
        let _ = writeln!(
            out,
            "tables: TCR_EL1 {tcr:#x} holds IPS {ips}, but this CPU implements {want} \
             ({} bits): a device above that would fault at the first read",
            paging::phys_bits()
        );
        super::halt();
    }
    let _ = writeln!(
        out,
        "physical addresses: {} bits, and TCR_EL1 IPS holds it",
        paging::phys_bits()
    );

    if !paging::enabled() {
        let _ = writeln!(out, "tables: SCTLR_EL1.M is clear");
        super::halt();
    }

    // The plan, page by page. `kernel_mappings` is the shared description of
    // what the kernel maps; the walk below is the hardware's answer to the same
    // question, and a disagreement is a failure whichever side is wrong.
    let plan = vm::kernel_mappings(info, sections);
    let mut pages = 0usize;
    for mapping in &plan {
        assert!(
            mapping.is_page_granular(),
            "the plan produced a mapping that is not page-granular: {mapping:?}"
        );
        let mut page = mapping.virt;
        while page < mapping.end() {
            let want = mapping.phys_of(page);
            match paging::translate(page) {
                Some((phys, flags, Memory::Normal)) if flags == mapping.flags && phys == want => {}
                other => {
                    let _ = writeln!(
                        out,
                        "tables: {page:#x} translates to {other:?}, but the plan \
                         says {:?} -> {want:#x}",
                        mapping.flags
                    );
                    super::halt();
                }
            }
            pages += 1;
            page += PAGE_SIZE;
        }
    }
    let _ = writeln!(
        out,
        "plan: {} mappings, all {pages} pages translate as the plan says \
         (image r-x / r-- / rw-, direct map rw-)",
        plan.len()
    );

    // The direct map is *usable* and not just translated, and the store is the
    // check of that.
    //
    // The frame is asked for rather than picked, and that is a correction worth
    // recording: the first version of this check wrote to "the last page of the
    // first region the record calls usable", on the reasoning that nothing had
    // been given it. That is false the moment there is an allocator - the frame
    // it landed on was the very root table `install` had just built, and writing
    // `0xa5` over the first entry's low byte turned the device window's table
    // descriptor into a block descriptor. The symptom was a device page that
    // translated to a wrong frame with wrong permissions, which reads like a
    // mapping bug and was a *check* bug. Taking a frame from the allocator is the
    // only way to know a page is nobody else's.
    let Some(scratch_phys) = crate::hal::kernel::alloc_frames(0) else {
        let _ = writeln!(out, "tables: the frame allocator is exhausted");
        super::halt();
    };
    let scratch = crate::hal::phys_to_virt(scratch_phys);
    match paging::translate(scratch) {
        Some((phys, flags, Memory::Normal))
            if phys == scratch_phys && flags == MapFlags::READ | MapFlags::WRITE =>
        {
            // SAFETY: the allocator just handed this frame over, so nothing else
            // holds it; the translation above confirmed it is reachable through
            // the direct map, and it is leaked on purpose - the boot path has no
            // reason to give a frame back.
            unsafe {
                core::ptr::write_volatile(scratch as *mut u8, 0xa5);
                if core::ptr::read_volatile(scratch as *const u8) != 0xa5 {
                    let _ = writeln!(out, "tables: the direct map did not store 0xa5");
                    super::halt();
                }
            }
            let _ = writeln!(
                out,
                "direct map: {scratch:#x} -> {scratch_phys:#x}, rw and it stores"
            );
        }
        other => {
            let _ = writeln!(
                out,
                "tables: the direct map at {scratch:#x} translates to {other:?}, \
                 expected {scratch_phys:#x} as READ|WRITE Normal"
            );
            super::halt();
        }
    }

    // The device window, which is the one range that has to be mapped as device
    // rather than as memory: a PL011 mapped Normal lets the core reorder the
    // stores that are this console's characters. The check translates the
    // console's own registers, so what it proves is that the wire the banner came
    // out of is device memory - and stage 5b moved where that is, so the frame
    // this expects and the address it asks about are two different numbers now.
    match paging::translate(pl011::BASE) {
        Some((phys, flags, Memory::Device))
            if phys == pl011::PHYS && flags == MapFlags::READ | MapFlags::WRITE => {}
        other => {
            let _ = writeln!(
                out,
                "tables: the PL011 at {:#x} translates to {other:?}, \
                 expected {:#x} as READ|WRITE Device",
                pl011::BASE,
                pl011::PHYS
            );
            super::halt();
        }
    }
    let _ = writeln!(
        out,
        "device window: PL011 {:#x} -> {:#x}, Device, through the kernel half at {:#x}",
        pl011::BASE,
        pl011::PHYS,
        paging::DEVICE_VIRT,
    );

    if paging::translate(0).is_some() {
        let _ = writeln!(out, "tables: the null page is mapped");
        super::halt();
    }

    // The low half is *empty*, and this is the check that says so: neither the
    // image's own physical address nor a device's is an address the kernel can
    // reach any more. Two stages got it here - 1e ran on the loader's tables,
    // which mapped all of RAM at its own address, and 1f kept the kernel's out
    // of it, while the device window stayed low - and stage 5b is the one that
    // moved the last of it. `TTBR0` resolves nothing at all now, which is the
    // strongest form the claim has before a process's own tables replace it.
    if let Some((phys, flags, _)) = paging::translate(info.kernel_phys_base) {
        let _ = writeln!(
            out,
            "tables: {:#x} is still mapped identity ({flags:?} -> {phys:#x}); \
             the kernel's tables should have kept the low half empty",
            info.kernel_phys_base
        );
        super::halt();
    }
    if let Some((phys, flags, _)) = paging::translate(pl011::PHYS) {
        let _ = writeln!(
            out,
            "tables: the device window's own address {:#x} is still mapped \
             ({flags:?} -> {phys:#x}); it belongs in the kernel half now",
            pl011::PHYS
        );
        super::halt();
    }
    let _ = writeln!(
        out,
        "no low half: {:#x} is unreachable, and so is the device window at {:#x} \
         that used to live there",
        info.kernel_phys_base,
        paging::DEVICE_PHYS,
    );
}

/// Stage 6a's evidence: the kernel runs code at EL0, and answers what it asks.
///
/// Everything above this stage ran at EL1. What makes EL0 different is not a
/// privilege level as such but *where the address space comes from*: a program's
/// code and stack are in the half `TTBR0` names, the kernel's are in `TTBR1`'s,
/// and the two registers are what this stage is the first to tell apart. So the
/// check is built the way stage 1c's was - the smallest thing that can be wrong
/// in all the ways the mechanism can, and nothing that can only be wrong for a
/// reason the mechanism does not have yet.
///
/// The verdict is read out of a page the program wrote, in [`el0_returned`],
/// rather than by watching the gate: a trap is only observable by the code that
/// answers it, and the honest way to check an answer is from the side that
/// receives it.
fn check_el0() {
    use fmt::Write;

    let mut out = pl011::Writer;
    macro_rules! fail {
        ($($arg:tt)*) => {{
            let _ = writeln!(out, "el0: {}", format_args!($($arg)*));
            super::halt();
        }};
    }

    // A trap from EL0 lands on `SP_EL1` whatever `SPSel` says, so the kernel has
    // to *be* on `SP_EL1` before a program exists: if the firmware left this core
    // on `SP_EL0`, then `SP_EL1` holds nothing and the first trap would push a
    // frame onto whatever was there. Measured and reported rather than assumed,
    // because which one it is is a fact about the handoff this kernel was
    // given - and a silent fix here would hide one the next machine has.
    let spsel: u64;
    // SAFETY: `SPSel` is a readable PSTATE field; reading it has no side effects.
    unsafe { core::arch::asm!("mrs {}, spsel", out(reg) spsel, options(nomem, nostack)) };
    if spsel & 1 == 0 {
        let sp: usize;
        // SAFETY: as above, and writing `SP_EL1` before selecting it is the only
        // order that leaves `sp` valid across the switch.
        unsafe {
            core::arch::asm!("mov {}, sp", out(reg) sp, options(nomem, nostack));
            core::arch::asm!("msr sp_el1, {}", in(reg) sp, options(nomem, nostack));
            core::arch::asm!("msr spsel, #1", options(nomem, nostack));
        }
    }
    let _ = writeln!(
        out,
        "\nEL0: the kernel was on SP_EL{} (SPSel {})",
        if spsel & 1 == 0 { "0" } else { "1" },
        spsel & 1,
    );

    // Three frames: the program's code, its stack, and the page it answers on.
    // Three frames, asked for one at a time rather than through a closure: a
    // closure with `fail!` in it borrows `out` for as long as it lives, and the
    // checks below have things of their own to say.
    let Some(code) = zeroed_frame() else {
        fail!("out of frames for the EL0 program's code page")
    };
    let Some(stack) = zeroed_frame() else {
        fail!("out of frames for the EL0 stack")
    };
    let Some(answer) = zeroed_frame() else {
        fail!("out of frames for the EL0 answer page")
    };

    // Both symbols are placed by the linker in the kernel's own image, and
    // neither is read through - only measured, to copy the instructions between
    // them.
    unsafe extern "C" {
        static __aarch64_el0_program: u8;
        static __aarch64_el0_program_end: u8;
    }
    let program = &raw const __aarch64_el0_program;
    let program_end = &raw const __aarch64_el0_program_end;
    let program_len = program_end as usize - program as usize;
    if program_len > PAGE_SIZE {
        fail!("the EL0 program is {program_len} bytes and it has one page");
    }
    // SAFETY: both ranges are inside the kernel's own image - the program is a
    // `.rodata` block the linker placed - and the destination is a frame just
    // taken from the allocator, so nothing overlaps.
    unsafe {
        core::ptr::copy_nonoverlapping(
            program,
            crate::hal::phys_to_virt(code) as *mut u8,
            program_len,
        );
    }

    let mut space = paging::UserSpace::new();
    space.map(Mapping {
        virt: PROGRAM_VA,
        phys: code,
        length: PAGE_SIZE,
        flags: MapFlags::USER | MapFlags::READ | MapFlags::EXEC,
    });
    space.map(Mapping {
        virt: STACK_VA,
        phys: stack,
        length: PAGE_SIZE,
        flags: MapFlags::USER | MapFlags::READ | MapFlags::WRITE,
    });
    space.map(Mapping {
        virt: ANSWER_VA,
        phys: answer,
        length: PAGE_SIZE,
        flags: MapFlags::USER | MapFlags::READ | MapFlags::WRITE,
    });

    // The space's own tables before the machine is switched to them, because
    // `switch` is what makes a walk report the *user* root: a mapping that never
    // went in would otherwise be a fault at the first instruction rather than a
    // wrong answer here, and the walk is also the only thing that can say which
    // permissions the hardware will actually enforce.
    for (name, virt, frame, flags) in [
        (
            "program",
            PROGRAM_VA,
            code,
            MapFlags::USER | MapFlags::READ | MapFlags::EXEC,
        ),
        (
            "stack",
            STACK_VA,
            stack,
            MapFlags::USER | MapFlags::READ | MapFlags::WRITE,
        ),
        (
            "answer",
            ANSWER_VA,
            answer,
            MapFlags::USER | MapFlags::READ | MapFlags::WRITE,
        ),
    ] {
        match space.translate(virt) {
            Some((phys, got, Memory::Normal)) if phys == frame && got == flags => {}
            other => fail!(
                "the {name} page at {virt:#x} should be {frame:#x} as {flags:?} Normal, \
                 and the space's own tables say {other:?}"
            ),
        }
    }

    ANSWER_FRAME.store(answer, core::sync::atomic::Ordering::Relaxed);
    space.switch();
    let _ = writeln!(
        out,
        "EL0: root {:#x} active, program at {PROGRAM_VA:#x} ({program_len} bytes), \
         stack at {STACK_VA:#x}, answers at {ANSWER_VA:#x}",
        space.root(),
    );

    // Enter through `enter_probe` - the same full register-file restore the
    // `hal::Contexts::enter_user`/`resume_user` path uses - rather than the minimal
    // direct `enter_user`, so this check exercises the production `eret` sequence
    // (load `x0..x30`/`sp`/`pc`/`pstate` from a `UserFrame`) as well as the answer
    // round trip. The registers start zero because the program sets its own; the
    // verdict below is what proves the restore put the CPU into the program.
    let frame = context::UserFrame {
        x: [0; 31],
        sp: (STACK_VA + PAGE_SIZE) as u64,
        pc: PROGRAM_VA as u64,
        pstate: context::PSTATE_EL0T,
    };
    // SAFETY: `frame`'s `pc`/`sp` are mapped in the space just made active, the
    // stack pointer is its page's top, and `SPSel` is on the kernel stack. This
    // is the boot path, single-core, before the scheduler exists - `enter_probe`'
    // one extra condition for returning to this frame instead of halting.
    //
    // And return it does: the way back reads this frame's resume pair and lands
    // on the instruction after its `bl`, which is what lets `boot` fall through
    // to the init handoff like the first machine's does, with no stash and no
    // second entry point. The verdict below is therefore an ordinary call rather
    // than a continuation the gate reaches: the code the program left with comes
    // back as this function's own return value, in `x0`, because the landing is
    // the ABI's own return sequence and not something invented beside it.
    let status = unsafe { context::enter_probe(&frame) };
    el0_returned(status);
    // The probe's three frames stay mapped in the space `space.switch()` made
    // active until pid 1's own space replaces it; nothing at EL0 runs between
    // this return and the handoff except `/init` itself.
}

/// The EL0 probe's verdict: what [`check_el0`] reads back once the program has
/// been given away.
///
/// `status` is what the program passed in `x0`, and it is checked because a gate
/// that lost the argument registers would still deliver *something*. It is an
/// ordinary call from [`check_el0`] rather than a continuation the trap gate
/// reaches: `context::enter_probe`'s landing *is* the ABI's return sequence, so
/// the exit code arrives as its return value and the boot path falls through to
/// the init handoff exactly as the first machine's does - with no stash, no
/// `on_exit` handler and no second entry point.
fn el0_returned(status: u64) {
    use fmt::Write;

    let mut out = pl011::Writer;
    macro_rules! fail {
        ($($arg:tt)*) => {{
            let _ = writeln!(out, "el0: {}", format_args!($($arg)*));
            super::halt();
        }};
    }

    let answer = ANSWER_FRAME.load(core::sync::atomic::Ordering::Relaxed);
    // SAFETY: the frame was allocated by `check_el0` and is still mapped through
    // the direct map, which is in `TTBR1` and reaches it whatever `TTBR0` names.
    let (own, replied) = unsafe {
        let at = crate::hal::phys_to_virt(answer) as *const u64;
        (
            core::ptr::read_volatile(at),
            core::ptr::read_volatile(at.add(1)),
        )
    };

    if status != 7 {
        fail!("the program exited with {status}, and it asked for 7");
    }
    if own != 0xaaaa {
        fail!(
            "the program's own value did not survive the round trip: {own:#x}, \
             and it wrote {:#x}",
            0xaaaau64
        );
    }
    if replied != mpos_err::ENOSYS as u64 {
        fail!(
            "the program read {replied:#x} back from an unknown system call, and \
             this kernel answers {:#x} (ENOSYS)",
            mpos_err::ENOSYS as u64
        );
    }

    let _ = writeln!(
        out,
        "EL0: the program wrote {own:#x}, called a number no system call has, read \
         {replied:#x} back and left with status {status}",
    );
}

// ---------------------------------------------------------------------------
// Stage 6b's check: the operations a process's address space is made of
// ---------------------------------------------------------------------------

/// Where the address-space check's pages go, in the half a process owns.
///
/// Five addresses inside one 2 MiB region, so the whole check walks one hierarchy
/// of tables and a wrong descriptor shows up as a wrong answer rather than as a
/// missing mapping. Named rather than taken from the bottom up, for the same
/// reason [`PROGRAM_VA`] is, and 3 MiB is far enough above zero that a stray
/// pointer is a fault rather than a page somebody else wanted.
const AS_VA_A: usize = 0x0030_0000;
const AS_VA_B: usize = 0x0030_1000;
/// The pair that is mapped *shared* in two spaces, and the reason there are two
/// addresses here rather than one: a shared mapping of one page cannot tell a
/// space that mapped the right frame from one that mapped the wrong one.
const AS_VA_C: usize = 0x0030_2000;
const AS_VA_C2: usize = 0x0030_3000;
/// The page `try_map` is asked for, first with a frame and then with no
/// permission at all.
const AS_VA_D: usize = 0x0030_4000;

/// The pages[`check_user_memory`] maps and walks: a writable page, one that
/// becomes copy-on-write, and the shared pair.
const AS_PAGES: usize = 4;

/// Whether every byte of `frame` is `value`.
///
/// The only way to see what a copy actually copied: a `translate` can only say
/// which frame a mapping points at, and the whole question about a copy-on-write
/// resolve is what the bytes in the new frame are.
fn frame_is(frame: usize, value: u8) -> bool {
    // SAFETY: `frame` is a frame this check took from the allocator and still
    // holds; the direct map reaches it, and no one else is writing it.
    let bytes = unsafe {
        core::slice::from_raw_parts(crate::hal::phys_to_virt(frame) as *const u8, PAGE_SIZE)
    };
    bytes.iter().all(|&byte| byte == value)
}

/// What [`paging::UserSpace::walk_user_pages`] reports for one page of a space,
/// or `None` when the walk does not report that page at all.
///
/// The walk is the only door to the two things the hardware walk does not report:
/// the copy-on-write marker, and a page's absence from a space nothing is
/// translating through.
fn walked_flags(space: &paging::UserSpace, virt: usize) -> Option<MapFlags> {
    let mut found = None;
    space.walk_user_pages(|at, _, flags| {
        if at == virt {
            found = Some(flags);
        }
    });
    found
}

/// Stage 6b's evidence: the operations a process's address space is made of, run
/// against this machine and read back.
///
/// Stage 6a proved the kernel can *enter* a space it built. This is what happens
/// next, and the difference is that every operation here is destructive - a
/// mapping taken back, a permission removed, a frame made read-only so that it
/// can be copied later. A space that can only be filled and never changed is not
/// one a `fork` or a demand-paged `mmap` can be written on.
///
/// Every operation is run and its effect is read back through three different
/// doors, deliberately: the space's *own* tables for what the kernel intends,
/// a store to a user address with that space switched in for what the MMU
/// enforces, and the frame's own bytes through the direct map for what actually
/// happened. A check that only asked the first would pass on descriptors the CPU
/// never walked.
///
/// The copy-on-write half is exercised in the order a `fork` uses it: marking the
/// parent's pages, walking them into a second space, then resolving the child's
/// first write. That is the sequence whose mistakes matter, and it ends by
/// reading the parent's bytes to say that the child's write did not reach them.
fn check_user_memory() {
    use fmt::Write;

    let mut out = pl011::Writer;
    macro_rules! fail {
        ($($arg:tt)*) => {{
            let _ = writeln!(out, "space: {}", format_args!($($arg)*));
            super::halt();
        }};
    }

    // The frames the pages are backed by, each filled with a byte of its own so
    // that a copy which copied the wrong frame - or copied nothing - shows up as
    // a wrong byte rather than as two plausible-looking frames.
    let Some(a) = zeroed_frame() else {
        fail!("out of frames for the address space's own check")
    };
    let Some(b) = zeroed_frame() else {
        fail!("out of frames for the address space's own check")
    };
    // Two contiguous frames in one allocation, which is what the two-page mapping
    // below needs: `Mapping` describes a range, and a range of frames has to be
    // contiguous for one `phys` to describe it.
    let Some(shared) = crate::hal::kernel::alloc_frames(1) else {
        fail!("out of frames for the address space's own check")
    };
    let Some(d) = zeroed_frame() else {
        fail!("out of frames for the address space's own check")
    };
    // SAFETY: four frames the allocator has just handed over and nothing else
    // holds; the direct map reaches all of them, including the second page of
    // `shared`, which the order-1 allocation made contiguous with the first.
    unsafe {
        core::ptr::write_bytes(crate::hal::phys_to_virt(a) as *mut u8, 0x11, PAGE_SIZE);
        core::ptr::write_bytes(crate::hal::phys_to_virt(b) as *mut u8, 0x22, PAGE_SIZE);
        core::ptr::write_bytes(
            crate::hal::phys_to_virt(shared) as *mut u8,
            0x33,
            2 * PAGE_SIZE,
        );
        core::ptr::write_bytes(crate::hal::phys_to_virt(d) as *mut u8, 0x44, PAGE_SIZE);
    }

    let user = MapFlags::USER | MapFlags::READ | MapFlags::WRITE;

    let mut space = paging::UserSpace::new();
    space.map(Mapping {
        virt: AS_VA_A,
        phys: a,
        length: PAGE_SIZE,
        flags: user,
    });
    space.map(Mapping {
        virt: AS_VA_B,
        phys: b,
        length: PAGE_SIZE,
        flags: user,
    });
    // Two pages in one call: the shared pair is one `Mapping` of two pages, which
    // is the shape the plan uses and the only way to exercise the loop that walks
    // a mapping's length.
    space.map(Mapping {
        virt: AS_VA_C,
        phys: shared,
        length: 2 * PAGE_SIZE,
        flags: user,
    });

    // What the kernel means, page by page, read out of the space's *own* tables -
    // the space is not the active one yet, so this says nothing about what the
    // CPU would do with these addresses. The store further down is that half.
    for (virt, phys, flags) in [
        (AS_VA_A, a, user),
        (AS_VA_B, b, user),
        (AS_VA_C, shared, user),
        (AS_VA_C2, shared + PAGE_SIZE, user),
    ] {
        match space.translate(virt) {
            Some((got, got_flags, Memory::Normal)) if got == phys && got_flags == flags => {}
            other => fail!(
                "the space's own tables put {virt:#x} at {other:?}, and it was mapped \
                 {phys:#x} as {flags:?} Normal"
            ),
        }
    }
    if space.phys_for_virt(AS_VA_A) != Some(a) {
        fail!(
            "phys_for_virt({AS_VA_A:#x}) is {:?}, and the frame mapped there is {a:#x}",
            space.phys_for_virt(AS_VA_A)
        );
    }

    // The walk a `fork` is written on: every mapped page, lowest first. A walk
    // that missed a page would leave the child with a hole its parent cannot see,
    // and one that reported the pages in a different order would be a second rule
    // where the trait describes one.
    let mut walked = [(0usize, 0usize, MapFlags::empty()); AS_PAGES];
    let mut total = 0usize;
    space.walk_user_pages(|virt, phys, flags| {
        if total < walked.len() {
            walked[total] = (virt, phys, flags);
        }
        total += 1;
    });
    if total != AS_PAGES {
        fail!("the walk saw {total} pages, and {AS_PAGES} are mapped");
    }
    for (index, want) in [
        (AS_VA_A, a, user),
        (AS_VA_B, b, user),
        (AS_VA_C, shared, user),
        (AS_VA_C2, shared + PAGE_SIZE, user),
    ]
    .into_iter()
    .enumerate()
    {
        if walked[index] != want {
            fail!(
                "the walk's page {index} is {:?}, and the space maps {want:?} there",
                walked[index]
            );
        }
    }

    // An address that is already spoken for, answered rather than asserted: this
    // is the shape a `MAP_FIXED` request arrives in, and the reason `try_map`
    // exists apart from `map`.
    if space
        .try_map(Mapping {
            virt: AS_VA_A,
            phys: d,
            length: PAGE_SIZE,
            flags: user,
        })
        .is_ok()
    {
        fail!("try_map put a second frame over {AS_VA_A:#x}, which already had one");
    }
    // And a free address, with a page that has *no* permission at all - which is
    // what `PROT_NONE` is, and which this machine expresses as a page EL1 may
    // reach and EL0 may not. That a user space accepts such a mapping is the
    // whole reason `map` does not require `USER`.
    let unreachable = MapFlags::READ;
    if space
        .try_map(Mapping {
            virt: AS_VA_D,
            phys: d,
            length: PAGE_SIZE,
            flags: unreachable,
        })
        .is_err()
    {
        fail!("try_map refused {AS_VA_D:#x}, which nothing had mapped");
    }
    match space.translate(AS_VA_D) {
        Some((got, flags, Memory::Normal)) if got == d && flags == unreachable => {}
        other => fail!(
            "a page with no permission at {AS_VA_D:#x} came back {other:?}, and \
             {unreachable:?} was asked for"
        ),
    }

    // Permissions changed in place, with the frame left where it was: a range
    // that is protected and then unprotected keeps its bytes, which is what makes
    // `mprotect` mean something rather than only `munmap`.
    space.protect(AS_VA_A, PAGE_SIZE, MapFlags::USER | MapFlags::READ);
    match space.translate(AS_VA_A) {
        Some((got, flags, Memory::Normal))
            if got == a && flags == MapFlags::USER | MapFlags::READ => {}
        other => fail!(
            "protecting {AS_VA_A:#x} read-only left it {other:?}, and the frame should \
             still be {a:#x}"
        ),
    }
    space.protect(AS_VA_A, PAGE_SIZE, user);

    // The copy-on-write marker, set without a flush and then flushed once: the
    // pair a `fork` uses, because a flush per page would ask the CPU to re-walk
    // pages the process is about to touch anyway.
    space.set_cow_bit_quietly(AS_VA_A);
    space.flush_if_active();
    match space.translate(AS_VA_A) {
        // Read-only as far as the CPU is concerned, and that bit is load-bearing:
        // a copy-on-write page left writable would never fault, so it would never
        // be copied, and the two spaces would share one frame after all.
        Some((got, flags, Memory::Normal))
            if got == a && flags == MapFlags::USER | MapFlags::READ => {}
        other => fail!(
            "a copy-on-write page at {AS_VA_A:#x} reads back as {other:?} to the \
             hardware walk, and it has to be read-only for the write to fault"
        ),
    }
    match walked_flags(&space, AS_VA_A) {
        Some(flags) if flags == MapFlags::USER | MapFlags::READ | MapFlags::COW => {}
        other => fail!(
            "the walk reports {other:?} for the copy-on-write page at {AS_VA_A:#x}, and \
             a fork has to see its marker to know it is waiting to be copied"
        ),
    }
    // `protect` replaces a mapping's flags rather than merging them, so the
    // marker goes the way the caller's set says - which is also the only way to
    // take one off again, and the reason it is checked rather than assumed.
    space.protect(AS_VA_A, PAGE_SIZE, user);
    if walked_flags(&space, AS_VA_A) != Some(user) {
        fail!(
            "protecting {AS_VA_A:#x} writable again left the walk reporting {:?}",
            walked_flags(&space, AS_VA_A)
        );
    }
    // The same marker through the form that flushes on its own, on the page the
    // fork below is about.
    space.set_cow_bit(AS_VA_B);
    match walked_flags(&space, AS_VA_B) {
        Some(flags) if flags == MapFlags::USER | MapFlags::READ | MapFlags::COW => {}
        other => fail!("marking {AS_VA_B:#x} copy-on-write read back as {other:?}"),
    }

    // The `fork`, by hand and in the order one does it: walk the parent, map
    // every page into the child copy-on-write, then resolve the child's first
    // write. Every frame the two spaces share is still shared at this point, and
    // neither space may write any of them.
    let mut child = paging::UserSpace::new();
    space.walk_user_pages(|virt, phys, flags| child.map_cow(virt, phys, flags));
    match walked_flags(&child, AS_VA_B) {
        Some(flags) if flags == MapFlags::USER | MapFlags::READ | MapFlags::COW => {}
        other => fail!(
            "the child's copy of {AS_VA_B:#x} came back {other:?}, and a fork has to \
             mark what it shares"
        ),
    }
    let resolved = child.resolve_cow(AS_VA_B);
    let Some((old, new)) = resolved else {
        fail!("resolving a write to the child's {AS_VA_B:#x} found no copy-on-write page")
    };
    if old != b {
        fail!("the child's {AS_VA_B:#x} was backed by {old:#x}, and the parent maps {b:#x}");
    }
    if new == b {
        fail!("resolving the write handed back {new:#x}, which is the frame it shared");
    }
    if !frame_is(new, 0x22) {
        fail!("the child's own frame at {new:#x} does not hold the bytes that were copied");
    }
    if !frame_is(b, 0x22) {
        fail!(
            "the parent's frame at {b:#x} changed, and the child's write was supposed to \
             copy rather than share"
        );
    }
    match child.translate(AS_VA_B) {
        Some((got, flags, Memory::Normal)) if got == new && flags == user => {}
        other => fail!(
            "after resolving, the child's {AS_VA_B:#x} reads back {other:?}, and it \
             should be {new:#x} as {user:?} Normal"
        ),
    }
    if child.resolve_cow(AS_VA_B).is_some() {
        fail!(
            "a second resolve of the child's {AS_VA_B:#x} copied a page that is no \
               longer copy-on-write"
        );
    }
    // The parent's mapping and the parent's bytes, both unchanged: the child's
    // write reached neither, which is the whole of what copy-on-write promises.
    match space.translate(AS_VA_B) {
        Some((got, flags, Memory::Normal))
            if got == b && flags == MapFlags::USER | MapFlags::READ => {}
        other => fail!(
            "the parent's {AS_VA_B:#x} reads back {other:?} after the child resolved its \
             write, and it should still be {b:#x} and still read-only"
        ),
    }

    // What the MMU enforces, as opposed to what the walk reports: make the space
    // the active one and store to a user address from EL1. `AP[1]` says EL0 may
    // reach this page, and on this machine that is also what lets EL1 reach it -
    // so the store lands only if the descriptor the CPU is walking says writable,
    // which no software walk can decide.
    space.switch();
    // SAFETY: `space` is the active address space, so `AS_VA_A` is mapped by the
    // tables `TTBR0_EL1` names right now, and `user` leaves it writable.
    unsafe { core::ptr::write_volatile(AS_VA_A as *mut u8, 0x5a) };
    // SAFETY: the frame is the check's own and the direct map reaches it.
    let stored = unsafe { core::ptr::read_volatile(crate::hal::phys_to_virt(a) as *const u8) };
    if stored != 0x5a {
        fail!(
            "a store through the active mapping of {AS_VA_A:#x} left {stored:#x} in the \
             frame it maps, and the MMU was supposed to enforce that mapping"
        );
    }
    // SAFETY: as above, and restoring the byte so the frame reads as it did.
    unsafe { core::ptr::write_volatile(crate::hal::phys_to_virt(a) as *mut u8, 0x11) };

    // A *shared* mapping: the same two frames in a second space, writable in both
    // and with no marker - which is the difference `MAP_SHARED` turns on, and the
    // reason such a mapping must not be copy-on-write. Both spaces map the same
    // frames, and neither owns them exclusively.
    let mut other = paging::UserSpace::new();
    other.map_shared(AS_VA_C, shared, user);
    other.map_shared(AS_VA_C2, shared + PAGE_SIZE, user);
    if walked_flags(&other, AS_VA_C) != Some(user) {
        fail!(
            "the shared page at {AS_VA_C:#x} came back {:?}, and a shared mapping is not \
             copy-on-write",
            walked_flags(&other, AS_VA_C)
        );
    }
    match other.resolve_cow(AS_VA_C) {
        None => {}
        Some((old, copied)) => fail!(
            "resolving a write to the shared page at {AS_VA_C:#x} copied {old:#x} to \
             {copied:#x}, and a shared mapping has to be written in place"
        ),
    }
    // The store is what makes it shared rather than merely equal: it goes through
    // the *second* space's address and comes back out of the frame the *first*
    // space maps.
    other.switch();
    // SAFETY: `other` is the active address space and `AS_VA_C` is mapped
    // writable in it.
    unsafe { core::ptr::write_volatile(AS_VA_C as *mut u8, 0x66) };
    // SAFETY: `shared` is a frame the check owns and the direct map reaches it.
    let seen = unsafe { core::ptr::read_volatile(crate::hal::phys_to_virt(shared) as *const u8) };
    if seen != 0x66 {
        fail!(
            "a store through the second space's mapping of {AS_VA_C:#x} left {seen:#x} \
             in the frame the first space maps there"
        );
    }
    if space.phys_for_virt(AS_VA_C2) != Some(shared + PAGE_SIZE) {
        fail!(
            "the two spaces map the second shared page to different frames: {:?} and \
             {:#x}",
            space.phys_for_virt(AS_VA_C2),
            shared + PAGE_SIZE
        );
    }

    // Taking a mapping back: the frames come to the caller and the address stops
    // translating - from the walk and, because this space was the active one when
    // the mapping went, from the machine.
    let dropped = space.unmap(AS_VA_C, 2 * PAGE_SIZE);
    if dropped != [shared, shared + PAGE_SIZE] {
        fail!(
            "unmapping the shared pair gave back {dropped:x?}, and {shared:#x} and {:#x} \
             were mapped there",
            shared + PAGE_SIZE
        );
    }
    if space.translate(AS_VA_C).is_some() || space.translate(AS_VA_C2).is_some() {
        fail!("the shared pair still translates after being unmapped");
    }
    let mut left = 0usize;
    space.walk_user_pages(|_, _, _| left += 1);
    // `AS_PAGES` was the four the check started with; the page `try_map` added is
    // the fifth, and the two that were unmapped are the shared pair - so A, B and
    // D are what a walk should still find. The arithmetic is the check's and it
    // was wrong on the first run, which is the point of counting rather than
    // trusting the unmapped addresses alone.
    if left != AS_PAGES - 1 {
        fail!(
            "after unmapping two pages the walk sees {left}, and {} are left",
            AS_PAGES - 1
        );
    }

    // `Drop`, and the measurement that makes it a check rather than a claim: a
    // space's tables go back to the allocator and the frames its leaves point at
    // do *not*. The count is the instrument because both mistakes are off by a
    // specific number of frames - a leak by the four this space's tables cost, a
    // wrong free by the one page it mapped.
    //
    // The space is made the active one first, because that is the dangerous
    // order: a `Drop` that freed the tables before leaving would have the CPU
    // walking frames the allocator had already taken back. The address is
    // `AS_VA_A` again on purpose - the same address in a different space is what
    // an address space *is*.
    let before = crate::hal::kernel::frames_free();
    let tables;
    {
        let mut scratch = paging::UserSpace::new();
        scratch.map(Mapping {
            virt: AS_VA_A,
            phys: d,
            length: PAGE_SIZE,
            flags: user,
        });
        scratch.switch();
        // What the space's own tables take, measured while it is alive and after
        // the page above was mapped: the mapped frame is not part of this number,
        // which is the whole reason the number can be checked.
        tables = before - crate::hal::kernel::frames_free();
    }
    let after = crate::hal::kernel::frames_free();
    if tables == 0 {
        fail!("a user address space's tables cost no frames, so nothing was measured");
    }
    if after != before {
        fail!(
            "dropping a space moved the free frame count from {before} to {after} \
             ({tables} frames were its tables); its tables should come back and the \
             page it mapped should not"
        );
    }

    let _ = writeln!(
        out,
        "\nspace: {AS_PAGES} pages mapped, walked in order and read back through the \
         MMU; two protected, one copy-on-write and resolved into a frame of its own \
         ({new:#x}) with the parent's {b:#x} left alone; two shared between spaces and \
         written through one of them; two unmapped and given back",
    );
    let _ = writeln!(
        out,
        "space: dropping an active space gave back all {tables} frames its tables took, \
         and the page it mapped not among them",
    );
}

// ---------------------------------------------------------------------------
// Stage 6c's check: the address-space questions that need a running process
// ---------------------------------------------------------------------------

/// The pages [`check_paging_ops`] walks through the machine's active-root and
/// named-root operations, in a `TTBR0` span that collides with neither 6a's
/// program (`0x20_0000`) nor 6b's address-space set (`0x30_0000`).
const PG_VA_A: usize = 0x0040_0000;
const PG_VA_B: usize = 0x0040_1000;
/// Deliberately left unmapped, so "nothing there" is distinguishable from
/// "there but refused".
const PG_VA_UNMAPPED: usize = 0x0040_2000;

/// Stage 6c's evidence: the half of `hal::Paging` that reads the space the CPU is
/// running *right now*, and the half that walks a space named by its root rather
/// than installed. Stage 6b built what a `UserSpace` does to itself; a scheduler
/// and a signal/uaccess path (stages 3 and 6) ask different questions - which
/// root is active, which is the kernel's, is this user range touchable, is this
/// page copy-on-write, put a word into a task that is not running. None of those
/// existed until now, and none is one the compiler would catch wrong: they are
/// each answered by re-reading the hardware state they claim to report.
fn check_paging_ops() {
    use fmt::Write;

    let mut out = pl011::Writer;
    macro_rules! fail {
        ($($arg:tt)*) => {{
            let _ = writeln!(out, "paging: {}", format_args!($($arg)*));
            super::halt();
        }};
    }

    let user = MapFlags::USER | MapFlags::READ | MapFlags::WRITE;

    let Some(a) = zeroed_frame() else {
        fail!("out of frames for the running-process paging check")
    };
    let Some(b) = zeroed_frame() else {
        fail!("out of frames for the running-process paging check")
    };
    // SAFETY: two frames the allocator just handed over and nothing else holds.
    unsafe { core::ptr::write_bytes(crate::hal::phys_to_virt(a) as *mut u8, 0x5a, PAGE_SIZE) };

    let mut space = paging::UserSpace::new();
    space.map(Mapping {
        virt: PG_VA_A,
        phys: a,
        length: PAGE_SIZE,
        flags: user,
    });
    space.map(Mapping {
        virt: PG_VA_B,
        phys: b,
        length: PAGE_SIZE,
        flags: user,
    });
    let root = space.root();
    space.switch();

    // The active root is the installed space's and the kernel's is a different
    // one: the two registers this machine only learned to tell apart at stage 6a.
    if paging::active_root() != root {
        fail!(
            "active_root() reports {:#x}, not the installed {root:#x}",
            paging::active_root()
        );
    }
    if paging::kernel_root() == root {
        fail!("kernel_root() and active_root() are the same space ({root:#x})");
    }

    // Walking a root the caller names - the shape a `fork` and a signal frame are
    // written on - user-gated and not, and the unmapped page answered as absent.
    if paging::phys_for_virt_of(root, PG_VA_A) != Some(a) {
        fail!(
            "phys_for_virt_of({PG_VA_A:#x}) is {:?}, expected {a:#x}",
            paging::phys_for_virt_of(root, PG_VA_A)
        );
    }
    if paging::user_phys_for_virt_of(root, PG_VA_A) != Some(a) {
        fail!(
            "user_phys_for_virt_of refused a USER page: {:?}",
            paging::user_phys_for_virt_of(root, PG_VA_A)
        );
    }
    if paging::phys_for_virt_of(root, PG_VA_UNMAPPED).is_some() {
        fail!("phys_for_virt_of named a frame at the unmapped {PG_VA_UNMAPPED:#x}");
    }

    // uaccess: a mapped page for read and for write; an unmapped page refused; and
    // a kernel-half address refused *because* a user pointer is a TTBR0 address by
    // definition - a check that only asked the hardware would let the direct map
    // answer true.
    if !paging::user_accessible(PG_VA_A, PAGE_SIZE, true) {
        fail!("user_accessible refused a writable write on a USER|RW page");
    }
    if !paging::user_accessible(PG_VA_A, PAGE_SIZE, false) {
        fail!("user_accessible refused a read on a USER|RW page");
    }
    if paging::user_accessible(PG_VA_UNMAPPED, PAGE_SIZE, false) {
        fail!("user_accessible allowed an unmapped page");
    }
    if paging::user_accessible(crate::hal::direct_map_offset(), PAGE_SIZE, false) {
        fail!("user_accessible allowed a kernel-half (direct-map) address");
    }

    // The copy-on-write marker is invisible to the CPU's walk and visible to this
    // one. Marking a page copy-on-write makes it read-only *in the tables*, and
    // the distinction that matters is that this is not the same thing as taking
    // the write away from the process: the store is answered by the fault handler,
    // which splits the page, so uaccess must still call it writable. This check
    // used to assert the opposite - that a COW page stops being writable - and
    // that expectation was the bug's other half: it is exactly what made the
    // second machine's range check refuse a buffer a `fork` child reads into, and
    // the first machine has always answered the other way (`write_user_u32` and
    // `user_accessible` in `arch/x86_64/paging.rs`, both of which read
    // `is_cow_page`). Measured: a shell's `read` in a fork child answered `EFAULT`
    // on this machine and does not on x86_64 (`NOTES.md` §13.9).
    if paging::is_cow_page(PG_VA_A) {
        fail!("a freshly mapped page reports itself copy-on-write");
    }
    space.set_cow_bit(PG_VA_A);
    if !paging::is_cow_page(PG_VA_A) {
        fail!("a page marked copy-on-write is not reported as one");
    }
    if !paging::user_accessible(PG_VA_A, PAGE_SIZE, true) {
        fail!(
            "a copy-on-write page is reported unwritable, but its write is the fault \
             the handler resolves"
        );
    }
    space.protect(PG_VA_A, PAGE_SIZE, user);
    if paging::is_cow_page(PG_VA_A) {
        fail!("protecting the page writable again left the copy-on-write marker set");
    }

    // A word written into a space through its root, not through a live switch -
    // read back from the frame through the direct map, because that is the only
    // door that shows the store actually landed rather than returned `true`.
    if !paging::write_user_u32(root, PG_VA_B, 0xdead_beef) {
        fail!("write_user_u32 refused a mapped user page");
    }
    // SAFETY: `b` is a frame this check owns and the direct map reaches.
    let wrote = unsafe { core::ptr::read(crate::hal::phys_to_virt(b) as *const u32) };
    if wrote != 0xdead_beef {
        fail!("write_user_u32 put {wrote:#x} in the frame, not 0xdeadbeef");
    }
    if paging::write_user_u32(root, PG_VA_UNMAPPED, 1) {
        fail!("write_user_u32 wrote to an unmapped address");
    }

    // Leaving the process's space: `TTBR0` goes back to the kernel's root, the
    // space stops being active, and its pages stop resolving from the low half -
    // and `switch_to_root` re-installs a complete low half on the way back.
    paging::switch_to_kernel();
    if paging::active_root() != paging::kernel_root() {
        fail!("switch_to_kernel left active_root != kernel_root");
    }
    if paging::translate(PG_VA_A).is_some() {
        fail!("the user page {PG_VA_A:#x} still translated after switch_to_kernel");
    }
    // SAFETY: `root` is this space's complete, mapped low half.
    unsafe { paging::switch_to_root(root) };
    if paging::active_root() != root {
        fail!("switch_to_root did not make {root:#x} the active space");
    }

    // Drop the space back on the kernel's own root, so the next check (and 6a's
    // EL0 entry) starts from the no-process state rather than a half-built space.
    paging::switch_to_kernel();
    let _ = writeln!(
        out,
        "paging: active/kernel roots told apart; a named-root walk user-gated and \
         unmapped-refused; uaccess read+write vs unmapped vs the kernel half; the \
         copy-on-write marker the CPU cannot see; a u32 written through a space's \
         root and read back from its frame {b:#x}; leaving to the kernel and \
         re-installing",
    );
}

/// Stage 6d's evidence: the machine has a persistent clock, and this kernel
/// reaches it - the hardware half of `crate::hal::WallClock`, which until now had
/// no aarch64 source at all and is one of the two things that kept `hal` from
/// compiling here.
///
/// The claim that could silently be wrong is "the register answers across the
/// device window," so the check does not trust the constant: it programs an epoch
/// into the PL031 and reads the *device* back, which fails if `BASE` were a
/// number nothing maps rather than the window's alias of `0x0901_0000`. It also
/// pins the two answers the module owes a caller: an epoch past the 32-bit
/// counter is refused rather than wrapped to 1970, and the software anchor - which
/// is what `boot_epoch` really reads on a machine with no shadowed host clock -
/// moves when asked.
fn check_wallclock() {
    use fmt::Write;

    let mut out = pl011::Writer;
    macro_rules! fail {
        ($($arg:tt)*) => {{
            let _ = writeln!(out, "wall clock: {}", format_args!($($arg)*));
            super::halt();
        }};
    }

    // Reading the counter through the kernel-half window must yield a live clock.
    // The plausible-epoch band is the pin that distinguishes "the driver reached
    // the PL031" from "it read zeroes, or the wrong alias, or unmapped memory": a
    // counter that returns a date is the device answering, and a host-shadowed RTC
    // on a machine booted this decade sits well inside it.
    pl031::init();
    let anchored = pl031::boot_epoch();
    if !(1_000_000_000..4_300_000_000).contains(&anchored) {
        fail!("boot_epoch() read {anchored:#x} from the PL031 - not a live UTC epoch");
    }
    if pl031::read_epoch() < anchored {
        fail!("the counter went backwards from the boot anchor");
    }

    // The counter is a *writable* clock, which is what `/dev/rtc0`'s `RTC_SET_TIME`
    // and `hwclock -w` need. It is written to a value no boot would read (1979) so
    // that "the write landed" cannot be satisfied by what was already there, read
    // back *from the device*, and put back - the same leave-it-as-you-found-it rule
    // the software anchor above follows, and the reason the restore is checked: a
    // check that left this counter at 1979 would hand the machine a wrong clock,
    // which is exactly the defect this check's neighbours were found through.
    // `landed` allows the counter to have ticked between the load and the read
    // (the load sets what `DR` reads now, and the clock does not stop for it) -
    // a second of slack, against writes that differ from what was there by
    // decades, so nothing can be hidden by it.
    let landed = |got: u64, want: u64| got == want || got == want + 1;
    let wanted: u64 = 0x1234_5678;
    if !pl031::write_epoch(wanted) {
        fail!("write_epoch({wanted:#x}) refused a value the 32-bit counter can hold");
    }
    if !landed(pl031::read_epoch(), wanted) {
        fail!(
            "the counter reads {:#x} after write_epoch({wanted:#x})",
            pl031::read_epoch()
        );
    }
    let past_the_end: u64 = u32::MAX as u64 + 1;
    if pl031::write_epoch(past_the_end) {
        fail!("write_epoch({past_the_end:#x}) claimed a value the counter cannot hold");
    }
    if !pl031::write_epoch(anchored) || !landed(pl031::read_epoch(), anchored) {
        fail!(
            "the counter was not put back: {:#x}, expected {anchored:#x}",
            pl031::read_epoch()
        );
    }

    // The in-memory anchor still moves, the way `settimeofday` does on both
    // machines, and that is what `boot_epoch` answers from. It is moved to a
    // value that is *not* a plausible epoch (`1 << 40` seconds into it, a year
    // past thirty-one thousand) so that "the setter moved it" cannot be
    // satisfied by the value that was already there - and then it is put
    // **back**, because this check runs on every aarch64 boot, in front of
    // whatever the machine is about to run: the test sweep, or a distribution's
    // own `init`. A check that leaves the clock anchored at `1 << 40` breaks the
    // thing it is proving works.
    //
    // Measured, and how this was found: the Alpine boot's `syslogd` died at EL0
    // in musl's `asctime_r` - the branch musl implements as `a_crash()` -
    // because the timestamp it formatted was `36812-02-20`, and that year is
    // exactly this constant read back as seconds since the epoch. `t-localtime`
    // reads the same value on both boot shapes.
    let anchor: u64 = 0x0000_0100_0000_0000;
    pl031::set_boot_epoch(anchor);
    if pl031::boot_epoch() != anchor {
        fail!(
            "boot_epoch() is {:#x} after set_boot_epoch({anchor:#x})",
            pl031::boot_epoch()
        );
    }
    // Back to the epoch the counter really reads, and checked rather than
    // assumed: a restore that silently failed would be the same damage with a
    // quieter cause.
    pl031::set_boot_epoch(anchored);
    if pl031::boot_epoch() != anchored {
        fail!(
            "the boot anchor was not restored: {:#x}, expected {anchored:#x}",
            pl031::boot_epoch()
        );
    }

    let _ = writeln!(
        out,
        "wallclock: PL031 reached at {:#x} through the kernel half; boot anchored to \
         a live UTC epoch ({anchored:#x}); the counter loads and reads back, so \
         write_epoch persists; the software anchor moves on set_boot_epoch",
        pl031::BASE
    );
}

/// Stage 6e's evidence: the CPU and interrupt-controller primitives `hal::Cpu`
/// and `hal::Interrupts` need on this machine and did not have - enabling user
/// SIMD, masking a line, and parking on an interrupt. Each is read back through
/// the register the write moved, not assumed: FP access from `CPACR_EL1`, the
/// distributor's own set-enable bit, and the clock's tick count across `idle`.
fn check_cpu() {
    use fmt::Write;

    let mut out = pl011::Writer;
    macro_rules! fail {
        ($($arg:tt)*) => {{
            let _ = writeln!(out, "cpu: {}", format_args!($($arg)*));
            super::halt();
        }};
    }

    // `enable_user_simd` must give EL0 the FPU, or a user program's first FP
    // instruction traps. The proof is `CPACR_EL1.FPEN` reading back as full access.
    vectors::enable_user_simd();
    let fpen = (vectors::cpacr() >> 20) & 0b11;
    if fpen != 0b11 {
        fail!("enable_user_simd left CPACR_EL1.FPEN at {fpen:#b}, not 0b11");
    }

    // Masking must reach the distributor. Use the serial's line - wired but idle -
    // so the test cannot disturb the clock: enable, read the bit set, disable,
    // read it clear. A `disable` that silently did nothing shows up here rather
    // than as a line that keeps interrupting a driver that asked it to stop.
    vectors::disable_interrupts();
    gic::enable(gic::SERIAL_INTID);
    if !gic::is_enabled(gic::SERIAL_INTID) {
        fail!("enable(SERIAL_INTID) did not set the distributor's enable bit");
    }
    gic::disable(gic::SERIAL_INTID);
    if gic::is_enabled(gic::SERIAL_INTID) {
        fail!("disable(SERIAL_INTID) left the distributor's enable bit set");
    }

    // `idle` must be woken by an interrupt and return with them masked - the two
    // halves of its contract. Re-arm the clock for the next period and park on it:
    // the tick count rising says something woke it, and `interrupts_enabled`
    // false after says it closed the door on the way out.
    timer::arm_next_period();
    let before = timer::ticks();
    vectors::idle();
    if timer::ticks() == before {
        fail!("idle() returned without any interrupt waking it");
    }
    if vectors::interrupts_enabled() {
        fail!("idle() returned with interrupts still enabled");
    }

    // And the one-shot must wake between two ticks - the half of the sub-tick
    // contract a pin cannot see from a syscall. Arm the virtual timer two
    // milliseconds out - half a period, so a tick cannot be the thing that
    // answered - park on it, and read the figures back: woken it must be, and
    // with the tick count unmoved, or this machine's `SUB_TICK_WAKE` is a lie
    // every sleep would pay for.
    let wake_ns = timer::since_boot_nanos() + 2_000_000;
    timer::arm_wake_ns(Some(wake_ns));
    let before = timer::ticks();
    vectors::idle();
    let fired = timer::ticks() == before;
    if !fired {
        let ctl = timer::vtimer_control();
        let state = gic::state();
        fail!(
            "the virtual timer's one-shot did not wake idle() two milliseconds \
             out (ticks moved to {}, ctl {:#06x} = enable {} imask {} istatus {}, \
             cval {:#x}, count {:#x}, base {:#x}, gic pending {:#x} enabled {:#x} \
             hpplr {:#x})",
            timer::ticks(),
            ctl,
            ctl & 1,
            (ctl >> 1) & 1,
            (ctl >> 2) & 1,
            timer::vtimer_compare(),
            timer::vtimer_count(),
            timer::vtimer_base(),
            state.highest_pending,
            state.enabled0,
            state.highest_pending
        );
    }
    if vectors::interrupts_enabled() {
        fail!("the one-shot's idle() returned with interrupts still enabled");
    }

    let _ = writeln!(
        out,
        "cpu: CPACR.FPEN full access after enable_user_simd; a distributor line \
         enabled then masked and read back; idle() woken by the clock (tick \
         {before} -> {}) and returning with interrupts off",
        timer::ticks()
    );
}

/// Stage 7's first evidence: the console's *receive* half actually reads.
///
/// Every marker above this one is about transmit - the boot has been talking, not
/// listening. An interactive shell needs the other direction, and a headless boot
/// cannot type into the wire, so the check drives the PL011's own loopback (`LBE`,
/// which ties TX back to RX inside the UART and takes the wire output off): a
/// written probe byte must come out of the receive FIFO. That exercises the exact
/// registers the read path uses (`FR.RXFE` for "is there a byte", `DR` for the
/// byte) without needing a terminal, and it would fail if RX were misread - the
/// counterfactual a pin needs.
fn check_serial_rx() {
    use fmt::Write;

    let mut out = pl011::Writer;
    let probe = b'x';
    if !pl011::loopback_echoes(probe) {
        let _ = writeln!(
            out,
            "serial rx: loopback probe {probe:#04x} did not read back through RX"
        );
        super::halt();
    }
    let _ = writeln!(out, "serial rx: loopback echoed the probe through RXFE/DR");
}

/// Stage 6h's evidence: the scheduler's thread-pointer write reaches the
/// machine's register and comes back, exercised through the `hal::Cpu` capability
/// rather than a raw `msr`.
///
/// The register is `TPIDR_EL0`, the base an EL0 program's TLS relocations add
/// to: x86_64's `%fs` base. `sched` used to write it with a local `wrmsr`. That
/// made the module name an x86 machine register, which blocked the aarch64
/// un-gate. The capability that replaces it has to be proven to move the
/// register, or the un-gate would rest on a `set_thread_pointer` that silently
/// did nothing.
///
/// The proof is the read-back, not the write. A sentinel goes in through
/// `hal::set_thread_pointer` and `hal::thread_pointer` must hand the same value
/// out. This runs before [`check_el0`] enters EL0, so a failure is a terminator
/// (`halt`) and the success marker is not one, the discipline the other stage
/// checks keep. The register is restored to zero afterward so the sentinel does
/// not leak into the EL0 program, which starts with no thread pointer.
fn check_tls() {
    use fmt::Write;

    let mut out = pl011::Writer;
    macro_rules! fail {
        ($($arg:tt)*) => {{
            let _ = writeln!(out, "tls: {}", format_args!($($arg)*));
            super::halt();
        }};
    }

    // A value that is not a plausible real pointer, so a stuck-at-zero or
    // stuck-at-garbage register cannot accidentally match it.
    const SENTINEL: usize = 0xdead_beef_0000_1234;
    crate::hal::set_thread_pointer(SENTINEL);
    let got = crate::hal::thread_pointer();
    if got != SENTINEL {
        fail!("set_thread_pointer({SENTINEL:#x}) then thread_pointer() read {got:#x}");
    }

    // Clear it back: a fresh EL0 image starts with no thread pointer, and this
    // check must not hand the next stage a sentinel it never asked for.
    crate::hal::set_thread_pointer(0);
    if crate::hal::thread_pointer() != 0 {
        fail!("clearing the thread pointer did not take - a stale base would follow into EL0");
    }

    let _ = writeln!(
        out,
        "tls: set_thread_pointer({SENTINEL:#x}) read back through thread_pointer(); \
         cleared to 0"
    );
}

/// Stage 1c's evidence: the generic timer's interrupt reaches the vector table,
/// is acknowledged on the GIC, counted, retired, and returned from - at the rate
/// the machine's own counter says it should.
///
/// The wait is the counter's, not the clock's, and that is the point of the
/// check. The failure it must be able to report is "no interrupt ever arrived",
/// and waiting on ticks - or on `wfi` - would hang on exactly that failure, with
/// nothing printed. Spinning on `CNTPCT_EL0` gives the window an engine that
/// does not need the thing being tested, so a broken vector table ends as a
/// number that is wrong rather than as silence.
///
/// It runs after the tables go in, so the tick it counts is one taken through
/// the kernel's own address space: a window that still counts to 25 is a window
/// where translation, attributes and interrupts all agree.
fn check_timer() {
    use fmt::Write;

    let mut out = pl011::Writer;
    let _ = writeln!(
        out,
        "timer: CNTFRQ_EL0 {} Hz, tick at {} Hz ({} counts apart)",
        timer::frequency(),
        timer::TICK_HZ,
        timer::period()
    );

    super::enable_interrupts();
    if !super::interrupts_enabled() {
        let _ = writeln!(out, "timer: DAIF.I is still set after enabling");
        super::halt();
    }

    const WINDOW_MS: u64 = 100;
    let expected = WINDOW_MS * timer::TICK_HZ as u64 / 1000;
    // A window's worth of ticks is not a round number of periods - the first
    // one lands partway through - so the check is the rate, not an equality.
    // Two ticks of slack, against a measurement of exactly 25 in three runs of
    // three: the slack is for the first tick's phase, not for the clock.
    let tolerance = 2;

    let before = timer::ticks();
    timer::busy_wait_ms(WINDOW_MS);
    let got = timer::ticks() - before;

    if !(expected - tolerance..=expected + tolerance).contains(&got) {
        // Name what the machine says, not just what went wrong: a timer that
        // never fires has three different causes down one wire (see
        // `gic::State`), and this is the only place they can be told apart.
        let gic = gic::state();
        let _ = writeln!(
            out,
            "timer: {got} interrupts in {WINDOW_MS} ms, expected ~{expected} \
             at {} Hz",
            timer::TICK_HZ
        );
        let _ = writeln!(
            out,
            "  timer: CTRL {:#x} (enabled {}, masked {}, fired {}), CNTPCT {}",
            timer::control(),
            timer::control() & 1,
            (timer::control() >> 1) & 1,
            (timer::control() >> 2) & 1,
            timer::counter(),
        );
        let _ = writeln!(
            out,
            "  gic:   CTLR {:#x}, ISENABLER0 {:#x}, CPU CTLR {:#x}, PMR {:#x}, HPPIR {}",
            gic.dist_control, gic.enabled0, gic.cpu_control, gic.priority_mask, gic.highest_pending,
        );
        super::halt();
    }

    let _ = writeln!(
        out,
        "timer: {got} interrupts in {WINDOW_MS} ms (~{expected} at {} Hz), \
         {} ns of counter",
        timer::TICK_HZ,
        timer::since_boot_nanos()
    );
}
