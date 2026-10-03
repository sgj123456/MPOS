//! aarch64 machine-dependent code.
//!
//! Everything in here may touch aarch64 specifics; nothing outside `arch::` may.
//!
//! This is the second machine (`PLAN.md` §9.1). Stage 1a landed the build
//! plumbing and a body that links; 1b the PL011 and the entry; 1c the exception
//! vectors, a GICv2 and the generic timer; 1d a page-table builder for the
//! `-kernel` bring-up; and **1e replaced that bring-up with the real boot path**:
//! `mpos-boot` now builds the tables, relocates the image into the higher half
//! and branches here with the handoff record in `x0`, so this module consumes
//! that record instead of inventing a layout, and `paging.rs` walks the loader's
//! tables instead of building its own.
//!
//! What is still absent, and where it lands: contexts and user entry (2), then
//! `sched` (3), `console`/`vfs` (4) and the drivers (5). Everything above the
//! seam reaches those through `crate::hal`, which this machine implements trait
//! by trait as its stages land - which is also why `hal` itself is still
//! x86_64-only (`src/lib.rs`'s ledger).

pub(crate) mod critical;
pub(crate) mod gic;
pub(crate) mod paging;
pub(crate) mod pl011;
pub(crate) mod timer;
pub(crate) mod vectors;

use core::fmt;

use crate::boot::BootInfo;
use crate::boot::handoff::Handoff;
use crate::mm::vm::{KernelSections, MapFlags, Mapping};

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
/// `BootInfo::kernel_virt_base` plus an offset rather than at the offset alone.
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
/// By the time this runs the machine is already the kernel's: the loader built
/// the page tables from the firmware's memory map, mapped the image at
/// `KERNEL_VIRT_BASE`, applied the relocations and switched `TTBR0_EL1` and
/// `TTBR1_EL1` onto them. So there is no page-table step here - what is left is
/// to check that what the loader built says what the plan says, which is what
/// [`check_paging`] does.
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

    init_interrupts();
    // The tables are read before the clock is opened, so that the tick the check
    // measures is one taken through tables this kernel has already looked at:
    // the GIC's registers are reached through the device window from here on,
    // and a window that still counts to 25 is a window where translation,
    // attributes and interrupts all agree.
    check_paging(&info);
    check_timer();
    halt()
}

