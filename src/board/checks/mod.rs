//! The milestone checks every machine runs, in one copy.
//!
//! `M0`/`M2` (the allocators), `M4` (the scheduler), and the `M6`/`M7`/`M8` families
//! (the syscall gate, ramfs, mounts, pipes, tty and signals) are answers about
//! `mm`, `sched`, `vfs` and the syscall dispatch - none of which is any
//! machine's. They lived in the x86_64 body while it was the only machine that
//! had them, which is how a second copy would have been written the day the
//! second machine wanted them: this module is that day's answer, and the reason
//! it is here rather than in the library is the one `board/steps.rs` records.
//!
//! What stays in a body is what only that machine can answer: its own paging and
//! interrupt proof (`check_arch`), its UART's transmit and receive paths, and the
//! device probes for its bus.

pub(crate) mod ipc;
pub(crate) mod vfs;

// ---------------------------------------------------------------------------
// The families, as the harness's cases
// ---------------------------------------------------------------------------

use alloc::string::String;

use mpos::boot::BootInfo;

use super::report;

/// One case per family, and each body still answers by panicking.
///
/// That is the next slice's work rather than a shape worth keeping: a family is
/// one chain of `assert!`s today, and splitting a chain into named cases requires
/// its assertions to answer a value first, because `panic = "abort"` means a
/// panic ends the machine and hides every case after it (`src/board/kunit.rs`
/// argues both halves). Until then a failing family still takes the boot down
/// with the `assert!` that failed in the log, which is what it did before.
pub(crate) fn case_vfs(_info: &BootInfo) -> Result<(), String> {
    vfs::run();
    Ok(())
}

pub(crate) fn case_ipc(_info: &BootInfo) -> Result<(), String> {
    ipc::run();
    Ok(())
}

/// The boot stack's canary, which is a check *of the boot* rather than of a
/// subsystem: it is the deepest the boot stack ever gets that is measured here.
pub(crate) fn case_boot_stack(info: &BootInfo) -> Result<(), String> {
    report::check_boot_stack(info, "after the self-tests");
    Ok(())
}
