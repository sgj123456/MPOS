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
pub(crate) mod pl031;
pub(crate) mod signal;
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
/// `milestone` is a function the caller (the `main` boot wiring) hands in, run
/// once the PL011 is up so the machine's own log has reached the wire. It exists
/// because `crate::console` is above `arch` and `arch` may not name it
/// (`AGENTS.md` §4, downward-only) - yet the console service is exactly what
/// stage 4a un-gated for this machine, and a milestone check for it belongs in
/// the boot wiring, not the machine layer. So `main` supplies a `fn()` that
/// writes through `crate::console`, and `arch` only ever invokes the pointer:
/// the console name stays on the `main` side of the seam.
///
/// # Safety
///
/// The caller is the loader's branch to `_start`, with `x0` holding the record
/// it wrote and every address that record names mapped for as long as the kernel
/// runs. Nothing chooses this call from Rust, and the pointer is read before it
/// can be validated, because the magic check inside `collect` happens *after*
/// the read: its validity is the caller's contract and not something this
/// function can establish.
pub unsafe fn boot(handoff: *const Handoff, milestone: fn(), init: fn(*const u8, usize) -> !) -> ! {
    pl011::init();

    // SAFETY: the loader's contract - `handoff` points at the record it wrote,
    // still mapped at the address it put it, and every physical range that
    // record names is reachable through the direct map it chose.
    let info = unsafe { crate::boot::handoff::collect(handoff) };
    banner(handoff as usize, &info);

    // Stash the init handoff for `el0_returned` to run once the EL0 milestone probe
    // is done. It cannot be an ordinary fall-through: `enter_frame` is `-> !` and
    // control returns to EL1 only inside `el0_returned`, which has no caller frame
    // to return to. What the handoff needs from the record is the initramfs archive
    // - already a direct-mapped `'static` slice read out of `info` here - and NOT
    // the raw record pointer: `collect` runs before `paging::install` swaps in the
    // kernel's own tables, and the identity address the loader handed us is not in
    // them, so re-reading the record after the probe faults. The pointer+length of
    // the direct-mapped archive survive the switch because the direct map does.
    match info.initramfs {
        Some(bytes) => {
            INITRAMFS_PTR.store(bytes.as_ptr() as usize, Ordering::Relaxed);
            INITRAMFS_LEN.store(bytes.len(), Ordering::Relaxed);
        }
        None => {
            INITRAMFS_PTR.store(0, Ordering::Relaxed);
            INITRAMFS_LEN.store(0, Ordering::Relaxed);
        }
    }
    POST_EL0.store(init as usize, Ordering::Relaxed);

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

    // The console service (`crate::console`, un-gated for this machine at stage
    // 4a) has never been on the wire yet - every line above this was the machine's
    // own `pl011::Writer`. `main`'s `milestone` writes one through the service and
    // reports it; `arch` only calls the pointer, so the console name stays in the
    // wiring layer that may hold it.
    milestone();

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
    check_rng(&info);
    check_blk(&info);
    check_fs(&info);
    check_net(&info);
    check_input(&info);
    check_timer();
    check_context();
    check_user_memory();
    check_paging_ops();
    check_wallclock();
    check_cpu();
    check_serial_rx();
    check_tls();
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
/// `DeviceID` for the entropy device (`VIRTIO_ID_RNG`). The value the
/// specification fixes for the one device this machine drives first over MMIO -
/// measured: the `virtio-rng-device` QEMU attaches to a `virt` MMIO transport
/// answers `DeviceID` 4 (vendor "QEMU"), and the driver crate's `DeviceType::Rng`
/// is the same 4, so `VirtIORng::new` accepts the window this selects.
const VIRTIO_ID_RNG: u32 = 4;
/// `DeviceID` for the block device (`VIRTIO_ID_BLOCK`), the second device driven
/// over MMIO - the disk root the drivers stage names (`PLAN.md` §9.1).
const VIRTIO_ID_BLOCK: u32 = 2;
/// `DeviceID` for the network adapter (`VIRTIO_ID_NET`), the third device driven
/// over MMIO - the `net` stage's transport proof on this machine (`PLAN.md` §9.1).
const VIRTIO_ID_NET: u32 = 1;
/// `DeviceID` for an input device (`VIRTIO_ID_INPUT`), the fourth transport the
/// aarch64 MMIO virtio path is proven to reach (`PLAN.md` §9.1).
const VIRTIO_ID_INPUT: u32 = 18;

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

/// Stage 7's first driven device: virtio-rng over the memory-mapped transport.
///
/// `check_virtio` proved the kernel's page tables reach the machine's transports
/// and that they answer as transports; this opens the one window the tree says
/// carries an entropy device (`DeviceID` 4) as a *real virtio device* over MMIO
/// and asks it for bytes - the drivers stage's first device driven to a marker
/// (`PLAN.md` §9.1). The window is handed to the driver as a kernel-virtual
/// address, so `drivers::virtio::mmio` names neither the tree nor the boot
/// record: discovery is the machine layer's job, the driver's job is a register
/// block it is given. Success is 32 host-generated bytes back; a window that is
/// not an RNG, a device that answers short, or none at all is a named
/// `M7_RNG_FAILED` - never a zeroed buffer reported as entropy.
fn check_rng(info: &BootInfo) {
    use fmt::Write;

    let mut out = pl011::Writer;

    /// Say which claim failed and stop; the marker is what `xtask boot-aarch64`
    /// polls for, as `check_virtio` does.
    macro_rules! fail {
        ($($arg:tt)*) => {{
            let _ = writeln!(out, "M7_RNG_FAILED: {}", format_args!($($arg)*));
            halt();
        }};
    }

    let Some(bytes) = info.dtb else {
        fail!("the record carries no device tree");
    };
    let tree = match fdt::Fdt::new(bytes) {
        Ok(tree) => tree,
        Err(error) => fail!("the device tree will not walk: {error:?}"),
    };

    // The entropy device's window as (kernel-virtual header, size). Collected,
    // not opened inside the closure: `Fdt::each`'s callback cannot borrow `out`.
    let mut rng: Option<(usize, usize)> = None;
    let walked = tree.each(
        |node| node.is_compatible("virtio,mmio"),
        |node| {
            if rng.is_some() {
                return true;
            }
            for (address, size) in node.reg() {
                let (address, size) = (address as usize, size as usize);
                let Some(virt) = paging::device_virt(address, size) else {
                    continue;
                };
                if read_device(virt, VIRTIO_MMIO_MAGIC) == VIRTIO_MAGIC
                    && read_device(virt, VIRTIO_MMIO_DEVICE_ID) == VIRTIO_ID_RNG
                {
                    rng = Some((virt, size));
                    break;
                }
            }
            true
        },
    );
    if let Err(error) = walked {
        fail!("the device tree will not walk: {error:?}");
    }
    let Some((virt, size)) = rng else {
        fail!("the tree describes no virtio-rng (device {VIRTIO_ID_RNG}) window");
    };

    // Install the device as the machine's entropy source *before* checking it, so
    // a successful read also leaves `crate::rand`'s seed able to mix hardware
    // bytes (the boot calls `rand::init` next, which reads this global). A window
    // that maps but does not open as a virtio-rng is a named failure, not a silent
    // fall-through to a weaker seed.
    let Some(dev) = crate::drivers::virtio::mmio::init_global(virt, size) else {
        fail!("the window at {virt:#x} mapped but did not open as a virtio-rng transport");
    };
    let mut buf = [0u8; 32];
    let got = dev.fill(&mut buf);
    if got != buf.len() {
        fail!(
            "the virtio-rng returned {got} of {} entropy bytes",
            buf.len()
        );
    }
    // A sum over the returned bytes so the log proves they are the device's, not
    // a buffer left zero: a request that came back empty of entropy still fills
    // nothing, and only the `got == 32` guard above would hide that.
    let sum: u32 = buf.iter().map(|&b| b as u32).sum();
    let _ = writeln!(
        out,
        "virtio-rng over MMIO installed, returned 32 bytes (sum {sum})"
    );
    let _ = writeln!(out, "M7_RNG_OK");
}

/// Every `virtio,mmio` window this machine maps whose `DeviceID` is `want`, as
/// `(kernel-virtual header, size)`. QEMU assigns the MMIO slots in an order a
/// port cannot rely on, so a caller that owns a specific image picks its window by
/// what it contains (a sector pattern, a filesystem that mounts) rather than by
/// position. `Err` if the record carries no tree, or the tree will not walk.
fn virtio_device_windows(
    info: &BootInfo,
    want: u32,
) -> Result<alloc::vec::Vec<(usize, usize)>, &'static str> {
    let bytes = info.dtb.ok_or("the record carries no device tree")?;
    let tree = fdt::Fdt::new(bytes).map_err(|_| "the device tree will not walk")?;
    let mut windows = alloc::vec::Vec::new();
    tree.each(
        |node| node.is_compatible("virtio,mmio"),
        |node| {
            for (address, size) in node.reg() {
                let (address, size) = (address as usize, size as usize);
                let Some(virt) = paging::device_virt(address, size) else {
                    continue;
                };
                if read_device(virt, VIRTIO_MMIO_MAGIC) == VIRTIO_MAGIC
                    && read_device(virt, VIRTIO_MMIO_DEVICE_ID) == want
                {
                    windows.push((virt, size));
                }
            }
            true
        },
    )
    .map_err(|_| "the device tree will not walk")?;
    Ok(windows)
}

