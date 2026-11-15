//! The `kernel_services` suite: a trap's report names the trap it took, and a
//! `siginfo_t` renders the record it was given.
//!
//! A child of `kernel_services` and compiled only into a test build, for both
//! machines. The first three cases pin a table: one reporting door is shared by
//! six exception vectors on x86_64, and until 2026-11-14 it named all six
//! "general protection fault". A chromium crash that was really `#UD` (`0f 0b`
//! in musl's `a_crash`, after the allocator found the heap corrupted) was
//! therefore reported as a protection fault, and two diagnoses of that log -
//! one of them this repository's - went looking for a fault the machine had
//! never raised (`docs/notes/` §6.158). The words themselves are this kernel's,
//! not Linux's - the measurement that says so is in `trap_name`'s own doc.
//!
//! The other three cases pin [`crate::hal::SignalInfo::to_wire`] on each
//! machine: three writers used to lay `siginfo_t` out themselves, and a CPU
//! fault's `si_addr` occupies the very eight bytes a sender's `si_pid`/
//! `si_uid` do (`docs/notes/` §6.160). These cases render a record and read the
//! bytes, which is deterministic on a boot; handing a fault record to a
//! handler *at the trap* is not, and that stays a named gap in §6.159.

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
        Case {
            name: "a_sender_siginfo_keeps_its_pid_and_uid_at_sixteen",
            run: a_sender_siginfo_keeps_its_pid_and_uid_at_sixteen,
        },
        Case {
            name: "a_fault_siginfo_puts_the_address_at_sixteen",
            run: a_fault_siginfo_puts_the_address_at_sixteen,
        },
        Case {
            name: "a_child_siginfo_keeps_its_status_and_cpu_time",
            run: a_child_siginfo_keeps_its_status_and_cpu_time,
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

/// Three writers used to lay `siginfo_t` out themselves: x86_64's frame,
/// arm64's frame, and `sigtimedwait`. They now call
/// [`crate::hal::SignalInfo::to_wire`], so this case pins the bytes every
/// sender-shaped record keeps on **both** machines: `si_signo`/`si_errno`/
/// `si_code` at 0/4/8, `si_pid`/`si_uid` at 16/20, and the `sigval` at 24.
/// A renderer that moved the union, or that mistook `SI_USER` for a fault,
/// reddens here before any program can read a wrong sender.
fn a_sender_siginfo_keeps_its_pid_and_uid_at_sixteen(
    _info: &crate::boot::BootInfo,
) -> Result<(), String> {
    use crate::hal::SignalInfo;
    let info = SignalInfo {
        signo: 10,
        errno: 3,
        code: 0,
        pid: 4242,
        uid: 1000,
        value: 0x1122_3344_5566_7788,
        status: 0,
        utime: 0,
        stime: 0,
        addr: 0,
        fault: false,
        tid: 0,
        overrun: 0,
    };
    let w = info.to_wire();
    let i32_at = |at: usize| i32::from_le_bytes(w[at..at + 4].try_into().unwrap());
    let u64_at = |at: usize| u64::from_le_bytes(w[at..at + 8].try_into().unwrap());
    case_check!(i32_at(0) == 10, "si_signo is {}, not 10", i32_at(0));
    case_check!(i32_at(4) == 3, "si_errno is {}, not 3", i32_at(4));
    case_check!(i32_at(8) == 0, "si_code is {}, not SI_USER", i32_at(8));
    case_check!(i32_at(16) == 4242, "si_pid is {}, not 4242", i32_at(16));
    case_check!(i32_at(20) == 1000, "si_uid is {}, not 1000", i32_at(20));
    case_check!(
        u64_at(24) == 0x1122_3344_5566_7788,
        "si_value is {:#x}, not the sigqueue payload",
        u64_at(24)
    );
    Ok(())
}

/// The fault record that Item 280 adds. Linux 7.2.14's null-store handler
/// reads `si_code = 1` (`SEGV_MAPERR`) at +8 and `si_addr = 0x10` at +16,
/// with +24..+48 zero (`build/scratch/siginfo/wire.c`); before this change
/// the same record was `SI_USER` with the sender's pid at +16. This case
/// pins the *layout* on both machines without depending on a handler being
/// entered at a trap - the delivery itself is still a named gap (§6.159).
fn a_fault_siginfo_puts_the_address_at_sixteen(
    _info: &crate::boot::BootInfo,
) -> Result<(), String> {
    use crate::hal::SignalInfo;
    let info = SignalInfo {
        signo: 11,
        errno: 0,
        code: 1,
        pid: 77,
        uid: 88,
        value: 0xdead_beef,
        status: 42,
        utime: 9,
        stime: 7,
        addr: 0x10,
        fault: true,
        tid: 0,
        overrun: 0,
    };
    let w = info.to_wire();
    let i32_at = |at: usize| i32::from_le_bytes(w[at..at + 4].try_into().unwrap());
    let u64_at = |at: usize| u64::from_le_bytes(w[at..at + 8].try_into().unwrap());
    case_check!(i32_at(8) == 1, "si_code is {}, not SEGV_MAPERR", i32_at(8));
    case_check!(u64_at(16) == 0x10, "si_addr is {:#x}, not 0x10", u64_at(16));
    case_check!(
        i32_at(16) != 77,
        "the sender's pid is still at +16 beside the fault address"
    );
    case_check!(
        u64_at(24) == 0 && u64_at(32) == 0 && u64_at(40) == 0,
        "a fault record's +24..+48 are not zero"
    );
    case_check!(!info.chld_union(), "a fault record took the _sigchld union");
    Ok(())
}

/// A child's death is the one sender whose union is not the `sigval`: the
/// class test must still pick `si_status` and the two `clock_t`s when a fault
/// record (whose `SEGV_MAPERR` is the same code 1) exists beside it. This is
/// the counterfactual for the discriminator: an old `1..=6` keyed only on the
/// code would pass the fault case above but fail to keep this one intact.
fn a_child_siginfo_keeps_its_status_and_cpu_time(
    _info: &crate::boot::BootInfo,
) -> Result<(), String> {
    use crate::hal::SignalInfo;
    let info = SignalInfo {
        signo: 17,
        errno: 0,
        code: 1,
        pid: 4242,
        uid: 1000,
        value: 0,
        status: 7,
        utime: 9,
        stime: 3,
        addr: 0,
        fault: false,
        tid: 0,
        overrun: 0,
    };
    let w = info.to_wire();
    let i32_at = |at: usize| i32::from_le_bytes(w[at..at + 4].try_into().unwrap());
    let u64_at = |at: usize| u64::from_le_bytes(w[at..at + 8].try_into().unwrap());
    case_check!(info.chld_union(), "a child's death lost its _sigchld union");
    case_check!(i32_at(24) == 7, "si_status is {}, not 7", i32_at(24));
    case_check!(u64_at(32) == 9, "si_utime is {}, not 9", u64_at(32));
    case_check!(u64_at(40) == 3, "si_stime is {}, not 3", u64_at(40));
    Ok(())
}
