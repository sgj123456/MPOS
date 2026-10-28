//! `fanotify`: the notification class of Linux's file-notification interface.
//!
//! This is `inotify`'s sibling in the vfs - the same hook sites feed it, the
//! same queue discipline drains it - and it differs in exactly the two ways
//! Linux's fanotify does. A watcher is **not** per-inode-descriptor: one
//! group (the descriptor `fanotify_init(2)` returns) holds a set of marks,
//! and one group sends **one event carrying an fd and a pid**, not a batch
//! of wd records. A daemon that dispatches on the object itself (a virus
//! scanner, a backup's "did this file get touched" audit) is the caller
//! that shape exists for; a listener that names objects by watch descriptor
//! keeps using inotify.
//!
//! What this module serves, measured against Linux 7.2.11 with real root
//! (`build/scratch/fanokey/fanotify_probe.c`, run as PID 1 of a QEMU VM
//! because `fanotify_init(2)` itself refuses an ordinary user with `EPERM`):
//!
//! * `fanotify_init(2)` - the notification class only. `FAN_CLOEXEC` and
//!   `FAN_NONBLOCK` are the descriptor's own; the event-flags word is
//!   validated and carried. The content classes, the `FAN_REPORT_*` event
//!   shapes (fid/pidfd/name), `FAN_ENABLE_AUDIT` and the two
//!   `FAN_UNLIMITED_*` limits are **honestly refused** with `EOPNOTSUPP`,
//!   not faked (`docs/uapi/` §5.1 item 114).
//! * `fanotify_mark(2)` - `FAN_MARK_ADD`/`FAN_MARK_REMOVE`/`FAN_MARK_FLUSH`
//!   on **inode marks** (`FAN_MARK_INODE`), with the notification mask bits
//!   this module reports. Mount marks (`FAN_MARK_MOUNT`) and filesystem
//!   marks (`FAN_MARK_FILESYSTEM`) are refused by name: the tree here has
//!   no inode-to-mount ancestry to answer "is this object in that mount"
//!   without guessing, and a mount mark served as a filesystem mark would
//!   be a wrong shape rather than a partial one.
//! * The event stream: `FAN_OPEN`, `FAN_ACCESS`, `FAN_MODIFY`, `FAN_ATTRIB`,
//!   `FAN_CLOSE_WRITE`, `FAN_CLOSE_NOWRITE`, `FAN_OPEN_EXEC`, with the
//!   measured record (`event_len` 24, `metadata_len` 24, `vers` 3, the
//!   `mask`, an `fd` for the object and the triggering `pid`), one event per
//!   object open as `read(2)` asks for them. A buffer shorter than one
//!   metadata record is `EINVAL` (measured); a queued record bigger than the
//!   buffer is not consumed (measured); a non-blocking read with nothing
//!   queued is `EAGAIN`.
//!
//! The event fd is a **fresh description over the marked inode**, opened when
//! the reader drains the record - not the description the mutation hook had
//! in hand, and not a dup of it: Linux's event fd is its own open of the
//! object, and a caller that reads it reads from offset zero. What is *not*
//! Linux's is the flags word: `event_f_flags` is validated against the
//! measured set but the descriptor handed over is a plain read-side
//! description, so `O_CLOEXEC`/`O_NONBLOCK` on it are not applied. Named
//! rather than quietly half-done (`docs/notes/` §6.31).

use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use super::{Inode, Metadata, NodeKind, Ready, VfsError, VfsResult};

// ---------------------------------------------------------------------------
// The uapi vocabulary (`include/uapi/linux/fanotify.h`, Linux 6.12)
// ---------------------------------------------------------------------------

/// The event bits this module can report: the notification-class file events.
pub const FAN_ACCESS: u32 = 0x0000_0001;
pub const FAN_MODIFY: u32 = 0x0000_0002;
pub const FAN_ATTRIB: u32 = 0x0000_0004;
pub const FAN_CLOSE_WRITE: u32 = 0x0000_0008;
pub const FAN_CLOSE_NOWRITE: u32 = 0x0000_0010;
pub const FAN_OPEN: u32 = 0x0000_0020;
pub const FAN_OPEN_EXEC: u32 = 0x0000_1000;

/// Served notification masks - the set a mark may name.
pub const SERVED_MASKS: u32 = FAN_ACCESS
    | FAN_MODIFY
    | FAN_ATTRIB
    | FAN_CLOSE_WRITE
    | FAN_CLOSE_NOWRITE
    | FAN_OPEN
    | FAN_OPEN_EXEC;

/// The bits that ride on a record's `mask` without being a *mark's* to ask
/// for: the queue-overflow and error records.
pub const FAN_FS_ERROR: u32 = 0x0000_8000;

