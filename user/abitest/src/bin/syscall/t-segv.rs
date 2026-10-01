//! t-segv - a fault the *process* made is the process's to bear, never the
//! machine's.
//!
//! Pinned here: an instruction-fetch fault at an address the process does not
//! have - jumping into a `PROT_NONE` page - ends that process with `SIGSEGV`
//! and leaves the machine running. The parent observes the child's death
//! through `wait4` and then keeps going, which is the assertion that matters:
//! the kernel was still there to run it.
//!
//! This is a counterfactual for a real panic. The `#PF` handler runs a
//! diagnostic that prints the faulting instruction's own bytes, and it read
//! them at `rip` - which on an *instruction-fetch* fault is the faulting
//! address itself, so the read faulted again from ring 0 and the machine died
//! with `KERNEL PANIC: #PF ...` while describing a user fault. A traced
//! `niri` hit it by jumping to a freed heap address. With the guard in place
//! the diagnostic cannot fault, and this program sees an ordinary `SIGSEGV`.
//!
//! Two faults, because the fixed one is *not* the common case and must not
//! break the common one: a jump into a `PROT_NONE` page (fetch), and a store
//! through a null pointer (write, where `rip`'s page is mapped and the
//! diagnostic's read was never in doubt).

#![no_std]
#![no_main]

#[path = "../../../lib/lin.rs"]
mod lin;

use core::panic::PanicInfo;

/// The signal the child is expected to die of.
const SIGSEGV: i32 = 11;

/// Fork a child, run `body` in it, and report whether it died of `SIGSEGV`.
/// `body` is expected to fault, so it never returns.
fn child_dies_of_segv(failed: &mut bool, label: &[u8], body: impl FnOnce()) {
    let pid = lin::fork();
    if pid < 0 {
        lin::check_true(failed, label, false);
        return;
    }
    if pid == 0 {
        body();
        // A body that returned did not fault, so say so with a code no
        // signal produces and let the parent's assertion fail.
        lin::exit(70);
    }
    let mut status = 0i32;
    let waited = lin::wait4(pid as usize, &raw mut status, 0);
    lin::check_eq(failed, label, waited, pid);
    // Linux's wait-status encoding: the low seven bits are the signal, and
    // zero there means an ordinary exit.
    lin::check_eq(
        failed,
        b"the child died of SIGSEGV, not of an exit code",
        (status & 0x7f) as isize,
        SIGSEGV as isize,
    );
}

#[unsafe(no_mangle)]
extern "C" fn mpos_user_main(_argc: usize, _argv: *const *const u8) -> ! {
    lin::puts(b"t-segv\n");
    let mut failed = false;

    // A page the process holds and may not touch, so the faulting address is
    // deterministic rather than whatever happens to be unmapped under ASLR.
    let guard = lin::mmap(
        0,
        4096,
        lin::PROT_NONE,
        lin::MAP_PRIVATE | lin::MAP_ANONYMOUS,
        -1,
        0,
    );
    lin::check_true(&mut failed, b"a PROT_NONE page was mapped", guard > 0);

    if guard > 0 {
        let entry = guard as usize;
        // The fetch fault: an indirect call into a page with no execute
        // permission. This is the shape that used to panic the kernel.
        child_dies_of_segv(
            &mut failed,
            b"a jump into a PROT_NONE page is SIGSEGV",
            || {
                let jump: extern "C" fn() = unsafe { core::mem::transmute(entry) };
                unsafe { jump() };
            },
        );
    }

    // The write fault, which the diagnostic always could describe: `rip` is
    // inside the program's own text, so reading the instruction there never
    // faults. It is here so a guard that refused to read *any* instruction
    // bytes - the wrong fix - would still be caught.
    child_dies_of_segv(
        &mut failed,
        b"a store through a null pointer is SIGSEGV",
        || {
            unsafe { core::ptr::write_volatile(0x10usize as *mut u8, 1) };
        },
    );

    // The machine is still here, which is the point of the program: both
    // children faulted inside the kernel's fault handler, and neither took the
    // kernel with it.
    lin::puts(b"ok: the machine survived both faults\n");

    if guard > 0 {
        lin::munmap(guard as usize, 4096);
    }

    lin::done(failed)
}

#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    lin::die(b"t-segv: panicked\n")
}
