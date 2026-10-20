//! The x86_64 signal frame: `struct rt_sigframe`, laid out for a handler to run
//! on and read back by `rt_sigreturn`.
//!
//! The layout is not a choice. The libc's `__restore_rt` trampoline issues
//! `rt_sigreturn` with `rsp` pointing just past `pretcode`, and this kernel
//! reads the saved registers from exactly where it expects them; a handler
//! written against `<signal.h>` reads the `siginfo_t` and the `ucontext` from
//! those same offsets. So the field order of `struct sigcontext`, the offsets
//! of the three parts of the frame and the register shuffling are this
//! machine's ABI, and they live here rather than in the syscall layer
//! (`NOTES.md` §13.2).
//!
//! What does *not* live here is policy: which stack the frame goes on (the
//! disposition's `SA_ONSTACK`), which mask is restored, where the bytes are
//! written and who the signal belongs to. Those are the caller's
//! (`syscall/linux/proc.rs`), and the two scheduler calls they need - reading
//! and writing the process's own memory, installing the handler's mask -
//! cannot be made from here, because `arch -> sched` is the one upward edge
//! the seam exists to keep out.

use crate::arch::x86_64::context::{FpuState, uregs};
use crate::hal::{
    Current, FPU_BYTES, SignalDelivery, SignalFrame, SignalFrameEntry, SignalFrameExit,
};

/// Byte offsets of the fields of `struct sigcontext`, in the Linux x86_64 order.
/// They are not a choice: `__restore_rt` hands the structure back to
/// `rt_sigreturn`, and this kernel reads the saved registers from exactly where
/// the libc expects them to be.
mod sc {
    pub const R8: usize = 0;
    pub const R9: usize = 8;
    pub const R10: usize = 16;
    pub const R12: usize = 32;
    pub const R13: usize = 40;
    pub const R14: usize = 48;
    pub const R15: usize = 56;
    pub const RDI: usize = 64;
    pub const RSI: usize = 72;
    pub const RBP: usize = 80;
    pub const RBX: usize = 88;
    pub const RDX: usize = 96;
    pub const RAX: usize = 104;
    pub const RSP: usize = 120;
    pub const RIP: usize = 128;
    pub const EFLAGS: usize = 136;
    /// The floating-point image is *not* inline in a `sigcontext`: this word is
    /// a pointer, and this kernel points it at the image it put in the frame
    /// (see `FPSTATE`). `offsetof(struct sigcontext, fpstate)` is 184 - the
    /// twenty-three general registers, then the four 16-bit segment words, then
    /// `err`/`trapno`/`oldmask`/`cr2` - measured on Linux 7.2.
    pub const FPSTATE: usize = 184;
}

/// The bytes of `struct rt_sigframe` on x86_64, which is what the kernel
/// pushes on a handler's stack:
///
/// ```text
///   0  char *pretcode      the restorer - where the handler returns to
///   8  struct ucontext     uc_flags, uc_link, stack_t, sigcontext, sigmask
/// 312  struct siginfo      signo, errno, code, and a lot of padding
/// 440  struct _fpstate     the interrupted x87/SSE image, 512 bytes
/// ```
///
/// The three parts are laid out the way the Linux ABI lays them out, because
/// the libc's `__restore_rt` trampoline is what reads them back: it issues
/// `rt_sigreturn` with `rsp` pointing just past `pretcode`, and the kernel
/// finds the saved registers at a fixed offset from there.
///
/// The `pretcode` word at the front is x86_64's way of saying where a handler
/// returns to, and the second machine says it differently rather than merely
/// elsewhere: aarch64 has no such word, and the restorer goes in `x30`. That is
/// why `SignalFrame::frame_at` exists - the *frame's* address is what the two
/// disagree about (`sp - 8` here, `sp` there) - and why `enter` takes the
/// restorer as an answer rather than as an offset.
///
/// The floating-point image is a fourth part here and a record *inside*
/// `uc_mcontext` on aarch64, and that is the ABI's split rather than this
/// kernel's: Linux's `sigcontext` carries an `fpstate` pointer, and the bytes it
/// names are the `fxsave`/`fxrstor` block verbatim - `struct _fpstate`'s field
/// order *is* the instruction's, measured by comparing a frame's 512 bytes with
/// an `fxsave` taken immediately before the trapped signal: they differ in
/// nothing but the 96 bytes of padding Linux uses for its XSAVE extension
/// record, which this machine has no extended state for (`.workbuddy/
/// probe_signal_fp2.c`). Those bytes stay zero here.
const SIGFRAME_BYTES: usize = FPSTATE + FPU_BYTES;
/// Offset of `struct ucontext` inside the frame - that is, `sizeof(char *)`.
const UCONTEXT: usize = 8;
/// Offset of `uc_mcontext` (a `struct sigcontext`) inside the `ucontext` - that
/// is, `sizeof(unsigned long) + sizeof(uc_link) + sizeof(stack_t)`.
const UCONTEXT_MCONTEXT: usize = 40;
/// Offset of the saved signal mask within the `ucontext`.
const UCONTEXT_SIGMASK: usize = 296;
/// Offset of the `struct siginfo` within the frame.
const SIGINFO: usize = 312;
/// Offset of the `_fpstate` within the frame: immediately after the
/// `siginfo_t`, which is 312 + 128. The frame's own address is 8 mod 16 by
/// construction (`enter` aligns `sp - SIGFRAME_BYTES` down and subtracts the
/// return-address word), and 440 is 8 mod 16 too, so this offset makes the image
/// 16-aligned - what `fxsave`/`fxrstor` require. Linux aligns it to 64 because
/// its area is an XSAVE area; there is no XSAVE state here to align for.
const FPSTATE: usize = 440;

