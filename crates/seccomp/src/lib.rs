#![no_std]

//! `SECCOMP_MODE_FILTER`: the classic-BPF program a thread can put between
//! itself and the syscall table.
//!
//! A filter is a program over `struct seccomp_data` - the call's number, the
//! architecture it was made on, the instruction pointer it came from, and its
//! six arguments - whose *return value* is what may happen to the call: let it
//! through, fail it with an errno, kill the thread, and so on. The instruction
//! set is classic BPF as the socket filters use it, with the packet idioms left
//! out - there is no packet here, so `BPF_MSH` is not an instruction - and the
//! loads that do exist read the *little-endian* field of the data block rather
//! than a header's big-endian word. That is not a liberty: it is what the
//! kernel's own classic-to-eBPF migration makes of these loads, and therefore
//! what every filter actually written expects - `libseccomp`'s generated code
//! compares `nr` against the syscall number directly.
//!
//! The rules held here are the kernel's, and each is asserted below: a program
//! is 1 to [`MAX_INSNS`] instructions and must *end* with a return; jumps go
//! forward and land inside the program; an instruction this module does not
//! implement is refused rather than treated as a jump to nowhere; a load past
//! the end of the data block ends the program with a return of 0, which the
//! action table reads as `KILL_THREAD`; and when filters are *stacked* the
//! answer is the most restrictive of them - compared as a *signed* action, so
//! that `KILL_PROCESS` (`0x8000_0000`) outranks everything instead of sorting
//! above `ALLOW`.
//!
//! Nothing here knows what a task is. The program, its validation and its
//! evaluation are the testable half and live in this module; the task table
//! decides who has one, and the syscall gate decides when it runs.

extern crate alloc;

use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, Ordering};

/// Whether *any* thread on this machine has ever installed a filter.
///
/// The syscall gate asks this before it looks anything up, and that ordering is
/// the point: a machine that never installs one - which is every boot in this
/// image, and every process that does not sandbox itself - pays for the feature
/// with one relaxed load per syscall rather than with the table lock and an
/// empty-list test. It cannot be cleared, exactly like the filter it stands
/// for, so it can only ever err towards doing the check.
static ARMED: AtomicBool = AtomicBool::new(false);

/// Record that a filter now exists. Called once per install, and never undone:
/// see [`ARMED`].
pub fn arm() {
    ARMED.store(true, Ordering::Relaxed);
}

/// Whether the gate has any reason to look at a thread's filters.
pub fn any_installed() -> bool {
    ARMED.load(Ordering::Relaxed)
}

/// Linux's `BPF_MAXINSNS`: the ceiling on one program's length.
pub const MAX_INSNS: usize = 4096;

/// `struct seccomp_data`, the block a filter reads: the number and the
/// architecture in one 32-bit word each, then the instruction pointer, then the
/// six arguments - all eight-byte, all little-endian like everything else this
/// kernel runs.
pub const DATA_BYTES: usize = 64;

/// `AUDIT_ARCH_X86_64`. A filter starts by checking this, because its numbers
/// are the ones it was compiled for; a call made in another ABI (a 32-bit
/// `int 0x80` on a machine that has one) must not be filtered by them.
pub const ARCH: u32 = 0xC000_003E;

/// The action field: the high half of a filter's return value. The low half is
/// the action's own data - an errno, a signal, a tracer message.
const ACTION_FULL: u32 = 0xffff_0000;

/// Kill the whole process group, not just this thread.
pub const RET_KILL_PROCESS: u32 = 0x8000_0000;
/// Kill this thread. Zero, and so what an out-of-range load returns.
pub const RET_KILL_THREAD: u32 = 0x0000_0000;
/// Raise `SIGSYS` in the caller, carrying the call's number.
pub const RET_TRAP: u32 = 0x0003_0000;
/// Fail the call with the errno in the low half.
pub const RET_ERRNO: u32 = 0x0005_0000;
/// Hand the call to a userspace supervisor; nothing here listens.
pub const RET_USER_NOTIF: u32 = 0x7fc0_0000;
/// Hand the call to a tracer; with no tracer attached the kernel allows it.
pub const RET_TRACE: u32 = 0x7ff0_0000;
/// Allow the call, having said so.
pub const RET_LOG: u32 = 0x7ffc_0000;
/// Allow the call.
pub const RET_ALLOW: u32 = 0x7fff_0000;