/// Child/mount-shaped notification bits: Linux knows them, this tree has no
/// ancestry to match them with, so a mark naming them is refused by name.
pub const FAN_MOVED_FROM: u32 = 0x0000_0040;
pub const FAN_MOVED_TO: u32 = 0x0000_0080;
pub const FAN_CREATE: u32 = 0x0000_0100;
pub const FAN_DELETE: u32 = 0x0000_0200;
pub const FAN_DELETE_SELF: u32 = 0x0000_0400;
pub const FAN_MOVE_SELF: u32 = 0x0000_0800;
pub const FAN_EVENT_ON_CHILD: u32 = 0x0800_0000;
pub const FAN_RENAME: u32 = 0x1000_0000;
pub const FAN_ONDIR: u32 = 0x4000_0000;
pub const UNSERVED_NOTIFICATION_MASKS: u32 = FAN_MOVED_FROM
    | FAN_MOVED_TO
    | FAN_CREATE
    | FAN_DELETE
    | FAN_DELETE_SELF
    | FAN_MOVE_SELF
    | FAN_FS_ERROR
    | FAN_EVENT_ON_CHILD
    | FAN_RENAME
    | FAN_ONDIR;

/// Permission events: they belong to the content classes, which this kernel
/// does not serve.
pub const FAN_OPEN_PERM: u32 = 0x0001_0000;
pub const FAN_ACCESS_PERM: u32 = 0x0002_0000;
pub const FAN_OPEN_EXEC_PERM: u32 = 0x0004_0000;
const PERMISSION_MASKS: u32 = FAN_OPEN_PERM | FAN_ACCESS_PERM | FAN_OPEN_EXEC_PERM;

/// The queue-overflow record's bit. Linux sends this one instead of the
/// records it had to drop, once, with `fd = FAN_NOFD`.
pub const FAN_Q_OVERFLOW: u32 = 0x0000_4000;

/// `fanotify_init(2)`'s flags word.
pub const FAN_CLOEXEC: u32 = 0x0000_0001;
pub const FAN_NONBLOCK: u32 = 0x0000_0002;
pub const FAN_CLASS_CONTENT: u32 = 0x0000_0004;
pub const FAN_CLASS_PRE_CONTENT: u32 = 0x0000_0008;
pub const FAN_UNLIMITED_QUEUE: u32 = 0x0000_0010;
pub const FAN_UNLIMITED_MARKS: u32 = 0x0000_0020;
pub const FAN_ENABLE_AUDIT: u32 = 0x0000_0040;
pub const FAN_REPORT_PIDFD: u32 = 0x0000_0080;
pub const FAN_REPORT_TID: u32 = 0x0000_0100;
pub const FAN_REPORT_FID: u32 = 0x0000_0200;
pub const FAN_REPORT_DIR_FID: u32 = 0x0000_0400;
pub const FAN_REPORT_NAME: u32 = 0x0000_0800;
pub const FAN_REPORT_TARGET_FID: u32 = 0x0000_1000;

/// `fanotify_mark(2)`'s flags word.
pub const FAN_MARK_ADD: u32 = 0x0000_0001;
pub const FAN_MARK_REMOVE: u32 = 0x0000_0002;
pub const FAN_MARK_DONT_FOLLOW: u32 = 0x0000_0004;
pub const FAN_MARK_ONLYDIR: u32 = 0x0000_0008;
pub const FAN_MARK_MOUNT: u32 = 0x0000_0010;
pub const FAN_MARK_IGNORED_MASK: u32 = 0x0000_0020;
pub const FAN_MARK_IGNORED_SURV_MODIFY: u32 = 0x0000_0040;
pub const FAN_MARK_FLUSH: u32 = 0x0000_0080;
pub const FAN_MARK_FILESYSTEM: u32 = 0x0000_0100;
pub const FAN_MARK_EVICTABLE: u32 = 0x0000_0200;
pub const FAN_MARK_IGNORE: u32 = 0x0000_0400;

/// The event record's `vers` word (`FANOTIFY_METADATA_VERSION`).
pub const FANOTIFY_METADATA_VERSION: u8 = 3;
/// One `struct fanotify_event_metadata` on the wire.
pub const FAN_EVENT_METADATA_LEN: usize = 24;
/// The record's `fd` when it carries no object (the overflow record).
pub const FAN_NOFD: i32 = -1;

/// The event-flags word is validated against what Linux accepted in the
/// measurement: the two access shapes, append, the descriptor bits and the
/// per-event no-atime request (that last one is accepted from root here).
pub const EVENT_F_FLAGS_ALLOWED: u32 = 0o1 | 0o2 | 0o2000 | 0o4000 | 0o100000 | 0o40000 | 0o2000000;

