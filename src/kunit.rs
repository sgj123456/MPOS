//! The boot's self-tests: the vocabulary a suite is written in.
//!
//! A *suite* is the cases that run at one boot phase, and a *case* is one named
//! test. Both are declared as a `const` by the module the suite tests - `mm`'s is
//! `crate::mm::kunit::SUITE`, and it can read `mm`'s private state because it is a
//! child of `mm` - so the wiring layer's harness (`src/board/kunit.rs`) has only
//! to list them and run them. The vocabulary lives here rather than beside the
//! runner because both sides need it, and the suite modules are the side that
//! cannot reach up.
//!
//! Two things about the shape are load-bearing, and they are argued here rather
//! than at each suite:
//!
//! **A case answers a value.** The kernel is `panic = "abort"`, so a case that
//! asserted would end the machine and hide every case after it - and hiding the
//! rest of what is broken is the one thing a test system must not do. A case
//! returns `Err(why)`, and `why` is a `String` rather than a `&'static str`
//! because half the checks worth making are about *values*: "text at 0x… has the
//! wrong permissions" is what a reader needs, and a static string would have to
//! drop the address.
//!
//! **A suite says when.** [`Phase`] is the boot step a suite needs to have
//! happened, named by the prerequisite rather than by a position: `mm`'s cases
//! run after the heap is up on one machine and before the scheduler starts on the
//! other, and both are [`Phase::Heap`] because that is what they need. The
//! alternative - every suite at one point before the handoff, which is what a
//! unit-test framework usually does - is not open here: `mm` asserts increments on
//! a *fresh* heap and `vfs` needs the bootstrap root mounted, so running them
//! later would mean re-measuring every assertion against a machine that has
//! already been used. That is a test redesign rather than a refactor, and it
//! belongs to whoever decides the assertions should be weaker than they are.
//!
//! Compiled only into a test build (`--features selftest`), like the harness that
//! runs it and like every suite below it: a production kernel has none of this.

use alloc::string::String;

use crate::boot::BootInfo;

/// The boot step a suite needs to have happened.
///
/// The two machine-specific variants are declared where their machine's checks
/// are: a phase with no suite in it is a name nothing constructs, which the
/// compiler would report and which would be the right report.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    /// The frame allocator and the slab heap are up.
    Heap,
    /// The machine's own page tables, interrupts and clock are up.
    #[cfg(target_arch = "x86_64")]
    Arch,
    /// The scheduler has adopted the boot context.
    Scheduler,
    /// The console's *polled* receiver can be read, and nothing else reads it.
    #[cfg(target_arch = "aarch64")]
    ConsoleInput,
    /// The console's transmit ring is armed and an interrupt drains it.
    #[cfg(target_arch = "aarch64")]
    ConsoleTx,
    /// The bootstrap root is mounted and the archive unpacked.
    Vfs,
    /// The tty table and the signal path are up.
    Ipc,
    /// No suite runs here: the harness reports what the phases above found.
    Done,
}

/// One test: a name, and a body that answers a reason rather than panicking.
pub struct Case {
    /// The case's own name. The log spells it `<suite>.<case>`, which is what a
    /// reader greps for and what `xtask` counts.
    pub name: &'static str,
    pub run: fn(&BootInfo) -> Result<(), String>,
}

/// The cases that run at one phase, under one name.
pub struct Suite {
    pub name: &'static str,
    pub phase: Phase,
    pub cases: &'static [Case],
}

/// A check that answers the failed case's message.
///
/// A macro rather than a function because the body has to *return* from the case,
/// and because the message is formatted at the failure rather than at every call:
/// `case_check!(flags == expect, "{name} at {virt:#x} has the wrong permissions")`
/// reads like the `assert!` it replaces, which is what makes a suite's conversion
/// from panicking to answering a mechanical change (`docs/NOTES.md` §15.20).
#[macro_export]
macro_rules! case_check {
    ($condition:expr) => {
        if !$condition {
            return Err(::alloc::format!(
                "{}:{}: {}",
                file!(),
                line!(),
                stringify!($condition)
            ));
        }
    };
    ($condition:expr, $($arg:tt)*) => {
        if !$condition {
            return Err(::alloc::format!($($arg)*));
        }
    };
}