/// The class and its operands, as `linux/bpf_common.h` spells them.
const CLASS_MASK: u16 = 0x0007;
const LD: u16 = 0x00;
const LDX: u16 = 0x01;
const ST: u16 = 0x02;
const STX: u16 = 0x03;
const ALU: u16 = 0x04;
const JMP: u16 = 0x05;
const RET: u16 = 0x06;
const MISC: u16 = 0x07;

const SIZE_MASK: u16 = 0x0018;
const W: u16 = 0x00;
const H: u16 = 0x08;
const B: u16 = 0x10;

const MODE_MASK: u16 = 0x00e0;
const IMM: u16 = 0x00;
const ABS: u16 = 0x20;
const IND: u16 = 0x40;
const MEM: u16 = 0x60;
const LEN: u16 = 0x80;
/// `BPF_MSH` - the packet-header walk. Deliberately not implemented: there is
/// no packet, so a program naming it is refused rather than quietly doing
/// something else.
const MSH: u16 = 0xa0;

const OP_MASK: u16 = 0x00f0;
const SOURCE_MASK: u16 = 0x0008;
const K_SRC: u16 = 0x00;
const X_SRC: u16 = 0x08;
/// `BPF_RET | BPF_A`, whose code is its own because the `A` spelling is the
/// `0x10` bit rather than one of the K/X pair.
const RETURN_A: u16 = RET | 0x10;

/// The arithmetic operations, in `op`'s field.
const ADD: u16 = 0x00;
const SUB: u16 = 0x10;
const MUL: u16 = 0x20;
const DIV: u16 = 0x30;
const OR: u16 = 0x40;
const AND: u16 = 0x50;
const LSH: u16 = 0x60;
const RSH: u16 = 0x70;
/// Negate, which has no `X` form - it has no operand to take from the index.
const NEG: u16 = 0x80;
const MOD: u16 = 0x90;
const XOR: u16 = 0xa0;

/// The jump conditions.
const JA: u16 = 0x00;
const JEQ: u16 = 0x10;
const JGT: u16 = 0x20;
const JGE: u16 = 0x30;
const JSET: u16 = 0x40;

/// Move the accumulator to the index and back.
const TAX: u16 = 0x00;
const TXA: u16 = 0x80;

/// The scratch words a program can keep between instructions, which is
/// `BPF_MEMWORDS` of them, addressed modulo their count as the kernel's
/// interpreter does.
const MEM_WORDS: usize = 16;

/// One instruction: the class and operands, the two forward jump offsets, and
/// the immediate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Insn {
    pub code: u16,
    pub jt: u8,
    pub jf: u8,
    pub k: u32,
}

/// Why a program was refused. The caller turns these into the errno the ABI
/// asks for; keeping them apart is what lets a host test say which rule caught
/// it rather than only that something did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Invalid {
    /// No instructions at all.
    Empty,
    /// More than [`MAX_INSNS`].
    TooLong,
    /// The last instruction is not a return, so the program could run off its
    /// own end.
    NotTerminated,
    /// A jump out of the program, forward or back.
    JumpOutOfRange,
    /// An instruction this module does not implement.
    UnknownInstruction,
}

/// A validated filter. Constructing one is the only way to get one, and
/// [`Program::parse`] is the only constructor, so the interpreter below may
/// assume every jump lands inside it and every instruction is one it knows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Program {
    insns: Vec<Insn>,
}

