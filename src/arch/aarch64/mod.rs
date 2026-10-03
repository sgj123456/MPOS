//! aarch64 machine-dependent code.
//!
//! Everything in here may touch aarch64 specifics; nothing outside `arch::` may.
//!
//! This is the second machine (`PLAN.md` §9.1). Stage 1a landed the build
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
//! constants the rest of this file is compiled with (`check_devices`); and
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

pub(crate) mod context;
pub(crate) mod critical;
pub(crate) mod gic;
pub(crate) mod paging;
pub(crate) mod pl011;
pub(crate) mod timer;
pub(crate) mod vectors;

use core::arch::global_asm;
use core::fmt;
use core::sync::atomic::{AtomicUsize, Ordering};

use crate::boot::BootInfo;
use crate::boot::handoff::Handoff;
use crate::fdt;
use crate::mm::vm::{self, KernelSections, MapFlags, Mapping};
use crate::mm::{self, PAGE_SIZE};

use paging::Memory;

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
/// # Safety
///
/// The caller is the loader's branch to `_start`, with `x0` holding the record
/// it wrote and every address that record names mapped for as long as the kernel
/// runs. Nothing chooses this call from Rust, and the pointer is read before it
/// can be validated, because the magic check inside `collect` happens *after*
/// the read: its validity is the caller's contract and not something this
/// function can establish.
pub unsafe fn boot(handoff: *const Handoff) -> ! {
    pl011::init();

    // SAFETY: the loader's contract - `handoff` points at the record it wrote,
    // still mapped at the address it put it, and every physical range that
    // record names is reachable through the direct map it chose.
    let info = unsafe { crate::boot::handoff::collect(handoff) };
    banner(handoff as usize, &info);

    // The clock this machine's kernel wants is the EL1 physical timer, and the
    // vector table and GIC are configured for EL1 as well; a machine that
    // started this image at EL2 would fault or, worse, arm a timer nothing
    // ever reads. Which level the entry arrives at is the firmware's decision
    // (`PLAN.md` §9.1's risk list), so it is checked rather than assumed - and
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

    // Stage 5a: the machine's own description of itself, held up against the
    // constants the rest of this file is compiled with. Before the allocator
    // and before the kernel's own tables, because it needs neither - the tree
    // is in the loader's direct map, which is the address space the kernel is
    // still running in.
    check_devices(&info);

    // SAFETY: the record's contract, unchanged - every region it names is live
    // memory the kernel may hand out, and the loader's copy of it is the only
    // description of this machine there is.
    let stats = unsafe { mm::init(&info) };
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
    check_paging(&info, &sections, frames);
    check_virtio(&info);
    check_timer();
    check_context();
    check_user_memory();
    check_el0();
    halt()
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
fn check_devices(info: &BootInfo) {
    use fmt::Write;

    let mut out = pl011::Writer;

    /// News this machine cannot act on: say which claim failed and stop. The
    /// marker is what `xtask boot-aarch64` polls for, so a check that fails
    /// ends the run rather than waiting out the deadline.
    macro_rules! fail {
        ($($arg:tt)*) => {{
            let _ = writeln!(out, "M4_DEVICES_FAILED: {}", format_args!($($arg)*));
            halt();
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

    // The timer's line, decoded the way its binding says: a triple per
    // interrupt - shared or per-CPU, the number in that space, and the trigger
    // - whose first cell decides the offset, since an SPI counts from 32 and a
    // PPI from 16. This kernel's clock is the EL1 physical timer, which is PPI
    // 14 and therefore INTID 30; the whole of that sentence is what the check
    // holds the tree to.
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
    }
    let Some((kind, number)) = timer_line else {
        fail!(
            "the tree's timer is not on INTID {}, which is the line this kernel's \
             clock is armed on",
            gic::TIMER_INTID
        );
    };
    let _ = writeln!(
        out,
        "devices: PL011 at {serial_base:#x}, GIC at {dist:#x}/{cpu:#x}, timer on {} \
         {number} (INTID {})",
        if kind == 1 { "PPI" } else { "SPI" },
        gic::TIMER_INTID,
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
    let _ = writeln!(out, "M4_DEVICES_OK");
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
                "M2_CONTEXT_OK: {hand_overs} hand-overs, both directions, every \
                 callee-saved register and v31 back where its task left it",
            );
        }
        Err(bad) => {
            let _ = writeln!(
                out,
                "M2_CONTEXT_FAILED: {bad} registers did not survive a hand-over"
            );
            halt();
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
fn check_paging(info: &BootInfo, sections: &KernelSections, frames: usize) {
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

    if !paging::enabled() {
        let _ = writeln!(out, "M3_TABLES_FAILED: SCTLR_EL1.M is clear");
        halt();
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
                        "M3_TABLES_FAILED: {page:#x} translates to {other:?}, but the plan \
                         says {:?} -> {want:#x}",
                        mapping.flags
                    );
                    halt();
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
    let Some(scratch_phys) = mm::alloc_frames(0) else {
        let _ = writeln!(out, "M3_TABLES_FAILED: the frame allocator is exhausted");
        halt();
    };
    let scratch = mm::phys_to_virt(scratch_phys);
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
                    let _ = writeln!(out, "M3_TABLES_FAILED: the direct map did not store 0xa5");
                    halt();
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
                "M3_TABLES_FAILED: the direct map at {scratch:#x} translates to {other:?}, \
                 expected {scratch_phys:#x} as READ|WRITE Normal"
            );
            halt();
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
                "M3_TABLES_FAILED: the PL011 at {:#x} translates to {other:?}, \
                 expected {:#x} as READ|WRITE Device",
                pl011::BASE,
                pl011::PHYS
            );
            halt();
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
        let _ = writeln!(out, "M3_TABLES_FAILED: the null page is mapped");
        halt();
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
            "M3_TABLES_FAILED: {:#x} is still mapped identity ({flags:?} -> {phys:#x}); \
             the kernel's tables should have kept the low half empty",
            info.kernel_phys_base
        );
        halt();
    }
    if let Some((phys, flags, _)) = paging::translate(pl011::PHYS) {
        let _ = writeln!(
            out,
            "M3_TABLES_FAILED: the device window's own address {:#x} is still mapped \
             ({flags:?} -> {phys:#x}); it belongs in the kernel half now",
            pl011::PHYS
        );
        halt();
    }
    let _ = writeln!(
        out,
        "no low half: {:#x} is unreachable, and so is the device window at {:#x} \
         that used to live there",
        info.kernel_phys_base,
        paging::DEVICE_PHYS,
    );

    let _ = writeln!(out, "M3_TABLES_OK");
}

