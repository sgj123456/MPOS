//! What every port's boot *reports*, in one copy.
//!
//! [`steps`](super::steps) is the boot's shared *work* - the five steps with
//! ordering contracts in them. This module is the shared *reporting*: the banner
//! a boot opens with, the boot-stack canary check, where the boot task spends the
//! rest of the machine's life, and the two things a dying kernel says about the
//! context it died in.
//!
//! It is a separate module because the two are different kinds of duplication to
//! remove. A step is shared because a machine that ran it differently would be
//! wrong; a report is shared because there is nothing in it a machine *could*
//! answer differently - every line below is a function of `BootInfo`, the
//! scheduler, or a `hal` register read. Before this module existed, all five lived
//! in the first machine's boot body and the second machine had none of them (an
//! empty `panic_context`) or a machine-shaped substitute (`hal::halt` where the
//! first machine parked). `NOTES.md` §13.23 and Phase 15's stages 15.4/15.5 are
//! where that came from - and the bodies themselves are gone now, replaced by the
//! one flow beside this file.
//!
//! What is *not* here: which console the lines go to (the console service, above
//! this), when [`banner`] is called (the flow's first act, so the record is on the
//! wire before anything it describes is used), and the panic handler itself, which
//! is `src/main.rs`'s - architecture-independent, and it calls the one function
//! below.

#[cfg(feature = "selftest")]
use alloc::string::String;

use mpos::boot::BootInfo;
use mpos::boot::handoff::BOOT_STACK_CANARY;
#[cfg(feature = "selftest")]
use mpos::kunit::{Case, Phase, Suite};
use mpos::{arch, hal, println, sched};

/// One line per fact the loader's record carries, before anything acts on it.
///
/// It goes through `println!` like every other boot line, and deliberately not
/// through `arch::print_args`: by the time a body's `init` runs, the console
/// service is up on both machines (the console came up before it), so the
/// banner is a console line and not a wire write.
pub fn banner(info: &BootInfo) {
    const MIB: usize = 1024 * 1024;
    println!("mpos {} booting", env!("CARGO_PKG_VERSION"));
    println!(
        "bootloader: {} {}",
        info.bootloader_name, info.bootloader_version
    );
    println!(
        "kernel: phys {:#x}, virt {:#x}",
        info.kernel_phys_base, info.kernel_virt_base
    );
    println!("hhdm offset: {:#x}", info.hhdm_offset);
    println!("cmdline: {:?}", info.cmdline);
    println!("memory map ({} regions):", info.region_count);
    for region in info.regions() {
        println!("  {:?}", region);
    }
    println!("allocatable: {} MiB", info.allocatable_bytes() / MIB);
}

/// Check that the boot stack never ran into the canary the loader planted at its
/// bottom.
///
/// The boot stack is the one stack nothing in the kernel owns: the loader
/// allocated it, the kernel spends its entry through the handoff on it, and after
/// that the boot task keeps it forever - it is adopted, so there is no `Stack`
/// whose canary the reaper would check. That leaves it the one stack whose
/// overflow has no witness at all, which is the worst place for the gap: the boot
/// path is the deepest single-threaded run in the machine, with the pid-1 handoff
/// nesting an ELF parse and a whole address space on top of every initialisation
/// step that preceded it.
///
/// An overflow here is not a clean failure, and *how* it fails is the machine's:
/// on x86_64 whatever trap comes next builds its frame where the overflow already
/// wrote, so the `iretq` out of it pops a word that was never a selector and the
/// machine dies reporting a `#GP` on a descriptor that was never loaded - a
/// symptom pointing at the GDT, and it costs a long afternoon to find out it was
/// the stack. What both machines can say is that the word is still there.
///
/// Called at the deepest points in the boot on each machine: before the handoff,
/// and again after init has been spawned - `spawn_init` is the single deepest call
/// the boot task ever makes.
pub fn check_boot_stack(info: &BootInfo, when: &str) {
    let base = info.boot_stack_base;
    // Zero means the loader planted no canary, which is how an older loader's
    // handoff reads. Nothing to check, and not an error.
    if base == 0 {
        return;
    }
    // SAFETY: the loader put the canary at this address and told the kernel where
    // it is; the record is the loader's, so the address is live memory.
    let canary = unsafe { core::ptr::read_volatile(base as *const u64) };
    assert_eq!(
        canary, BOOT_STACK_CANARY,
        "the boot stack ran off its bottom by {when}: the canary at {base:#x} is \
         {canary:#x}, not {BOOT_STACK_CANARY:#x}. The loader gave it 64 pages; a \
         build that needs more says so here rather than as a secondary fault on a \
         machine that cannot report it."
    );
    println!("boot stack: intact {when}");
}