/// Stage 1e's evidence: the tables the machine is running on are the loader's,
/// and they say what the plan says.
///
/// The order of the checks is the order of the claims. That this function is
/// running at all is the first and largest one - the image's entry point was
/// reached through the higher-half alias, so the loader's relocation, its
/// mapping of the alias and its switch of both base registers all worked, and a
/// failure of any of the three would have been a machine that never arrived.
/// Everything after it is about *which* mappings exist.
fn check_paging(info: &BootInfo) {
    use fmt::Write;

    // SAFETY: the machine is running on the loader's tables - that is what
    // reaching this function means - and it never stops running on them.
    unsafe { paging::adopt() };
    let (ttbr0, ttbr1) = paging::roots();
    let mut out = pl011::Writer;
    let _ = writeln!(
        out,
        "page tables: TTBR0 {ttbr0:#x}, TTBR1 {ttbr1:#x} (the loader's, read back)"
    );

    if !paging::enabled() {
        let _ = writeln!(out, "M3_TABLES_FAILED: SCTLR_EL1.M is clear");
        halt();
    }

    let sections = kernel_sections();
    // What the loader added to a linked address to reach the address the kernel
    // is running at. The loader chose both ends and put them in the record, so
    // this is arithmetic over the record rather than a constant.
    let offset = info.kernel_virt_base - info.kernel_phys_base;

    // W^X across the whole image, at the address it is *running* at. The
    // identity copy is not checked: the kernel does not execute there and the
    // loader maps it writable and executable on purpose, because it has no
    // symbols for the sections it is copying.
    //
    // Each section's expectation is spelled as the `Mapping` the loader built
    // for it - the plan's own type, `mm::vm`'s - page-aligned at both ends
    // exactly as `vm::map_kernel_image` aligns it, rather than as a hand-rolled
    // span of page arithmetic. That is what makes the walk below a walk of a
    // mapping instead of a guess about how one would have been built.
    let page_down = |at: usize| at & !(4096 - 1);
    let page_up = |at: usize| (at + 4095) & !4095;
    for (name, range, flags) in [
        ("text", &sections.text, MapFlags::READ | MapFlags::EXEC),
        ("rodata", &sections.rodata, MapFlags::READ),
        ("data", &sections.data, MapFlags::READ | MapFlags::WRITE),
    ] {
        let start = page_down(range.start);
        let mapping = Mapping {
            virt: start,
            phys: start - offset,
            length: page_up(range.end) - start,
            flags,
        };
        // The loop is page by page, which is only a description of `mapping` if
        // the mapping is page-granular. The plan's own predicate, and the same
        // invariant x86_64's `paging::install` asserts over its window.
        assert!(
            mapping.is_page_granular(),
            "the {name} mapping is not page-granular: {mapping:?}"
        );

        let mut page = mapping.virt;
        while page < mapping.end() {
            let want = mapping.phys_of(page);
            match paging::translate(page) {
                Some((phys, got, Memory::Normal)) if got == flags && phys == want => {}
                Some((phys, got, memory)) => {
                    let _ = writeln!(
                        out,
                        "M3_TABLES_FAILED: {name} at {page:#x} is {got:?} {memory:?} -> {phys:#x}, \
                         expected {flags:?} Normal -> {want:#x}"
                    );
                    halt();
                }
                None => {
                    let _ = writeln!(out, "M3_TABLES_FAILED: {name} at {page:#x} is unmapped");
                    halt();
                }
            }
            page += 4096;
        }
    }
    let _ = writeln!(
        out,
        "image mapped r-x / r-- / rw- at {:#x} (phys {:#x})",
        info.kernel_virt_base, info.kernel_phys_base
    );

    // The direct map: a frame out of the record's own memory map, written and
    // read back through it. Taken from the first region the allocator may use
    // rather than from a constant, because the constants this stage used to
    // carry described the bring-up machine and the record describes the real
    // one. The top page of the region is chosen so the write is well clear of
    // the loader's own allocations, which the map reports as reclaimable rather
    // than usable.
    let Some(scratch_phys) = info
        .regions()
        .iter()
        .find(|r| r.kind.is_allocatable() && r.length >= 2 * 4096)
        .map(|r| r.base + r.length - 4096)
    else {
        let _ = writeln!(out, "M3_TABLES_FAILED: the record names no usable RAM");
        halt();
    };
    let scratch = info.phys_to_virt(scratch_phys);
    // `READ | WRITE | EXEC`, and the `EXEC` is a named gap rather than an
    // oversight. The loader maps both of its physical ranges - the identity map
    // it is running on and the direct map - with one call and one set of flags
    // (`vm::map_physical_memory`), so both come out executable; on x86_64 that
    // is a window that closes the moment the kernel installs its own tables,
    // whose plan (`mm::vm::kernel_mappings`) gives the direct map `READ | WRITE`
    // and nothing else. This machine has not installed its own tables yet, so
    // here the window is still open, and the check says so instead of asserting
    // a permission the loader did not set. The identity map has to stay
    // executable in any case: the loader is *running* on it until the branch.
    // `PLAN.md` §9.1 carries this as the follow-on stage's inheritance.
    match paging::translate(scratch) {
        Some((phys, flags, Memory::Normal))
            if phys == scratch_phys
                && flags == MapFlags::READ | MapFlags::WRITE | MapFlags::EXEC =>
        {
            // SAFETY: `scratch` translates to `scratch_phys`, a page the record
            // calls usable - that is, memory nothing has been given and the
            // kernel's allocator has not started. Nothing else has it.
            unsafe {
                core::ptr::write_volatile(scratch as *mut u8, 0xa5);
                if core::ptr::read_volatile(scratch as *const u8) != 0xa5 {
                    let _ = writeln!(out, "M3_TABLES_FAILED: the direct map did not store 0xa5");
                    halt();
                }
            }
        }
        Some((phys, flags, memory)) => {
            let _ = writeln!(
                out,
                "M3_TABLES_FAILED: the direct map at {scratch:#x} is {flags:?} {memory:?} -> {phys:#x}"
            );
            halt();
        }
        None => {
            let _ = writeln!(
                out,
                "M3_TABLES_FAILED: the direct map has no page at {scratch:#x}"
            );
            halt();
        }
    }
    let _ = writeln!(
        out,
        "direct map: {scratch:#x} -> {scratch_phys:#x}, writable (rwx: the loader's \
         window, see the check's comment)"
    );

    // The device window, which is the one range that has to be mapped as device
    // rather than as memory: a PL011 mapped Normal lets the core reorder the
    // stores that are this console's characters. The check translates the
    // console's own registers rather than a constant of this file's, so what it
    // proves is that the wire the banner came out of is device memory.
    match paging::translate(pl011::BASE) {
        Some((phys, flags, Memory::Device))
            if phys == pl011::BASE && flags == MapFlags::READ | MapFlags::WRITE => {}
        other => {
            let _ = writeln!(
                out,
                "M3_TABLES_FAILED: the PL011 at {:#x} translates to {other:?}, \
                 expected {:#x} as READ|WRITE Device",
                pl011::BASE,
                pl011::BASE
            );
            halt();
        }
    }
    let _ = writeln!(
        out,
        "device window: PL011 at {:#x} maps to itself, Device",
        pl011::BASE
    );

    if paging::translate(0).is_some() {
        let _ = writeln!(out, "M3_TABLES_FAILED: the null page is mapped");
        halt();
    }

    let _ = writeln!(out, "M3_TABLES_OK");
}

/// Bring the machine's interrupts up: the vector table, then the controller,
/// then the clock.
///
/// The order is the x86_64 `init_interrupts`'s, and for the same reason: the
/// vectors must exist before anything can raise one, and the controller must
/// know about the timer's line before the timer is armed to raise it. Unmasking
/// the CPU is the caller's separate step, because a boot path brings the
/// machine up and *then* opens the door.
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
        "aarch64: loaded by mpos-boot, running on its tables in the higher half \
         (stages 1a-1e)"
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