/// Stage 7's second driven device over MMIO: virtio-blk, verified by a read.
///
/// `check_rng` proved a request device answers; a block device is the one the
/// disk root needs, and it is driven the same way - open the window as an
/// `MmioTransport`, hand it to the crate's `VirtIOBlk`. The pin is a *verified*
/// read: the raw image xtask lays down has a sector whose byte `i` is `i as u8`,
/// and with two block devices attached (this one and the ext4 one `check_fs`
/// mounts) the pattern is picked by content, not position - the window whose
/// sector 0 matches is this image. Capacity is read too (a config-space path).
fn check_blk(info: &BootInfo) {
    use fmt::Write;

    let mut out = pl011::Writer;

    /// Say which claim failed and stop; the marker is what `xtask boot-aarch64`
    /// polls for.
    macro_rules! fail {
        ($($arg:tt)*) => {{
            let _ = writeln!(out, "M7_BLK_FAILED: {}", format_args!($($arg)*));
            halt();
        }};
    }

    let windows = match virtio_device_windows(info, VIRTIO_ID_BLOCK) {
        Ok(windows) => windows,
        Err(message) => fail!("{message}"),
    };
    if windows.is_empty() {
        fail!("the tree describes no virtio-blk (device {VIRTIO_ID_BLOCK}) window");
    }

    // Pick the raw pattern image by content: read each block window's sector 0 and
    // keep the one that carries the `i as u8` pattern. The ext4 image's sector 0 is
    // a boot-sector of zeros, so it is not mistaken for this one.
    let mut buf = [0u8; crate::drivers::virtio::mmio::SECTOR];
    let mut chosen: Option<(usize, usize)> = None;
    for &(virt, size) in &windows {
        if crate::drivers::virtio::mmio::read_blocks(virt, size, &mut buf).is_some()
            && buf.iter().enumerate().all(|(i, &b)| b == i as u8)
        {
            chosen = Some((virt, size));
            break;
        }
    }
    let Some((virt, size)) = chosen else {
        fail!("no virtio-blk window returned the known pattern sector");
    };

    // Read the device's geometry too - a configuration-space path distinct from
    // the data read above. The raw image xtask lays down is 1 MiB = 2048 sectors,
    // so the capacity the device reports must agree; a transport that read a
    // sector but misreports its size would mount a filesystem over the wrong span.
    let sectors = crate::drivers::virtio::mmio::capacity(virt, size).unwrap_or(0);
    if sectors != 2048 {
        fail!("the virtio-blk reported capacity {sectors}, not the 2048 sectors of the image");
    }
    let _ = writeln!(
        out,
        "virtio-blk over MMIO read a verified sector ({sectors} sectors)"
    );
    // The write path, not just the read: hand a distinctive sector to the device,
    // flush, and read it back through a fresh open. A round trip that does not
    // match means the write virtqueue or flush did not land the buffer - the other
    // half of the data path `read_blocks` never exercised. Sector 3 is chosen so it
    // does not disturb the pattern sector 0 the read above already validated.
    let mut wbuf = [0u8; crate::drivers::virtio::mmio::SECTOR];
    for (i, b) in wbuf.iter_mut().enumerate() {
        *b = (i ^ 0xa5) as u8;
    }
    if crate::drivers::virtio::mmio::write_readback(virt, size, 3, &wbuf).is_none() {
        fail!("the virtio-blk MMIO write path did not round-trip a sector");
    }
    let _ = writeln!(out, "virtio-blk over MMIO wrote and read back a sector");
    let _ = writeln!(out, "M7_BLK_OK");
}