impl Program {
    /// Read a program out of the bytes a caller handed the kernel - the
    /// `sock_filter` array behind a `sock_fprog`.
    pub fn parse(bytes: &[u8]) -> Result<Program, Invalid> {
        if bytes.is_empty() {
            return Err(Invalid::Empty);
        }
        if bytes.len() % 8 != 0 {
            // A truncated instruction is not a program with a spare half.
            return Err(Invalid::UnknownInstruction);
        }
        let count = bytes.len() / 8;
        if count > MAX_INSNS {
            return Err(Invalid::TooLong);
        }
        let mut insns = Vec::with_capacity(count);
        for chunk in bytes.chunks_exact(8) {
            insns.push(Insn {
                code: u16::from_le_bytes([chunk[0], chunk[1]]),
                jt: chunk[2],
                jf: chunk[3],
                k: u32::from_le_bytes([chunk[4], chunk[5], chunk[6], chunk[7]]),
            });
        }

        // Every instruction has to be one this module implements - the
        // interpreter's fallback arm is for the impossible case, not for a
        // filter that named something unsupported.
        for insn in &insns {
            if !known(insn.code) {
                return Err(Invalid::UnknownInstruction);
            }
        }
        // And the program has to end by returning: the last instruction is the
        // only one guaranteed to be reachable for every input.
        if insns[count - 1].code & CLASS_MASK != RET {
            return Err(Invalid::NotTerminated);
        }
        for (pc, insn) in insns.iter().enumerate() {
            // Classic BPF jumps forwards only, so the bound is the end of the
            // program: a jump to `count` or past it has nothing to land on.
            let land = |offset: usize| pc + 1 + offset < count;
            let ok = match insn.code & CLASS_MASK {
                JMP if insn.code & OP_MASK == 0x00 => {
                    // `BPF_JA`: `k` is the offset, and it is unsigned.
                    land(insn.k as usize)
                }
                JMP => land(insn.jt as usize) && land(insn.jf as usize),
                _ => true,
            };
            if !ok {
                return Err(Invalid::JumpOutOfRange);
            }
        }
        Ok(Program { insns })
    }