/// Linux's default queue size bound; `FAN_UNLIMITED_QUEUE` lifts it, which
/// this kernel does not serve (the cap is a bound the allocator needs, and
/// the ring is a `Vec` here).
const MAX_QUEUED_EVENTS: usize = 16_384;
/// One group per live descriptor; the same shape `inotify`'s instance cap
/// takes (the global allocator panics on failure, so a caller-steerable
/// allocation keeps a bound).
const MAX_GROUPS: usize = 128;
/// One mark per object per group - the table is a `Vec` because a group
/// typically holds a handful.
const MAX_MARKS: usize = 4_096;

static LIVE_GROUPS: AtomicUsize = AtomicUsize::new(0);
/// Every live group, for the emit path to walk. `Weak` because a closed
/// descriptor must retire its group (and its marks) without a sweep.
static GROUPS: critical_section::Mutex<core::cell::RefCell<Vec<Weak<FanotifyGroup>>>> =
    critical_section::Mutex::new(core::cell::RefCell::new(Vec::new()));

/// One mark: the object's `(dev, ino)` key and the mask the group asked for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Mark {
    pub key: (u64, u64),
    pub mask: u32,
}

/// One queued event. The object is an inode, not a descriptor, because the
/// event fd must be a **fresh open** when the reader drains it (Linux's own
/// shape, see the module header); the overflow record is the one with no
/// object.
#[derive(Clone)]
struct QueuedEvent {
    mask: u32,
    pid: i32,
    inode: Option<Arc<dyn Inode>>,
}

struct GroupState {
    queue: Vec<QueuedEvent>,
    /// Sticky: set when a record was dropped for a full queue. The single
    /// overflow record clears it, so a flood gets one report.
    dropped: bool,
    waiters: Vec<crate::sched::TaskId>,
    marks: Vec<Mark>,
}

/// One `fanotify_init(2)` descriptor: its queue and its marks.
pub struct FanotifyGroup {
    nonblock: AtomicBool,
    /// Whether the descriptions handed to a reader carry `FD_CLOEXEC` - the
    /// `O_CLOEXEC` half of the measured `event_f_flags` vocabulary. Read by
    /// the machine build's `pop_record`; the host build has no descriptors to
    /// set it on, hence the allow.
    #[cfg_attr(not(target_os = "none"), allow(dead_code))]
    event_cloexec: bool,
    state: critical_section::Mutex<core::cell::RefCell<GroupState>>,
}

impl FanotifyGroup {
    /// A fresh group. The caller has already validated the flags word
    /// ([`init_accepts`]); the queue cap is the constant here.
    fn new(nonblock: bool, event_cloexec: bool) -> Arc<FanotifyGroup> {
        Arc::new(FanotifyGroup {
            nonblock: AtomicBool::new(nonblock),
            event_cloexec,
            state: critical_section::Mutex::new(core::cell::RefCell::new(GroupState {
                queue: Vec::new(),
                dropped: false,
                waiters: Vec::new(),
                marks: Vec::new(),
            })),
        })
    }

    pub fn nonblocking(&self) -> bool {
        self.nonblock.load(Ordering::Relaxed)
    }

    pub fn set_nonblocking(&self, on: bool) {
        self.nonblock.store(on, Ordering::Relaxed);
    }

    fn pending(&self) -> bool {
        critical_section::with(|cs| !self.state.borrow(cs).borrow().queue.is_empty())
    }

    /// The mask this group wants for one object, if any mark names it.
    fn wanted(&self, key: (u64, u64)) -> u32 {
        critical_section::with(|cs| {
            self.state
                .borrow(cs)
                .borrow()
                .marks
                .iter()
                .find(|mark| mark.key == key)
                .map(|mark| mark.mask)
                .unwrap_or(0)
        })
    }

    /// Append one record, minding the cap. Returns the tasks to wake.
    fn push(&self, event: QueuedEvent) -> Vec<crate::sched::TaskId> {
        critical_section::with(|cs| {
            let mut st = self.state.borrow(cs).borrow_mut();
            if st.queue.len() >= MAX_QUEUED_EVENTS {
                if !st.dropped {
                    st.dropped = true;
                    st.queue.push(QueuedEvent {
                        mask: FAN_Q_OVERFLOW,
                        pid: 0,
                        inode: None,
                    });
                }
                return st.waiters.clone();
            }
            if st.dropped {
                // The overflow record stays where it landed; a later record
                // queues behind it and clears the flag on the next drain.
                st.queue.push(event);
                return st.waiters.clone();
            }
            st.queue.push(event);
            st.waiters.clone()
        })
    }

