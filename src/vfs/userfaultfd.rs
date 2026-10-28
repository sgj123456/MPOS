//! `userfaultfd(2)`: a process asks to be told about its own page faults, and
//! answers them itself.
//!
//! The shape is a descriptor carrying a queue of 32-byte pagefault records. A
//! process registers an address range; touching an absent page in it does not
//! allocate a frame - it queues a record naming the page and **parks the
//! faulting task** until the process answers with `UFFDIO_COPY` (here is the
//! page) or `UFFDIO_ZEROPAGE` (a zero page will do). That is the whole of what
//! the served subset does, and it is what a live-migration or lazy-restore
//! program is built on: the fault becomes a message, and the message's answer
//! is the page.
//!
//! This module is the descriptor's `Inode` face. The machinery - the ranges,
//! the queue, the parking - lives in [`crate::sched::userfault`], because the
//! fault path does and `sched` may not call up into `vfs`
//! (`docs/ARCHITECTURE.md` §3). The split is `sysv_msg`'s: the queue is
//! process state, the file is the descriptor.
//!
//! # What this kernel serves, and what it refuses by name
//!
//! Measured on Linux 7.2.11 as real root (`build/scratch/uffd/probe.c`,
//! `probe3.c`; the call is `EPERM` for an unprivileged user, so there is no
//! cheaper source), the record is `docs/notes/06-process-management.md` §6.33.
//! Served: `UFFDIO_API`, `UFFDIO_REGISTER` in `MODE_MISSING`,
//! `UFFDIO_UNREGISTER`, `UFFDIO_WAKE`, `UFFDIO_COPY`, `UFFDIO_ZEROPAGE`, and a
//! `read(2)` of one record at a time.
//!
//! Refused with `EOPNOTSUPP` rather than faked, each a real Linux mode this
//! kernel has no machinery for: `MODE_WP` and `MODE_MINOR` registration,
//! `UFFDIO_WRITEPROTECT`, `UFFDIO_CONTINUE`, `UFFDIO_MOVE`, `UFFDIO_POISON`,
//! and `UFFD_FEATURE_*` in the API handshake (so `features` answers `0` where
//! Linux answers `0x1ffff`). `UFFDIO_COPY`'s `MODE_WP` bit is refused the same
//! way. The distinction matters: a program that asked for write-protect
//! tracking and got a success would believe in faults it will never receive.
//!
//! Three more departures, all visible from userland and all deliberate:
//!
//! * **No `UFFD_FEATURE_EVENT_FORK`**, so a `fork` gives the child no uffd
//!   context and its touch of a registered page is `SIGBUS` - which is Linux's
//!   own answer when the feature was not negotiated (measured).
//! * **`ptid` is always 0**: the `THREAD_ID` feature is not offered, and Linux
//!   reports 0 for it when it was not asked for.
//! * **The queue is bounded** at [`crate::sched::userfault::MAX_PENDING`],
//!   because the global allocator still panics on failure (`docs/notes/` §8.0)
//!   and a record is an allocation a user steers. A page whose record cannot be
//!   queued resolves the ordinary way instead of hanging.
//!
//! # The one thing this kernel does that Linux does not
//!
//! Linux's fault handler queues the event and parks the faulting task
//! *itself*. Here the queue lives in `sched`, but `populate_in` runs with the
//! task table borrowed, and parking switches tasks - so the delivery is done by
//! `resolve_anon_fault` **before** it takes the table. The observable shape is
//! the same: the faulting instruction does not return until the page is there.

use super::userfaultfd_abi::*;
use super::{Inode, Metadata, NodeKind, Ready, VfsError, VfsResult};
use crate::sched::policy::TaskId;
use crate::sched::userfault::UserfaultObject;
use alloc::sync::Arc;

/// One `userfaultfd` descriptor.
pub struct UserfaultFd {
    object: Arc<UserfaultObject>,
}