    /// Run the filter over one call and answer its action.
    ///
    /// Total, and it must be: this runs on every syscall of a filtered thread,
    /// so an input no program can take - an out-of-range load, a division by
    /// zero, a shift past the accumulator - has to end the program rather than
    /// the kernel. Each of those returns [`RET_KILL_THREAD`], which is both the
    /// kernel's answer (a load out of range returns 0, and 0 is that action)
    /// and the safe one to be wrong in.
    pub fn run(&self, data: &[u8; DATA_BYTES]) -> u32 {
        let mut a: u32 = 0;
        let mut x: u32 = 0;
        let mut mem = [0u32; MEM_WORDS];
        let mut pc = 0usize;
        loop {
            let Some(insn) = self.insns.get(pc) else {
                return RET_KILL_THREAD;
            };
            let (k, jt, jf) = (insn.k, insn.jt as usize, insn.jf as usize);
            let code = insn.code;
            let (class, size, mode, op, src) = (
                code & CLASS_MASK,
                code & SIZE_MASK,
                code & MODE_MASK,
                code & OP_MASK,
                code & SOURCE_MASK,
            );
            match class {
                LD => match (size, mode) {
                    (W, IMM) | (H, IMM) | (B, IMM) => a = imm(k, size),
                    (W, ABS) | (H, ABS) | (B, ABS) => match load(data, k as usize, size) {
                        Some(v) => a = v,
                        None => return RET_KILL_THREAD,
                    },
                    (W, IND) | (H, IND) | (B, IND) => {
                        match load(data, x.wrapping_add(k) as usize, size) {
                            Some(v) => a = v,
                            None => return RET_KILL_THREAD,
                        }
                    }
                    (W, MEM) => a = mem[(k as usize) % MEM_WORDS],
                    (_, LEN) => a = DATA_BYTES as u32,
                    _ => return RET_KILL_THREAD,
                },
                LDX => match (size, mode) {
                    (W, IMM) => x = k,
                    (_, LEN) => x = DATA_BYTES as u32,
                    (W, MEM) => x = mem[(k as usize) % MEM_WORDS],
                    _ => return RET_KILL_THREAD,
                },
                ST => mem[(k as usize) % MEM_WORDS] = a,
                STX => mem[(k as usize) % MEM_WORDS] = x,
                ALU if src == K_SRC => match op {
                    ADD => a = a.wrapping_add(k),
                    SUB => a = a.wrapping_sub(k),
                    MUL => a = a.wrapping_mul(k),
                    DIV => {
                        if k == 0 {
                            return RET_KILL_THREAD;
                        }
                        a /= k;
                    }
                    OR => a |= k,
                    AND => a &= k,
                    LSH => a <<= k & 31,
                    RSH => a >>= k & 31,
                    NEG => a = a.wrapping_neg(),
                    MOD => {
                        if k == 0 {
                            return RET_KILL_THREAD;
                        }
                        a %= k;
                    }
                    XOR => a ^= k,
                    _ => return RET_KILL_THREAD,
                },
                ALU => match op {
                    ADD => a = a.wrapping_add(x),
                    SUB => a = a.wrapping_sub(x),
                    MUL => a = a.wrapping_mul(x),
                    DIV => {
                        if x == 0 {
                            return RET_KILL_THREAD;
                        }
                        a /= x;
                    }
                    OR => a |= x,
                    AND => a &= x,
                    LSH => a <<= x & 31,
                    RSH => a >>= x & 31,
                    MOD => {
                        if x == 0 {
                            return RET_KILL_THREAD;
                        }
                        a %= x;
                    }
                    XOR => a ^= x,
                    _ => return RET_KILL_THREAD,
                },
                JMP => {
                    // `BPF_JA` is the one jump with no condition: `k` is the
                    // offset and both `jt`/`jf` are ignored.
                    if op == JA {
                        pc += 1 + k as usize;
                        continue;
                    }
                    let taken = match (op, src) {
                        (JEQ, K_SRC) => a == k,
                        (JEQ, _) => a == x,
                        (JGT, K_SRC) => a > k,
                        (JGT, _) => a > x,
                        (JGE, K_SRC) => a >= k,
                        (JGE, _) => a >= x,
                        (JSET, K_SRC) => a & k != 0,
                        (JSET, _) => a & x != 0,
                        _ => return RET_KILL_THREAD,
                    };
                    pc += 1 + if taken { jt } else { jf };
                    continue;
                }
                // Three returns, and the three codes are distinct: `ret k` is
                // the common one, `ret a` is how a computed action arrives, and
                // `ret x` the index. Matching on `src` alone would fold
                // `ret a` into `ret k`, since neither sets the `x` bit.
                RET if code == RETURN_A => return a,
                RET if src == X_SRC => return x,
                RET => return k,
                MISC if op == TAX => x = a,
                MISC if op == TXA => a = x,
                _ => return RET_KILL_THREAD,
            }
            pc += 1;
        }
    }
}

impl Program {
    /// The validator's view of one code. Written as a match on the same fields
    /// the interpreter switches on, so an instruction added to one and not the
    /// other is a compile error rather than a hole.
    fn known(code: u16) -> bool {
        let (class, size, mode, op, src) = (
            code & CLASS_MASK,
            code & SIZE_MASK,
            code & MODE_MASK,
            code & OP_MASK,
            code & SOURCE_MASK,
        );
        match class {
            LD => matches!(
                (size, mode),
                (W | H | B, IMM) | (W | H | B, ABS) | (W | H | B, IND) | (W, MEM) | (_, LEN)
            ),
            LDX => matches!((size, mode), (W, IMM) | (W, MEM) | (_, LEN)),
            ST | STX => true,
            ALU => {
                matches!(
                    op,
                    ADD | SUB | MUL | DIV | OR | AND | LSH | RSH | NEG | MOD | XOR
                ) && (src == K_SRC || (src == X_SRC && op != NEG))
            }
            JMP => {
                code == (JMP | JA)
                    || matches!(op, JEQ | JGT | JGE | JSET) && (src == K_SRC || src == X_SRC)
            }
            RET => code == RET || code == RETURN_A || code == (RET | X_SRC),
            MISC => op == TAX || op == TXA,
            _ => false,
        }
    }
}

