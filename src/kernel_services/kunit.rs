//! The `kernel_services` suite: a trap's report names the trap it took.
//!
//! A child of `kernel_services` and compiled only into a test build. The claim
//! is small and the bug it pins is not: one reporting door is shared by six
//! exception vectors on x86_64, and until 2026-11-14 it named all six "general
//! protection fault". A chromium crash that was really `#UD` (`0f 0b` in musl's
//! `a_crash`, after the allocator found the heap corrupted) was therefore
//! reported as a protection fault, and two diagnoses of that log - one of them
//! this repository's - went looking for a fault the machine had never raised
//! (`docs/notes/` §6.158).
//!
//! What a case can hold to here is the *table*, which is the whole of the
//! change: the reporter's body is three lines over [`crate::kernel_services::trap_name`],
//! and the machine passes the vector it dispatched through. The words themselves
//! are this kernel's, not Linux's - the measurement that says so is in
//! `trap_name`'s own doc.

use alloc::string::String;

use crate::case_check;
use crate::kunit::{Case, Phase, Suite};

/// The suite this module contributes to the boot's self-tests.
pub const SUITE: Suite = Suite {
    name: "kernel_services",
    // Nothing here asks a service of the machine: the table is pure, so the
    // earliest phase that has an allocator for a failing case's message is
    // enough. The reporter itself cannot run from a suite - it would kill the
    // boot it is reporting on - which is why the table is what is pinned.
    phase: Phase::Arch,
    cases: &[
        Case {
            name: "an_invalid_opcode_is_not_a_protection_fault",
            run: an_invalid_opcode_is_not_a_protection_fault,
        },
        Case {
            name: "the_vectors_one_door_serves_each_answer_their_own_word",
            run: the_vectors_one_door_serves_each_answer_their_own_word,
        },
        Case {
            name: "a_vector_the_idt_never_installs_is_still_named",
            run: a_vector_the_idt_never_installs_is_still_named,
        },
    ],
};

/// The bug, in one claim: `#UD` (vector 6) used to be reported as `#GP`.
///
/// A counterfactual that is *only* this: the old reporter answered
/// `"general protection fault"` for every vector, so a table that answered that
/// word for 6 - or a reporter that went back to a single literal - reddens here
/// and nowhere else.
fn an_invalid_opcode_is_not_a_protection_fault(
    _info: &crate::boot::BootInfo,
) -> Result<(), String> {
    let ud = super::trap_name(6);
    let gp = super::trap_name(13);
    case_check!(
        ud == "invalid opcode",
        "vector 6 is `{ud}`, not the invalid opcode `#UD` is"
    );
    case_check!(
        gp == "general protection fault",
        "vector 13 is `{gp}`, not the protection fault `#GP` is"
    );
    case_check!(
        ud != gp,
        "the invalid opcode and the protection fault are the same word `{ud}`"
    );
    Ok(())
}

/// Every vector the one door serves, named with a word of its own.
///
/// Six handlers reach [`crate::kernel_services`]'s reporter (`#DE` 0, `#UD` 6,
/// `#NP` 11, `#SS` 12, `#GP` 13, `#AC` 17, and `#PF` 14 through its own path) -
/// a table that collapsed any two of them would make two different faults
/// indistinguishable in the one place that records them.
fn the_vectors_one_door_serves_each_answer_their_own_word(
    _info: &crate::boot::BootInfo,
) -> Result<(), String> {
    let vectors = [0u8, 6, 11, 12, 13, 14, 17];
    for (index, &vector) in vectors.iter().enumerate() {
        let name = super::trap_name(vector);
        case_check!(
            name != "trap",
            "vector {vector} has no word of its own (it answered `{name}`)"
        );
        for &other in &vectors[..index] {
            case_check!(
                super::trap_name(other) != name,
                "vectors {other} and {vector} are both `{name}`"
            );
        }
    }
    Ok(())
}

/// A vector with no row still answers a word, because the fault path cannot
/// fail: the reporter runs where an `Option` would have to be unwrapped, and an
/// unwrap there is the machine ending inside its own diagnostic.
fn a_vector_the_idt_never_installs_is_still_named(
    _info: &crate::boot::BootInfo,
) -> Result<(), String> {
    for vector in [2u8, 9, 15, 22, 200, 255] {
        let name = super::trap_name(vector);
        case_check!(!name.is_empty(), "vector {vector} answered an empty name");
    }
    Ok(())
}
