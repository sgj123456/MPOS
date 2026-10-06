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
pub(crate) mod mm;
pub(crate) mod sched;
pub(crate) mod vfs;
