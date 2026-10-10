//! The kernel's boot wiring: one flow, and what a machine's own evidence is.
//!
//! The kernel's entry and its panic handler are architecture-independent and live
//! in `src/main.rs`. What is left over is the boot - what the kernel does once the
//! machine layer has brought the machine up - and that is one flow
//! ([`flow`]), plus the harness and the suites of a *test* build ([`kunit`],
//! [`suites`]).
//!
//! There is one [`start`] for every machine, and one sequence behind it. What a
//! machine contributes to the kernel's boot is nothing at all: the entry, the
//! descriptor tables, the page tables, the wire and the description of its devices
//! are the machine layer's (`mpos::arch::boot`, whose single `#[cfg]` seam picks
//! this build's port - `AGENTS.md` §3, downward-only), and which transport carries
//! which *device* is a question the device probe asks of both rather than of a
//! preferred one (`drivers::virtio::probe`). The machine's name therefore appears
//! nowhere in the boot; where it still legitimately does is in [`suites`] and
//! [`kunit`], which is a test build's record of what one machine's UART or
//! register file does.
//!
//! The half of the flow that *is* shared logic rather than a step - the five
//! ordered steps, the exit hooks and the net bring-up - is [`steps`], beside the
//! flow that calls it: each carries an ordering contract that used to be written
//! down once per machine (`NOTES.md` §14.9).

/// The stages every machine's boot runs: the console service, the device step and
/// the kernel's own boot, which never returns.
mod flow;
/// The harness that runs a test build's suites, and the one place that lists them.
///
/// The suites themselves are elsewhere - each beside the module it tests, which
/// is what lets one read that module's private state, and the wiring's own in
/// [`suites`] and [`report`] - so this module is behind `--features selftest` and
/// a production kernel compiles none of them: that is what "no test openings in
/// shipped code" means here, and it is what let the boot path's test branches
/// become a handful of `#[cfg]`-gated calls instead of a command-line word the
/// kernel parsed on every boot. The flag is a cargo feature rather than a `--cfg`
/// for a measured reason - a `--cfg` cannot be *added* to this build, because
/// `RUSTFLAGS` replaces the machine's flags in `.cargo/config.toml` rather than
/// extending them (`docs/NOTES.md` §15.19).
#[cfg(feature = "selftest")]
mod kunit;
mod report;
mod steps;
/// The wiring layer's own suites: what only a *booted* machine answers, and the
/// read-back of what the device step registered. Test builds only, like
/// [`kunit`] that lists them.
#[cfg(feature = "selftest")]
mod suites;

/// Run the kernel from the loader's record. Never returns.
///
/// One function for every machine. It hands the record to the machine layer's own
/// entry, which brings the machine up and calls back at the three points the
/// kernel owns: the console service ([`flow::console_up`]), the machine's devices
/// ([`flow::devices`]) and the kernel's own boot ([`flow::init`]).
///
/// # Safety
///
/// As `mpos::arch::boot`, which is what this hands the record to.
pub unsafe fn start(handoff: *const mpos::boot::handoff::Handoff) -> ! {
    // SAFETY: the caller's contract, restated in `mpos::arch::boot`'s own
    // `# Safety` - the record handed over is the one the loader wrote, and the
    // machine layer's translation of it is what checks the magic.
    unsafe { mpos::arch::boot(handoff, flow::console_up, flow::devices, flow::init) }
}

/// What the panic handler prints about the context it died in.
///
/// One definition for both machines ([`report`]): the walk is a function of the
/// stack pointer, the kernel's text range and the scheduler's record of the
/// running task, and none of those is a machine's to answer differently. What a
/// machine *does* contribute is `hal::stack_pointer` - the register read - and
/// the floor its own traps recover to.
pub use report::panic_context;
