//! The aarch64 kernel image: the bring-up entry.
//!
//! The x86_64 half of this file's job is done by `mpos-boot`, which has already
//! built page tables, mapped the image and read the kernel off the ESP by the
//! time `_start` runs. aarch64 has no loader yet - stage 1e adds one - so this
//! is the *bring-up* path `PLAN.md` §9.1 names: QEMU's `-kernel` places the ELF
//! in RAM and jumps to `_start` with the MMU off and the firmware's device tree
//! in `x0`. It is an instrument for proving the toolchain, the linker script,
//! the gate list and the machine body agree, not the shape the port ships with;
//! its boot stack is the linker script's rather than one the loader mapped.
//!
//! What the two paths do share is the interface: the ELF's entry point takes
//! its one argument in a register, and on x86_64 that register holds the
//! handoff record. `x0` is that register here, so stage 1e changes what the
//! register carries and nothing about this file.

use core::arch::global_asm;
use core::panic::PanicInfo;

// The entry the ELF names - `ENTRY(_start)` in the linker script, and where
// QEMU's `-kernel` jumps. Written in assembly because the one thing it must do
// is put a stack under the frame pointer, which a Rust prologue would itself
// need a stack for.
//
// `x0` carries the firmware's hand-off pointer and is deliberately untouched:
// the branch below is a tail call, so the callee sees the register exactly as
// the entry did.
global_asm!(
    ".section .text._start",
    ".globl _start",
    "_start:",
    "  ldr x1, =__stack_top",
    "  mov sp, x1",
    "  b {entry}",
    entry = sym rust_entry,
);

/// What `_start` branches to, once there is a stack.
extern "C" fn rust_entry(handoff: usize) -> ! {
    zero_bss();
    mpos::arch::boot(handoff)
}

/// Zero `.bss` before the first Rust static is read.
///
/// A `-kernel` boot loads the ELF's `PT_LOAD` segments; whether the `NOBITS`
/// tail comes back zeroed is the loader's business and not something to build
/// on, and a static that should read zero reading whatever was in RAM is a bug
/// that only shows on the second boot. The linker script keeps the boot stack
/// out of `.bss` precisely so that this walk cannot overwrite the frames this
/// function is running on.
fn zero_bss() {
    unsafe extern "C" {
        static __bss_start: u8;
        static __bss_end: u8;
    }
    let start = &raw const __bss_start as usize;
    let end = &raw const __bss_end as usize;
    // SAFETY: the linker script defines these as the bounds of one `NOLOAD`
    // section inside the image's own writable range, and nothing else in the
    // boot touches it before this runs.
    unsafe { core::ptr::write_bytes(start as *mut u8, 0, end - start) };
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