/// A little-endian word into the frame, at a byte offset.
fn put(bytes: &mut [u8], at: usize, value: u64) {
    bytes[at..at + 8].copy_from_slice(&value.to_le_bytes());
}

/// A little-endian 32-bit field, for the `siginfo_t`'s pid and uid.
fn put_i32(bytes: &mut [u8], at: usize, value: i32) {
    bytes[at..at + 4].copy_from_slice(&value.to_le_bytes());
}

/// The inverse of [`put`].
fn get(bytes: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(bytes[at..at + 8].try_into().unwrap())
}

/// The signal frame, encoded and decoded for this machine.
///
/// The two operations are [`SignalFrame::enter`] and [`SignalFrame::exit`]'s
/// bodies: everything between the ABI's register file and the bytes a libc's
/// trampoline reads, and nothing else.
impl SignalFrame for Current {
    const FRAME_BYTES: usize = SIGFRAME_BYTES;

    fn frame_at(sp: usize) -> usize {
        // `__restore_rt` is reached by the handler's `ret`, which popped the
        // restorer word, so the frame starts eight bytes below where the
        // trampoline's `rt_sigreturn` finds the stack pointer.
        sp.wrapping_sub(8)
    }

    unsafe fn enter(
        regs: &mut [u64],
        out: &mut [u8],
        delivery: &SignalDelivery,
        frame: *mut (),
    ) -> Option<SignalFrameEntry> {
        // The gate frame carries nothing this layout needs: an x86_64
        // `sigcontext` names the floating-point image by pointer rather than
        // holding it, and the registers the pointer names are in the task's
        // record - saved there by the lazy-FPU scheme - so the image arrives in
        // `delivery`, not here. aarch64's `enter` is the one that reads the gate
        // frame: `SignalFrame::enter`'s `frame` parameter.
        let _ = frame;
        debug_assert_eq!(
            regs.len(),
            uregs::COUNT,
            "the ABI's register file is 27 words"
        );
        debug_assert!(
            out.len() >= SIGFRAME_BYTES,
            "the frame buffer is the frame's own size"
        );
        let out = &mut out[..SIGFRAME_BYTES];

        // The ABI makes `pretcode` mandatory on x86_64: without a restorer the
        // handler would return into whatever it found on the stack and the
        // process could never leave it at all, so there is no frame to write.
        // The caller has already folded the disposition's `SA_RESTORER` and a
        // zero address into this `None`.
        let restorer = delivery.restorer?;

        // A handler runs on the process's own stack, unless the disposition
        // asked for the alternate one. Either way the frame goes just below
        // where the stack pointer was: the interrupted code gets its memory
        // back untouched when the handler returns.
        let sp = if delivery.on_alt {
            delivery
                .alt
                .map_or(regs[uregs::RSP], |(base, size)| (base + size) as u64)
        } else {
            regs[uregs::RSP]
        };

        // A function is entered with its return address at `rsp % 16 == 8`; a
        // handler is entered the same way, with the restorer standing in for
        // the `call` that would have put it there.
        let frame = (((sp as usize) - SIGFRAME_BYTES) & !15) - 8;

        out.fill(0);
        // The restorer - where the handler's `ret` goes.
        put(out, 0, restorer as u64);

        // The saved register file. These are the ABI's own slots, which is why
        // the frame can be written from the register file rather than from the
        // gate frame's field order.
        let mcontext = UCONTEXT + UCONTEXT_MCONTEXT;
        put(out, mcontext + sc::R8, regs[uregs::R8]);
        put(out, mcontext + sc::R9, regs[uregs::R9]);
        put(out, mcontext + sc::R10, regs[uregs::R10]);
        put(out, mcontext + sc::R12, regs[uregs::R12]);
        put(out, mcontext + sc::R13, regs[uregs::R13]);
        put(out, mcontext + sc::R14, regs[uregs::R14]);
        put(out, mcontext + sc::R15, regs[uregs::R15]);
        put(out, mcontext + sc::RDI, regs[uregs::RDI]);
        put(out, mcontext + sc::RSI, regs[uregs::RSI]);
        put(out, mcontext + sc::RBP, regs[uregs::RBP]);
        put(out, mcontext + sc::RBX, regs[uregs::RBX]);
        put(out, mcontext + sc::RDX, regs[uregs::RDX]);
        // The interrupted call's answer, or - for a call the disposition asked
        // to run again - the call's number, which is what `syscall` reads when
        // the rewound frame resumes on it.
        put(
            out,
            mcontext + sc::RAX,
            delivery.restart.unwrap_or(regs[uregs::RAX]),
        );
        // `syscall` destroyed `rcx` and `r11` on the way in - it parked the
        // return address in `rcx` and the caller's flags in `r11` - so the
        // ABI's `rip`/`eflags` slots are where those two values live, and there
        // is nothing of the caller's left in the registers themselves to save.
        //
        // The `syscall` instruction is two bytes (`0f 05`), so the address the
        // CPU left in `rcx` is two past the instruction a restart resumes on.
        put(
            out,
            mcontext + sc::RIP,
            match delivery.restart {
                Some(_) => regs[uregs::RIP].wrapping_sub(2),
                None => regs[uregs::RIP],
            },
        );
        put(out, mcontext + sc::RSP, regs[uregs::RSP]);
        put(out, mcontext + sc::EFLAGS, regs[uregs::EFLAGS]);

        // `uc_stack` describes the alternate stack, as the handler will see it
        // through `sigaltstack(NULL, ...)`.
        let stack_at = UCONTEXT + 16;
        if let Some((base, size)) = delivery.alt {
            put(out, stack_at, base as u64);
            put(out, stack_at + 16, size as u64);
        }

        // The mask to put back on the way out - what the process had blocked
        // when the signal arrived. A libc reads this as the wire's `sigset_t`,
        // so it goes out through the same conversion `rt_sigprocmask` brings its
        // sets in through; `rt_sigreturn` converts on the way back.
        put(out, UCONTEXT + UCONTEXT_SIGMASK, delivery.blocked_wire);

        // The interrupted floating-point image, and the word of `sigcontext`
        // that names it. A handler is an asynchronous call, so the interrupted
        // code's x87/SSE registers have to survive it: without this the handler
        // computes in those registers and the code the signal interrupted
        // resumes with the handler's values. The image is the machine's own
        // `fxsave` block, byte for byte, because that is what `_fpstate` is
        // (see `SIGFRAME_BYTES`).
        //
        // A caller that could not produce an image gets the architectural reset
        // state rather than a zeroed block: `mxcsr` of zero unmasks every SSE
        // exception, so an all-zero image is not "nothing" but a machine waiting
        // to fault. The pointer is written either way - Linux's frames always
        // name an image, and a handler that reads `uc_mcontext.fpstate` must not
        // find a null one.
        put(out, mcontext + sc::FPSTATE, (frame + FPSTATE) as u64);
        match &delivery.fpu {
            Some(fpu) => out[FPSTATE..FPSTATE + FPU_BYTES].copy_from_slice(fpu),
            None => {
                out[FPSTATE..FPSTATE + FPU_BYTES].copy_from_slice(&FpuState::reset().to_bytes())
            }
        }

        // A `siginfo_t`: signo, errno, code, and - for a signal that names a
        // sender - the pid and uid, with the union at offset 24. The offsets
        // are Linux x86_64's, so a handler written against `<signal.h>` reads
        // the right fields.
        let info = &delivery.info;
        out[SIGINFO..SIGINFO + 4].copy_from_slice(&info.signo.to_le_bytes());
        out[SIGINFO + 4..SIGINFO + 8].copy_from_slice(&info.errno.to_le_bytes());
        out[SIGINFO + 8..SIGINFO + 12].copy_from_slice(&info.code.to_le_bytes());
        put_i32(out, SIGINFO + 16, info.pid);
        put_i32(out, SIGINFO + 20, info.uid);
        // Which union member that word is depends on the code's class, and the
        // rule is `hal`'s - a child's death puts `si_status` there, every other
        // sender puts the `sigval`.
        if info.chld_union() {
            put_i32(out, SIGINFO + 24, info.status);
            // And the rest of the `_sigchld` member: the child's CPU in
            // `USER_HZ` ticks, `si_utime` at offset 32 and `si_stime` at 40 -
            // Linux x86_64's own offsets, where the 8-byte `clock_t`s sit after
            // the 4-byte status. The frame buffer is zeroed before this runs, so
            // a record whose sender left them zero writes nothing here.
            put(out, SIGINFO + 32, info.utime);
            put(out, SIGINFO + 40, info.stime);
        } else {
            put(out, SIGINFO + 24, info.value);
        }

        // Enter the handler with the three arguments the ABI gives it: the
        // signal number, a `siginfo_t`, and the `ucontext` to hand back. The
        // caller turns this register file back into the gate frame, which is
        // where `sysretq` reads `rip`/`rsp`/`rflags` from.
        regs[uregs::RIP] = delivery.handler as u64;
        regs[uregs::RSP] = frame as u64;
        regs[uregs::RDI] = info.signo as u64;
        regs[uregs::RSI] = (frame + SIGINFO) as u64;
        regs[uregs::RDX] = (frame + UCONTEXT) as u64;

        Some(SignalFrameEntry {
            sp: frame,
            siginfo: frame + SIGINFO,
            ucontext: frame + UCONTEXT,
        })
    }

