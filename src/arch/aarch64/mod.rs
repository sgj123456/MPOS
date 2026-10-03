//! aarch64 machine-dependent code.
//!
//! Everything in here may touch aarch64 specifics; nothing outside `arch::` may.
//!
//! This is the second machine (`PLAN.md` §9.1). Stage 1a landed the build
//! plumbing and a body that links; stage 1b is the one that *speaks* - a PL011
//! writer and the boot banner - which is what proves the toolchain, the linker
//! script, the gate list and QEMU's wiring all agree rather than each merely
//! compiling.
//!
//! What is still absent, and where it lands: the exception vectors and the GIC
//! (1c), the page tables (1d), the loader's arm (1e). Everything above the seam
//! reaches those through `crate::hal`, which this machine implements trait by
//! trait as its stages land - which is also why `hal` itself is still x86_64-only
//! (`src/lib.rs`'s ledger).

pub(crate) mod critical;
pub(crate) mod gic;
pub(crate) mod paging;
pub(crate) mod pl011;
pub(crate) mod timer;
pub(crate) mod vectors;

use core::fmt;

use crate::mm::vm::{KernelSections, MapFlags};

/// Where the linker script put each part of the kernel image, as virtual
/// addresses.
///
/// The aarch64 half of the seam, and the same shape as `arch/x86_64`'s: the
/// plan in `mm::vm` says what permissions each section gets, and only here does
/// anything know their names. The linker script spells these symbols
/// deliberately name-for-name like the x86_64 one so the two bodies stay
/// comparable.
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
        // Data runs to the end of the image: `.bss` and the boot stack follow it
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

/// Run the machine from the entry stub's hand-off register. Never returns.
///
/// `x0` is the register the interface is defined in: once stage 1e's loader
/// exists it will hold the handoff record `boot/src/handoff.rs` writes, which is
/// the same record `main.rs` receives in `RDI` on x86_64. That the *register* is
/// the whole interface is the seam `src/boot/mod.rs` describes; which record it
/// carries is the stage.
///
/// On the `-kernel` bring-up path it carries zero, which is measured rather
/// than assumed: QEMU 11.1.1 with `-M virt` and a bare aarch64 ELF starts the
/// image with `x0 = 0` and `sp = 0` (QEMU's own `-d cpu` log and this banner
/// agree). A device tree is not handed over on this path - AAVMF will pass one
/// through the UEFI configuration table at stage 1e, and stage 5's parser reads
/// it from there. Nothing here depends on the register, which is why the port is
/// not weakened by the difference.
pub fn boot(handoff: usize) -> ! {
    pl011::init();
    banner(handoff);

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
    // The tables go in before the clock is opened, so that the tick the check
    // measures is one taken with the MMU on: the GIC's registers are reached
    // through the device mapping from then on, and a window that still counts to
    // 25 is a window where translation, attributes and interrupts all agree.
    check_paging();
    check_timer();
    halt()
}

