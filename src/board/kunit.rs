//! The harness that runs the boot's self-tests, and the one place that lists them.
//!
//! The *vocabulary* is the library's (`mpos::kunit`): a suite is a `const` beside
//! the module it tests, so that `mm`'s can read `mm`'s private state, and this file
//! only assembles the list and drives it. That split is what let the suites move
//! into their modules without a registry: a `Suite` is a name, a phase and a slice
//! of cases, so `SUITES` below can name another module's `const` directly.
//!
//! This whole module exists only in a test build. The module is behind
//! `--features selftest`, and so is every call site in the two bodies, so a kernel
//! that ships compiles neither the suites nor this (`docs/NOTES.md` §15.20 says why
//! the switch is a cargo feature rather than a `--cfg`).
//!
//! A failing case does **not** stop the boot. The kernel's suites and the user
//! programs the sweep runs afterwards are the two halves of one gate, and ending
//! the machine at the first bad case would trade the second half's evidence - a
//! hundred programs' worth - for a tidier log. The harness's own line is the
//! verdict, and `xtask` asserts it beside the sweep's summary
//! (`xtask/src/selftest.rs`).

use mpos::boot::BootInfo;
// Re-exported so a body's call site reads `kunit::reached(kunit::Phase::Heap, …)`
// and names the harness it is talking to, rather than reaching past it into the
// library for the vocabulary.
pub(crate) use mpos::kunit::{Case, Phase, Suite};
use mpos::println;

use super::checks;

/// Every suite, in the one place that names them.
///
/// A list rather than a linker-section registry: the kernel has no
/// `distributed_slice`-shaped dependency, and adding one to carry a dozen entries
/// would be a mechanism larger than what it carries. This is also the file a
/// reader opens to answer "what does a test boot actually check", which a registry
/// assembled at link time would not give them.
pub(crate) const SUITES: &[Suite] = &[
    mpos::mm::kunit::SUITE,
    mpos::sched::kunit::SUITE,
    // The two families still checked from this layer. Each is one case whose body
    // answers by panicking, and each moves into its own module next - `vfs` to
    // `mpos::vfs::kunit`, and the syscall gate with the tty and the signal path to
    // `mpos::syscall::kunit`, which is where that cross-subsystem half belongs
    // (`docs/NOTES.md` §15.20, §15.21).
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
/// than a hope: a suite that silently never ran leaves its cases out of the count,
/// and the count is asserted from outside.
fn finish() {
    let (passed, failed) = critical_section::with(|cs| *TALLY.borrow(cs).borrow());
    println!("mpos-kunit: {passed} passed, {failed} failed");
}