/// The validator's copy, so `Program::parse` can ask without an instance.
fn known(code: u16) -> bool {
    Program::known(code)
}

/// An immediate, in the size the instruction asks for: `BPF_LD | BPF_H | BPF_IMM`
/// is the low half, not a two-byte jump into the stack's high bytes.
fn imm(k: u32, size: u16) -> u32 {
    match size {
        H => k & 0xffff,
        B => k & 0xff,
        _ => k,
    }
}

/// One little-endian load out of the data block, or `None` when it runs past
/// the end - which the caller turns into `KILL_THREAD`, as the kernel's own
/// `load_pointer` returning NULL does.
fn load(data: &[u8; DATA_BYTES], offset: usize, size: u16) -> Option<u32> {
    let n = match size {
        H => 2,
        B => 1,
        _ => 4,
    };
    let end = offset.checked_add(n)?;
    if end > DATA_BYTES {
        return None;
    }
    let mut word = [0u8; 4];
    word[..n].copy_from_slice(&data[offset..end]);
    Some(u32::from_le_bytes(word))
}

/// Which of two actions is more restrictive.
///
/// The comparison is on the *signed* action field, which is what makes the
/// table's own order come out of the arithmetic: `KILL_PROCESS` is
/// `0x8000_0000`, so as an `i32` it is the smallest value there is and outranks
/// every other action, while `ALLOW` (`0x7fff_0000`) is the largest. A plain
/// unsigned minimum would let `ALLOW` beat a kill.
///
/// ```
/// use mpos_seccomp::{RET_ALLOW, RET_ERRNO, RET_KILL_PROCESS, more_restrictive};
///
/// // The signed comparison is what makes KILL_PROCESS (0x8000_0000) outrank
/// // ALLOW (0x7fff_0000); an unsigned min would let ALLOW win a kill.
/// assert_eq!(more_restrictive(RET_ALLOW, RET_KILL_PROCESS), RET_KILL_PROCESS);
/// assert_eq!(more_restrictive(RET_KILL_PROCESS, RET_ALLOW), RET_KILL_PROCESS);
/// // Ties return the first argument, and its data half travels with it -
/// // two ERRNO answers for different errnos are equally restrictive.
/// let eperm = RET_ERRNO | 1;
/// let einval = RET_ERRNO | 22;
/// assert_eq!(more_restrictive(eperm, einval), eperm);
/// ```
pub fn more_restrictive(a: u32, b: u32) -> u32 {
    if (a & ACTION_FULL) as i32 <= (b & ACTION_FULL) as i32 {
        a
    } else {
        b
    }
}

/// The action of a filter's answer, with its data bits masked off - what
/// [`more_restrictive`] compares and what the gate switches on.
pub fn action(ret: u32) -> u32 {
    ret & ACTION_FULL
}

/// The data half of an action: the errno `RET_ERRNO` carries, or the signal
/// `RET_TRAP` does.
pub fn action_data(ret: u32) -> u32 {
    ret & 0x0000_ffff
}