/// The boot's own case in the self-test harness: the canary at the bottom of the
/// boot stack, measured once the boot has been through everything a test build
/// runs.
///
/// It is a suite in *this* module because the check it drives is this module's: a
/// boot stack is the boot's, not a subsystem's, so there is no subsystem suite for
/// it to live in and the wiring layer is the only place that can name `report`
/// (`docs/NOTES.md` §15.23). `Phase::Ipc` is the last phase the boot does work in,
/// so this is the deepest the boot stack is measured from a suite - the deepest
/// point of all, the handoff nesting an ELF parse and an address space on top of
/// every step, is what the two production calls in the bodies measure.
#[cfg(feature = "selftest")]
pub const SUITE: Suite = Suite {
    name: "boot",
    phase: Phase::Ipc,
    cases: &[Case {
        name: "stack_depth",
        run: stack_depth,
    }],
};

/// [`check_boot_stack`], in the shape a case answers.
#[cfg(feature = "selftest")]
fn stack_depth(info: &BootInfo) -> Result<(), String> {
    check_boot_stack(info, "after the self-tests");
    Ok(())
}

/// Where the boot task spends the rest of the machine's life.
///
/// It *blocks* here rather than halting on the spot, and the difference is the
/// whole of the wake latency. `wait_for_interrupt` from a task stops the core
/// rather than giving it up: this task stays runnable, so the picker hands it the
/// CPU, and a task that has halted can only be taken off it by an interrupt - of
/// which the only one that reliably arrives is the clock. Any wake that lands
/// behind it in the ready queue therefore waits for the next tick, which is 4 ms
/// at this build's 250 Hz, on every blocking call in the system. Parking in the
/// scheduler instead leaves the idle halt where the rest of the kernel already
/// assumes it is - in the scheduler's own loop, where a wake is followed by a turn
/// and not by a tick (`NOTES.md` §10.2 measured the first machine before and after).
pub fn park() -> ! {
    loop {
        sched::block_until(|| false);
    }
}

/// How much of the running task's kernel stack is in use, and whether it is still
/// inside its own block at all.
///
/// A wild write and a stack that ran off its bottom look the same from the place
/// the damage is noticed - a collection's bookkeeping is wrong - and they want
/// opposite investigations. The one number that separates them is how far the
/// stack pointer has come down from the top of the block it was given: a stack
/// still thousands of bytes above its base did not overflow, and one sitting at or
/// below the base did.
pub fn report_stack_use() {
    let sp = hal::stack_pointer();
    match sched::current_stack_range() {
        Some((base, top)) => println!(
            "  kernel stack: sp {sp:#x} of [{base:#x}..{top:#x}), {} bytes in use, \
             {} bytes to spare",
            top.saturating_sub(sp),
            sp.saturating_sub(base)
        ),
        None => println!("  kernel stack: sp {sp:#x}, not a task stack"),
    }
}

/// What the panic handler prints about the context it died in.
///
/// A panic names where core *noticed* the problem, which is rarely where it was
/// caused: `slice::get_unchecked` fires deep inside a collection whose bookkeeping
/// a write somewhere else broke, and the location line points at core, not at the
/// culprit. The stack is the only trail back, and the words worth following are
/// the return addresses into the kernel's own text - which
/// [`arch::kernel_sections`] names exactly, rather than by a base constant a port
/// would have to repeat. Everything else on the stack is data, so it is skipped
/// rather than dumped as noise.
pub fn panic_context() {
    report_stack_use();
    mpos::syscall::trail::dump();
    let sections = arch::kernel_sections();
    let sp = hal::stack_pointer();
    println!("  stack from {sp:#x}, kernel-text words only:");
    // SAFETY: the words at and above the stack pointer are the running stack, the
    // kernel's own memory, read-only here and bounded to 64 words.
    let words = unsafe { core::slice::from_raw_parts(sp as *const u64, 64) };
    for (i, word) in words.iter().enumerate() {
        let value = *word as usize;
        if (sections.text.start..sections.text.end).contains(&value) {
            println!("    sp+{:3} {:#018x}", i * 8, value);
        }
    }
}
