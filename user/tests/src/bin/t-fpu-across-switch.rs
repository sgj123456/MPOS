//! t-fpu-across-switch - the floating-point registers belong to a *task*, not
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
//! write different bit patterns to the register, so a shared register file
//! shows up as a value that is neither one, never as a coincidence.
//! `sched_yield` rather than a timer makes the switch happen on purpose instead
//! of hoping for one.
//!
//! **Which register, and why it is not the same one on both machines.** The
//! claim is about the registers a *context switch* carries, and the two ports
//! carry different sets, each by its own ABI:
//!
//! * x86_64: `xmm7`. The switch saves the whole `FpuState`, so any lane would
//!   do, and this is the one whose corruption was actually observed.
//! * aarch64: `v15.d[0]`. The switch saves `q8..q15` and **not** `q0..q7`,
//!   because AAPCS64 makes the low eight caller-saved and the kernel's own code
//!   is entitled to use them as scratch (`arch/src/arch/aarch64/context.rs`).
//!   Pinning `v0` on that machine would be a claim about a register no kernel
//!   promises to carry, and it would fail for a reason that is not the bug
//!   this test exists for. `v15` is also call-preserved, which is what keeps
//!   the `sched_yield` in the loop from being the thing that moves it.

use mpos_user_tests::Report;

/// 1.5 and 0.25 as bit patterns. Chosen so the two are unmistakable in a
/// failure line and neither is zero: a zeroed block is what a missing restore
/// would also produce, and that should read as `CORRUPT`, not as a coincidence.
const PARENT_BITS: u64 = 0x3ff8_0000_0000_0000; // 1.5
const CHILD_BITS: u64 = 0x3fd0_0000_0000_0000; // 0.25

/// How many times each side gives up the CPU. Every yield is an opportunity for
/// the other task to run, so this is far more switches than the bug needs to
/// show itself; the number is small only because the guest runs without KVM.
const ROUNDS: usize = 400;

#[cfg(target_arch = "x86_64")]
mod lane {
    /// Write `bits` to `xmm7`, taking the value from a general-purpose register
    /// so the kernel's `soft-float` build does not have to pass a float in XMM.
    /// Rust's x86 inline assembly is Intel-syntax: destination first, no `%` on
    /// a register name.
    pub(super) fn store(bits: u64) {
        unsafe {
            core::arch::asm!(
                "movq xmm7, {v}",
                v = in(reg) bits,
                options(nostack, nomem, preserves_flags)
            );
        }
    }

    pub(super) fn load() -> u64 {
        let bits: u64;
        unsafe {
            core::arch::asm!(
                "movq {v}, xmm7",
                v = out(reg) bits,
                options(nostack, nomem, preserves_flags)
            );
        }
        bits
    }
}

#[cfg(target_arch = "aarch64")]
mod lane {
    /// The low 64 bits of `v15`: call-preserved by AAPCS64 and inside the
    /// `q8..q15` window this machine's switch saves. `v15` is named in the
    /// template rather than bound to an operand because Rust has no register
    /// class for it, and it is declared clobbered so the allocator knows the
    /// asm moves it.
    pub(super) fn store(bits: u64) {
        unsafe {
            core::arch::asm!(
                "mov v15.d[0], {v}",
                v = in(reg) bits,
                lateout("v15") _,
                options(nostack, nomem, preserves_flags)
            );
        }
    }

    pub(super) fn load() -> u64 {
        let bits: u64;
        unsafe {
            core::arch::asm!(
                "mov {v}, v15.d[0]",
                v = out(reg) bits,
                options(nostack, nomem, preserves_flags)
            );
        }
        bits
    }
}

fn main() {
    println!("t-fpu-across-switch");
    let mut r = Report::new();

    lane::store(PARENT_BITS);
    r.check(
        "a value written to the lane reads back before any switch",
        lane::load() == PARENT_BITS,
    );

    // Forked rather than spawned: a thread would be a second task on this
    // machine's own scheduler too, but the property is about the switch, and
    // the plain fork the original used is the one whose child shares nothing
    // but the CPU.
    let pid = unsafe { libc::fork() };
    if pid < 0 {
        r.check("fork succeeds", false);
        r.finish();
    }

    if pid == 0 {
        // The child: hammer its own lane and hand the CPU back, so every
        // resumption of the parent is a chance for the register file to have
        // been shared. It says nothing - its copy of the report is not the one
        // that is judged - and it leaves through `_exit` so that it takes no
        // share of the parent's buffered stdout with it.
        for _ in 0..ROUNDS {
            lane::store(CHILD_BITS);
            rustix::thread::sched_yield();
        }
        unsafe { libc::_exit(0) };
    }

    // The parent, whose value the child must not have been able to touch.
    let mut seen = PARENT_BITS;
    for _ in 0..ROUNDS {
        rustix::thread::sched_yield();
        seen = lane::load();
        if seen != PARENT_BITS {
            break;
        }
    }

    let mut status: libc::c_int = 0;
    let reaped = unsafe { libc::waitpid(pid, &mut status, 0) };
    r.check("waitpid reaps the child and returns its pid", reaped == pid);
    r.check(
        "the child exited normally, not by signal",
        libc::WIFEXITED(status),
    );

    // The heart of it. Reported as a plain equality against the pattern the
    // parent wrote, so a failure prints the value that displaced it - which is
    // what names the bug, and which of the two tasks displaced it.
    r.check(
        "the lane survived another task running (the register file is per task)",
        seen == PARENT_BITS,
    );

    r.finish();
}