/// Whether this kernel implements the action, which is the whole content of
/// `seccomp(2)`'s availability probe.
///
/// Every constant below appears because the gate does what Linux does with it,
/// and each of those was measured rather than assumed (NOTES.md §8.2 holds the
/// observations). Two kinds of word are refused:
///
/// * an action this kernel does not implement - which today is only
///   `USER_NOTIF`, whose whole purpose is to hand the call to a userspace
///   supervisor this machine has no way to summon; and
/// * an action with anything in its **data half**. Measured on Linux: asking
///   about `ERRNO|1` is refused, so the probe is a question about eight whole
///   words and not about their high halves. A caller can be confident that a
///   "supported" answer covers the errno it intends to use, because it cannot
///   make the probe say yes about an errno at all.
///
/// Refusing matters more than the answer looks like it should. The probe exists
/// because Linux treats an action it does not know as `KILL_PROCESS`, so a
/// program asks *before* it relies on one; answering yes to something not
/// implemented lets a filter kill its own process instead of falling back.
///
/// ```
/// use mpos_seccomp::{RET_ALLOW, RET_ERRNO, RET_USER_NOTIF, supported};
///
/// assert!(supported(RET_ALLOW));
/// assert!(supported(RET_ERRNO)); // the bare action, no data half
/// // USER_NOTIF is the one action this kernel has no supervisor for.
/// assert!(!supported(RET_USER_NOTIF));
/// // And any word with anything in its data half is refused - measured on
/// // Linux, where asking about ERRNO|1 is a "no". The probe is a question
/// // about eight whole words, not their high halves.
/// assert!(!supported(RET_ERRNO | 1));
/// ```
pub fn supported(ret: u32) -> bool {
    matches!(
        ret,
        RET_KILL_THREAD | RET_KILL_PROCESS | RET_TRAP | RET_ERRNO | RET_TRACE | RET_LOG | RET_ALLOW
    )
}