impl UserfaultFd {
    /// A fresh descriptor, or `None` when the machine already holds
    /// [`crate::sched::userfault::MAX_DESCRIPTORS`].
    pub fn new(nonblocking: bool) -> Option<Arc<UserfaultFd>> {
        let object = UserfaultObject::create(nonblocking)?;
        Some(Arc::new(UserfaultFd { object }))
    }

    /// The machinery behind the descriptor - the ioctl handlers ask it.
    pub fn object(&self) -> &Arc<UserfaultObject> {
        &self.object
    }

    fn pending(&self) -> bool {
        self.object.has_pending()
    }

    fn nonblocking(&self) -> bool {
        self.object.nonblocking()
    }
}

impl Inode for UserfaultFd {
    /// A queue has no position: `lseek(2)` on such a descriptor is `ESPIPE`.
    fn seekable(&self) -> bool {
        false
    }

    fn metadata(&self) -> Metadata {
        Metadata {
            ino: 0x9a00_0000 + self.object.id(),
            kind: NodeKind::Anon { mode: 0o600 },
            size: 0,
        }
    }

    /// Linux's name for the descriptor in `/proc/<pid>/fd` (measured; the
    /// bracket form is what the family's other members use).
    fn fd_name(&self) -> Option<alloc::string::String> {
        Some(alloc::string::String::from("anon_inode:[userfaultfd]"))
    }

    /// One record per read. A buffer smaller than one is `EINVAL` (measured),
    /// nothing queued parks, and a non-blocking descriptor answers `EAGAIN`
    /// (measured).
    fn read_at(&self, _offset: u64, buf: &mut [u8]) -> VfsResult<usize> {
        if buf.len() < MSG_BYTES {
            return Err(VfsError::Invalid);
        }
        loop {
            if let Some(p) = self.object.take_pending() {
                let rec = pagefault_record(
                    if p.write { PAGEFAULT_FLAG_WRITE } else { 0 },
                    p.page as u64,
                    0,
                );
                buf[..MSG_BYTES].copy_from_slice(&rec);
                return Ok(MSG_BYTES);
            }
            if self.nonblocking() {
                return Err(VfsError::Again);
            }
            self.wait_for_news()?;
        }
    }

    /// Nothing is written to a queue: Linux's refusal is `EBADF`.
    fn write_at(&self, _offset: u64, _buf: &[u8]) -> VfsResult<usize> {
        Err(VfsError::BadFd)
    }

    fn poll_ready(&self) -> Ready {
        Ready {
            read: self.pending(),
            write: false,
        }
    }

    fn poll_subscribe(&self, id: TaskId) {
        self.object.subscribe_reader(id);
    }

    fn poll_unsubscribe(&self, id: TaskId) {
        self.object.unsubscribe_reader(id);
    }

    fn as_userfaultfd(&self) -> Option<&UserfaultFd> {
        Some(self)
    }
}

impl UserfaultFd {
    /// Park until a record lands. Interruptible, like every wait a syscall
    /// parks in: a live-migration daemon is also a program with timers.
    #[cfg(target_os = "none")]
    fn wait_for_news(&self) -> VfsResult<()> {
        self.poll_subscribe(crate::sched::current());
        let end = crate::sched::block_until_interruptible(|| self.pending());
        self.poll_unsubscribe(crate::sched::current());
        match end {
            crate::sched::WaitEnd::Interrupted => Err(VfsError::Interrupted),
            _ => Ok(()),
        }
    }

    #[cfg(not(target_os = "none"))]
    fn wait_for_news(&self) -> VfsResult<()> {
        panic!("blocking on a userfaultfd needs the kernel scheduler");
    }
}

/// The ioctl request words this family answers, so `handle_ioctl` can route
/// them by the type magic `0xAA` and the handlers can match on the word.
pub fn is_userfaultfd_request(request: u32) -> bool {
    (request >> 8) & 0xff == 0xAA
}
