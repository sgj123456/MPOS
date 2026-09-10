//! The syscall numbers. Treat them as frozen API: the libc binds these values.
//!
//! The file stands alone on purpose: a user program includes it straight out
//! of the kernel source (`#[path = "..."] mod nr;`), so kernel and programs
//! cannot disagree about a number without one of them failing to build.

pub const EXIT: u64 = 0;
pub const READ: u64 = 1;
pub const WRITE: u64 = 2;
pub const YIELD: u64 = 3;
pub const GETPID: u64 = 4;
pub const SLEEP: u64 = 5;
pub const PIPE: u64 = 6;
pub const KILL: u64 = 7;
pub const OPEN: u64 = 8;
pub const CLOSE: u64 = 9;
pub const SPAWN: u64 = 10;
pub const WAIT: u64 = 11;
pub const LISTDIR: u64 = 12;
pub const DUP2: u64 = 13;
pub const CREATE: u64 = 14;
pub const UNLINK: u64 = 15;

/// The node kinds a LISTDIR buffer carries. One byte per entry, ahead of its
/// NUL-terminated name. Kernel and `ls` read the same table, so the mapping
/// cannot drift.
pub mod kind {
    pub const FILE: u8 = 0;
    pub const DIR: u8 = 1;
    pub const CHARDEV: u8 = 2;
}
