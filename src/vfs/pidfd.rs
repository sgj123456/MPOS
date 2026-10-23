//! `pidfd_open(2)`: a stable descriptor for a process.
//!
//! A pid is a number a process can lose and another process can inherit - the
//! whole reason `pidfd_open` exists. The descriptor holds the *task* rather than
//! the number: this kernel's task table hands a freed slot to the next tenant,
//! so the descriptor records the slot **and** the generation that tenant was
//! given (`sched::runtime::task::Table::generations`), and every later use asks
//! both. A reused slot therefore reads "gone", not "that other process now".
//!
//! The shape is Linux's, measured on a current host:
//!
//! * `fstat` answers mode `0700` with **no** file-type bits - an anonymous
//!   inode, not a regular file, a character device or a socket - and size 0,
//!   nlink 1 (`docs/notes/` §15.86; `.workbuddy/probe_pidfd6.c`);
//! * `/proc/self/fd/<n>` reads back `anon_inode:[pidfd]`;
//! * `read`, `pread` and `write` are `EINVAL` (the inode has no byte stream at
//!   all, so even a zero-length call) and `lseek` is `ESPIPE`;
//! * the descriptor is readable when the **process** has died - the last thread
//!   leaving, not the leader - and `POLLHUP` joins `POLLIN` only once the task
//!   has actually been reaped. A pidfd handles both questions from two
//!   different facts: [`PidFd::ready`] asks the group, [`PidFd::hung_up`] asks
//!   whether the slot is still the task's.
//!
//! `pidfd_open` itself answers the flags Linux accepts (`O_NONBLOCK` alone) and
//! `FD_CLOEXEC` is set on every descriptor the door makes; the syscall layer
//! does that, since it owns the descriptor table.

use alloc::sync::Arc;
use core::sync::atomic::{AtomicU64, Ordering};

use super::{Ino, Inode, Metadata, NodeKind, Ready, VfsError, VfsResult};
use crate::sched::TaskId;

/// An inode number nobody else claims. The value is an identity for `fstat`,
/// not a device offset; like the other anonymous descriptors it only has to be
/// distinct.
static NEXT_INO: AtomicU64 = AtomicU64::new(1);

/// One process's pidfd.
pub struct PidFd {
    ino: Ino,
    /// The group leader the descriptor was made for, and the generation its
    /// slot carried at that moment.
    id: TaskId,
    generation: u64,
}

impl PidFd {
    pub fn new(id: TaskId, generation: u64) -> Arc<PidFd> {
        Arc::new(PidFd {
            ino: NEXT_INO.fetch_add(1, Ordering::Relaxed),
            id,
            generation,
        })
    }

    /// The group leader the descriptor names. Kept beside [`Self::generation`]
    /// because a caller that resolves a pidfd to a child - `waitid`'s
    /// `P_PIDFD` - needs the task, not just the answer "still there".
    pub fn id(&self) -> TaskId {
        self.id
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Whether the *process* the descriptor names has died: its last thread has
    /// left, whether or not anybody has reaped it yet.
    ///
    /// This is the `POLLIN` question. Linux's process pidfd becomes readable on
    /// `group_dead`; a leader that exited while a worker still runs is not the
    /// process's death (`docs/notes/` §15.86, `.workbuddy/probe_pidfd5.c`).
    /// On the host there is no scheduler and no task table, so the question has
    /// no answer to give and the stand-in says "not ready" rather than inventing
    /// one; the tests that need a live table are the booted ones.
    #[cfg(target_os = "none")]
    pub fn ready(&self) -> bool {
        !crate::sched::pidfd_group_alive(self.id, self.generation)
    }

    #[cfg(not(target_os = "none"))]
    pub fn ready(&self) -> bool {
        false
    }

    /// Whether the task behind the descriptor is gone for good - reaped, or the
    /// slot handed to a new tenant. This is the `POLLHUP` question, and it is
    /// deliberately not the same as [`Self::ready`]: a zombie is readable but
    /// not hung up, which is what lets a waiter tell "there is a death to
    /// collect" from "there is nothing left to collect".
    #[cfg(target_os = "none")]
    pub fn hung_up(&self) -> bool {
        !crate::sched::pidfd_still_names(self.id, self.generation)
    }

    #[cfg(not(target_os = "none"))]
    pub fn hung_up(&self) -> bool {
        false
    }
}

impl Inode for PidFd {
    fn as_pidfd(&self) -> Option<&PidFd> {
        Some(self)
    }

