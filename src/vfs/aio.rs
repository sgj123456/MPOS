//! `io_setup(2)` and the `io_*` family — the legacy **POSIX AIO** interface
//! Linux has had since 2.5, and the one `io_uring` replaced.
//!
//! The two are easy to confuse and are not related: `io_uring` (number 425,
//! `src/vfs/io_uring.rs`) is a **ring of shared memory** the caller writes
//! submission entries into and reads completions out of; this is a **context
//! handle** — an opaque `aio_context_t` the kernel returns from `io_setup`, a
//! `struct iocb` per request passed by pointer to `io_submit`, and a
//! `struct io_event` per completion read out of `io_getevents`.
//!
//! # What this serves, and the shape of the promise
//!
//! **The synchronous subset**, which is the same scope §6.32 records for
//! `io_uring` and for the same reason: this kernel's block layer is
//! synchronous, so an operation submitted to it finishes before `io_submit`
//! returns. That is still a real completion queue — the event is posted and
//! `io_getevents` drains it — but the completions are never *deferred*, so
//! `io_submit` may block where Linux's would not.
//!
//! **That difference is a performance shape, not an ABI one**, and the
//! measurement says so: Linux's own `io_getevents` with a zero timeout returns
//! whatever has completed, so a caller that submits and then polls sees the
//! same sequence of answers (submit count, then event count, then the results).
//! What a caller can observe here and not on Linux is *when* the work happened;
//! what it cannot observe is a wrong answer.
//!
//! Served: `IOCB_CMD_PREAD`, `PWRITE`, `FSYNC`, `FDSYNC`, `NOOP`. Refused by
//! name with `EINVAL`, as Linux refuses an opcode it does not know:
//! `IOCB_CMD_POLL`, `PREADV`, `PWRITEV` (a vectored shape needs an `iov`
//! import this door does not do), and every reserved number.
//!
//! # Bounds
//!
//! `docs/notes/` §8.0's allocator panics on failure, so every user-steered
//! count is capped: the number of live contexts at [`MAX_CONTEXTS`], the events
//! a context may queue at [`MAX_EVENTS`], and the `nr` argument of one
//! `io_submit` at [`MAX_SUBMIT`] — Linux's own `aio-max-nr` idea, sized small
//! because nothing in the images uses this.

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::cell::RefCell;

use critical_section::Mutex;

use crate::vfs::VfsError;
use crate::vfs::file::RawFd;

/// How many AIO contexts may exist at once.
///
/// A context costs one `Vec` of completions and a small record, so the cap is
/// about bounding a user-steered table rather than memory: Linux's own limit is
/// `aio-max-nr` requests *in flight*, not contexts, and a caller here gets a
/// context that completes synchronously.
const MAX_CONTEXTS: usize = 8;

/// How many completions one context may hold before `io_submit` refuses.
///
/// The events are drained by `io_getevents`; a caller that submits without ever
/// reading would otherwise grow the queue without bound. Refusing with
/// `EAGAIN` is Linux's own answer at its `aio-max-nr` ceiling.
const MAX_EVENTS: usize = 64;

/// The most `iocb`s one `io_submit` may name.
const MAX_SUBMIT: usize = 16;

/// One completed operation, in the caller's `struct io_event` shape: `data` is
/// the `iocb`'s own `aio_data` returned verbatim, `obj` is the `iocb` pointer,
/// and `res` is the operation's result — the byte count, or the negated errno.
#[derive(Clone, Copy)]
pub struct Event {
    pub data: u64,
    pub obj: u64,
    pub res: i64,
    pub res2: i64,
}

/// One AIO context: the completion queue and the identity the caller holds.
pub struct AioContext {
    events: Mutex<RefCell<Vec<Event>>>,
}

impl AioContext {
    /// How much room is left before [`MAX_EVENTS`].
    pub fn has_room(&self) -> bool {
        critical_section::with(|cs| self.events.borrow(cs).borrow().len() < MAX_EVENTS)
    }

    /// Post one completion.
    pub fn push(&self, event: Event) -> Result<(), VfsError> {
        critical_section::with(|cs| {
            let mut q = self.events.borrow(cs).borrow_mut();
            if q.len() >= MAX_EVENTS {
                return Err(VfsError::Again);
            }
            q.push(event);
            Ok(())
        })
    }

