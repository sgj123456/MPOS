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

use mpos::boot::handoff::Handoff;

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
    unsafe { mpos::arch::boot(handoff, console_milestone) }
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
