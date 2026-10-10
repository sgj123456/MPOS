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
/// One vocabulary for every machine, because the *boot* is one sequence now
/// (`src/board/flow.rs`): it reaches the same phases on both, and a phase a
/// machine has no suite for simply runs nothing there - which is the honest
/// "this machine's checks have nothing to say at this step" rather than a name
/// that does not exist. The variants used to be gated to the machine whose checks
/// declared them, on the reasoning that a phase nothing constructs is a name the
/// compiler should report; with one flow every phase is constructed everywhere, so
/// that reasoning no longer holds and the names are neutral.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    /// The frame allocator and the slab heap are up.
    Heap,
    /// The machine's own page tables, interrupts and clock are up.
    Arch,
    /// The scheduler has adopted the boot context.
    Scheduler,
    /// The console's *polled* receiver can be read, and nothing else reads it.
    ConsoleInput,
    /// The console's transmit ring is armed and an interrupt drains it.
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

/// How many task verdicts can be in flight at once.
///
/// Four, because each suite that spawns a task takes a slot of its own rather than
/// sharing one: a task cannot return a value to the case that spawned it, so it
/// stores its answer here and the case reads it out - and two suites' tasks must
/// not be able to overwrite each other's (`docs/NOTES.md` §15.22).
const VERDICT_SLOTS: usize = 4;

/// What a spawned task found, one slot per suite.
static VERDICTS: critical_section::Mutex<
    core::cell::RefCell<[Option<Result<(), String>>; VERDICT_SLOTS]>,
> = critical_section::Mutex::new(core::cell::RefCell::new([None, None, None, None]));

/// Store what a task found, for the case that spawned it.
///
/// A task's half of a case cannot answer through `case_check!`: there is no case
/// to return to inside a task, and an `assert!` there would end the machine
/// instead of naming what broke. Storing the verdict is how a task's claim
/// becomes part of its case's answer.
pub fn record_verdict(slot: usize, what: Result<(), String>) {
    critical_section::with(|cs| VERDICTS.borrow(cs).borrow_mut()[slot] = Some(what));
}

/// Take the verdict the task in `slot` stored, or fail because it never stored
/// one.
///
/// A slot still empty after the task has been reaped is a failure of its own: it
/// means the task left without reaching its verdict, so the half of the case it
/// owned was never checked at all.
pub fn take_verdict(slot: usize, who: &str) -> Result<(), String> {
    let answer = critical_section::with(|cs| VERDICTS.borrow(cs).borrow_mut()[slot].take());
    answer.unwrap_or_else(|| {
        Err(alloc::format!(
            "{who} was reaped without reaching a verdict, so its half was never checked"
        ))
    })
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