    /// Drop every mark (and the queue) - the clear-on-close half.
    /// Take the marks for one object: returns the group's mask for it, or
    /// `None`. Used by the mark syscall.
    fn mark_of(&self, key: (u64, u64)) -> Option<u32> {
        critical_section::with(|cs| {
            self.state
                .borrow(cs)
                .borrow()
                .marks
                .iter()
                .find(|m| m.key == key)
                .map(|m| m.mask)
        })
    }

    fn set_mask(&self, key: (u64, u64), mask: u32) {
        critical_section::with(|cs| {
            let mut st = self.state.borrow(cs).borrow_mut();
            match st.marks.iter_mut().find(|m| m.key == key) {
                Some(mark) => {
                    if mask == 0 {
                        st.marks.retain(|m| m.key != key);
                    } else {
                        mark.mask = mask;
                    }
                }
                None => {
                    if mask != 0 && st.marks.len() < MAX_MARKS {
                        st.marks.push(Mark { key, mask });
                    }
                }
            }
        });
    }

    /// Drains at most one event into `buf`: the packed-record walk lives in
    /// `read_at`, this is its single-record half. Returns the bytes written
    /// (always one record here) or `None` when the queue is empty.
    fn pop_record(&self, buf: &mut [u8]) -> VfsResult<Option<usize>> {
        let event = critical_section::with(|cs| {
            let mut st = self.state.borrow(cs).borrow_mut();
            if st.queue.is_empty() {
                None
            } else {
                // The overflow report is consumed once; the flag is the
                // queue's own state, not the record's.
                let ev = st.queue.remove(0);
                if ev.mask == FAN_Q_OVERFLOW {
                    st.dropped = false;
                }
                Some(ev)
            }
        });
        let Some(event) = event else { return Ok(None) };
        let fd = match &event.inode {
            Some(inode) => match crate::vfs::file::open_inode(inode.clone()) {
                Ok(fd) => {
                    #[cfg(target_os = "none")]
                    if self.event_cloexec {
                        let _ = crate::sched::current_fds()
                            .fd_flags_set(fd, crate::vfs::file::FD_CLOEXEC);
                    }
                    fd
                }
                Err(e) => {
                    // The event survives a refused descriptor: the caller
                    // can drain it once it has room again.
                    critical_section::with(|cs| {
                        self.state.borrow(cs).borrow_mut().queue.insert(0, event);
                    });
                    return Err(e);
                }
            },
            None => FAN_NOFD,
        };
        write_record(buf, event.mask, fd, event.pid);
        Ok(Some(FAN_EVENT_METADATA_LEN))
    }
}

impl Drop for FanotifyGroup {
    fn drop(&mut self) {
        LIVE_GROUPS.fetch_sub(1, Ordering::Relaxed);
    }
}

/// Serialize one `struct fanotify_event_metadata` into `buf`'s front. The
/// layout is the uapi's, native-endian, with the two length words equal to
/// [`FAN_EVENT_METADATA_LEN`] and `vers` = 3 - what the measurement read
/// back from Linux.
fn write_record(buf: &mut [u8], mask: u32, fd: i32, pid: i32) {
    buf[0..4].copy_from_slice(&(FAN_EVENT_METADATA_LEN as u32).to_ne_bytes());
    buf[4] = FANOTIFY_METADATA_VERSION;
    buf[5] = 0;
    buf[6..8].copy_from_slice(&(FAN_EVENT_METADATA_LEN as u16).to_ne_bytes());
    buf[8..16].copy_from_slice(&(mask as u64).to_ne_bytes());
    buf[16..20].copy_from_slice(&fd.to_ne_bytes());
    buf[20..24].copy_from_slice(&pid.to_ne_bytes());
}

