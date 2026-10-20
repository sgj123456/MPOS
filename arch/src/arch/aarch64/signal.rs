//! The aarch64 signal frame: `struct rt_sigframe`, laid out for a handler to run
//! on and read back by `rt_sigreturn`.
//!
//! The layout is not a choice. arm64's `rt_sigreturn` finds the frame at the
//! handler's own `sp`, and a handler written against `<signal.h>` reads its
//! `siginfo_t`, its `ucontext` and the saved registers through these offsets;
//! what makes them *these* offsets is a mixture of the uapi headers and C's
//! layout rules, so each one below says which.
//!
//! Three facts are worth stating here because they are where this machine
//! differs from the x86_64 twin, and each was read off the kernel rather than
//! assumed (`arch/arm64/kernel/signal.c` and the two uapi headers):
//!
//! * **the restorer is a register, not a word on the stack.** `setup_return`
//!   puts the disposition's `sa_restorer` (or, on Linux, the vDSO trampoline)
//!   in `x30`, and nothing is pushed - so `rt_sigreturn` is handed a stack
//!   pointer that *is* the frame, which is why [`SignalFrame::frame_at`] is the
//!   identity here and `sp - 8` on x86_64.
//! * **there is a `struct frame_record` above the frame**, 16 bytes of
//!   `{ fp, lr }` holding the interrupted `x29`/`x30`, with the handler's `x29`
//!   aimed at it. It is the unwind chain: a backtrace started inside a handler
//!   walks through it into the code the signal interrupted. It is part of the
//!   frame's *space* (`FRAME_BYTES` covers it) but not of its fields.
//! * **`uc_mcontext` is 8 bytes further out than the header's field widths
//!   suggest.** `struct sigcontext` contains a 16-byte-aligned member, so the
//!   structure is 16-aligned and the compiler pads it up to one inside the
//!   `ucontext` - measured against the headers: `uc_mcontext` is at 176, not at
//!   the 168 the fields add up to, and `ucontext` is 4560 bytes. The libc pads
//!   the same way (musl's own `ucontext_t` declares the reserved area as a
//!   `long double[256]`), which is what makes the handler's `ucontext` and the
//!   kernel's agree.
//!
//! What does *not* live here is policy: which stack the frame goes on (the
//! disposition's `SA_ONSTACK`), which mask is restored, where the bytes are
//! written and who the signal belongs to. Those are the caller's
//! (`syscall/linux/proc.rs`), and the two scheduler calls they need - reading
//! and writing the process's own memory, installing the handler's mask - cannot
//! be made from here, because `arch -> sched` is the one upward edge the seam
//! exists to keep out.

use crate::hal::{Current, SignalDelivery, SignalFrame, SignalFrameEntry, SignalFrameExit};

use super::context::{FPCR_OFFSET, FPSR_OFFSET};
use super::vectors::{TRAP_FPU_BYTES, TRAP_FPU_OFFSET, TRAP_ORIG_X0_OFFSET};

/// Byte offsets of the fields of `struct sigcontext`, from arm64's
/// `uapi/asm/sigcontext.h`.
mod sc {
    /// `fault_address`: the address a fault was taken on, zero for a signal
    /// nobody faulted on - which is every delivery this kernel makes.
    pub const FAULT_ADDRESS: usize = 0;
    /// `regs[31]`: the interrupted `x0..x30`, in the ABI's own order.
    pub const REGS: usize = 8;
    /// The interrupted `sp`, `pc` and `pstate`.
    pub const SP: usize = 256;
    pub const PC: usize = 264;
    pub const PSTATE: usize = 272;
    /// `__u8 __reserved[4096] __attribute__((__aligned__(16)))`, which starts at
    /// the next multiple of sixteen after `pstate` - the attribute is the whole
    /// of the eight bytes between them, and the region the signal's optional
    /// records live in.
    pub const RESERVED: usize = 288;
    pub const BYTES: usize = 288 + 4096;
}

/// Byte offsets inside `struct ucontext`, from arm64's `uapi/asm/ucontext.h`.
///
/// The fields before `uc_mcontext` are `uc_flags` (0), `uc_link` (8), the
/// `stack_t` of 16 and the `sigset_t` of 40; both of the first two are written
/// as zero and never read back (the kernel ignores them on a `rt_sigreturn`),
/// so only the one *inside* the frame - `uc_stack` - is named here.
mod uc {
    /// `stack_t`: `ss_sp`, `ss_flags`, `ss_size`.
    pub const STACK: usize = 16;
    /// `sigset_t`, which is one 8-byte word on this machine - the same size the
    /// syscall boundary uses.
    pub const SIGMASK: usize = 40;
    /// Where `uc_mcontext` starts: after the `__unused[1024 / 8 -
    /// sizeof(sigset_t)]` at 48 and the padding the module header explains.
    pub const MCONTEXT: usize = 176;
    pub const BYTES: usize = 176 + super::sc::BYTES;
}