/// The architecture word a filter checks first, as the block's own bytes.
pub fn data_bytes(nr: i32, ip: u64, args: [u64; 6]) -> [u8; DATA_BYTES] {
    let mut out = [0u8; DATA_BYTES];
    out[0..4].copy_from_slice(&nr.to_le_bytes());
    out[4..8].copy_from_slice(&ARCH.to_le_bytes());
    out[8..16].copy_from_slice(&ip.to_le_bytes());
    for (i, arg) in args.iter().enumerate() {
        let at = 16 + i * 8;
        out[at..at + 8].copy_from_slice(&arg.to_le_bytes());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One instruction, as the ABI's eight bytes.
    fn insn(code: u16, jt: u8, jf: u8, k: u32) -> [u8; 8] {
        let mut out = [0u8; 8];
        out[0..2].copy_from_slice(&code.to_le_bytes());
        out[2] = jt;
        out[3] = jf;
        out[4..8].copy_from_slice(&k.to_le_bytes());
        out
    }

    fn program(insns: &[[u8; 8]]) -> Result<Program, Invalid> {
        let mut bytes = Vec::new();
        for i in insns {
            bytes.extend_from_slice(i);
        }
        Program::parse(&bytes)
    }

    /// The load-and-compare every real filter opens with, and the numbers it
    /// uses: the offsets are `offsetof` in `struct seccomp_data`.
    const NR_OFF: u32 = 0;

    #[test]
    fn a_program_must_be_bounded_and_end_with_a_return() {
        assert_eq!(program(&[]), Err(Invalid::Empty));
        // One short of a whole instruction.
        assert_eq!(Program::parse(&[0u8; 7]), Err(Invalid::UnknownInstruction));
        // Ends with a load, so it could run off its own end.
        assert_eq!(
            program(&[insn(LD | W | ABS, 0, 0, NR_OFF)]),
            Err(Invalid::NotTerminated)
        );
        assert!(program(&[insn(RET | K_SRC, 0, 0, RET_ALLOW)]).is_ok());

        let long: Vec<[u8; 8]> =
            core::iter::repeat_n(insn(RET | K_SRC, 0, 0, RET_ALLOW), MAX_INSNS + 1).collect();
        assert_eq!(program(&long), Err(Invalid::TooLong));
    }

    #[test]
    fn jumps_have_to_land_inside_the_program() {
        // `jeq` with both offsets zero lands on the instruction after it,
        // which is in range.
        assert!(
            program(&[
                insn(JMP | JEQ | K_SRC, 0, 0, 60),
                insn(RET | K_SRC, 0, 0, RET_ALLOW),
            ])
            .is_ok()
        );
        // The same jump with `jf` one past the end does not.
        assert_eq!(
            program(&[
                insn(JMP | JEQ | K_SRC, 0, 1, 60),
                insn(RET | K_SRC, 0, 0, RET_ALLOW),
            ]),
            Err(Invalid::JumpOutOfRange)
        );
        // `ja` jumps by `k`, not by a pair of offsets.
        assert_eq!(
            program(&[insn(JMP | 0x00, 0, 0, 5), insn(RET | K_SRC, 0, 0, 0)]),
            Err(Invalid::JumpOutOfRange)
        );
    }

    #[test]
    fn an_instruction_this_kernel_does_not_implement_is_refused() {
        // `BPF_MSH` is the packet-header walk: meaningful for a socket filter,
        // meaningless here, and refused rather than treated as a no-op.
        assert_eq!(
            program(&[
                insn(LD | B | MSH, 0, 0, 0),
                insn(RET | K_SRC, 0, 0, RET_ALLOW),
            ]),
            Err(Invalid::UnknownInstruction)
        );
    }

    /// The shape `libseccomp` emits: check the architecture, load the number,
    /// and answer per call.
    #[test]
    fn a_filter_decides_by_the_call_number() {
        let p = program(&[
            // ld [arch]
            insn(LD | W | ABS, 0, 0, 4),
            // jeq ARCH, +1, 0 - a foreign ABI is killed
            insn(JMP | JEQ | K_SRC, 1, 0, ARCH),
            insn(RET | K_SRC, 0, 0, RET_KILL_THREAD),
            // ld [nr]
            insn(LD | W | ABS, 0, 0, NR_OFF),
            // jeq 16 (write), +1, 0
            insn(JMP | JEQ | K_SRC, 1, 0, 16),
            insn(RET | K_SRC, 0, 0, RET_ERRNO | 1),
            insn(RET | K_SRC, 0, 0, RET_ALLOW),
        ])
        .expect("the filter is well formed");

        let allow = Program::run(&p, &data_bytes(16, 0, [0; 6]));
        assert_eq!(allow, RET_ALLOW);
        let denied = Program::run(&p, &data_bytes(60, 0, [0; 6]));
        assert_eq!(action(denied), RET_ERRNO);
        assert_eq!(action_data(denied), 1);
    }

    #[test]
    fn a_load_past_the_data_block_kills_rather_than_reading_anything() {
        // The block is 64 bytes; a word at 64 is one past its end.
        let p = program(&[
            insn(LD | W | ABS, 0, 0, 64),
            insn(RET | K_SRC, 0, 0, RET_ALLOW),
        ])
        .expect("a load out of range is a run-time matter, not a parse one");
        assert_eq!(Program::run(&p, &data_bytes(0, 0, [0; 6])), RET_KILL_THREAD);
        // And a half-word at 63 is the same, for the size's sake.
        let p = program(&[
            insn(LD | H | ABS, 0, 0, 63),
            insn(RET | K_SRC, 0, 0, RET_ALLOW),
        ])
        .expect("parses");
        assert_eq!(Program::run(&p, &data_bytes(0, 0, [0; 6])), RET_KILL_THREAD);
    }

    /// A filter that reads an *argument*: `args[0]` is eight bytes at 16, and
    /// a 32-bit machine reads it as two words - which is how a real filter
    /// checks a pointer's range.
    #[test]
    fn arguments_are_eight_byte_fields_at_their_own_offsets() {
        // ld [args[0] low] ; jeq 0x1234, +1, 0 ; ret kill ; ret allow
        let p = program(&[
            insn(LD | W | ABS, 0, 0, 16),
            insn(JMP | JEQ | K_SRC, 1, 0, 0x1234),
            insn(RET | K_SRC, 0, 0, RET_KILL_THREAD),
            insn(LD | W | ABS, 0, 0, 20),
            insn(JMP | JEQ | K_SRC, 1, 0, 0x5678),
            insn(RET | K_SRC, 0, 0, RET_KILL_THREAD),
            insn(RET | K_SRC, 0, 0, RET_ALLOW),
        ])
        .expect("parses");

        // A value that needs both halves, which is the whole point: the low
        // word is at 16 and the high word at 20, and a 32-bit filter has to
        // look at each in turn. (A value that fits in 32 bits would leave the
        // second load reading zero and prove nothing about the offset.)
        let hit = Program::run(
            &p,
            &data_bytes(0, 0, [0x0000_5678_0000_1234, 0, 0, 0, 0, 0]),
        );
        assert_eq!(hit, RET_ALLOW);
        let miss = Program::run(
            &p,
            &data_bytes(0, 0, [0x0000_5678_0000_1235, 0, 0, 0, 0, 0]),
        );
        assert_eq!(miss, RET_KILL_THREAD);
        // And the second load is really at 20 and not at 16: a value whose
        // halves differ only in the high word is caught by it.
        let high = Program::run(
            &p,
            &data_bytes(0, 0, [0x0000_9999_0000_1234, 0, 0, 0, 0, 0]),
        );
        assert_eq!(high, RET_KILL_THREAD);
    }

    #[test]
    fn the_accumulator_index_and_scratch_words_work() {
        // tax (x = a); st [0] (mem[0] = a); ldx 0; txa; ret a
        let p = program(&[
            insn(LD | W | IMM, 0, 0, 7),
            insn(MISC | 0x00, 0, 0, 0),
            insn(ST, 0, 0, 0),
            insn(LDX | W | IMM, 0, 0, 0),
            insn(LDX | W | MEM, 0, 0, 0),
            insn(MISC | 0x80, 0, 0, 0),
            insn(RET | 0x10, 0, 0, 0),
        ])
        .expect("parses");
        assert_eq!(Program::run(&p, &data_bytes(0, 0, [0; 6])), 7);
    }

    #[test]
    fn the_availability_probe_answers_only_about_actions_this_kernel_does() {
        // Everything the gate implements, and nothing else. `USER_NOTIF` is the
        // refusal that costs something: it is the only action in the set the
        // probe will not vouch for, because summoning the supervisor it names
        // is not a thing this machine can do.
        for yes in [
            RET_KILL_THREAD,
            RET_KILL_PROCESS,
            RET_TRAP,
            RET_ERRNO,
            RET_TRACE,
            RET_LOG,
            RET_ALLOW,
        ] {
            assert!(supported(yes), "{yes:#x} is implemented");
        }
        assert!(!supported(RET_USER_NOTIF));
        assert!(!supported(0x1234_0000));
        // Measured on Linux: a word with anything in its data half is refused,
        // even though the action in its high half is one that is supported.
        assert!(!supported(RET_ERRNO | 1));
        assert!(!supported(RET_TRAP | 1));
        assert!(!supported(RET_ALLOW | 1));
    }

    #[test]
    fn the_lowest_number_wins_when_filters_are_stacked() {
        // The ordering the table promises, by action value: a kill outranks an
        // errno, an errno outranks an allow - and `KILL_PROCESS`, which is the
        // *largest* value, outranks everything as a signed number.
        assert_eq!(more_restrictive(RET_ALLOW, RET_ERRNO | 13), RET_ERRNO | 13);
        assert_eq!(more_restrictive(RET_ERRNO | 13, RET_ALLOW), RET_ERRNO | 13);
        assert_eq!(more_restrictive(RET_ERRNO | 13, RET_TRAP), RET_TRAP);
        assert_eq!(more_restrictive(RET_TRAP, RET_KILL_THREAD), RET_KILL_THREAD);
        assert_eq!(
            more_restrictive(RET_ALLOW, RET_KILL_PROCESS),
            RET_KILL_PROCESS
        );
        assert_eq!(
            more_restrictive(RET_KILL_PROCESS, RET_ALLOW),
            RET_KILL_PROCESS
        );
        // The data half is not what is compared: two errnos are the same
        // action, and the first asked keeps its own errno.
        assert_eq!(
            more_restrictive(RET_ERRNO | 1, RET_ERRNO | 2),
            RET_ERRNO | 1
        );
    }
}