/// `fanotify_init(2)`'s answer to one word pair: `Ok(Some(nonblock))` for the
/// class this kernel serves, `Err(EopNotSupp)` for a shape it names, and
/// `Err(Invalid)` for a word no Linux accepts.
///
/// Measured on Linux 7.2.11 as root: every `FAN_REPORT_*` bit, the content
/// classes and `FAN_ENABLE_AUDIT` are accepted there (`0x100` accepted, so
/// was `0x40`), a bit outside the uapi's words is `EINVAL`, and a plain user
/// gets `EPERM` before any of it. So the refusal ordering here is: privilege
/// first (the caller's `EPERM`), then the words this kernel cannot honour
/// (`EOPNOTSUPP`, each named), then unknown bits (`EINVAL`).
pub fn init_accepts(flags: u32) -> Result<bool, VfsError> {
    let class = flags & (FAN_CLASS_CONTENT | FAN_CLASS_PRE_CONTENT);
    if class != 0 {
        // The content classes exist to answer permission events; this kernel
        // serves the notification class alone.
        return Err(VfsError::Unsupported);
    }
    // `FAN_UNLIMITED_QUEUE`/`FAN_UNLIMITED_MARKS` are Linux-shaped ("never
    // drop, never cap") and this kernel's queue and mark table both have a
    // bound, so accepting them would promise a limit-free queue and then
    // quietly overflow it - refused by name instead (the worst-case rule,
    // §8.0: the bound stays, and the caller is told).
    const NAMED: u32 = FAN_ENABLE_AUDIT
        | FAN_UNLIMITED_QUEUE
        | FAN_UNLIMITED_MARKS
        | FAN_REPORT_PIDFD
        | FAN_REPORT_TID
        | FAN_REPORT_FID
        | FAN_REPORT_DIR_FID
        | FAN_REPORT_NAME
        | FAN_REPORT_TARGET_FID;
    if flags & NAMED != 0 {
        return Err(VfsError::Unsupported);
    }
    const ALLOWED: u32 = FAN_CLOEXEC | FAN_NONBLOCK;
    if flags & !ALLOWED != 0 {
        return Err(VfsError::Invalid);
    }
    Ok(flags & FAN_NONBLOCK != 0)
}

/// Validate the event-flags word the caller asked the event descriptors to
/// carry. Linux accepted each of the words in [`EVENT_F_FLAGS_ALLOWED`] in the
/// measurement and refused anything outside the access/flags vocabulary.
pub fn event_flags_accepts(event_f_flags: u32) -> Result<(), VfsError> {
    if event_f_flags & !EVENT_F_FLAGS_ALLOWED != 0 {
        return Err(VfsError::Invalid);
    }
    Ok(())
}

/// `fanotify_mark(2)`'s flags word, split into what this kernel can do.
/// `Ok(())` when the word is servable, `Err` naming the reason otherwise - the
/// syscall answers `EINVAL` for a malformed word and `EOPNOTSUPP` for a
/// Linux-shaped one this kernel does not implement.
pub fn mark_flags_accepts(flags: u32) -> Result<(), VfsError> {
    let scope = flags & (FAN_MARK_MOUNT | FAN_MARK_FILESYSTEM);
    if scope != 0 {
        // No inode-to-mount ancestry in this tree: a mount mark cannot be
        // answered without guessing (module header).
        return Err(VfsError::Unsupported);
    }
    const NAMED: u32 =
        FAN_MARK_IGNORED_MASK | FAN_MARK_IGNORED_SURV_MODIFY | FAN_MARK_EVICTABLE | FAN_MARK_IGNORE;
    if flags & NAMED != 0 {
        return Err(VfsError::Unsupported);
    }
    const ALLOWED: u32 =
        FAN_MARK_ADD | FAN_MARK_REMOVE | FAN_MARK_DONT_FOLLOW | FAN_MARK_ONLYDIR | FAN_MARK_FLUSH;
    if flags & !ALLOWED != 0 {
        return Err(VfsError::Invalid);
    }
    let add = flags & FAN_MARK_ADD != 0;
    let remove = flags & FAN_MARK_REMOVE != 0;
    let flush = flags & FAN_MARK_FLUSH != 0;
    if add && remove {
        // Measured: `FAN_MARK_ADD|FAN_MARK_REMOVE` is `EINVAL`.
        return Err(VfsError::Invalid);
    }
    if !add && !remove && !flush {
        return Err(VfsError::Invalid);
    }
    if flush && (add || remove) {
        return Err(VfsError::Invalid);
    }
    Ok(())
}

/// Validate the mask a mark asks for, against the class this group has.
pub fn mark_mask_accepts(mask: u32) -> Result<(), VfsError> {
    if mask == 0 {
        // Measured: a zero mask is `EINVAL`.
        return Err(VfsError::Invalid);
    }
    if mask & PERMISSION_MASKS != 0 {
        // Measured: a permission mask on a notification group is `EINVAL`
        // (the class decides which event family the mask may name).
        return Err(VfsError::Invalid);
    }
    if mask & UNSERVED_NOTIFICATION_MASKS != 0 {
        return Err(VfsError::Unsupported);
    }
    if mask & !SERVED_MASKS != 0 {
        return Err(VfsError::Invalid);
    }
    Ok(())
}