/// `struct rt_sigframe { struct siginfo info; struct ucontext uc; }` - not a
/// uapi structure (the kernel builds it, the libc reads it), but a fixed shape
/// all the same: `siginfo_t` is 128 bytes on this machine, exactly as on x86_64
/// and as the kernel's own `static_assert`s say.
const SIGINFO: usize = 0;
const UCONTEXT: usize = 128;

/// The record Linux puts between the handler's stack and the interrupted frame:
/// `{ u64 fp; u64 lr; }`, holding the interrupted `x29` and `x30`. It sits at
/// the frame's own end, so its offset is the frame's size - the sigframe is
/// `[0, FRAME_RECORD)` and the record is the 16 bytes after it.
const FRAME_RECORD: usize = UCONTEXT + uc::BYTES;
const FRAME_RECORD_BYTES: usize = 16;

/// What [`enter`](SignalFrame::enter) writes, and what
/// [`exit`](SignalFrame::exit) reads back: the frame itself plus the record
/// above it. The handler's `sp` is the frame, so both halves are one block of
/// memory the caller writes and reads in one go.
const FRAME_BYTES: usize = FRAME_RECORD + FRAME_RECORD_BYTES;

/// The head of one record inside `uc_mcontext.__reserved`: `struct _aarch64_ctx
/// { __u32 magic; __u32 size; }`, 16-byte aligned, terminated by a zero head.
mod ctx {
    pub const MAGIC: usize = 0;
    pub const SIZE: usize = 4;
}

/// `FPSIMD_MAGIC`, the word that marks a floating-point record.
const FPSIMD_MAGIC: u32 = 0x4650_8001;

/// `struct fpsimd_context`: the head, then `fpsr` and `fpcr` as 32-bit words,
/// then thirty-two 16-byte `vregs` - 528 bytes, the same size as this machine's
/// own save area but *not* the same order.
mod fpsimd {
    pub const FPSR: usize = 8;
    pub const FPCR: usize = 12;
    pub const VREGS: usize = 16;
    pub const BYTES: usize = 16 + 512;
}

/// The register-file slots the frame's `regs[31]`/`sp`/`pc`/`pstate` are, in
/// the ABI order `hal::AbiRegs` carries (`user_pt_regs`: `x0..x30`, `sp`, `pc`,
/// `pstate`).
const ABI_X0: usize = 0;
const ABI_X29: usize = 29;
const ABI_X30: usize = 30;
const ABI_SP: usize = 31;
const ABI_PC: usize = 32;
const ABI_PSTATE: usize = 33;

/// A little-endian 64-bit word into the frame, at a byte offset.
fn put(bytes: &mut [u8], at: usize, value: u64) {
    bytes[at..at + 8].copy_from_slice(&value.to_le_bytes());
}

/// A little-endian 32-bit field, for `siginfo_t`'s pid and uid.
fn put_i32(bytes: &mut [u8], at: usize, value: i32) {
    bytes[at..at + 4].copy_from_slice(&value.to_le_bytes());
}

fn put_u32(bytes: &mut [u8], at: usize, value: u32) {
    bytes[at..at + 4].copy_from_slice(&value.to_le_bytes());
}

/// The inverse of [`put`].
fn get(bytes: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(bytes[at..at + 8].try_into().unwrap())
}

fn get_u32(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap())
}

/// The interrupted floating-point state, as the trap frame holds it
/// (`context::FpuState`: `q0..q31`, `FPCR`, `FPSR`), written into the record
/// (`fpsr`, `fpcr`, `vregs`) at the offset Linux reads it from.
///
/// The two shapes are the same bytes in two orders: the record's `vregs` are
/// the save area's first 512 bytes unchanged, and its `fpsr`/`fpcr` are the two
/// words after them, swapped. Writing `head` last is what Linux does, and it is
/// what makes a half-written record unreadable rather than plausible.
fn preserve_fpsimd(out: &mut [u8], at: usize, state: &[u8]) {
    out[at + fpsimd::VREGS..at + fpsimd::VREGS + 512].copy_from_slice(&state[..512]);
    put_u32(out, at + fpsimd::FPCR, u32_at(state, FPCR_OFFSET));
    put_u32(out, at + fpsimd::FPSR, u32_at(state, FPSR_OFFSET));
    put_u32(out, at + ctx::MAGIC, FPSIMD_MAGIC);
    put_u32(out, at + ctx::SIZE, fpsimd::BYTES as u32);
}

