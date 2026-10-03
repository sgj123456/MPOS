//! The aarch64 kernel image: boot entry and panic handler.
//!
//! The x86_64 half of this file's job is done by `mpos-boot`, and since stage 1e
//! so is this one's: the loader reads the kernel off the ESP, applies its
//! relocations against `KERNEL_VIRT_BASE`, builds the page tables, exits boot
//! services and branches here with the machine already running in the higher
//! half. So there is no entry stub and no stack to set up — the one interface is
//! the register the record arrives in, exactly as on x86_64 (`RDI` there, `x0`
//! here), and this function's own prologue is the first thing to use the stack
//! the loader mapped.
//!
//! What this file had until stage 1e was a `global_asm!` `_start` that loaded
//! `sp` from a `__stack_top` symbol the linker script cut out of the image, and
//! a `.bss` walk beneath it. Both belonged to the `-kernel` bring-up: QEMU
//! placed the ELF at a fixed physical address and jumped in with `sp = 0`, so
//! the image had to carry its own stack and could not assume its `NOBITS` tail
//! came back zeroed. The loader does both of those now — `vm::load` zeroes every
//! segment's memory tail and `main.rs` allocates and maps the boot stack — which
//! is why x86_64's entry has neither, and why this one no longer does.

use core::panic::PanicInfo;

use alloc::string::String;
use alloc::vec;

use mpos::boot::handoff::Handoff;
use mpos::{hal, sched, vfs};

/// `mpos-boot` branches here at `KERNEL_VIRT_BASE + entry`, at EL1 with the MMU
/// already on and the machine's own tables installed, on a boot stack the loader
/// mapped through the direct map, with `x0` holding the [`Handoff`] record it
/// wrote.
///
/// The register is the whole interface, and the record has to be checked rather
/// than trusted: nothing in the machine enforces that `x0` holds what this
/// function was compiled to expect, so `boot::handoff::collect` asserts the
/// magic before anything reads a field. That check is the only reason this
/// pointer can be dereferenced at all — the same contract x86_64's `_start`
/// carries, spelled in a different register.
///
/// # Safety
///
/// The caller is the loader's branch to this symbol, with `x0` holding the
/// record it wrote and every address that record names mapped for as long as the
/// kernel runs. Nothing chooses this call from Rust.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn _start(handoff: *const Handoff) -> ! {
    // SAFETY: the caller's contract, restated in `arch::aarch64::boot`'s own
    // `# Safety`: the branch into this symbol carried the record the loader
    // wrote, and the checks there read it under exactly that assumption.
    // `console_milestone` is run by the machine boot once its PL011 is up; it is
    // defined here (in the wiring, which may name `crate::console`) so that
    // `arch` never reaches up into the console service it sits below.
    unsafe { mpos::arch::boot(handoff, console_milestone, init_handoff) }
}