/// Stage 7's disk-root primitive: a filesystem mounted over the MMIO block.
///
/// `check_blk` proved raw sector reads travel the memory-mapped transport; this
/// proves the whole `block driver -> filesystem` path by mounting an ext4 over one
/// of the block windows. With two block devices attached (the pattern image and an
/// ext4 image xtask lays down), the ext4 one is picked by trying each: `Ext4Fs::mount`
/// reads the superblock and group descriptors through the `BlockDev`, and only the
/// image that is actually an ext4 mounts. A filesystem that mounts is the disk root
/// the drivers stage names - reading a file through it and booting a root from it
/// are the task-context follow-on (`PLAN.md` §9.1). `M7_FS_FAILED` if none mounts.
fn check_fs(info: &BootInfo) {
    use fmt::Write;

    let mut out = pl011::Writer;

    macro_rules! fail {
        ($($arg:tt)*) => {{
            let _ = writeln!(out, "M7_FS_FAILED: {}", format_args!($($arg)*));
            halt();
        }};
    }

    let windows = match virtio_device_windows(info, VIRTIO_ID_BLOCK) {
        Ok(windows) => windows,
        Err(message) => fail!("{message}"),
    };
    // Reading a file through the mounted filesystem uses the `FileSystem`/`Inode`
    // trait surface directly (root -> lookup -> read_at); no fd table or current
    // task is involved, so it runs at boot.
    use crate::vfs::{FileSystem, Inode};
    let mut opened = false;
    for &(virt, size) in &windows {
        let Some(dev) = crate::drivers::virtio::mmio::blockdev(virt, size) else {
            continue;
        };
        // `mount` only returns Ok for the window whose bytes are an ext4 the driver
        // can open; the raw pattern image is a clean `Err` and is skipped.
        let Ok(fs) = crate::vfs::ext4::Ext4Fs::mount(dev.clone()) else {
            continue;
        };
        // The disk is registered with the block layer *after* the mount proved
        // the bytes, so the raw pattern image and any non-ext4 window cannot take
        // the name. That registration is what makes the device a kernel disk:
        // the devfs build publishes `/dev/vda` from `blk::disks()`, and the init
        // handoff mounts this same device through the VFS (`PLAN.md` §13.3's disk
        // root). The major is Linux's for virtio-blk, so `/proc/partitions` and
        // `lsblk`-shaped readers see the name they expect.
        crate::blk::add_disk("vda", 254, "/dev/vda", dev);
        // Read the one file the image carries, back through the block driver.
        let Ok(hello) = fs.root().lookup("hello") else {
            fail!("ext4 mounted but /hello was not found through it");
        };
        let mut buf = [0u8; 64];
        let Ok(n) = hello.read_at(0, &mut buf) else {
            fail!("reading /hello through the mounted ext4 failed");
        };
        let known: &[u8] = b"mpos reads from ext4 over virtio-MMIO\n";
        if &buf[..n] != known {
            fail!("the bytes read from /hello ({n}) did not match the file's contents");
        }
        opened = true;
        break;
    }
    if !opened {
        fail!("no virtio-blk window over MMIO mounted as ext4 and served a file");
    }
    let _ = writeln!(
        out,
        "ext4 mounted over the MMIO virtio-blk device, read /hello (36 bytes)"
    );
    let _ = writeln!(out, "M7_FS_OK");
}