/// The reverse: read the record back into the save area's order.
fn restore_fpsimd(state: &mut [u8], record: &[u8]) {
    state[..512].copy_from_slice(&record[fpsimd::VREGS..fpsimd::VREGS + 512]);
    put_u32(state, FPCR_OFFSET, get_u32(record, fpsimd::FPCR));
    put_u32(state, FPSR_OFFSET, get_u32(record, fpsimd::FPSR));
}

/// A 32-bit register value out of the saved 528-byte area.
fn u32_at(state: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(state[at..at + 4].try_into().unwrap())
}

/// Where the interrupted floating-point save area is inside the gate frame: the
/// `UserFrame` the ABI reads, then the 528 bytes the trap stub saved.
///
/// The frame is the trap stub's own (`vectors.rs`) and its layout is the offset
/// asserts there. A null frame is a call the kernel made on its own behalf,
/// which has no trap frame.
///
/// # Safety
/// `frame` must be the gate frame for the calling task - the pointer
/// `hal::Gate` was handed - or null.
unsafe fn trap_fpu_ptr(frame: *mut ()) -> Option<*const u8> {
    if frame.is_null() {
        return None;
    }
    Some((frame as *const u8).wrapping_add(TRAP_FPU_OFFSET))
}

/// [`trap_fpu_ptr`]'s writing half, used by the restore path.
///
/// # Safety
/// As `trap_fpu_ptr`, and the caller must be the only live user of that area -
/// the `rt_sigreturn` handler, which runs on the trap the frame belongs to.
unsafe fn trap_fpu_ptr_mut(frame: *mut ()) -> Option<*mut u8> {
    if frame.is_null() {
        return None;
    }
    Some((frame as *mut u8).wrapping_add(TRAP_FPU_OFFSET))
}

/// The interrupted call's first argument, from the word beside the frame.
///
/// # Safety
/// As `trap_fpu`.
unsafe fn trap_orig_x0(frame: *mut ()) -> u64 {
    // SAFETY: the caller's contract, for the word after the save area.
    unsafe {
        core::ptr::read_unaligned((frame as *const u8).add(TRAP_ORIG_X0_OFFSET).cast::<u64>())
    }
}

/// The signal frame, encoded and decoded for this machine.
///
/// The two operations are [`SignalFrame::enter`] and [`SignalFrame::exit`]'s
/// bodies: everything between the ABI's register file and the bytes a libc's
/// trampoline reads, and nothing else.
impl SignalFrame for Current {
    const FRAME_BYTES: usize = FRAME_BYTES;

    fn frame_at(sp: usize) -> usize {
        // The handler's `sp` *is* the `rt_sigframe`: this machine's restorer is
        // in `x30`, so the trampoline pushes nothing before calling the kernel.
        // Linux's `sys_rt_sigreturn` reads `regs->sp` as the frame and checks it
        // is 16-aligned; this is the identity and not `sp - 8`.
        sp
    }