/// The init handoff, run by the machine layer after the EL0 milestone probe - on
/// the boot stack the probe's `eret` resumed onto, which `sched::init` then adopts
/// as task 0. It mirrors the x86_64 `main`'s tail minus the subsystems this machine
/// has not been given yet (no PCI/virtio-blk/net/DRM: the root is a ramfs unpacked
/// from the loader's initramfs, exactly as the x86_64 kernel boot does before the
/// data root is mounted by init's own `mount(2)`).
///
/// Starting the scheduler is what lets the syscall gate work: from here on
/// `sched::started()` is true, a real task is current, and `/init`'s `svc` routes
/// into the shared `syscall::dispatch` table rather than the inline refusal the
/// pre-scheduler probe still gets. `/init` is the four-instruction `exit_group(0)`
/// program `xtask` builds; running it to completion and reaping it proves the
/// whole load -> enter EL0 -> trap -> dispatch -> exit round trip on this machine.
fn init_handoff(initramfs: *const u8, len: usize) -> ! {
    // The archive is the initramfs bytes `boot` read out of the handoff record's
    // direct-mapped `BootInfo` *before* the address-space switch and stashed here.
    // Re-reading the record from its raw pointer after that switch faults (the
    // loader's identity address is not in the kernel's own tables), so the pointer
    // and length are handed over directly. A null pointer means a diskless boot.
    // SAFETY: `boot` passed a pointer and length naming a live direct-mapped range.
    let archive: Option<&[u8]> = if initramfs.is_null() {
        None
    } else {
        Some(unsafe { core::slice::from_raw_parts(initramfs, len) })
    };

    // The scheduler adopts the running context (the boot stack) as task 0 and
    // installs its loop. Must come before any spawn and before the gate relies on
    // `sched::started()`.
    sched::init();

    // A ramfs root, then the initramfs unpacked into it. With no archive this is a
    // diskless boot: there is nothing to hand the machine to, so stop honestly
    // rather than invent an init.
    vfs::file::init_standard(vfs::tty::init());
    let root = vfs::ramfs::RamFs::new();
    vfs::mount_root(&*root);
    let Some(archive) = archive else {
        mpos::println!("initramfs: none - nothing to hand off to");
        hal::halt()
    };
    let members =
        vfs::tar::unpack_into(vfs::global(), archive).expect("the aarch64 initramfs was broken");
    mpos::println!("initramfs: {members} members unpacked");

    // `/init` is read from the ramfs and loaded as a task; it lands on pid 1.
    let init = vfs::read_file("/init").expect("the initramfs carries /init");
    let argv = vec![String::from("/init")];
    let pid = sched::spawn_init(&init, &argv, &[]).expect("the aarch64 init image loaded");
    assert_eq!(pid, sched::INIT_PID, "the first process must be pid 1");
    mpos::println!("mpos: aarch64 machine handed to /init");

    // Run it to completion. Its `exit_group` traps, the gate routes it into the
    // syscall table, the scheduler reaps it, and this wait ends - the marker below
    // is the whole stage-6 proof, printed only once a real program has actually
    // run and left on the second machine.
    while sched::has_live_child(pid) {
        sched::sleep_ticks(1);
    }
    mpos::println!("M6_INIT_OK");
    // Bring-up end: `/init` here is a one-shot `exit_group`, so when it leaves
    // there is nothing left to run and no interactive console yet. Halt cleanly.
    // Parking via `block_until` instead would switch to the scheduler's idle task,
    // and that switch faults on this machine - its kernel stack is not installed the
    // way `Table::enter` builds one (`msr SP_EL1` on a null stack, `sched::Table::enter`).
    // That idle/park path is a named follow-on for the drivers stage, where a real
    // shell keeps the machine runnable and the idle loop must work; refusing to park
    // here is honest about what is built, not a hidden stub.
    hal::halt()
}

/// Stage 4b's evidence: `crate::console` - un-gated for this machine one stage ago -
/// actually reaches the wire. Every boot line before this one was `arch`'s own
/// `pl011::Writer`; this is the first through the console service (`println!` ->
/// `print_args` -> the `hal::Console` sink -> the PL011). If the service were
/// mis-wired on this machine the milestone line simply would not appear, and
/// `cargo xtask boot-aarch64` asserts on its presence.
fn console_milestone() {
    // Bring the service's sink up (idempotent over the PL011 the boot already
    // poked) and send one line, then the marker, through the same path a
    // `println!` anywhere above the machine layer will use.
    mpos::console::init();
    mpos::println!("console: kernel log reached the PL011 through crate::console");
    mpos::println!("M4_CONSOLE_OK");
}

/// The panic path: one line on the console, then stop.
///
/// It prints through `arch`'s own machine writer (`arch::print_args`) rather than
/// `mpos::println!` not because the console service is missing on this machine -
/// stage 4a un-gated it - but because a panic must reach the host at *any* point
/// in boot, including before `crate::console` has been brought up or after its
/// state has been corrupted, and the machine writer has no dependencies beyond the
/// mapped device window. A panic that printed nothing because a higher layer was
/// mid-init would be the worst version of the gap: the machine stops with no word
/// about why.
#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    mpos::arch::print_args(format_args!("\nKERNEL PANIC: {info}\n"));
    mpos::arch::halt()
}
