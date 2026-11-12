#![no_main]

//! `seccomp(2)`'s `SECCOMP_SET_MODE_FILTER`: is the byte string behind a
//! `sock_fprog` a filter at all, and does one answer every call once it is?
//!
//! `crates/seccomp/src/lib.rs`'s `Program::parse` is the only constructor of a
//! [`Program`], and it reads exactly the bytes a caller handed the kernel - a
//! `sock_filter[]` of eight bytes each, in the caller's own memory. Everything
//! the kernel later does with a filter rests on what this validator promised:
//! that a rejected program is a named [`Invalid`], and that an accepted one has
//! its last instruction a return and every jump inside itself, so the
//! interpreter may run it without looking back. The interpreter is fuzzed
//! beside it because the module calls `run` **total** - an out-of-range load, a
//! division by zero or a shift past the accumulator has to end the *program*
//! and not the kernel, and `panic = "abort"` means a panic here is a machine
//! that stops on a filter a program installed for itself.
//!
//! This target asserts the parser's own contract rather than only absence of a
//! crash: the refusal reasons it documents, the shape an accepted program is
//! guaranteed to have, and that `run` answers - twice the same - for every
//! input block. The bit fields mirrored below are the module's, and a change to
//! them is a change to the ABI this target pins.

use libfuzzer_sys::fuzz_target;
use mpos::seccomp::{DATA_BYTES, Insn, Invalid, MAX_INSNS, Program, data_bytes};

/// The class field of a `sock_filter`'s code word, and the classes this target
/// has to recognise: the one a program must end with and the two that carry a
/// landing site. Mirrored from `crates/seccomp/src/lib.rs` so a mismatch is the
/// fuzz run's failure and not a compile error (a private constant cannot be
/// named from here, and widening it for a test's sake is the wrong direction).
const CLASS_MASK: u16 = 0x0007;
const JMP: u16 = 0x0005;
const RET: u16 = 0x0006;
/// The operation field, which is what tells `BPF_JA` (whose offset is `k`)
/// apart from the conditional jumps (whose landing sites are `jt`/`jf`).
const OP_MASK: u16 = 0x00f0;
const JA: u16 = 0x0000;

/// The same eight-byte little-endian decode `parse` does, so the properties
/// asserted below are about the instructions the module actually built.
fn decode(bytes: &[u8]) -> Vec<Insn> {
    bytes
        .chunks_exact(8)
        .map(|chunk| Insn {
            code: u16::from_le_bytes([chunk[0], chunk[1]]),
            jt: chunk[2],
            jf: chunk[3],
            k: u32::from_le_bytes([chunk[4], chunk[5], chunk[6], chunk[7]]),
        })
        .collect()
}