    unsafe fn enter(
        regs: &mut [u64],
        out: &mut [u8],
        delivery: &SignalDelivery,
        frame: *mut (),
    ) -> Option<SignalFrameEntry> {
        const ABI_WORDS: usize = 34;
        debug_assert_eq!(regs.len(), ABI_WORDS, "the ABI's register file is 34 words");
        debug_assert!(
            out.len() >= FRAME_BYTES,
            "the frame buffer is the frame's own size"
        );
        let out = &mut out[..FRAME_BYTES];

        // The handler returns through `x30`, and on Linux that is the
        // disposition's `sa_restorer` when it set `SA_RESTORER` and the vDSO's
        // trampoline otherwise. This kernel has no vDSO - `NOTES.md` records that
        // as a decision rather than an omission - so a disposition that names no
        // restorer has nothing to return into and there is no frame to write.
        // The caller has already folded the flag and a zero address into this
        // `None`.
        let restorer = delivery.restorer?;

        // The frame goes just below the interrupted stack pointer - or below the
        // alternate one's top, when the disposition asked for it - with one
        // 16-byte `frame_record` between it and the program's own stack. That is
        // `get_sigframe`'s arithmetic: `round_down(sp - 16, 16)` for the record,
        // then the frame below it. Both are 16-aligned, which the frame's own
        // `__reserved` region needs and which `rt_sigreturn` re-checks.
        let sp = if delivery.on_alt {
            delivery
                .alt
                .map_or(regs[ABI_SP], |(base, size)| (base + size) as u64)
        } else {
            regs[ABI_SP]
        };
        let record = ((sp as usize) & !15) - FRAME_RECORD_BYTES;
        let at = record - FRAME_RECORD;

        out.fill(0);

        // The unwind record: the interrupted frame pointer and link register,
        // where the handler's own `x29` will point.
        put(out, FRAME_RECORD, regs[ABI_X29]);
        put(out, FRAME_RECORD + 8, regs[ABI_X30]);

        // The saved register file, in the ABI's own order. `regs` *is* that file
        // (this machine's trap frame is `user_pt_regs` field for field), so the
        // thirty-one registers are a copy rather than a shuffle - except for the
        // one slot the interrupted *call* turned into an answer, below.
        for (i, value) in regs[..31].iter().enumerate() {
            put(out, UCONTEXT + uc::MCONTEXT + sc::REGS + i * 8, *value);
        }
        put(out, UCONTEXT + uc::MCONTEXT + sc::FAULT_ADDRESS, 0);
        put(out, UCONTEXT + uc::MCONTEXT + sc::SP, regs[ABI_SP]);
        // A call the disposition asked to run again is rewound onto its own
        // `svc`, which is four bytes (the twin's `syscall` is two): the trap left
        // `pc` one instruction past it, exactly as x86_64's `rcx` does.
        let pc = match delivery.restart {
            Some(_) => regs[ABI_PC].wrapping_sub(4),
            None => regs[ABI_PC],
        };
        put(out, UCONTEXT + uc::MCONTEXT + sc::PC, pc);
        // Linux does not reset this slot: the handler runs with the status word
        // the interrupted code had (it clears only the MTE/BTI bits, which this
        // machine does not implement).
        put(out, UCONTEXT + uc::MCONTEXT + sc::PSTATE, regs[ABI_PSTATE]);
        // `x0` is the call's *answer* - unless the call is being run again, in
        // which case it is the first argument it was made with, which the trap
        // kept beside the frame because this slot stopped holding it (`orig_x0`,
        // exactly as arm64's `pt_regs` keeps it).
        // SAFETY: the gate frame for this task, or null; the caller is the
        // signal handler's own delivery, on this task's own trap.
        let x0 = match delivery.restart {
            Some(_) => unsafe { trap_orig_x0(frame) },
            None => regs[ABI_X0],
        };
        put(out, UCONTEXT + uc::MCONTEXT + sc::REGS, x0);

        // `uc_flags`/`uc_link` stay zero: nothing here is a `makecontext` link,
        // and the kernel ignores both on the way back.
        let stack_at = UCONTEXT + uc::STACK;
        if let Some((base, size)) = delivery.alt {
            put(out, stack_at, base as u64);
            put(out, stack_at + 16, size as u64);
        }

        // The mask to put back on the way out: the wire's `sigset_t`, one 8-byte
        // word, the same shape `rt_sigprocmask` brings its sets in through.
        put(out, UCONTEXT + uc::SIGMASK, delivery.blocked_wire);

        // The floating-point record, in the region `__reserved` keeps for
        // exactly this, followed by the zero head that ends the chain - the
        // shape `parse_user_sigframe` walks.
        let reserved = UCONTEXT + uc::MCONTEXT + sc::RESERVED;
        // SAFETY: the gate frame for this task, as above, and its layout is the
        // one `vectors.rs` fixes: `TRAP_FPU_BYTES` from there are the trap's
        // own floating-point save.
        if let Some(state) = unsafe { trap_fpu_ptr(frame) } {
            let state = unsafe { core::slice::from_raw_parts(state, TRAP_FPU_BYTES) };
            preserve_fpsimd(out, reserved, state);
        }
        put_u32(out, reserved + fpsimd::BYTES + ctx::MAGIC, 0);
        put_u32(out, reserved + fpsimd::BYTES + ctx::SIZE, 0);

        // A `siginfo_t`: signo, errno, code, and - for a signal that names a
        // sender - the pid and uid, with the union at offset 24. These are the
        // offsets the kernel's own `static_assert`s pin for arm64, and the ones
        // x86_64 uses.
        let info = &delivery.info;
        out[SIGINFO..SIGINFO + 4].copy_from_slice(&info.signo.to_le_bytes());
        out[SIGINFO + 4..SIGINFO + 8].copy_from_slice(&info.errno.to_le_bytes());
        out[SIGINFO + 8..SIGINFO + 12].copy_from_slice(&info.code.to_le_bytes());
        put_i32(out, SIGINFO + 16, info.pid);
        put_i32(out, SIGINFO + 20, info.uid);
        // Which union member that word is depends on the code's class, and the
        // rule is `hal`'s: a child's death puts `si_status` there, every other
        // sender the `sigval`.
        if info.chld_union() {
            put_i32(out, SIGINFO + 24, info.status);
            // The rest of the `_sigchld` member: the child's CPU in `USER_HZ`
            // ticks, `si_utime` at 32 and `si_stime` at 40 - the same offsets
            // arm64's own `static_assert`s pin and x86_64 uses. The frame is
            // zeroed before this runs, so a zero sender writes nothing.
            put(out, SIGINFO + 32, info.utime);
            put(out, SIGINFO + 40, info.stime);
        } else {
            put(out, SIGINFO + 24, info.value);
        }

        // Enter the handler: the signal number in `x0`, the `siginfo_t` and the
        // `ucontext` in `x1`/`x2`, the restorer in `x30`, the unwind record in
        // `x29`, and `sp`/`pc` where the answer below says they are.
        regs[ABI_X0] = info.signo as u64;
        regs[1] = (at + SIGINFO) as u64;
        regs[2] = (at + UCONTEXT) as u64;
        regs[ABI_X29] = record as u64;
        regs[ABI_X30] = restorer as u64;
        regs[ABI_SP] = at as u64;
        regs[ABI_PC] = delivery.handler as u64;

        Some(SignalFrameEntry {
            sp: at,
            siginfo: at + SIGINFO,
            ucontext: at + UCONTEXT,
        })
    }