/// Stage 8's net device: virtio-net over the memory-mapped transport, opened as
/// the adapter the *stack* will drive.
///
/// The net adapter is the third device driven the same way - find the window the
/// tree says carries one (`DeviceID` 1), open it as an `MmioTransport`, hand it
/// to the crate's `VirtIONet`. Two claims come out of the open, and both are about
/// the device the network stack is then given rather than about a throwaway one:
/// the 6-byte MAC is present and non-zero (a `virt` MMIO adapter always carries
/// the MAC QEMU assigned), and a frame handed to the transmit queue is accepted -
/// the rings move buffers over the mapped transport, the same class of proof the
/// block milestone made by reading a real sector back. It still claims nothing
/// about a packet reaching a network: that is the stack, and the programs that
/// drive it.
///
/// The handle is *published* rather than dropped ([`mmio::init_net_global`]), and
/// that is the point of this stage's change: an open posts the receive ring (64
/// buffers through the neutral `Hal`), so a check that opened the device, read its
/// MAC and dropped it would leave the stack to open a second one - and the ring
/// posted here is the ring the stack's first frame arrives on.
fn check_net(info: &BootInfo) {
    use fmt::Write;

    let mut out = pl011::Writer;

    macro_rules! fail {
        ($($arg:tt)*) => {{
            let _ = writeln!(out, "M8_NET_FAILED: {}", format_args!($($arg)*));
            halt();
        }};
    }

    // The tree walk is the same one the block and input checks use: every window
    // whose `DeviceID` says net, in tree order. The first that *opens* is the
    // machine's adapter - `check_fs` picks its disk by content for the same
    // reason, but a net device has no content to compare, so the open itself is
    // the selection.
    let windows = match virtio_device_windows(info, VIRTIO_ID_NET) {
        Ok(windows) => windows,
        Err(reason) => fail!("{reason}"),
    };
    if windows.is_empty() {
        fail!("the tree describes no virtio-net (device {VIRTIO_ID_NET}) window");
    }
    let mut opened = None;
    for &(virt, size) in &windows {
        if let Some(dev) = crate::drivers::virtio::mmio::init_net_global(virt, size) {
            opened = Some((virt, dev));
            break;
        }
    }
    let Some((virt, dev)) = opened else {
        fail!("no virtio-net window opened as a transport");
    };
    let mac = crate::drivers::virtio::NetDevice::mac(&*dev);
    if mac.iter().all(|&b| b == 0) {
        fail!("the virtio-net answered a zero MAC, which a configured adapter never has");
    }
    let _ = writeln!(
        out,
        "virtio-net over MMIO opened, MAC {:#04x}:{:#04x}:{:#04x}:{:#04x}:{:#04x}:{:#04x}",
        mac[0], mac[1], mac[2], mac[3], mac[4], mac[5]
    );
    // The data path, not just the config space: a bare minimum Ethernet frame (60
    // bytes) of zeros - the shape a real frame has, not its content - handed to the
    // transmit queue, which has to take it.
    if !crate::drivers::virtio::NetDevice::transmit(&*dev, &[0u8; 60]) {
        fail!("the virtio-net TX queue did not accept a frame over MMIO at {virt:#x}");
    }
    let _ = writeln!(out, "virtio-net TX virtqueue accepted a frame over MMIO");
    let _ = writeln!(out, "M8_NET_OK");
}