/// Stage 1d's evidence: the kernel builds its own tables, switches the machine
/// onto them, and the mappings the hardware now uses are the ones the plan
/// asked for.
///
/// The order of the checks is the order of the claims. That execution continued
/// at all is the first and largest one - a table that omits the page the CPU is
/// reading would have stopped the machine at the `msr sctlr_el1`, and this
/// function would not be running. Everything after it is about *which* mappings
/// exist: the image's sections with the permissions the plan gave them, at both
/// the address the kernel is using and the higher-half alias; the direct map
/// writable and usable; and the null page absent.
fn check_paging() {
    use fmt::Write;

    let mut out = pl011::Writer;
    let sections = kernel_sections();
    // SAFETY: the bring-up contract. This is the first thing in the kernel to
    // touch the tables, it runs once, and the machine is executing at the
    // physical address the image was loaded at.
    let tables = unsafe { paging::install(&sections) };
    let _ = writeln!(
        out,
        "page tables: {tables} frames ({} KiB), image at {:#x} aliased to {:#x}",
        tables * 4096 / 1024,
        paging::IMAGE_LOAD_BASE,
        paging::KERNEL_VIRT_BASE,
    );

    if !paging::enabled() {
        let _ = writeln!(out, "M3_TABLES_FAILED: SCTLR_EL1.M is clear after install");
        halt();
    }

    // W^X across the whole image, at both addresses it is mapped at. The low
    // one is where the kernel is *running*: an image mapped writable there is an
    // image a stray store can rewrite, whatever the alias says.
    for (name, range, expect) in [
        ("text", &sections.text, MapFlags::READ | MapFlags::EXEC),
        ("rodata", &sections.rodata, MapFlags::READ),
        ("data", &sections.data, MapFlags::READ | MapFlags::WRITE),
    ] {
        // Page-aligned on both ends: `__text_start` is not a page boundary (the
        // ELF headers share its first page), and the mapping that covers it is.
        let first = range.start & !(4096 - 1);
        let last = (range.end + 4095) & !(4095);
        let mut page = first;
        while page < last {
            for (where_, base) in [
                ("identity", 0usize),
                ("alias", paging::KERNEL_VIRT_BASE - paging::IMAGE_LOAD_BASE),
            ] {
                let at = page + base;
                let (phys, flags) = match paging::translate(at) {
                    Some(answer) => answer,
                    None => {
                        let _ = writeln!(out, "M3_TABLES_FAILED: {name} {where_} {at:#x} unmapped");
                        halt();
                    }
                };
                if flags != expect || phys != page {
                    let _ = writeln!(
                        out,
                        "M3_TABLES_FAILED: {name} {where_} at {at:#x} is {flags:?} -> {phys:#x}, \
                         expected {expect:?} -> {page:#x}"
                    );
                    halt();
                }
            }
            page += 4096;
        }
    }
    let _ = writeln!(out, "image mapped r-x / r-- / rw- at both addresses");

    // The direct map: one frame the kernel knows is RAM, written and read back
    // through it. Chosen well above the image and well below the top of the
    // window this stage maps, so it is neither the kernel's own memory nor
    // something the firmware put there.
    let scratch_phys = paging::RAM_BASE + 0x0100_0000;
    let scratch = paging::HHDM_OFFSET + scratch_phys;
    let (back, flags) = match paging::translate(scratch) {
        Some(answer) => answer,
        None => {
            let _ = writeln!(
                out,
                "M3_TABLES_FAILED: the direct map has no page at {scratch:#x}"
            );
            halt();
        }
    };
    if back != scratch_phys || flags != MapFlags::READ | MapFlags::WRITE {
        let _ = writeln!(
            out,
            "M3_TABLES_FAILED: the direct map at {scratch:#x} is {flags:?} -> {back:#x}"
        );
        halt();
    }
    // SAFETY: `scratch` translates to `scratch_phys`, a frame of RAM inside the
    // window this stage maps and outside the image; nothing else in the kernel
    // has been given it.
    unsafe {
        core::ptr::write_volatile(scratch as *mut u8, 0xa5);
        if core::ptr::read_volatile(scratch as *const u8) != 0xa5 {
            let _ = writeln!(out, "M3_TABLES_FAILED: the direct map did not store 0xa5");
            halt();
        }
    }
    let _ = writeln!(
        out,
        "direct map: {scratch:#x} -> {scratch_phys:#x}, writable"
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

/// Say who this is and what it was given, then get out of the way.
///
/// Deliberately the same shape as the x86_64 banner (`src/main/x86_64.rs`), so
/// the two machines' first lines are comparable rather than merely both
/// present - and so that a broken linker script shows up as a wrong number here
/// rather than as a fault somewhere later.
fn banner(handoff: usize) {
    use fmt::Write;

    let sections = kernel_sections();
    let image = sections.data.end - sections.text.start;

    let mut out = pl011::Writer;
    let _ = writeln!(out, "mpos {} (aarch64) booting", env!("CARGO_PKG_VERSION"));
    // The address is fixed for this one machine and named as such; the device
    // tree is what stage 5 reads instead, and it comes from the UEFI
    // configuration table rather than from `x0` (see `boot`).
    let _ = writeln!(out, "console: PL011 at {:#x}", pl011::BASE);
    let _ = writeln!(
        out,
        "image: text {} KiB, rodata {} KiB, data {} KiB ({} KiB total)",
        (sections.text.end - sections.text.start) / 1024,
        (sections.rodata.end - sections.rodata.start) / 1024,
        (sections.data.end - sections.data.start) / 1024,
        image / 1024,
    );
    // Printed because it *is* the machine's contract with its loader, and this
    // stage is the one that establishes what the `-kernel` path actually puts
    // there rather than what it is supposed to.
    let _ = writeln!(out, "x0 at entry: {handoff:#018x}");
    let _ = writeln!(out, "exception level: EL{}", current_el());
    let _ = writeln!(
        out,
        "aarch64: bring-up, stage 1d (the kernel's own page tables; MMU off until they go in)"
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