    unsafe fn exit(bytes: &[u8], regs: &mut [u64], frame: *mut ()) -> SignalFrameExit {
        const ABI_WORDS: usize = 34;
        debug_assert_eq!(regs.len(), ABI_WORDS, "the ABI's register file is 34 words");
        debug_assert!(
            bytes.len() >= FRAME_BYTES,
            "the frame buffer is the frame's own size"
        );

        let mcontext = UCONTEXT + uc::MCONTEXT;
        for (i, slot) in regs[..31].iter_mut().enumerate() {
            *slot = get(bytes, mcontext + sc::REGS + i * 8);
        }
        regs[ABI_SP] = get(bytes, mcontext + sc::SP);
        regs[ABI_PC] = get(bytes, mcontext + sc::PC);
        regs[ABI_PSTATE] = get(bytes, mcontext + sc::PSTATE);

        // The floating-point half goes back into the trap frame, and the trap's
        // own exit is what loads the registers from it - which is the only order
        // that works: this runs inside the syscall, and the kernel is still
        // entitled to use the vector registers between here and the `eret`.
        //
        // A frame with no `fpsimd` record leaves what the trap saved alone
        // rather than refusing the frame. Linux's `restore_sigframe` refuses it
        // (`-EINVAL`, then a SIGSEGV), so this is a named divergence - and it
        // matters only to a caller hand-building a frame, because every libc
        // writes the record.
        let reserved = mcontext + sc::RESERVED;
        if get_u32(bytes, reserved + ctx::MAGIC) == FPSIMD_MAGIC
            && get_u32(bytes, reserved + ctx::SIZE) as usize == fpsimd::BYTES
            // SAFETY: the gate frame for this task, whose layout `vectors.rs`
            // fixes; this is the `rt_sigreturn` trap it belongs to, and nothing
            // else is reading or writing that save area.
            && let Some(state) = unsafe { trap_fpu_ptr_mut(frame) }
        {
            let state = unsafe { core::slice::from_raw_parts_mut(state, TRAP_FPU_BYTES) };
            restore_fpsimd(state, &bytes[reserved..]);
        }

        // The image went into the trap frame, not into the register file, so
        // there is nothing for the caller to apply: `SignalFrameExit::fpu` is
        // x86_64's hand-back, where the image's only other home is the task's
        // record.
        SignalFrameExit {
            blocked_wire: get(bytes, UCONTEXT + uc::SIGMASK),
            fpu: None,
        }
    }
}