    /// Take up to `want` completions into `out`, answering how many were taken.
    ///
    /// `min_nr` is **advisory** here for the reason the module header gives:
    /// this context's completions are never deferred, so anything that has been
    /// submitted has already completed and the queue is as full as it will get.
    /// A caller asking for more than is there gets what is there, which is what
    /// Linux's own zero-timeout `io_getevents` does.
    pub fn drain(&self, want: usize, out: &mut Vec<Event>) -> usize {
        critical_section::with(|cs| {
            let mut q = self.events.borrow(cs).borrow_mut();
            let n = want.min(q.len());
            out.extend(q.drain(..n));
            n
        })
    }
}

/// Every live context, by handle.
///
/// A `BTreeMap` rather than a `Vec` because a caller holds the *handle* and not
/// an index: a slot reused after a destroy must not answer for the context that
/// used to be there, which is the class of bug `docs/notes/` §6.90 records for
/// a table keyed on a recyclable number. Handles are never reused.
static CONTEXTS: Mutex<RefCell<alloc::collections::BTreeMap<u64, Arc<AioContext>>>> =
    Mutex::new(RefCell::new(alloc::collections::BTreeMap::new()));

/// The next handle to hand out. Starts at 1 so that zero — the value a caller's
/// `aio_context_t` has before `io_setup` writes it — is never a live context,
/// and `io_destroy(0)` is therefore `EINVAL` rather than a use-after-free.
static NEXT_ID: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(1);

/// Create a context and answer its handle.
///
/// # Errors
///
/// `Invalid` for a zero `nr_events` (measured: `io_setup(0, &ctx)` is
/// `EINVAL`), and `Again` at [`MAX_CONTEXTS`].
pub fn setup(nr_events: usize) -> Result<u64, VfsError> {
    if nr_events == 0 {
        return Err(VfsError::Invalid);
    }
    critical_section::with(|cs| {
        let mut table = CONTEXTS.borrow(cs).borrow_mut();
        if table.len() >= MAX_CONTEXTS {
            return Err(VfsError::Again);
        }
        let id = NEXT_ID.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
        table.insert(
            id,
            Arc::new(AioContext {
                events: Mutex::new(RefCell::new(Vec::new())),
            }),
        );
        Ok(id)
    })
}

/// Look a context up by handle.
pub fn context(id: u64) -> Option<Arc<AioContext>> {
    critical_section::with(|cs| CONTEXTS.borrow(cs).borrow().get(&id).cloned())
}

/// Destroy a context, answering `false` when the handle names none.
///
/// The queue goes with it, which is Linux's contract: an `io_destroy` discards
/// uncollected completions.
pub fn destroy(id: u64) -> bool {
    critical_section::with(|cs| CONTEXTS.borrow(cs).borrow_mut().remove(&id).is_some())
}

/// The cap one `io_submit`'s `nr` is held to.
pub const fn max_submit() -> usize {
    MAX_SUBMIT
}

/// How many contexts are live - the boot suite's window into the table.
pub fn live_contexts() -> usize {
    critical_section::with(|cs| CONTEXTS.borrow(cs).borrow().len())
}

/// One `iocb` as this door reads it: the six fields the served opcodes use.
///
/// The layout is Linux's `struct iocb`, 64 bytes: `aio_data` at 0, the opcode
/// and its neighbours inside the first word's tail, `aio_fildes` at 16,
/// `aio_buf` at 24, `aio_nbytes` at 32, `aio_offset` at 40.
pub struct Iocb {
    pub data: u64,
    pub opcode: u16,
    pub fildes: RawFd,
    pub buf: u64,
    pub nbytes: u64,
    pub offset: i64,
}

/// `struct iocb`'s size, which is the bound `io_submit` validates a pointer to.
pub const IOCB_BYTES: usize = 64;
/// `struct io_event`'s size.
pub const IO_EVENT_BYTES: usize = 32;

/// The opcodes this door serves. Linux numbers them 0..=8 and every other value
/// is `EINVAL` (measured: opcode 99 answers `EINVAL` at `io_submit`).
pub const IOCB_CMD_PREAD: u16 = 0;
pub const IOCB_CMD_PWRITE: u16 = 1;
