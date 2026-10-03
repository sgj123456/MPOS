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
    unsafe { mpos::arch::boot(handoff) }
}

/// The panic path: one line on the console, then stop.
///
/// It prints through `arch`'s own writer rather than `mpos::println!` for the
/// reason that macro is x86_64-only: this machine's console is a PL011, and the
/// kernel's console mux does not exist here until stage 4. A panic that printed
/// nothing because the mux was missing would be the worst version of the gap -
/// the machine would stop with no word about why.
#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    mpos::arch::print_args(format_args!("\nKERNEL PANIC: {info}\n"));
    mpos::arch::halt()
}