/// The fourth MMIO device class: the aarch64 virtio transport reaches a virtio-input
/// device. xtask attaches a `virtio-input-device` on the tree; `check_input` finds
/// its window by `DeviceID` and confirms the memory-mapped transport opens and
/// negotiates it as an input device. Like `check_net`'s MAC read this proves the
/// transport reaches the class, not that events flow - the input stack (`/dev/input`,
/// evdev nodes, the pump) is a later cut. `M8_INPUT_OK` prints only on a successful
/// negotiate, and a failure reuses `M8_INPUT_FAILED` (a terminator) to stop honestly.
fn check_input(info: &BootInfo) {
    use fmt::Write;

    let mut out = pl011::Writer;

    macro_rules! fail {
        ($($arg:tt)*) => {{
            let _ = writeln!(out, "M8_INPUT_FAILED: {}", format_args!($($arg)*));
            halt();
        }};
    }

    let windows = match virtio_device_windows(info, VIRTIO_ID_INPUT) {
        Ok(windows) => windows,
        Err(message) => fail!("{message}"),
    };
    if windows.is_empty() {
        fail!("the tree describes no virtio-input (device {VIRTIO_ID_INPUT}) window");
    }
    let mut opened = false;
    for &(virt, size) in &windows {
        if crate::drivers::virtio::mmio::probe_input(virt, size) {
            opened = true;
            break;
        }
    }
    if !opened {
        fail!("no virtio-input window opened and negotiated as an MMIO transport");
    }
    let _ = writeln!(
        out,
        "virtio-input over MMIO negotiated and read its device identity"
    );
    let _ = writeln!(out, "M8_INPUT_OK");
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

/// The `main`-supplied init handoff, stored by `boot` as a `fn(*const u8, usize) -> !`
/// pointer and run by `el0_returned` once the EL0 probe reports `M6_EL0_OK`. `0`
/// means "nothing after the probe" - a halt-only bring-up boot.
static POST_EL0: AtomicUsize = AtomicUsize::new(0);
/// The initramfs archive to hand to `/init`, as a pointer and length read out of
/// the direct-mapped `BootInfo` before the address-space switch, so they stay valid
/// when `el0_returned` runs. A null pointer means a diskless boot with no archive.
static INITRAMFS_PTR: AtomicUsize = AtomicUsize::new(0);
static INITRAMFS_LEN: AtomicUsize = AtomicUsize::new(0);

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

    // Enter through `enter_frame` - the same full register-file restore the
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
    // stack pointer is its page's top, and `SPSel` is on the kernel stack.
    unsafe { context::enter_frame(&frame, el0_returned) }
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

    // The init handoff runs here, on the boot stack the probe's `eret` resumed
    // onto. It is `-> !`: it starts the scheduler, hands the machine to `/init`
    // and parks the boot task, so control never comes back to this `halt`. With no
    // handoff stored (a halt-only bring-up boot, or a stage before 6E) the probe
    // still ends the boot as it always did.
    let run = POST_EL0.load(Ordering::Relaxed);
    if run != 0 {
        let archive = INITRAMFS_PTR.load(Ordering::Relaxed) as *const u8;
        let len = INITRAMFS_LEN.load(Ordering::Relaxed);
        // SAFETY: `boot` stored a real `fn(*const u8, usize) -> !` in `POST_EL0` and
        // the archive pointer and length it reads out of the direct-mapped
        // `BootInfo`, both still valid here. `SPSel` selects `SP_EL1` (checked at
        // the top of `check_el0`), so this runs on the boot stack the
        // `leave_for_kernel` `eret` restored.
        let run = unsafe { core::mem::transmute::<usize, fn(*const u8, usize) -> !>(run) };
        run(archive, len);
    }
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
            let _ = writeln!(out, "M6_PAGING_FAILED: {}", format_args!($($arg)*));
            halt();
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
    unsafe { core::ptr::write_bytes(mm::phys_to_virt(a) as *mut u8, 0x5a, PAGE_SIZE) };

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
    if paging::user_accessible(mm::hhdm_offset(), PAGE_SIZE, false) {
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
    // on this machine and does not on x86_64 (`PLAN.md` §13.9).
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
    let wrote = unsafe { core::ptr::read(mm::phys_to_virt(b) as *const u32) };
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
    let _ = writeln!(out, "M6_PAGING_OK");
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
            let _ = writeln!(out, "M6_WALL_FAILED: {}", format_args!($($arg)*));
            halt();
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
    let _ = writeln!(out, "M6_WALL_OK");
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
            let _ = writeln!(out, "M6_CPU_FAILED: {}", format_args!($($arg)*));
            halt();
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

    let _ = writeln!(
        out,
        "cpu: CPACR.FPEN full access after enable_user_simd; a distributor line \
         enabled then masked and read back; idle() woken by the clock (tick \
         {before} -> {}) and returning with interrupts off",
        timer::ticks()
    );
    let _ = writeln!(out, "M6_CPU_OK");
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
            "M6_SERIAL_RX_FAILED: loopback probe {probe:#04x} did not read back through RX"
        );
        halt();
    }
    let _ = writeln!(out, "serial rx: loopback echoed the probe through RXFE/DR");
    let _ = writeln!(out, "M6_SERIAL_RX_OK");
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
            let _ = writeln!(out, "M6_TLS_FAILED: {}", format_args!($($arg)*));
            halt();
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
    let _ = writeln!(out, "M6_TLS_OK");
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
    crate::drivers::tx_ring::enable();
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
    // Recorded before it is printed, and recorded *here* because this is the one
    // place the loader's command line is in hand. `/proc/cmdline` is where a
    // userspace init reads it (Alpine's own reads `root=` through this same
    // file), and the handoff reads its own instrument flag back out of it - the
    // `mpos.selftest` word that says "this boot is a test", the same word the
    // x86_64 boot gates its milestone checks on. Until this call the machine's
    // command line was reachable nowhere but this print.
    crate::boot::set_cmdline(info.cmdline);
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

