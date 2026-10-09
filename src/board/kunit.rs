//! The boot's self-tests, as suites of named cases.
//!
//! This is the harness the boot calls into, and it exists only in a test build:
//! the module is behind `--features selftest`, and so is every call site in the
//! two bodies. A production kernel compiles none of it and contains none of the
//! check code it names - which is the property the whole arrangement is for, and
//! the one a `--cfg` could not have expressed here (setting `RUSTFLAGS` replaces
//! `.cargo/config.toml`'s `[target.*] rustflags` rather than adding to them, so a
//! test build carrying a `--cfg` would lose the linker script and every `mpos_*`
//! capability flag; measured, and `docs/NOTES.md` §15.19 records it).
//!
//! # Why a case is a value and not a panic
//!
//! A case answers `Result<(), &'static str>`. That is not style: the kernel is
//! `panic = "abort"`, so a case that asserted would end the machine and hide
//! every case after it - and the first thing a test system has to be able to do
//! is report *all* of what is broken. The cases that still panic are the ones not
//! yet split: each of the four families below is one chain of assertions today,
//! and turning that chain into named cases is the next slice's work rather than
//! this one's.
//!
//! # Why the boot says *when*
//!
//! A suite runs at the boot step it needs, and the name of the step is the
//! prerequisite rather than a position: `checks::mm` runs after the heap comes up
//! on one machine and before the scheduler starts on the other, and both are
//! [`Phase::Heap`] because that is what the suite needs. The alternative - run
//! everything at one point just before the handoff - is what a unit-test suite
//! usually does, and it is not open here: `checks::mm` asserts increments on a
//! *fresh* heap and `checks::vfs` needs the bootstrap root mounted, so moving them
//! later would mean re-measuring every assertion against a machine that has
//! already been used. That is a test redesign, not a refactor, and it belongs to
//! whoever decides the assertions should be weaker than they are.

use mpos::boot::BootInfo;
use mpos::println;

use super::checks;

/// The boot step a suite needs to have happened.
///
/// The two machine-specific variants are declared where their machine's checks
/// are, rather than both everywhere: a phase with no suite in it is a name
/// nothing constructs, which the compiler would report and which would be the
/// right report - this machine has no such step to reach.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Phase {
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
pub(crate) struct Case {
    /// The case's own name. The log spells it `<suite>.<case>`, which is what a
    /// reader greps for and what `xtask` counts.
    pub(crate) name: &'static str,
    pub(crate) run: fn(&BootInfo) -> Result<(), &'static str>,
}

/// The cases that run at one phase, under one name.
pub(crate) struct Suite {
    pub(crate) name: &'static str,
    pub(crate) phase: Phase,
    pub(crate) cases: &'static [Case],
}

/// Every suite, in the one place that names them.
///
/// A list rather than a linker-section registry: the kernel has no
/// `distributed_slice`-shaped dependency and adding one for four suites would be
/// a mechanism larger than what it carries. This is also the file a reader opens
/// to answer "what does a test boot actually check", which a registry assembled
/// at link time would not give them.
pub(crate) const SUITES: &[Suite] = &[
    Suite {
        name: "mm",
        phase: Phase::Heap,
        cases: &[Case {
            name: "allocators",
            run: checks::case_mm,
        }],
    },
    Suite {
        name: "sched",
        phase: Phase::Scheduler,
        cases: &[Case {
            name: "tasks",
            run: checks::case_sched,
        }],
    },
    Suite {
        name: "vfs",
        phase: Phase::Vfs,
        cases: &[Case {
            name: "mounts",
            run: checks::case_vfs,
        }],
    },
    Suite {
        name: "ipc",
        phase: Phase::Ipc,
        cases: &[
            Case {
                name: "pipes_tty_signals",
                run: checks::case_ipc,
            },
            Case {
                name: "boot_stack_depth",
                run: checks::case_boot_stack,
            },
        ],
    },
    #[cfg(target_arch = "x86_64")]
    Suite {
        name: "arch",
        phase: Phase::Arch,
        cases: &[Case {
            name: "clock_tables_address_spaces",
            run: super::x86_64::suite_arch,
        }],
    },
    #[cfg(target_arch = "aarch64")]
    Suite {
        name: "arch",
        phase: Phase::ConsoleInput,
        cases: &[Case {
            name: "console_receive",
            run: super::aarch64::suite_console_input,
        }],
    },
    #[cfg(target_arch = "aarch64")]
    Suite {
        name: "arch",
        phase: Phase::ConsoleTx,
        cases: &[Case {
            name: "console_transmit",
            run: super::aarch64::suite_console_tx,
        }],
    },
];

/// How many cases have answered, so far.
static TALLY: critical_section::Mutex<core::cell::RefCell<(usize, usize)>> =
    critical_section::Mutex::new(core::cell::RefCell::new((0, 0)));

/// Run the suites that belong to `phase`, and report at [`Phase::Done`].
///
/// Called by the machine's own `init` at the boot steps it knows about, because
/// the prerequisite is the machine's knowledge: this function is told *when*, and
/// decides only *what*.
///
/// A failing case does **not** stop the boot. The kernel's suites and the user
/// programs the sweep runs afterwards are the two halves of one gate, and ending
/// the machine at the first bad case would trade the second half's evidence - a
/// hundred programs' worth - for a tidier log. The harness's own line is the
/// verdict, and `xtask` asserts it beside the sweep's summary (`xtask/src/
/// selftest.rs`).
pub(crate) fn reached(phase: Phase, info: &BootInfo) {
    for suite in SUITES.iter().filter(|suite| suite.phase == phase) {
        for case in suite.cases {
            let name = alloc::format!("{}.{}", suite.name, case.name);
            match (case.run)(info) {
                Ok(()) => {
                    println!("mpos-kunit: {name} OK");
                    count(true);
                }
                Err(reason) => {
                    println!("mpos-kunit: {name} FAILED: {reason}");
                    count(false);
                }
            }
        }
    }
    if phase == Phase::Done {
        finish();
    }
}

/// Add one case's answer to the tally.
fn count(passed: bool) {
    critical_section::with(|cs| {
        let mut tally = TALLY.borrow(cs).borrow_mut();
        if passed {
            tally.0 += 1;
        } else {
            tally.1 += 1;
        }
    });
}

/// The harness's own verdict, in the shape `xtask` asserts on.
///
/// One line, and it is the line that makes "the suites all ran" a claim rather
/// than a hope: a suite that silently never ran leaves its cases out of the
/// count, and the count is asserted from outside.
fn finish() {
    let (passed, failed) = critical_section::with(|cs| *TALLY.borrow(cs).borrow());
    println!("mpos-kunit: {passed} passed, {failed} failed");
}