    unsafe fn exit(bytes: &[u8], regs: &mut [u64], frame: *mut ()) -> SignalFrameExit {
        // As in `enter`: nothing in this frame comes from the gate frame.
        let _ = frame;
        debug_assert_eq!(
            regs.len(),
            uregs::COUNT,
            "the ABI's register file is 27 words"
        );
        debug_assert!(
            bytes.len() >= SIGFRAME_BYTES,
            "the frame buffer is the frame's own size"
        );

        let mcontext = UCONTEXT + UCONTEXT_MCONTEXT;
        regs[uregs::R8] = get(bytes, mcontext + sc::R8);
        regs[uregs::R9] = get(bytes, mcontext + sc::R9);
        regs[uregs::R10] = get(bytes, mcontext + sc::R10);
        regs[uregs::R12] = get(bytes, mcontext + sc::R12);
        regs[uregs::R13] = get(bytes, mcontext + sc::R13);
        regs[uregs::R14] = get(bytes, mcontext + sc::R14);
        regs[uregs::R15] = get(bytes, mcontext + sc::R15);
        regs[uregs::RDI] = get(bytes, mcontext + sc::RDI);
        regs[uregs::RSI] = get(bytes, mcontext + sc::RSI);
        regs[uregs::RBP] = get(bytes, mcontext + sc::RBP);
        regs[uregs::RBX] = get(bytes, mcontext + sc::RBX);
        regs[uregs::RDX] = get(bytes, mcontext + sc::RDX);
        // `sysretq` takes the return address from `rcx` and the flags from
        // `r11`, and no saved value can go there: whatever the process had in
        // them before the `syscall` was destroyed by the instruction itself.
        // So the `sigcontext`'s `rip`/`eflags` slots are the only place those
        // two come back from, and the caller's `restore_user_regs` reads them
        // out of these ABI slots.
        regs[uregs::RIP] = get(bytes, mcontext + sc::RIP);
        regs[uregs::RSP] = get(bytes, mcontext + sc::RSP);
        // Interrupts have to be on when the process resumes; everything else
        // about the flags is the process's own business.
        regs[uregs::EFLAGS] = get(bytes, mcontext + sc::EFLAGS) | 0x200;
        regs[uregs::RAX] = get(bytes, mcontext + sc::RAX);

        // The floating-point image. A frame whose pointer is zero is Linux's
        // "reset it", not "leave the registers alone": measured, a handler that
        // nulls `uc_mcontext.fpstate` and returns comes back with zeroed xmm
        // registers and `mxcsr` 0x1f80, the architectural init state
        // (`.workbuddy/probe_signal_fp2.c`'s `NO_RESTORE` run). So the answer is
        // the reset image - the same one `Context::empty` hands a task that has
        // never run a floating-point instruction, rather than a second
        // definition of the same state.
        //
        // The bytes come from the frame's own area, which is where `enter` put
        // them. Linux reads through the pointer, wherever it points; every
        // kernel-built frame points at its own image, so a hand-built frame is
        // the only one that could tell the difference (see
        // `SignalFrameExit::fpu`).
        let fpu = Some(if get(bytes, mcontext + sc::FPSTATE) != 0 {
            let mut image = [0u8; FPU_BYTES];
            image.copy_from_slice(&bytes[FPSTATE..FPSTATE + FPU_BYTES]);
            image
        } else {
            FpuState::reset().to_bytes()
        });

        SignalFrameExit {
            blocked_wire: get(bytes, UCONTEXT + UCONTEXT_SIGMASK),
            fpu,
        }
    }
}
