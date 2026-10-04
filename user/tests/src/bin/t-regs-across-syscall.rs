//! t-regs-across-syscall - what a trap from EL0 gives back unchanged.
//!
//! A system call is a call, and the ABI's rule for a call is that it preserves
//! the callee-saved registers and is free to destroy the rest. The floating
//! point half is stronger than that, and on Linux it is a measured fact rather
//! than an ABI promise: a syscall does not touch the caller's SIMD/FP state at
//! all, because neither machine's kernel uses it to make the call. That is why
//! a program can keep a value in `v0` across a `read(2)` and a `movaps`/`v8`
//! value across a `getpid`, and why a libc's own `memcpy` is free to use all
//! thirty-two of them.
//!
//! The claim is only true while it is *held*. x86_64's kernel is built
//! `+soft-float` - there is no vector state in use anywhere in it - so its half
//! has been true since the port. aarch64's baseline includes NEON, and LLVM
//! keeps its own values in `q0..q31` there, so the machine's first years had a
//! kernel that destroyed the caller's SIMD state on every `svc`; this program is
//! the pin that says so if it comes back.
//!
//! Both halves are checked in one `asm!` block per machine, because that is the
//! whole point: a stamp, a Rust call and a check would be testing the compiler's
//! register allocator (the callee-saved registers are exactly where Rust keeps
//! what has to survive a call, and it is entitled to rewrite them in between).
//! Keeping stamp, trap and check inside one block also makes the integer half
//! test the *kernel* and not the ABI: the values never leave the registers the
//! trap is supposed to give back.
//!
//! What is deliberately not checked: the caller-saved registers, which a syscall
//! is free to destroy (`rax`/`rcx`/`r11` on x86_64). The pattern goes into the
//! callee-saved integer registers and into every vector register, which is the
//! pair the two machines agree on.

use std::process::exit;

/// The stamp every register is filled with. One value with two distinct halves
/// makes a register that was written from somewhere else visible as a mismatch
/// rather than as a coincidence.
const PATTERN: u64 = 0x0123_4567_89ab_cdef;