/// Make one group for `fanotify_init(2)`: privilege, flags and the class are
/// the caller's checks ([`init_accepts`]), the cap is the machine's.
pub fn group_new(nonblock: bool, event_cloexec: bool) -> Option<Arc<FanotifyGroup>> {
    let held = LIVE_GROUPS.fetch_add(1, Ordering::Relaxed);
    if held >= MAX_GROUPS {
        LIVE_GROUPS.fetch_sub(1, Ordering::Relaxed);
        return None;
    }
    let group = FanotifyGroup::new(nonblock, event_cloexec);
    critical_section::with(|cs| {
        let mut groups = GROUPS.borrow(cs).borrow_mut();
        groups.retain(|g| g.strong_count() > 0);
        groups.push(Arc::downgrade(&group));
    });
    Some(group)
}

/// Add `mask` to the group's mark on `key`; a mark that already exists keeps
/// its bits (`FAN_MARK_ADD` adds to the mask, Linux's wording) and adding a
/// mark past the group's cap is refused with `ENOSPC`.
pub fn mark_add(group: &FanotifyGroup, key: (u64, u64), mask: u32) -> VfsResult<()> {
    let existing = group.mark_of(key);
    match existing {
        Some(old) => {
            group.set_mask(key, old | mask);
            Ok(())
        }
        None => {
            let count = critical_section::with(|cs| group.state.borrow(cs).borrow().marks.len());
            if count >= MAX_MARKS {
                return Err(VfsError::NoSpace);
            }
            group.set_mask(key, mask);
            Ok(())
        }
    }
}

/// Remove `mask`'s bits from the mark on `key`, dropping the mark when none
/// remain. Returns `None` when the object had no mark - the syscall's
/// `ENOENT`.
pub fn mark_remove(group: &FanotifyGroup, key: (u64, u64), mask: u32) -> Option<()> {
    let existing = group.mark_of(key)?;
    let left = if mask == 0 { 0 } else { existing & !mask };
    // A zero mask on REMOVE drops the whole mark, which is the measured
    // shape for "remove this object's mark".
    group.set_mask(key, if mask == 0 { 0 } else { left });
    Some(())
}

/// Drop every mark in the group; `FAN_MARK_FLUSH`'s answer.
pub fn mark_flush(group: &FanotifyGroup) {
    critical_section::with(|cs| {
        group.state.borrow(cs).borrow_mut().marks.clear();
    });
}

/// Whether a group's mark names this event. A mark fires when the event's
/// bit is in the mark's mask; the notification class has no `IN_ISDIR`
/// filter (that bit is a directory-event flag in the *uns*served set).
pub fn mark_matches(mask: u32, event: u32) -> bool {
    mask & event != 0
}

// ---------------------------------------------------------------------------
// The emit path: the vfs hooks tell this module what happened
// ---------------------------------------------------------------------------

/// The pid an event carries: the task that caused it, where it stands.
/// The host build has no tasks, so its tests never read this as anything but
/// zero.
#[cfg(target_os = "none")]
fn emitting_pid() -> i32 {
    crate::sched::current_pid_number() as i32
}

#[cfg(not(target_os = "none"))]
fn emitting_pid() -> i32 {
    0
}

/// The mask a mark would have wanted for `inode`, or 0; then queue one event
/// per group that wants it.
fn notice(inode: &Arc<dyn Inode>, mask: u32) {
    let key = crate::vfs::inotify::key_of(inode);
    let pid = emitting_pid();
    let groups: Vec<Arc<FanotifyGroup>> = critical_section::with(|cs| {
        let mut list = GROUPS.borrow(cs).borrow_mut();
        list.retain(|g| g.strong_count() > 0);
        list.iter().filter_map(Weak::upgrade).collect()
    });
    for group in groups {
        let wanted = group.wanted(key);
        if wanted == 0 || !mark_matches(wanted, mask) {
            continue;
        }
        let event = QueuedEvent {
            mask,
            pid,
            inode: Some(inode.clone()),
        };
        let waking = group.push(event);
        #[cfg(target_os = "none")]
        for id in waking {
            crate::sched::wake(id);
        }
        #[cfg(not(target_os = "none"))]
        let _ = waking;
    }
}

/// A file was opened: `FAN_OPEN`.
pub fn notice_open(inode: &Arc<dyn Inode>) {
    notice(inode, FAN_OPEN);
}

/// A file was opened for execution: `FAN_OPEN_EXEC`.
pub fn notice_open_exec(inode: &Arc<dyn Inode>) {
    notice(inode, FAN_OPEN_EXEC);
}

/// Bytes were read: `FAN_ACCESS`.
pub fn notice_access(inode: &Arc<dyn Inode>) {
    notice(inode, FAN_ACCESS);
}

/// Bytes were written: `FAN_MODIFY`.
pub fn notice_modify(inode: &Arc<dyn Inode>) {
    notice(inode, FAN_MODIFY);
}