/// The virtio-MMIO identity registers: the four words a transport answers before
/// any driver has touched it.
///
/// They are the whole reason this machine can be asked *which* of its windows
/// carry a device. QEMU's `virt` publishes thirty-two `virtio,mmio` nodes
/// whether or not anything is attached to them, and that is measured rather than
/// assumed: the tree decompiled with and without `-device virtio-rng-device`
/// differs only in the firmware's own random seeds, byte for byte. So the tree
/// cannot say, and the transport's own registers can.
/// `MagicValue`: the four letters `virt` as one little-endian word.
const VIRTIO_MMIO_MAGIC: usize = 0x000;
/// `Version`: which register layout this transport speaks.
const VIRTIO_MMIO_VERSION: usize = 0x004;
/// `DeviceID`: zero when no device is attached to the transport.
const VIRTIO_MMIO_DEVICE_ID: usize = 0x008;
/// `VendorID`: whose device it is, which the specification leaves to the
/// implementation.
const VIRTIO_MMIO_VENDOR_ID: usize = 0x00c;
/// `MagicValue`: the four ASCII letters `virt`, read as one little-endian word.
const VIRTIO_MAGIC: u32 = 0x7472_6976;
/// `Version`: 2 is the modern (1.0) transport, which is the only register layout
/// the driver crate reads. Version 1 is the legacy one, whose registers are at
/// different offsets and which has no device-feature words at all.
const VIRTIO_VERSION_MODERN: u32 = 2;

