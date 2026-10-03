//! aarch64 machine-dependent code.
//!
//! Everything in here may touch aarch64 specifics; nothing outside `arch::` may.
//!
//! This is the second machine (`PLAN.md` §9.1). Stage 1a landed the build
//! plumbing and a body that links; 1b the PL011 and the entry; 1c the exception
//! vectors, a GICv2 and the generic timer; 1d a page-table builder for the
//! `-kernel` bring-up; 1e replaced that bring-up with the real boot path, so a
//! UEFI loader reads the kernel off an ESP and branches here in the higher half;
//! and **1f is the kernel taking ownership of its own address space** - it
//! builds its page tables from `mm::vm::kernel_mappings` out of frames the
//! allocator hands it, instead of running on the loader's.
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
use crate::mm::vm::{self, KernelSections, MapFlags};
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
    check_timer();
    halt()
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
/// (which the plan does not describe yet - stage 5's `map_mmio` will), the null
/// page, and the low identity map being *gone*, which is the difference between
/// this stage and the last one.
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
    // out of is device memory.
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

    // The low identity map is gone, and this is the check that says so: the
    // image's own physical address is not an address the kernel can reach any
    // more. Stage 1e ran on the loader's tables, which mapped all of RAM at its
    // own address; the kernel's own tables map RAM through the direct map and
    // nothing else, which is what keeps the kernel image out of the half a
    // process will own.
    if let Some((phys, flags, _)) = paging::translate(info.kernel_phys_base) {
        let _ = writeln!(
            out,
            "M3_TABLES_FAILED: {:#x} is still mapped identity ({flags:?} -> {phys:#x}); \
             the kernel's tables should have kept only the device window in the low half",
            info.kernel_phys_base
        );
        halt();
    }
    let _ = writeln!(
        out,
        "no identity map: {:#x} is unreachable, only the device window lives low",
        info.kernel_phys_base
    );

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