    /// A descriptor, not a file: `lseek(2)` is `ESPIPE`, exactly as it is on a
    /// pipe or a terminal.
    fn seekable(&self) -> bool {
        false
    }

    fn metadata(&self) -> Metadata {
        Metadata {
            ino: 0x7000_0000 + self.ino,
            // The type-less anonymous inode: `mode_of` gives it `0700` and no
            // `S_IF*` bit, which is the shape Linux's `anon_inode` mount
            // reports for this family.
            kind: NodeKind::Anon { mode: 0o700 },
            size: 0,
        }
    }

    /// No byte stream, at any offset and for any length - `EINVAL`, Linux's
    /// answer for both reads and writes (`File::read` skips its zero-length
    /// short-circuit for this kind, because Linux refuses even that).
    fn read_at(&self, _offset: u64, _buf: &mut [u8]) -> VfsResult<usize> {
        Err(VfsError::Invalid)
    }

    fn write_at(&self, _offset: u64, _buf: &[u8]) -> VfsResult<usize> {
        Err(VfsError::Invalid)
    }

    /// The name `/proc/<pid>/fd/<n>` reads back. An anonymous inode has no path
    /// to be found by, so the inode itself is the only place this can come
    /// from.
    fn fd_name(&self) -> Option<alloc::string::String> {
        Some(alloc::string::String::from("anon_inode:[pidfd]"))
    }

    fn poll_ready(&self) -> Ready {
        if self.ready() {
            Ready {
                read: true,
                write: false,
            }
        } else {
            Ready::NONE
        }
    }

    fn poll_hup(&self) -> bool {
        self.hung_up()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The `fstat` shape is the measured one: no `S_IF*` bits in `st_mode`, a
    /// zero size and one name. The type bits are the half that matters - a
    /// `pidfd` reported as a character device makes `S_ISCHR` true, and a tool
    /// that classifies descriptors by `st_mode` then treats it as a terminal or
    /// a device it can read. The `st_mode` those bits become is `abi::mode_of`'s
    /// to spell, so its claim lives beside that function
    /// (`syscall::linux::abi::tests`), not here - `vfs` does not name `syscall`.
    #[test]
    fn a_pidfd_is_a_type_less_anonymous_inode() {
        let fd = PidFd::new(TaskId(5), 7);
        let meta = fd.metadata();
        assert_eq!(meta.kind, NodeKind::Anon { mode: 0o700 });
        assert_eq!(meta.size, 0);
        assert_eq!(fd.nlink(), 0);
        assert_eq!(fd.owner(), (0, 0));
    }

    #[test]
    fn a_pidfd_has_no_byte_stream_and_no_position() {
        let fd = PidFd::new(TaskId(5), 7);
        assert!(!fd.seekable(), "lseek(pidfd) is ESPIPE");
        let mut buf = [0u8; 8];
        assert!(matches!(fd.read_at(0, &mut buf), Err(VfsError::Invalid)));
        assert!(matches!(fd.write_at(0, &[]), Err(VfsError::Invalid)));
    }

    #[test]
    fn the_anonymous_name_is_linux_spelling_and_the_host_has_no_readiness() {
        let fd = PidFd::new(TaskId(5), 7);
        assert_eq!(
            fd.fd_name().as_deref(),
            Some("anon_inode:[pidfd]"),
            "/proc/self/fd/<n> reads back Linux's own name for the family"
        );
        // No task table on the host: the readiness questions cannot be asked,
        // and the stand-in says "not ready, not hung up" rather than a yes or a
        // no it cannot know. The booted suite is what pins the real answers.
        assert!(!fd.ready());
        assert!(!fd.hung_up());
        assert_eq!(fd.poll_ready(), Ready::NONE);
        assert!(!fd.poll_hup());
    }
}