/// The register file after a syscall, as `(name, value)` pairs in the order the
/// checks are printed.
type Stamp = Vec<(&'static str, u64)>;

#[cfg(target_arch = "x86_64")]
fn across_a_syscall() -> Stamp {
    use std::arch::asm;

    // `libc::SYS_getpid` rather than a literal: the number is the target's own,
    // from a maintained table, which is the rule this crate's tests share.
    // `rbx` (and `rbp`) are left out on purpose: LLVM reserves `rbx` and
    // refuses it as an inline-asm operand, and `rbp` is the frame pointer. The
    // vector registers below are the half this pin is really about.
    let nr = libc::SYS_getpid as u64;
    let (r12, r13, r14, r15);
    let (x0, x1, x2, x3, x4, x5, x6, x7, x8, x9, x10, x11, x12, x13, x14, x15);
    // SAFETY: the block is one `syscall` instruction with a valid number, and
    // every register it touches is named as an operand. The kernel is entered
    // and left by the instruction itself.
    unsafe {
        asm!(
            "syscall",
            in("rax") nr,
            inout("r12") PATTERN => r12,
            inout("r13") PATTERN => r13,
            inout("r14") PATTERN => r14,
            inout("r15") PATTERN => r15,
            inout("xmm0") PATTERN => x0,
            inout("xmm1") PATTERN => x1,
            inout("xmm2") PATTERN => x2,
            inout("xmm3") PATTERN => x3,
            inout("xmm4") PATTERN => x4,
            inout("xmm5") PATTERN => x5,
            inout("xmm6") PATTERN => x6,
            inout("xmm7") PATTERN => x7,
            inout("xmm8") PATTERN => x8,
            inout("xmm9") PATTERN => x9,
            inout("xmm10") PATTERN => x10,
            inout("xmm11") PATTERN => x11,
            inout("xmm12") PATTERN => x12,
            inout("xmm13") PATTERN => x13,
            inout("xmm14") PATTERN => x14,
            inout("xmm15") PATTERN => x15,
            // The argument registers and the two `syscall` destroys. They are
            // clobbers rather than checks: nothing promises them back.
            out("rdi") _,
            out("rsi") _,
            out("rdx") _,
            out("r10") _,
            out("r8") _,
            out("r9") _,
            out("rcx") _,
            out("r11") _,
        );
    }
    let mut stamp: Stamp = vec![("r12", r12), ("r13", r13), ("r14", r14), ("r15", r15)];
    for (index, value) in [x0, x1, x2, x3, x4, x5, x6, x7].into_iter().enumerate() {
        stamp.push((
            match index {
                0 => "xmm0",
                1 => "xmm1",
                2 => "xmm2",
                3 => "xmm3",
                4 => "xmm4",
                5 => "xmm5",
                6 => "xmm6",
                _ => "xmm7",
            },
            value,
        ));
    }
    for (index, value) in [x8, x9, x10, x11, x12, x13, x14, x15]
        .into_iter()
        .enumerate()
    {
        stamp.push((
            match index {
                0 => "xmm8",
                1 => "xmm9",
                2 => "xmm10",
                3 => "xmm11",
                4 => "xmm12",
                5 => "xmm13",
                6 => "xmm14",
                _ => "xmm15",
            },
            value,
        ));
    }
    stamp
}

#[cfg(target_arch = "aarch64")]
fn across_a_syscall() -> Stamp {
    use std::arch::asm;

    // `x19` is left out on purpose: LLVM reserves it and refuses it as an
    // inline-asm operand, so no block here can stamp or read it. `x20..x28` is
    // the rest of the callee-saved set, and the vector registers below are the
    // half that this pin is really about.
    let nr = libc::SYS_getpid as u64;
    let (r20, r21, r22, r23, r24, r25, r26, r27, r28);
    let (
        v0,
        v1,
        v2,
        v3,
        v4,
        v5,
        v6,
        v7,
        v8,
        v9,
        v10,
        v11,
        v12,
        v13,
        v14,
        v15,
        v16,
        v17,
        v18,
        v19,
        v20,
        v21,
        v22,
        v23,
        v24,
        v25,
        v26,
        v27,
        v28,
        v29,
        v30,
        v31,
    );
    // SAFETY: the block is one `svc` with a valid number in `x8`, and every
    // register it touches is named as an operand.
    unsafe {
        asm!(
            "svc #0",
            in("x8") nr,
            inout("x20") PATTERN => r20,
            inout("x21") PATTERN => r21,
            inout("x22") PATTERN => r22,
            inout("x23") PATTERN => r23,
            inout("x24") PATTERN => r24,
            inout("x25") PATTERN => r25,
            inout("x26") PATTERN => r26,
            inout("x27") PATTERN => r27,
            inout("x28") PATTERN => r28,
            inout("v0") PATTERN => v0,
            inout("v1") PATTERN => v1,
            inout("v2") PATTERN => v2,
            inout("v3") PATTERN => v3,
            inout("v4") PATTERN => v4,
            inout("v5") PATTERN => v5,
            inout("v6") PATTERN => v6,
            inout("v7") PATTERN => v7,
            inout("v8") PATTERN => v8,
            inout("v9") PATTERN => v9,
            inout("v10") PATTERN => v10,
            inout("v11") PATTERN => v11,
            inout("v12") PATTERN => v12,
            inout("v13") PATTERN => v13,
            inout("v14") PATTERN => v14,
            inout("v15") PATTERN => v15,
            inout("v16") PATTERN => v16,
            inout("v17") PATTERN => v17,
            inout("v18") PATTERN => v18,
            inout("v19") PATTERN => v19,
            inout("v20") PATTERN => v20,
            inout("v21") PATTERN => v21,
            inout("v22") PATTERN => v22,
            inout("v23") PATTERN => v23,
            inout("v24") PATTERN => v24,
            inout("v25") PATTERN => v25,
            inout("v26") PATTERN => v26,
            inout("v27") PATTERN => v27,
            inout("v28") PATTERN => v28,
            inout("v29") PATTERN => v29,
            inout("v30") PATTERN => v30,
            inout("v31") PATTERN => v31,
            // `x0` is the call's own answer and `x1..x5` its arguments;
            // `x9..x18` and `x29`/`x30` are the registers the compiler may not
            // keep anything in across an `asm!` block unless asked to.
            out("x0") _,
            out("x1") _,
            out("x2") _,
            out("x3") _,
            out("x4") _,
            out("x5") _,
            out("x6") _,
            out("x7") _,
            out("x9") _,
            out("x10") _,
            out("x11") _,
            out("x12") _,
            out("x13") _,
            out("x14") _,
            out("x15") _,
            out("x16") _,
            out("x17") _,
            out("x18") _,
            out("x30") _,
        );
    }
    let mut stamp: Stamp = vec![
        ("x20", r20),
        ("x21", r21),
        ("x22", r22),
        ("x23", r23),
        ("x24", r24),
        ("x25", r25),
        ("x26", r26),
        ("x27", r27),
        ("x28", r28),
    ];
    let names = [
        "v0", "v1", "v2", "v3", "v4", "v5", "v6", "v7", "v8", "v9", "v10", "v11", "v12", "v13",
        "v14", "v15", "v16", "v17", "v18", "v19", "v20", "v21", "v22", "v23", "v24", "v25", "v26",
        "v27", "v28", "v29", "v30", "v31",
    ];
    let values = [
        v0, v1, v2, v3, v4, v5, v6, v7, v8, v9, v10, v11, v12, v13, v14, v15, v16, v17, v18, v19,
        v20, v21, v22, v23, v24, v25, v26, v27, v28, v29, v30, v31,
    ];
    for (name, value) in names.into_iter().zip(values) {
        stamp.push((name, value));
    }
    stamp
}

fn main() {
    println!("t-regs-across-syscall");
    let stamp = across_a_syscall();
    let count = stamp.len();
    let broken: Vec<String> = stamp
        .iter()
        .filter(|(_, got)| *got != PATTERN)
        .map(|(name, got)| format!("{name} came back {got:#018x}"))
        .collect();
    if broken.is_empty() {
        println!("ok:   all {count} registers still held {PATTERN:#018x} across a getpid");
        println!("t-regs-across-syscall: all passed");
        return;
    }
    println!(
        "FAIL: the trap did not give back {} of {count} registers: {}",
        broken.len(),
        broken.join(", ")
    );
    println!("t-regs-across-syscall: FAILED");
    exit(1);
}