// ---------------------------------------------------------------------------
// `crate::hal`: this machine implements the six capability concerns for
// `crate::hal::Current`. Everything here is a one-line static dispatch onto the
// module that actually holds the body - `timer`/`pl031`/`vectors`/`gic`/`paging`/
// `context` - so `crate::hal::ticks()` is exactly the `timer::ticks()` a caller
// above the seam wants, monomorphised, with no architecture named above `arch`
// (`AGENTS.md` §4, `PLAN.md` §9.1). x86_64's `mod.rs` is the shape template; the
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

    unsafe fn install(
        info: &crate::boot::BootInfo,
        sections: &crate::mm::vm::KernelSections,
    ) -> usize {
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
        // The device window is mapped whole at boot, so a physical MMIO range
        // inside it is already reachable: the answer is its offset into the
        // kernel-half alias, and `None` if the range falls outside the window.
        paging::device_virt(phys, len)
    }

    #[inline]
    fn translate(virt: usize) -> Option<(usize, crate::mm::MapFlags)> {
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
        crate::drivers::tx_ring::enabled()
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
        crate::drivers::tx_ring::space()
    }

    #[inline]
    fn tx_submit(bytes: &[u8]) -> usize {
        crate::drivers::tx_ring::submit::<pl011::Pl011>(bytes)
    }

    #[inline]
    fn tx_wait_now() {
        crate::drivers::tx_ring::wait_now();
    }

    #[inline]
    fn tx_stop_waiting() {
        crate::drivers::tx_ring::stop_waiting();
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

/// The machine's entropy source: its MMIO virtio-rng, installed by the boot's
/// `check_rng` before `rand::init` runs, so the first stream is seeded from
/// bytes the host generated rather than from anything this machine chose. There
/// is no `RDRAND` here - the device is the source - and a machine with no rng
/// window folds in nothing, leaving the clock mix in `crate::rand` as the floor.
/// Keeping the source here is what lets `crate::rand` name no machine.
impl crate::hal::Entropy for crate::hal::Current {
    fn mix_into(seed: &mut [u8; 32]) {
        if let Some(entropy) = crate::drivers::virtio::mmio::entropy() {
            for (byte, host) in seed.iter_mut().zip(entropy) {
                *byte ^= host;
            }
        }
    }
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
