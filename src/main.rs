//! The kernel image: the entry point, and the panic handler.
//!
//! Both are one piece of architecture-independent code. The entry takes the
//! loader's record in the register the platform's calling convention puts a first
//! argument in - `RDI` on x86_64, `x0` on aarch64, which is the whole of the
//! difference and is the compiler's business, not this file's - registers the
//! kernel services the machine layer will ask back for, and hands the machine
//! the pointer set it needs (`mpos_arch::arch::boot`). The panic handler prints
//! through the machine's own wire rather than through the console service,
//! because it has to work at any point in boot, including before that service
//! exists.
//!
//! What is *not* here is anything a machine does: the bring-up, the devices and
//! the acceptance checks are this machine's, and they live in `board` - one body
//! per port, chosen in one place, exactly as `arch/src/arch/mod.rs` keeps one
//! body per machine for the library. Everything reusable lives in the `mpos`
//! library, which the host can unit-test without a machine at all.

#![no_std]
#![no_main]

extern crate alloc;

mod board;

use core::panic::PanicInfo;

use mpos::boot::handoff::Handoff;

/// Where the loader branches, on either machine.
///
/// The register is the whole interface, and the record is checked rather than
/// trusted: nothing in the machine enforces that the first argument register
/// holds what this function was compiled to expect, so the translation the
/// machine layer performs (`hal::kernel::boot_info`) asserts the magic before
/// anything reads a field.
///
/// # Safety
///
/// The caller is the loader's branch to this symbol, with the record it wrote in
/// the platform's first argument register and every address that record names
/// mapped for as long as the kernel runs. Nothing chooses this call from Rust,
/// and the pointer is read before it can be validated, so the validity of
/// `handoff` is the caller's contract and not something this function can
/// establish.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn _start(handoff: *const Handoff) -> ! {
    // The machine layer's way back up, before either port arms an exception
    // vector: from that moment a fault can be taken, and its handler asks this
    // table whether the page is one to populate.
    mpos::kernel_services::register();

    // SAFETY: the caller's contract, passed through unchanged; `arch::boot`
    // restates it in its own `# Safety`.
    unsafe { board::start(handoff) }
}

/// The panic path, on either machine.
///
/// One line on the machine's own wire, whatever the machine knows about the
/// context it died in, and stop. The line goes through the machine's writer
/// rather than `println!` because a panic must reach the host at *any* point in
/// boot - before the console service exists, and after its state has been
/// corrupted - while that writer has no dependencies beyond a mapped device
/// window. A panic that printed nothing because a higher layer was mid-init is
/// the worst version of the gap: the machine stops with no word about why.
#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    mpos::arch::print_args(format_args!("\nKERNEL PANIC: {info}\n"));
    board::panic_context();
    mpos::hal::exit_qemu(mpos::hal::TEST_EXIT_PANIC)
}
