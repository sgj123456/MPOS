//! t-fpu_across_switch - the floating-point registers belong to a *task*, not
//! to the CPU.
//!
//! Nothing in the syscall ABI mentions XMM, so a missing save/restore is
//! invisible to every other test here: the kernel's own code is built with
//! `soft-float` and never touches those registers, so a task only notices the
//! sharing when *another* task runs between two of its own uses.
//!
//! That is exactly how it was found. musl's `__init_tp` builds `{td, td}` in
//! `xmm0`, keeps it live across the `set_tid_address` syscall, and stores it
//! over the thread list's `prev`/`next` - so a preemption inside that window
//! zeroed the links, `pthread_key_delete` walked off the end of the ring, and
//! `fish` took a write fault on its own read-only first page forever.
//!
//! The test states the property directly and *discriminatingly*: two tasks
//! write different bit patterns to `xmm7`, so a shared register file shows up
//! as a value that is neither one, never as a coincidence. `sched_yield` rather
//! than a timer makes the switch happen on purpose instead of hoping for one.

#![no_std]
#![no_main]

#[path = "../../lib/lin.rs"]
mod lin;

use core::arch::asm;
use core::panic::PanicInfo;

/// 1.5 and 0.25 as bit patterns. Chosen so the two are unmistakable in a
/// failure line and neither is zero: a zeroed block is what a missing restore
/// would also produce, and that should read as `CORRUPT`, not as a coincidence.
const PARENT_BITS: u64 = 0x3ff8_0000_0000_0000; // 1.5
const CHILD_BITS: u64 = 0x3fd0_0000_0000_0000; // 0.25

/// How many times each side gives up the CPU. Every yield is an opportunity for
/// the other task to run, so this is far more switches than the bug needs to
/// show itself; the number is small only because the guest runs without KVM.
const ROUNDS: usize = 400;

/// Write `bits` to `xmm7`, taking the value from a general-purpose register so
/// the kernel's `soft-float` build does not have to pass a float in XMM. Rust's
/// x86 inline assembly is Intel-syntax: destination first, no `%` on a register
/// name.
fn store_xmm7(bits: u64) {
    unsafe {
        asm!("movq xmm7, {v}", v = in(reg) bits, options(nostack, nomem, preserves_flags));
    }
}

/// Read `xmm7` back out.
fn load_xmm7() -> u64 {
    let bits: u64;
    unsafe {
        asm!("movq {v}, xmm7", v = out(reg) bits, options(nostack, nomem, preserves_flags));
    }
    bits
}

/// The bit pattern the parent's register held when it first disagreed with
/// `PARENT_BITS`, or `PARENT_BITS` if it never did.
static mut CORRUPT: u64 = PARENT_BITS;

#[unsafe(no_mangle)]
extern "C" fn mpos_user_main(_argc: usize, _argv: *const *const u8) -> ! {
    lin::puts(b"t-fpu_across_switch\n");
    let mut failed = false;

    store_xmm7(PARENT_BITS);
    lin::check_eq(
        &mut failed,
        b"a value written to xmm7 reads back before any switch",
        load_xmm7() as isize,
        PARENT_BITS as isize,
    );

    let pid = lin::fork();
    if pid < 0 {
        lin::check_true(&mut failed, b"fork succeeds", false);
        lin::done(failed);
    }

    if pid == 0 {
        // The child: hammer its own xmm7 and hand the CPU back, so every
        // resumption of the parent is a chance for the register file to have
        // been shared. It says nothing - its stdout copy of `failed` is not the
        // one that is judged - and leaves the parent to compare.
        for _ in 0..ROUNDS {
            store_xmm7(CHILD_BITS);
            lin::sched_yield();
        }
        lin::exit_group(0);
    }

    // The parent, whose value the child must not have been able to touch.
    for _ in 0..ROUNDS {
        lin::sched_yield();
        let seen = load_xmm7();
        if seen != PARENT_BITS {
            unsafe { CORRUPT = seen };
            break;
        }
    }

    let mut status: i32 = 0;
    let rc = lin::wait4(pid as usize, &mut status, 0);
    lin::check_eq(
        &mut failed,
        b"wait4 reaps the child and returns its pid",
        rc,
        pid,
    );
    lin::check_true(
        &mut failed,
        b"the child exited normally, not by signal",
        lin::wifexited(status),
    );

    // The heart of it. Reported as a plain equality so the failure line carries
    // the value that displaced the parent's - which is what names the bug.
    lin::check_eq(
        &mut failed,
        b"xmm7 survived another task running (the register file is per task)",
        unsafe { CORRUPT } as isize,
        PARENT_BITS as isize,
    );

    lin::done(failed)
}

#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    lin::die(b"t-fpu_across_switch: panicked\n")
}