/// Metadata changed: `FAN_ATTRIB`.
pub fn notice_attrib(inode: &Arc<dyn Inode>) {
    notice(inode, FAN_ATTRIB);
}

/// The close-time report for one descriptor, the twin of inotify's
/// `close_last`: only the description that took the final reference reports,
/// because a `dup` is the same reader.
pub fn close_last(file: &Arc<crate::vfs::file::File>) {
    if Arc::strong_count(file) != 1 {
        return;
    }
    notice_close(&file.inode(), file.opened_for_write());
}

/// The batch form for process exit, registered with `sched::exit`: a
/// descriptor whose last reference dies with the exiting process still
/// reports its close, the same rule inotify's exit hook walks.
pub fn close_at_exit(files: &[Arc<crate::vfs::file::File>], _owner: u32) {
    crate::vfs::file::for_each_last(files, |file, _ofd| {
        notice_close(&file.inode(), file.opened_for_write());
    });
}

/// A descriptor was closed: `FAN_CLOSE_WRITE` or `FAN_CLOSE_NOWRITE`.
pub fn notice_close(inode: &Arc<dyn Inode>, opened_for_write: bool) {
    notice(
        inode,
        if opened_for_write {
            FAN_CLOSE_WRITE
        } else {
            FAN_CLOSE_NOWRITE
        },
    );
}

impl Inode for FanotifyGroup {
    fn as_fanotify(&self) -> Option<&FanotifyGroup> {
        Some(self)
    }

    /// `lseek(2)` on a notification queue is `ESPIPE`, like inotify's.
    fn seekable(&self) -> bool {
        false
    }

    fn metadata(&self) -> Metadata {
        Metadata {
            ino: 0x9400_0000,
            kind: NodeKind::Anon { mode: 0o600 },
            size: 0,
        }
    }

    fn fd_name(&self) -> Option<alloc::string::String> {
        Some(alloc::string::String::from("anon_inode:[fanotify]"))
    }

    /// The read: whole records, oldest first, packed while they fit. A
    /// buffer holding less than one metadata record is `EINVAL` (measured);
    /// nothing queued is a park, or `EAGAIN` when non-blocking.
    fn read_at(&self, _offset: u64, buf: &mut [u8]) -> VfsResult<usize> {
        if buf.len() < FAN_EVENT_METADATA_LEN {
            return Err(VfsError::Invalid);
        }
        loop {
            let mut filled = 0;
            while filled + FAN_EVENT_METADATA_LEN <= buf.len() {
                match self.pop_record(&mut buf[filled..])? {
                    None => break,
                    Some(n) => filled += n,
                }
            }
            if filled > 0 {
                return Ok(filled);
            }
            if self.nonblocking() {
                return Err(VfsError::Again);
            }
            self.wait_for_news()?;
        }
    }

    fn write_at(&self, _offset: u64, _buf: &[u8]) -> VfsResult<usize> {
        // The notification class has no response record; Linux answers a
        // write on such a group `EINVAL` (`fsnotify`'s permission families
        // are the writers).
        Err(VfsError::Invalid)
    }

    fn poll_ready(&self) -> Ready {
        Ready {
            read: self.pending(),
            write: false,
        }
    }

    fn poll_subscribe(&self, id: crate::sched::TaskId) {
        critical_section::with(|cs| {
            let mut st = self.state.borrow(cs).borrow_mut();
            if !st.waiters.contains(&id) {
                st.waiters.push(id);
            }
        });
    }

    fn poll_unsubscribe(&self, id: crate::sched::TaskId) {
        critical_section::with(|cs| {
            self.state
                .borrow(cs)
                .borrow_mut()
                .waiters
                .retain(|&w| w != id);
        });
    }
}

impl FanotifyGroup {
    /// Park until a record lands. Interruptible, the same reason inotify's
    /// wait is: a daemon that arms SIGALRM must come back to its loop.
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
        panic!("blocking on a fanotify fd needs the kernel scheduler; push() in tests");
    }
}