fuzz_target!(|bytes: &[u8]| {
    let parsed = Program::parse(bytes);

    // The refusals that are about the length of the byte string, in the order
    // `parse` applies them. Each is a promise with no ambiguity in it: an empty
    // string is `Empty` and not a truncated instruction, a length that is not a
    // whole number of instructions is `UnknownInstruction` (there is no half an
    // instruction), and a count past the ceiling is `TooLong`. Getting one of
    // these wrong is a filter that is refused for a reason the caller cannot
    // put in an `EINVAL` that names the rule.
    if bytes.is_empty() {
        assert_eq!(
            parsed,
            Err(Invalid::Empty),
            "an empty byte string is not a program"
        );
        return;
    }
    if bytes.len() % 8 != 0 {
        assert_eq!(
            parsed,
            Err(Invalid::UnknownInstruction),
            "a {} byte string is {} bytes short of a whole instruction",
            bytes.len(),
            bytes.len() % 8
        );
        return;
    }
    let count = bytes.len() / 8;
    if count > MAX_INSNS {
        assert_eq!(
            parsed,
            Err(Invalid::TooLong),
            "a {count} instruction program is over the {MAX_INSNS} ceiling"
        );
        return;
    }

    let insns = decode(bytes);
    let program = match parsed {
        Ok(program) => program,
        Err(reason) => {
            // A program of whole instructions within the ceiling is refused on
            // its content, and only two of the reasons can be checked against
            // the bytes without re-implementing `known`: the last instruction
            // has to be a return before any jump is judged, and a jump out of
            // range is only meaningful in a program that has one.
            match reason {
                Invalid::NotTerminated => assert_ne!(
                    insns[count - 1].code & CLASS_MASK,
                    RET,
                    "a program told it is un-terminated already ends with a return"
                ),
                Invalid::JumpOutOfRange => {
                    assert_eq!(
                        insns[count - 1].code & CLASS_MASK,
                        RET,
                        "a program ending with a load was refused for a jump, not for running off its end"
                    );
                    // And at least one jump really has to leave the program.
                    let leaves = insns.iter().enumerate().any(|(pc, insn)| {
                        let land = |offset: usize| pc + 1 + offset < count;
                        match insn.code & CLASS_MASK {
                            JMP if insn.code & OP_MASK == JA => !land(insn.k as usize),
                            JMP => !land(insn.jt as usize) || !land(insn.jf as usize),
                            _ => false,
                        }
                    });
                    assert!(
                        leaves,
                        "a program was refused for a jump out of range and every jump lands inside it"
                    );
                }
                // `UnknownInstruction` names the one rule this target cannot
                // re-derive from the bytes: whether a code is one the module
                // implements is `known`'s private answer. It is still a named
                // `Invalid`, and the exhaustive match here is what makes a new
                // variant something this target has to be taught - the wrong
                // direction for a silently-added one.
                Invalid::UnknownInstruction | Invalid::Empty | Invalid::TooLong => {}
            }
            return;
        }
    };

    // ---- an accepted program, and what it was accepted for ----
    assert!(
        (1..=MAX_INSNS).contains(&count),
        "an accepted program has {count} instructions"
    );
    assert_eq!(
        insns[count - 1].code & CLASS_MASK,
        RET,
        "an accepted program does not end with a return"
    );
    // Every jump the interpreter will follow lands inside the program, forward,
    // as classic BPF promises: `ja` by `k`, the conditional pair by `jt`/`jf`.
    for (pc, insn) in insns.iter().enumerate() {
        let land = |offset: usize| pc + 1 + offset < count;
        match insn.code & CLASS_MASK {
            JMP if insn.code & OP_MASK == JA => assert!(
                land(insn.k as usize),
                "ja at {pc} jumps {} past a {count} instruction program",
                insn.k
            ),
            JMP => {
                assert!(
                    land(insn.jt as usize),
                    "a taken jump at {pc} lands {} past a {count} instruction program",
                    insn.jt
                );
                assert!(
                    land(insn.jf as usize),
                    "an untaken jump at {pc} lands {} past a {count} instruction program",
                    insn.jf
                );
            }
            _ => {}
        }
    }

    // Parsing is a pure read of the bytes: the same string is the same program,
    // which is what lets the module hand back a value instead of a state.
    assert_eq!(
        Program::parse(bytes),
        Ok(program.clone()),
        "parsing the same bytes twice produced two different programs"
    );

    // The interpreter, over inputs a call can actually carry and a few that
    // stress the loads: it must answer for every one, the same way twice. The
    // data blocks are built from the program's own bytes too, so a load whose
    // value the filter branches on is exercised rather than only zeroes.
    let mut echo = [0u8; DATA_BYTES];
    for (i, slot) in echo.iter_mut().enumerate() {
        *slot = bytes[i % bytes.len()];
    }

    let blocks: [(&str, [u8; DATA_BYTES]); 5] = [
        ("all zeroes", [0u8; DATA_BYTES]),
        ("all ones", [0xffu8; DATA_BYTES]),
        (
            "write(2)'s number and a pointer argument",
            data_bytes(1, 0x4000_0000, [0x7fff_ffff_ffff_f000, 4, 0, 0, 0, 0]),
        ),
        (
            "a foreign-ABI architecture word",
            // `data_bytes` always stamps the native [`ARCH`], so a block whose
            // `arch` differs has to be written by hand: this is the input a
            // filter that guards on `arch` is supposed to refuse, and it is
            // the one branch a native block can never reach.
            {
                let mut block = data_bytes(0, 0, [0, 0, 0, 0, 0, 0]);
                block[4..8].copy_from_slice(&0xC000_0000u32.to_le_bytes());
                block
            },
        ),
        ("the program's own bytes", echo),
    ];
    for (what, data) in &blocks {
        let answer = program.run(data);
        let again = program.run(data);
        assert_eq!(
            answer, again,
            "running a filter over {what} answered {answer:#x} and then {again:#x}"
        );
    }
});