/// One 32-bit read of a device's registers, through the kernel's own tables.
fn read_device(virt: usize, offset: usize) -> u32 {
    // SAFETY: `virt` came from `paging::device_virt`, which only answers for a
    // window wholly inside the range `install` mapped as Device and never
    // executable, and every `offset` below is one of four words inside the
    // 512-byte stride a virtio-mmio transport owns.
    unsafe { core::ptr::read_volatile((virt + offset) as *const u32) }
}

/// Stage 5b's evidence: the kernel's own tables reach the machine's transports,
/// and the transports answer as transports.
///
/// Stage 5a held the tree up against constants, and 5b's first half put the
/// window those devices live in into the kernel half; what neither could say is
/// whether there is really a *device* on the other side of an address. This
/// reads four registers through the window and checks the two the specification
/// fixes, so a window with nothing behind it - a tree naming a hole, a mapping
/// at the wrong physical address, an attribute that turned the reads into
/// something else entirely - ends as a named failure here rather than as a
/// driver that times out three stages from now.
///
/// The two numbers it does *not* check are checked against nothing on purpose:
/// `DeviceID` is 0 on a transport with no device behind it (which is what most
/// of a bare `virt` machine's thirty-two windows are, and is legal), and
/// `VendorID` is the implementation's to choose. Both are reported, because
/// which device answers is the fact the next stage needs.
///
/// It runs after `check_paging`, and that is the point of it: every address
/// below is in the kernel half, and the walk is what proves the page tables
/// reach the window rather than that the window exists.
fn check_virtio(info: &BootInfo) {
    use fmt::Write;

    let mut out = pl011::Writer;

    /// News this machine cannot act on: say which claim failed and stop, as
    /// `check_devices` does and for the same reason - the marker is what
    /// `xtask boot-aarch64` polls for.
    macro_rules! fail {
        ($($arg:tt)*) => {{
            let _ = writeln!(out, "M5_VIRTIO_FAILED: {}", format_args!($($arg)*));
            halt();
        }};
    }

    let Some(bytes) = info.dtb else {
        fail!("the record carries no device tree, so the machine cannot say where its devices are");
    };
    let tree = match fdt::Fdt::new(bytes) {
        Ok(tree) => tree,
        Err(error) => fail!("the device tree will not walk: {error:?}"),
    };

    // What the walk collects, as numbers rather than as text: the closure
    // `Fdt::each` calls cannot borrow `out`, so nothing can be reported from
    // inside it and every failure has to survive until the walk is over.
    // `check_devices` walks the same tree through the same shape.
    let mut windows = 0usize;
    let mut attached = 0usize;
    let mut first: Option<(usize, u32, u32)> = None; // address, device id, vendor id
    let mut outside: Option<(usize, usize)> = None; // address, size
    let mut wrong_magic: Option<(usize, u32, u32)> = None; // address, what it said, version
    let mut wrong_version: Option<(usize, u32)> = None; // address, what it said

    let walked = tree.each(
        |node| node.is_compatible("virtio,mmio"),
        |node| {
            windows += 1;
            for (address, size) in node.reg() {
                let (address, size) = (address as usize, size as usize);
                let Some(virt) = paging::device_virt(address, size) else {
                    outside = Some((address, size));
                    continue;
                };
                let magic = read_device(virt, VIRTIO_MMIO_MAGIC);
                let version = read_device(virt, VIRTIO_MMIO_VERSION);
                let device = read_device(virt, VIRTIO_MMIO_DEVICE_ID);
                let vendor = read_device(virt, VIRTIO_MMIO_VENDOR_ID);
                if magic != VIRTIO_MAGIC {
                    // The first offender wins, not the last: a machine with one
                    // window in the wrong place is easier to read off a boot log
                    // than the thirty-second one to be asked.
                    wrong_magic.get_or_insert((address, magic, version));
                } else if version != VIRTIO_VERSION_MODERN {
                    wrong_version.get_or_insert((address, version));
                } else if device != 0 {
                    attached += 1;
                    if first.is_none() {
                        first = Some((address, device, vendor));
                    }
                }
            }
            true
        },
    );
    if let Err(error) = walked {
        fail!("the device tree will not walk: {error:?}");
    }
    if let Some((address, size)) = outside {
        fail!(
            "the tree's virtio window at {address:#x}..{:#x} is not inside the \
             {:#x}..{:#x} device window this kernel maps",
            address + size,
            paging::DEVICE_PHYS,
            paging::DEVICE_PHYS + paging::DEVICE_WINDOW,
        );
    }
    if let Some((address, magic, version)) = wrong_magic {
        fail!(
            "the transport at {address:#x} answered {magic:#x} for MagicValue (with \
             version {version}), and a virtio-mmio transport answers {VIRTIO_MAGIC:#x}"
        );
    }
    if let Some((address, version)) = wrong_version {
        fail!(
            "the transport at {address:#x} answers version {version}, and this \
             kernel's driver reads the modern (version {VIRTIO_VERSION_MODERN}) layout"
        );
    }
    if windows == 0 {
        fail!("the tree describes no virtio-mmio transport at all");
    }

    let _ = writeln!(
        out,
        "virtio: {windows} transports, every one answering virtio 1.0 through the \
         kernel half; {attached} {} a device",
        if attached == 1 { "carries" } else { "carry" },
    );
    if let Some((address, device, vendor)) = first {
        let _ = writeln!(
            out,
            "virtio: the first is at {address:#x}, device {device}, vendor {vendor:#x}"
        );
    }
    let _ = writeln!(out, "M5_VIRTIO_OK");
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
const ANSWER_VA: usize = 0x0022_0000;

/// The frame the answer page is, so [`el0_returned`] can read it.
///
/// A static because the two halves of this check do not share a stack frame:
/// [`check_el0`] enters EL0 and never returns, and the continuation is reached
/// through the trap gate, with nothing of the caller's to pass anything in.
static ANSWER_FRAME: AtomicUsize = AtomicUsize::new(0);

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
global_asm!(
    ".section .rodata.el0_program, \"a\"",
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
    answer = const ANSWER_VA as u64,
    exit_group = const 94u64,
);

unsafe extern "C" {
    static __aarch64_el0_program: u8;
    static __aarch64_el0_program_end: u8;
}

/// A fresh, zeroed frame from the frame allocator.
///
/// Zeroed for the reason the page tables' own frames are: a program that reads a
/// page it never wrote should read zeroes, and a frame straight from the
/// allocator holds whatever the last user left in it.
fn zeroed_frame() -> Option<usize> {
    let frame = mm::alloc_frames(0)?;
    // SAFETY: the allocator handed this frame over and nothing else holds it,
    // and the direct map reaches it.
    unsafe { core::ptr::write_bytes(mm::phys_to_virt(frame) as *mut u8, 0, PAGE_SIZE) };
    Some(frame)
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
            let _ = writeln!(out, "M6_EL0_FAILED: {}", format_args!($($arg)*));
            halt();
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
        core::ptr::copy_nonoverlapping(program, mm::phys_to_virt(code) as *mut u8, program_len);
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

    ANSWER_FRAME.store(answer, Ordering::Relaxed);
    space.switch();
    let _ = writeln!(
        out,
        "EL0: root {:#x} active, program at {PROGRAM_VA:#x} ({program_len} bytes), \
         stack at {STACK_VA:#x}, answers at {ANSWER_VA:#x}",
        space.root(),
    );

    // SAFETY: all three addresses are mapped in the space that was just made
    // active, and the stack pointer is its page's top - one past the end, which
    // is where a stack pointer belongs.
    unsafe { context::enter_user(PROGRAM_VA, STACK_VA + PAGE_SIZE, el0_returned) }
}

/// Where the EL0 program's `exit_group` lands: the kernel, on the stack
/// [`check_el0`] was using, once the program has been given away.
///
/// `status` is what the program passed in `x0`, and it is checked because a gate
/// that lost the argument registers would still deliver *something*.
extern "C" fn el0_returned(status: u64) -> ! {
    use fmt::Write;

    let mut out = pl011::Writer;
    macro_rules! fail {
        ($($arg:tt)*) => {{
            let _ = writeln!(out, "M6_EL0_FAILED: {}", format_args!($($arg)*));
            halt();
        }};
    }

    let answer = ANSWER_FRAME.load(Ordering::Relaxed);
    // SAFETY: the frame was allocated by `check_el0` and is still mapped through
    // the direct map, which is in `TTBR1` and reaches it whatever `TTBR0` names.
    let (own, replied) = unsafe {
        let at = mm::phys_to_virt(answer) as *const u64;
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
    if replied != crate::err::ENOSYS as u64 {
        fail!(
            "the program read {replied:#x} back from an unknown system call, and \
             this kernel answers {:#x} (ENOSYS)",
            crate::err::ENOSYS as u64
        );
    }

    let _ = writeln!(
        out,
        "EL0: the program wrote {own:#x}, called a number no system call has, read \
         {replied:#x} back and left with status {status}",
    );
    let _ = writeln!(out, "M6_EL0_OK");
    halt()
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
    let bytes =
        unsafe { core::slice::from_raw_parts(mm::phys_to_virt(frame) as *const u8, PAGE_SIZE) };
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
            let _ = writeln!(out, "M6_AS_FAILED: {}", format_args!($($arg)*));
            halt();
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
    let Some(shared) = mm::alloc_frames(1) else {
        fail!("out of frames for the address space's own check")
    };
    let Some(d) = zeroed_frame() else {
        fail!("out of frames for the address space's own check")
    };
    // SAFETY: four frames the allocator has just handed over and nothing else
    // holds; the direct map reaches all of them, including the second page of
    // `shared`, which the order-1 allocation made contiguous with the first.
    unsafe {
        core::ptr::write_bytes(mm::phys_to_virt(a) as *mut u8, 0x11, PAGE_SIZE);
        core::ptr::write_bytes(mm::phys_to_virt(b) as *mut u8, 0x22, PAGE_SIZE);
        core::ptr::write_bytes(mm::phys_to_virt(shared) as *mut u8, 0x33, 2 * PAGE_SIZE);
        core::ptr::write_bytes(mm::phys_to_virt(d) as *mut u8, 0x44, PAGE_SIZE);
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
    let stored = unsafe { core::ptr::read_volatile(mm::phys_to_virt(a) as *const u8) };
    if stored != 0x5a {
        fail!(
            "a store through the active mapping of {AS_VA_A:#x} left {stored:#x} in the \
             frame it maps, and the MMU was supposed to enforce that mapping"
        );
    }
    // SAFETY: as above, and restoring the byte so the frame reads as it did.
    unsafe { core::ptr::write_volatile(mm::phys_to_virt(a) as *mut u8, 0x11) };

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
    let seen = unsafe { core::ptr::read_volatile(mm::phys_to_virt(shared) as *const u8) };
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
    let before = mm::frame_stats().free;
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
        tables = before - mm::frame_stats().free;
    }
    let after = mm::frame_stats().free;
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
    let _ = writeln!(out, "M6_AS_OK");
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

/// Let the core take interrupts.
pub fn enable_interrupts() {
    vectors::enable_interrupts()
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

    enable_interrupts();
    if !interrupts_enabled() {
        let _ = writeln!(out, "M1_TIMER_FAILED: DAIF.I is still set after enabling");
        halt();
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
            "M1_TIMER_FAILED: {got} interrupts in {WINDOW_MS} ms, expected ~{expected} \
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
        halt();
    }

    let _ = writeln!(
        out,
        "M1_TIMER_OK: {got} interrupts in {WINDOW_MS} ms (~{expected} at {} Hz), \
         {} ns of counter",
        timer::TICK_HZ,
        timer::since_boot_nanos()
    );
}

/// Say who this is, what it was handed, and get out of the way.
///
/// Deliberately the same shape as the x86_64 banner (`src/main/x86_64.rs`), so
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
/// starts at EL2 (`PLAN.md` §9.1's risk list). Printing it here is the cheapest
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