// ---------------------------------------------------------------------------
// Host tests: the rules that are pure logic - flags, masks, matching
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_notification_class_is_the_only_one_served() {
        assert_eq!(init_accepts(0), Ok(false));
        assert_eq!(init_accepts(FAN_NONBLOCK), Ok(true));
        assert_eq!(init_accepts(FAN_CLOEXEC), Ok(false));
        assert_eq!(init_accepts(FAN_CLASS_CONTENT), Err(VfsError::Unsupported));
        assert_eq!(
            init_accepts(FAN_CLASS_PRE_CONTENT | FAN_NONBLOCK),
            Err(VfsError::Unsupported)
        );
        assert_eq!(init_accepts(FAN_REPORT_FID), Err(VfsError::Unsupported));
        assert_eq!(init_accepts(FAN_ENABLE_AUDIT), Err(VfsError::Unsupported));
        // Linux accepts these two; this kernel refuses them rather than
        // promise a limit-free queue it cannot keep (module header).
        assert_eq!(
            init_accepts(FAN_UNLIMITED_QUEUE | FAN_UNLIMITED_MARKS),
            Err(VfsError::Unsupported)
        );
        assert_eq!(init_accepts(0x8000_0000), Err(VfsError::Invalid));
    }

    #[test]
    fn the_event_flags_word_is_the_measured_vocabulary() {
        assert_eq!(event_flags_accepts(0), Ok(()));
        assert_eq!(event_flags_accepts(0o2000000), Ok(()));
        assert_eq!(event_flags_accepts(0o100000), Ok(()));
        // O_DIRECT is outside the measured set.
        assert_eq!(
            event_flags_accepts(0o40000 | 0o200000),
            Err(VfsError::Invalid)
        );
    }

    #[test]
    fn a_mark_word_is_malformed_or_named() {
        assert_eq!(mark_flags_accepts(FAN_MARK_ADD), Ok(()));
        assert_eq!(mark_flags_accepts(FAN_MARK_REMOVE), Ok(()));
        assert_eq!(mark_flags_accepts(FAN_MARK_FLUSH), Ok(()));
        assert_eq!(mark_flags_accepts(FAN_MARK_ADD | FAN_MARK_ONLYDIR), Ok(()));
        assert_eq!(
            mark_flags_accepts(FAN_MARK_ADD | FAN_MARK_REMOVE),
            Err(VfsError::Invalid)
        );
        assert_eq!(mark_flags_accepts(0), Err(VfsError::Invalid));
        assert_eq!(
            mark_flags_accepts(FAN_MARK_MOUNT | FAN_MARK_ADD),
            Err(VfsError::Unsupported)
        );
        assert_eq!(
            mark_flags_accepts(FAN_MARK_FILESYSTEM | FAN_MARK_ADD),
            Err(VfsError::Unsupported)
        );
        assert_eq!(
            mark_flags_accepts(FAN_MARK_IGNORE),
            Err(VfsError::Unsupported)
        );
        assert_eq!(
            mark_flags_accepts(FAN_MARK_ADD | 0x8000_0000),
            Err(VfsError::Invalid)
        );
    }

    #[test]
    fn a_mask_is_the_served_vocabulary_or_a_named_refusal() {
        assert_eq!(mark_mask_accepts(FAN_OPEN), Ok(()));
        assert_eq!(
            mark_mask_accepts(FAN_OPEN | FAN_ACCESS | FAN_MODIFY),
            Ok(())
        );
        assert_eq!(mark_mask_accepts(0), Err(VfsError::Invalid));
        assert_eq!(mark_mask_accepts(FAN_OPEN_PERM), Err(VfsError::Invalid));
        assert_eq!(mark_mask_accepts(FAN_CREATE), Err(VfsError::Unsupported));
        assert_eq!(mark_mask_accepts(FAN_ONDIR), Err(VfsError::Unsupported));
        assert_eq!(mark_mask_accepts(0x8000_0000), Err(VfsError::Invalid));
    }

    #[test]
    fn a_mark_fires_on_the_event_bits_it_names() {
        assert!(mark_matches(FAN_OPEN | FAN_ACCESS, FAN_OPEN));
        assert!(!mark_matches(FAN_OPEN | FAN_ACCESS, FAN_MODIFY));
        assert!(mark_matches(SERVED_MASKS, FAN_CLOSE_WRITE));
    }

    #[test]
    fn the_record_is_the_measured_layout() {
        let mut buf = [0u8; FAN_EVENT_METADATA_LEN];
        write_record(&mut buf, FAN_OPEN, 7, 1234);
        assert_eq!(
            u32::from_ne_bytes(buf[0..4].try_into().unwrap()) as usize,
            FAN_EVENT_METADATA_LEN
        );
        assert_eq!(buf[4], FANOTIFY_METADATA_VERSION);
        assert_eq!(
            u16::from_ne_bytes(buf[6..8].try_into().unwrap()) as usize,
            FAN_EVENT_METADATA_LEN
        );
        assert_eq!(
            u64::from_ne_bytes(buf[8..16].try_into().unwrap()),
            FAN_OPEN as u64
        );
        assert_eq!(i32::from_ne_bytes(buf[16..20].try_into().unwrap()), 7);
        assert_eq!(i32::from_ne_bytes(buf[20..24].try_into().unwrap()), 1234);
    }
}
