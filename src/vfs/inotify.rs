//! `inotify(7)`: the filesystem's news, delivered through a descriptor.
//!
//! A daemon that must notice a file appearing in `/dev`, a config file
//! being rewritten, or a unit directory changing has one tool for the
//! question: an inotify descriptor carrying watches, each naming one inode
//! and the events on it worth reporting. `mdev`/`udev` build their entire
//! existence on it, `dbus` watches its socket directories, and anything
//! that "re-reads config on change" reaches for nothing else. Without it
//! those programs fall back to polling the tree with `stat` - which is
//! exactly what `inotify` replaced.
//!
//! The shape is one queue per descriptor and one global registry from
//! `(dev, ino)` to the watches interested in it. A VFS mutation calls
//! [`emit`] after it has succeeded; the registry fans the event out to
//! every matching watch, each instance appends one wire-encoded record and
//! wakes its readers. Watches hold their instance weakly and the watched
//! inode not at all - `(dev, ino)` is the whole identity, the same key the
//! lock table uses (`src/vfs/locks.rs`), so a deleted file pins nothing.
//!
//! The deliberate departures from Linux, each visible from userland:
//! `IN_ACCESS` is never emitted (the read path does not bump atime, so the
//! event would be half a lie, and no target daemon watches for it);
//! `IN_UNMOUNT` is not emitted (unmount does not track watches; a stale
//! one simply stops matching); a watched directory's `IN_MOVE_SELF` is
//! unreachable while `renameat2` refuses directory renames; and a rename
//! that replaces a name really does unlink the old inode here, so the
//! replaced name truthfully reports `IN_DELETE` before `IN_CREATE`.
//! `IN_EXCL_UNLINK` is honoured at the unlink boundary itself - the one
//! moment the question has an answer without hunting the tree for names.

use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::cell::RefCell;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};

use critical_section::Mutex;

use super::{Ino, Inode, Metadata, NodeKind, Ready, VfsError, VfsResult};

// ---------------------------------------------------------------------------
// The ABI: event bits, watch flags, and the record layout
// ---------------------------------------------------------------------------

/// The event bits, `linux/inotify.h`. Values, not just names: a caller may
/// hard-code them, and `IN_ALL_EVENTS` is the sum Linux spells.
pub const IN_ACCESS: u32 = 0x0000_0001;
pub const IN_MODIFY: u32 = 0x0000_0002;
pub const IN_ATTRIB: u32 = 0x0000_0004;
pub const IN_CLOSE_WRITE: u32 = 0x0000_0008;
pub const IN_CLOSE_NOWRITE: u32 = 0x0000_0010;
pub const IN_OPEN: u32 = 0x0000_0020;
pub const IN_MOVED_FROM: u32 = 0x0000_0040;
pub const IN_MOVED_TO: u32 = 0x0000_0080;
pub const IN_CREATE: u32 = 0x0000_0100;
pub const IN_DELETE: u32 = 0x0000_0200;
pub const IN_DELETE_SELF: u32 = 0x0000_0400;
pub const IN_MOVE_SELF: u32 = 0x0000_0800;
/// The synthetic reports: a queue that dropped events, a watch that died.
pub const IN_Q_OVERFLOW: u32 = 0x0000_4000;
pub const IN_IGNORED: u32 = 0x0000_8000;

/// `IN_CLOSE` and `IN_MOVE`, the aggregate spellings a caller masks with.
pub const IN_CLOSE: u32 = IN_CLOSE_WRITE | IN_CLOSE_NOWRITE;
pub const IN_MOVE: u32 = IN_MOVED_FROM | IN_MOVED_TO;
/// Every event bit Linux names as watchable - `0xfff`, the ACCESS..MOVE_SELF
/// span including the gaps, exactly as the header spells it.
pub const IN_ALL_EVENTS: u32 = 0x0000_0fff;

/// Watch flags accepted in `inotify_add_watch`'s mask. `IN_ISDIR` is the
/// odd one: an input meaning "only directory events", and an output bit
/// meaning "this event is about a directory".
pub const IN_ONLYDIR: u32 = 0x0100_0000;
pub const IN_DONT_FOLLOW: u32 = 0x0200_0000;
pub const IN_EXCL_UNLINK: u32 = 0x0400_0000;
pub const IN_MASK_CREATE: u32 = 0x1000_0000;
pub const IN_MASK_ADD: u32 = 0x2000_0000;
pub const IN_ISDIR: u32 = 0x4000_0000;
pub const IN_ONESHOT: u32 = 0x8000_0000;

/// Everything `inotify_add_watch`'s mask may hold.
pub const IN_ON_ALL_EVENTS: u32 = IN_ALL_EVENTS
    | IN_ONESHOT
    | IN_ONLYDIR
    | IN_DONT_FOLLOW
    | IN_EXCL_UNLINK
    | IN_MASK_ADD
    | IN_MASK_CREATE;

/// `inotify_init1`'s flags - the `O_` flags under other names, as the
/// headers spell them.
pub const IN_CLOEXEC: usize = 0o200_0000;
pub const IN_NONBLOCK: usize = 0x800;

/// One `struct inotify_event`: a 16-byte header (`wd`, `mask`, `cookie`,
/// `len`, little-endian) and then `len` bytes of name. `len` counts the
/// name, its NUL and the padding that carries the whole record to a
/// multiple of 8 - the alignment that lets a caller walk the buffer with
/// word loads.
pub const EVENT_HEADER_SIZE: usize = 16;
pub const EVENT_ALIGN: usize = 8;

/// Linux's `fs.inotify.max_queued_events` default, per descriptor.
pub const MAX_QUEUED_EVENTS: usize = 16384;
/// Linux's `fs.inotify.max_user_instances` default, whole-system here: one
/// root, so the per-user and the per-system figure are the same number.
pub const MAX_INSTANCES: usize = 128;
/// Linux's `fs.inotify.max_user_watches` default, whole-system for the
/// same reason as above.
pub const MAX_WATCHES: usize = 8192;

/// The `wd` of a record no watch produced - the queue-overflow report.
pub const NO_WD: i32 = -1;

/// The name field a record will carry: the name, its NUL, and padding to
/// land the total record on the alignment. An empty name pads to nothing -
/// a nameless record is header-only, `len = 0`, as Linux sends it.
pub fn pad_name(name: &[u8]) -> Vec<u8> {
    if name.is_empty() {
        return Vec::new();
    }
    let with_nul = name.len() + 1;
    let padded = with_nul.next_multiple_of(EVENT_ALIGN);
    let mut out = Vec::with_capacity(padded);
    out.extend_from_slice(name);
    out.resize(padded, 0);
    out
}

/// The whole length of a record carrying `name_len` padded name bytes.
pub fn record_len(name: &[u8]) -> usize {
    EVENT_HEADER_SIZE + pad_name(name).len()
}

/// One queued event. `name` is the padded field as it goes on the wire
/// (bytes + NUL + padding), empty for a nameless record.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InotifyEvent {
    pub wd: i32,
    pub mask: u32,
    pub cookie: u32,
    pub name: Vec<u8>,
}

impl InotifyEvent {
    /// The record's size on the wire.
    pub fn len(&self) -> usize {
        EVENT_HEADER_SIZE + self.name.len()
    }

    /// Whether the record is empty of a name. Kept because `len` alone
    /// answers the same question; clippy wants the pair stated.
    pub fn is_empty(&self) -> bool {
        self.name.is_empty()
    }

    /// Serialize to the wire layout the caller's buffer receives.
    pub fn write_to(&self, buf: &mut [u8]) {
        let total = self.len();
        buf[..4].copy_from_slice(&self.wd.to_ne_bytes());
        buf[4..8].copy_from_slice(&self.mask.to_ne_bytes());
        buf[8..12].copy_from_slice(&self.cookie.to_ne_bytes());
        buf[12..16].copy_from_slice(&(self.name.len() as u32).to_ne_bytes());
        buf[EVENT_HEADER_SIZE..total].copy_from_slice(&self.name);
    }
}

/// The event-matching rule, one Linux line and its whole subtlety: a watch
/// fires when its mask names one of the event's bits - or, for a directory
/// event, when the watch carries `IN_ISDIR` ("only directory events").
/// `IN_ISDIR` in a stored mask is a filter, never an event bit, so it is
/// stripped from the first test.
pub fn event_matches(watch_mask: u32, ev_mask: u32, ev_is_dir: bool) -> bool {
    (watch_mask & !IN_ISDIR & ev_mask) != 0 || (ev_is_dir && (watch_mask & IN_ISDIR) != 0)
}

// ---------------------------------------------------------------------------
// One inotify file descriptor: its queue, its watches, its readers
// ---------------------------------------------------------------------------

struct InstanceState {
    /// The queue, oldest first. A `Vec` drained from the front rather than
    /// a ring: the cap is small, the shift is memmove-cheap, and the whole
    /// queue never holds more than `MAX_QUEUED_EVENTS` records.
    queue: Vec<InotifyEvent>,
    /// Set when an event was dropped for a full queue; the single
    /// `IN_Q_OVERFLOW` record goes with it, and both clear when the record
    /// is pushed - a flood gets one report, not a flood of reports.
    dropped: bool,
    /// Tasks parked in `read_at`, plus `poll` subscribers. Woken outside
    /// the borrow, the way every producer here does it.
    waiters: Vec<crate::sched::TaskId>,
    /// This instance's live watches as `(wd, dev, ino)`, so
    /// `inotify_rm_watch` resolves a descriptor number without scanning
    /// the global registry, and a wd maps back to its key for the IGNORED
    /// bookkeeping.
    watches: Vec<(i32, u64, u64)>,
}

/// One inotify descriptor: a watch list's worth of news, queued.
pub struct InotifyInstance {
    ino: Ino,
    /// `IN_NONBLOCK` as recorded at creation; the descriptor's status word
    /// is set to match, and `fcntl` keeps the pair in step.
    nonblock: AtomicBool,
    /// The next watch descriptor to hand out. Linux's first is 1 - tools
    /// print wds and people diff them - so the counter starts there.
    next_wd: AtomicU32,
    state: Mutex<RefCell<InstanceState>>,
}

/// Instances that exist, for the system-wide cap. Dropped in [`Drop`], so
/// closing the last descriptor frees the slot.
static LIVE_INSTANCES: AtomicUsize = AtomicUsize::new(0);

impl InotifyInstance {
    /// A fresh instance, unbudgeted by the caller's check. `Err` when the
    /// system-wide instance cap is full - the syscall answers `EMFILE`,
    /// Linux's own refusal.
    pub fn try_new(ino: Ino, nonblock: bool) -> Option<Arc<InotifyInstance>> {
        let held = LIVE_INSTANCES.fetch_add(1, Ordering::Relaxed);
        if held >= MAX_INSTANCES {
            LIVE_INSTANCES.fetch_sub(1, Ordering::Relaxed);
            return None;
        }
        Some(Arc::new(InotifyInstance {
            ino,
            nonblock: AtomicBool::new(nonblock),
            next_wd: AtomicU32::new(1),
            state: Mutex::new(RefCell::new(InstanceState {
                queue: Vec::new(),
                dropped: false,
                waiters: Vec::new(),
                watches: Vec::new(),
            })),
        }))
    }

    pub fn nonblocking(&self) -> bool {
        self.nonblock.load(Ordering::Relaxed)
    }

    pub fn set_nonblocking(&self, on: bool) {
        self.nonblock.store(on, Ordering::Relaxed);
    }

    /// Hand out the next watch descriptor.
    fn alloc_wd(&self) -> i32 {
        self.next_wd.fetch_add(1, Ordering::Relaxed) as i32
    }

    /// The live watch count of this instance - the input to the
    /// per-instance half of the caps.
    pub fn watch_count(&self) -> usize {
        critical_section::with(|cs| self.state.borrow(cs).borrow().watches.len())
    }

    /// Append one record to the queue, minding the cap. Returns the tasks
    /// to wake (empty on the host, where the list is never filled anyway);
    /// a drop for a full queue raises the sticky overflow flag and, once,
    /// the `IN_Q_OVERFLOW` record in its place.
    fn push(&self, ev: InotifyEvent) -> Vec<crate::sched::TaskId> {
        critical_section::with(|cs| {
            let mut st = self.state.borrow(cs).borrow_mut();
            if st.queue.len() >= MAX_QUEUED_EVENTS {
                st.dropped = true;
                // One overflow report per episode: `dropped` is raised,
                // and the record goes in only as the flag goes up. The
                // oldest unread record pays for it - the queue is capped,
                // and news that something was dropped outranks one piece
                // of news that was.
                if !st.queue.iter().any(|e| e.mask == IN_Q_OVERFLOW) {
                    st.queue.remove(0);
                    st.queue.push(InotifyEvent {
                        wd: NO_WD,
                        mask: IN_Q_OVERFLOW,
                        cookie: 0,
                        name: Vec::new(),
                    });
                }
            } else {
                if ev.mask != IN_Q_OVERFLOW {
                    st.dropped = false;
                }
                st.queue.push(ev);
            }
            st.waiters.clone()
        })
    }

    /// Take as many whole records as the buffer holds. A record that does
    /// not fit stays queued for the next read - a partially delivered
    /// record is undeliverable to a caller's parser.
    fn drain_records(&self, buf: &mut [u8]) -> usize {
        critical_section::with(|cs| {
            let mut st = self.state.borrow(cs).borrow_mut();
            let mut written = 0;
            while let Some(front) = st.queue.first() {
                let len = front.len();
                if written + len > buf.len() {
                    break;
                }
                front.write_to(&mut buf[written..written + len]);
                written += len;
                st.queue.remove(0);
            }
            written
        })
    }

    /// Whether a read would answer something right now.
    fn pending(&self) -> bool {
        critical_section::with(|cs| !self.state.borrow(cs).borrow().queue.is_empty())
    }

    /// `inotify_add_watch`'s per-instance bookkeeping: the wd to return
    /// for a key this instance already watches, if any.
    pub fn existing_wd(&self, key: (u64, u64)) -> Option<i32> {
        critical_section::with(|cs| {
            self.state
                .borrow(cs)
                .borrow()
                .watches
                .iter()
                .find(|(_, dev, ino)| (*dev, *ino) == key)
                .map(|(wd, _, _)| *wd)
        })
    }

    /// Record a new watch's wd for this instance.
    pub fn record_wd(&self, wd: i32, key: (u64, u64)) {
        critical_section::with(|cs| {
            self.state
                .borrow(cs)
                .borrow_mut()
                .watches
                .push((wd, key.0, key.1));
        });
    }

    /// Forget a watch by wd, answering its key. Used by `inotify_rm_watch`
    /// and by the registry's death sweep.
    pub fn forget_wd(&self, wd: i32) -> Option<(u64, u64)> {
        critical_section::with(|cs| {
            let mut st = self.state.borrow(cs).borrow_mut();
            let at = st.watches.iter().position(|(w, _, _)| *w == wd)?;
            let (_, dev, ino) = st.watches.remove(at);
            Some((dev, ino))
        })
    }

    /// Push a record and wake whoever was waiting for news. The wake loop
    /// is outside every borrow, the pipe/eventfd way: waking may retake
    /// locks a producer could be holding.
    pub fn deliver(&self, ev: InotifyEvent) {
        #[cfg(target_os = "none")]
        for id in self.push(ev) {
            crate::sched::wake(id);
        }
        #[cfg(not(target_os = "none"))]
        {
            self.push(ev);
        }
    }
}

impl Drop for InotifyInstance {
    fn drop(&mut self) {
        LIVE_INSTANCES.fetch_sub(1, Ordering::Relaxed);
    }
}

impl Inode for InotifyInstance {
    /// A queue has no position: `lseek(2)` on an inotify fd is `ESPIPE`.
    fn seekable(&self) -> bool {
        false
    }

    fn metadata(&self) -> Metadata {
        Metadata {
            ino: 0x9000_0000 + self.ino,
            kind: NodeKind::CharDev,
            size: 0,
        }
    }

    /// The read: whole records, oldest first. Fewer bytes than one header
    /// is `EINVAL`, the man page's refusal; nothing queued is a park, or
    /// `EAGAIN` when nonblocking.
    fn read_at(&self, _offset: u64, buf: &mut [u8]) -> VfsResult<usize> {
        if buf.len() < EVENT_HEADER_SIZE {
            return Err(VfsError::Invalid);
        }
        loop {
            let n = self.drain_records(buf);
            if n > 0 {
                return Ok(n);
            }
            if self.pending() {
                // Records are queued and none fits this buffer: a short
                // read of nothing, which is Linux's answer - a caller
                // big enough to hold a record will get it from the same
                // queue.
                return Ok(0);
            }
            if self.nonblocking() {
                return Err(VfsError::Again);
            }
            // The wait a catchable signal ends: a watcher's `SIGALRM`, or the
            // `SIGCHLD` of the very child whose config change it was watching
            // for, has to bring it back to its own loop. Nothing is consumed by
            // the ending, so the event is still there for the retry.
            self.wait_for_news()?;
        }
    }

    /// Nothing is written to a queue: the Linux refusal is `EBADF`, the fd
    /// being read-only.
    fn write_at(&self, _offset: u64, _buf: &[u8]) -> VfsResult<usize> {
        Err(VfsError::BadFd)
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

    fn as_inotify(&self) -> Option<&InotifyInstance> {
        Some(self)
    }
}

impl InotifyInstance {
    /// Park until a record lands in the queue. The producer's wake is the
    /// wakeup; `poll`'s subscribers ride the same list.
    ///
    /// Interruptible, like every other wait a syscall parks in - `inotify` is
    /// read by the daemons that also live on signals (`mdev`, `dbus`), and a
    /// signal slept through here is a daemon that stopped answering its own
    /// timers.
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
        panic!("blocking on an inotify fd needs the kernel scheduler; push() in tests");
    }
}

/// A nameless self-event constructor, shared by every hook that reports
/// news about an inode itself rather than about a name inside a directory.
pub fn self_event(wd: i32, mask: u32) -> InotifyEvent {
    InotifyEvent {
        wd,
        mask,
        cookie: 0,
        name: Vec::new(),
    }
}

/// A directory-child event: the name rides along, padded.
pub fn child_event(wd: i32, mask: u32, cookie: u32, name: &str) -> InotifyEvent {
    InotifyEvent {
        wd,
        mask,
        cookie,
        name: pad_name(name.as_bytes()),
    }
}

/// The instance's own string-agnostic helper for rm_watch's IGNORED.
pub fn ignored(wd: i32) -> InotifyEvent {
    self_event(wd, IN_IGNORED)
}

// ---------------------------------------------------------------------------
// The registry: from a watched inode to every instance that wants to know
// ---------------------------------------------------------------------------

/// One live watch: the `(dev, ino)` key of the watched inode, the inotify
/// descriptor that asked - weakly, through the inode node it is, so a
/// closed descriptor retires its watches with it - the descriptor number
/// to report, and the stored mask.
///
/// The stored mask keeps the event bits plus the filters that mean
/// something long-term (`IN_ISDIR`, `IN_ONESHOT`, `IN_EXCL_UNLINK`); the
/// input-only flags (`IN_ONLYDIR`, `IN_DONT_FOLLOW`, `IN_MASK_ADD`,
/// `IN_MASK_CREATE`) are answered at the call and never stored, exactly as
/// Linux's `inotify_new_watch` clears them.
#[derive(Clone)]
pub struct Watch {
    pub key: (u64, u64),
    /// The instance as its own anonymous inode, weakly. The syscall layer
    /// reaches an instance through a descriptor - `&dyn Inode`, from which
    /// a `Weak<InotifyInstance>` cannot be made but a `Weak<dyn Inode>`
    /// can - and delivery upgrades back through [`Inode::as_inotify`].
    instance: Weak<dyn Inode>,
    /// The instance's anonymous inode number: the identity test that does
    /// not need the weak reference to be alive, so an update from one
    /// descriptor never lands on another's watch.
    owner: Ino,
    pub wd: i32,
    pub mask: u32,
}

struct RegistryState {
    watches: Vec<Watch>,
    /// The next move cookie: one number per rename, shared by the
    /// `IN_MOVED_FROM` and `IN_MOVED_TO` pair so a reader can join them.
    cookie: u32,
}

impl RegistryState {
    const fn new() -> Self {
        RegistryState {
            watches: Vec::new(),
            cookie: 0,
        }
    }
}

static REGISTRY: Mutex<RefCell<RegistryState>> = Mutex::new(RefCell::new(RegistryState::new()));

/// The watch key of an inode: its mount's id and its own number - the same
/// pair the lock table keys on, because it answers the same question
/// ("which file is this, really") across mounts that reuse inode numbers.
pub fn key_of(inode: &Arc<dyn Inode>) -> (u64, u64) {
    (
        crate::vfs::mount::fs_id_of_inode(inode),
        inode.metadata().ino,
    )
}

/// The mask bits `inotify_add_watch` stores for later matching.
pub fn stored_mask(in_mask: u32) -> u32 {
    in_mask & (IN_ALL_EVENTS | IN_ISDIR | IN_ONESHOT | IN_EXCL_UNLINK)
}

/// name the errno and the caller can read back the wd either way.
#[derive(Debug, PartialEq, Eq)]
pub enum AddOutcome {
    New(i32),
    /// Re-added by the same instance: the mask was replaced, the wd kept.
    Replaced(i32),
    /// `IN_MASK_CREATE` and a watch was already there: EEXIST.
    Exists(i32),
    /// The system-wide watch cap: ENOSPC.
    NoSpace,
    /// The descriptor is not an inotify instance: EINVAL.
    NotAnInstance,
}

/// Add (or update) one watch. `inode` must be an inotify instance - the
/// caller checked - and `owner` is its anonymous inode number. Same
/// instance + same key is the dedup pair Linux defines: it updates in
/// place and keeps the descriptor number.
pub fn add_watch(inode: &Arc<dyn Inode>, owner: Ino, key: (u64, u64), in_mask: u32) -> AddOutcome {
    let Some(instance) = inode.as_inotify() else {
        return AddOutcome::NotAnInstance;
    };
    let mask = stored_mask(in_mask);
    let wants_create = in_mask & IN_MASK_CREATE != 0;
    let wants_merge = in_mask & IN_MASK_ADD != 0;
    critical_section::with(|cs| {
        let mut reg = REGISTRY.borrow(cs).borrow_mut();
        if let Some(existing) = reg
            .watches
            .iter()
            .position(|w| w.key == key && w.owner == owner)
        {
            let wd = reg.watches[existing].wd;
            if wants_create {
                return AddOutcome::Exists(wd);
            }
            if wants_merge {
                reg.watches[existing].mask |= mask;
            } else {
                reg.watches[existing].mask = mask;
            }
            return AddOutcome::Replaced(wd);
        }
        if reg.watches.len() >= MAX_WATCHES {
            // The cap refuses rather than prunes: a machine at eight
            // thousand watches is doing something the caller knows about,
            // and silently dropping their news would be the worse answer.
            return AddOutcome::NoSpace;
        }
        let wd = instance.alloc_wd();
        instance.record_wd(wd, key);
        reg.watches.push(Watch {
            key,
            instance: Arc::downgrade(inode),
            owner,
            wd,
            mask,
        });
        AddOutcome::New(wd)
    })
}

/// `inotify_rm_watch`: forget one watch and report its death to the queue
/// that owned it, Linux's `IN_IGNORED`. Answers false for a wd this
/// instance never held.
pub fn remove_watch(instance: &InotifyInstance, owner: Ino, wd: i32) -> bool {
    let Some(key) = instance.forget_wd(wd) else {
        return false;
    };
    critical_section::with(|cs| {
        let mut reg = REGISTRY.borrow(cs).borrow_mut();
        reg.watches
            .retain(|w| !(w.key == key && w.owner == owner && w.wd == wd));
    });
    instance.deliver(ignored(wd));
    true
}

/// The fan-out core: every watch whose key and mask admit this event gets
/// one record, `IN_ONESHOT` watches are spent and retired (with their
/// `IN_IGNORED`), and the woken tasks are collected for the caller to wake
/// outside every borrow.
fn emit_rec(key: (u64, u64), mask: u32, name: &str, cookie: u32, is_dir: bool) {
    let out_mask = mask | if is_dir { IN_ISDIR } else { 0 };
    let mut wake: Vec<crate::sched::TaskId> = Vec::new();
    let mut spent: Vec<(Arc<dyn Inode>, i32)> = Vec::new();
    critical_section::with(|cs| {
        let mut reg = REGISTRY.borrow(cs).borrow_mut();
        // Spent oneshots are named by (owner, wd) - the identity the
        // retain below can test without holding a second borrow of the
        // table it is editing.
        let mut spent_ids: Vec<(Ino, i32)> = Vec::new();
        for w in reg.watches.iter() {
            if w.key != key {
                continue;
            }
            let Some(node) = w.instance.upgrade() else {
                continue; // dead: the next add or sweep retires it
            };
            let Some(instance) = node.as_inotify() else {
                continue;
            };
            if !event_matches(w.mask, mask, is_dir) {
                continue;
            }
            // `IN_EXCL_UNLINK` honoured at the unlink boundary: a watch
            // that asked not to hear about removals by name does not get
            // the removal of the name itself reported.
            if w.mask & IN_EXCL_UNLINK != 0 && (mask & (IN_DELETE | IN_MOVED_FROM) != 0) {
                continue;
            }
            let ev = if name.is_empty() {
                self_event(w.wd, out_mask)
            } else {
                InotifyEvent {
                    wd: w.wd,
                    mask: out_mask,
                    cookie,
                    name: pad_name(name.as_bytes()),
                }
            };
            wake.extend(instance.push(ev));
            if w.mask & IN_ONESHOT != 0 {
                spent_ids.push((w.owner, w.wd));
                spent.push((node.clone(), w.wd));
            }
        }
        if !spent_ids.is_empty() {
            reg.watches
                .retain(|w| !spent_ids.contains(&(w.owner, w.wd)));
        }
    });
    for (node, wd) in &spent {
        if let Some(instance) = node.as_inotify() {
            instance.forget_wd(*wd);
        }
    }
    for (node, wd) in spent {
        if let Some(instance) = node.as_inotify() {
            wake.extend(instance.push(ignored(wd)));
        }
    }
    #[cfg(target_os = "none")]
    for id in wake {
        crate::sched::wake(id);
    }
    #[cfg(not(target_os = "none"))]
    let _ = wake;
}

/// Report news about an inode, or about a name inside a watched directory.
/// The mutation hook calls this after its own operation has succeeded;
/// `name` is empty for events about the inode itself.
pub fn emit(inode: &Arc<dyn Inode>, mask: u32, name: &str) {
    let is_dir = inode.metadata().kind == NodeKind::Dir;
    emit_rec(key_of(inode), mask, name, 0, is_dir);
}

/// Report a data change with the inode's own key already in hand - the
/// `File::write` chokepoint has the inode, and taking its key is the whole
/// cost.
pub fn emit_write(inode: &Arc<dyn Inode>) {
    emit(inode, IN_MODIFY, "");
}

/// Report one rename: the pair of directory events share the cookie the
/// reader joins them by, and the moved inode itself reports
/// `IN_MOVE_SELF`. `moved` is the inode that travelled.
pub fn emit_move(
    old_dir: &Arc<dyn Inode>,
    old_name: &str,
    new_dir: &Arc<dyn Inode>,
    new_name: &str,
    moved: &Arc<dyn Inode>,
) {
    let is_dir = moved.metadata().kind == NodeKind::Dir;
    let cookie = critical_section::with(|cs| {
        let mut reg = REGISTRY.borrow(cs).borrow_mut();
        reg.cookie = reg.cookie.wrapping_add(1).max(1);
        reg.cookie
    });
    emit_rec(key_of(old_dir), IN_MOVED_FROM, old_name, cookie, is_dir);
    emit_rec(key_of(new_dir), IN_MOVED_TO, new_name, cookie, is_dir);
    emit_rec(key_of(moved), IN_MOVE_SELF, "", cookie, is_dir);
}

/// Report the death of an inode: `IN_DELETE_SELF` to watches on it, then
/// every such watch is torn down and told `IN_IGNORED` - the pair Linux
/// sends a tool that watched a file being removed (its `inotifywait -m`
/// "WATCH DELETE" line is exactly these two records).
pub fn notify_deleted(child: &Arc<dyn Inode>) {
    let key = key_of(child);
    let is_dir = child.metadata().kind == NodeKind::Dir;
    emit_rec(key, IN_DELETE_SELF, "", 0, is_dir);
    let victims: Vec<(Arc<dyn Inode>, i32)> = critical_section::with(|cs| {
        let mut reg = REGISTRY.borrow(cs).borrow_mut();
        let found = reg
            .watches
            .iter()
            .filter(|w| w.key == key)
            .filter_map(|w| w.instance.upgrade().map(|n| (n, w.wd)))
            .collect();
        reg.watches.retain(|w| w.key != key);
        found
    });
    for (node, wd) in victims {
        if let Some(instance) = node.as_inotify() {
            instance.forget_wd(wd);
            instance.deliver(ignored(wd));
        }
    }
}

/// The close-time report for one descriptor: the last reference to an
/// open file description says `IN_CLOSE_WRITE` if the description was
/// opened for writing and `IN_CLOSE_NOWRITE` otherwise, Linux's pair.
/// The "last reference" test is the same `Arc::strong_count` question the
/// socket table asks at close; the batch form below covers process exit,
/// where several descriptors of the dying process may name one
/// description and none of them is individually last.
pub fn close_last(file: &Arc<crate::vfs::file::File>) {
    if Arc::strong_count(file) != 1 {
        return;
    }
    let mask = if file.opened_for_write() {
        IN_CLOSE_WRITE
    } else {
        IN_CLOSE_NOWRITE
    };
    emit(&file.inode(), mask, "");
}

/// Batch form of [`close_last`] for process exit, registered as a hook with
/// `sched::exit` rather than called by name from the teardown (`PLAN.md`
/// §11.1.3).
///
/// The `owner` is the exiting thread group, and a watch is not owned by one, so
/// it is ignored - the parameter is there because every hook in the list is
/// handed the same two things.
pub fn close_at_exit(files: &[Arc<crate::vfs::file::File>], _owner: u32) {
    let mut seen: Vec<*const crate::vfs::file::File> = Vec::new();
    for file in files {
        let ptr = Arc::as_ptr(file);
        if seen.contains(&ptr) {
            continue;
        }
        let total = Arc::strong_count(file);
        let in_batch = files
            .iter()
            .filter(|other| core::ptr::eq(Arc::as_ptr(other), ptr))
            .count();
        if total == in_batch {
            seen.push(ptr);
            let mask = if file.opened_for_write() {
                IN_CLOSE_WRITE
            } else {
                IN_CLOSE_NOWRITE
            };
            emit(&file.inode(), mask, "");
        }
    }
}

/// Sweep watches whose instance has died. Called from the add path's cap
/// refusal and nowhere else - a closed descriptor's tombstones cost memory
/// alone until the next watch is wanted.
pub fn sweep_dead() {
    critical_section::with(|cs| {
        REGISTRY.borrow(cs).borrow_mut().watches.retain(|w| {
            // A dead instance cannot upgrade; its tombstone goes.
            w.instance.upgrade().is_some()
        });
    });
}

#[cfg(all(test, not(target_os = "none")))]
mod tests {
    use super::*;
    use crate::vfs::FileSystem;

    /// The registry and the instance budget are one machine-wide pair of
    /// statics - every test that touches either takes this lock, so the
    /// parallel harness cannot make one test's watch be another's, or
    /// empty another's share of the cap. A crate-internal spin guard: the
    /// lib is `no_std` even under the host test build.
    static TESTS: AtomicBool = AtomicBool::new(false);
    struct Guard;
    fn lock_tests() -> Guard {
        while TESTS.swap(true, Ordering::Acquire) {
            core::hint::spin_loop();
        }
        Guard
    }
    impl Drop for Guard {
        fn drop(&mut self) {
            TESTS.store(false, Ordering::Release);
        }
    }

    /// One instance node (the fd-table's shape) plus its concrete handle,
    /// serialized against the other inventory tests.
    fn instance_node() -> (Arc<dyn Inode>, Arc<InotifyInstance>) {
        let _g = lock_tests();
        let instance = InotifyInstance::try_new(next_test_ino(), true).expect("under the cap");
        let node: Arc<dyn Inode> = instance.clone();
        (node, instance)
    }

    /// One instance for a test that needs its own, serialized against the
    /// inventory tests: the cap is system-wide, so a test that fills it
    /// (`the_instance_cap_answers_and_frees`) must not run beside one that
    /// needs a slot - which is exactly what it did, and what made this test
    /// fail on the days the parallel harness scheduled them together. The
    /// guard is held only for the creation; the slot, once taken, is the
    /// test's own. It is *not* reentrant, so a caller must not already hold
    /// it.
    fn instance(ino: Ino, nonblock: bool) -> Arc<InotifyInstance> {
        let _g = lock_tests();
        InotifyInstance::try_new(ino, nonblock).expect("under the cap")
    }

    /// The ABI, pinned against the headers: the event bits in order, the
    /// aggregates as Linux sums them, the init flags as the `O_` values.
    #[test]
    fn the_abi_constants_match_the_headers() {
        assert_eq!(IN_ACCESS, 0x1);
        assert_eq!(IN_MODIFY, 0x2);
        assert_eq!(IN_ATTRIB, 0x4);
        assert_eq!(IN_CLOSE_WRITE, 0x8);
        assert_eq!(IN_CLOSE_NOWRITE, 0x10);
        assert_eq!(IN_OPEN, 0x20);
        assert_eq!(IN_MOVED_FROM, 0x40);
        assert_eq!(IN_MOVED_TO, 0x80);
        assert_eq!(IN_CREATE, 0x100);
        assert_eq!(IN_DELETE, 0x200);
        assert_eq!(IN_DELETE_SELF, 0x400);
        assert_eq!(IN_MOVE_SELF, 0x800);
        assert_eq!(IN_Q_OVERFLOW, 0x4000);
        assert_eq!(IN_IGNORED, 0x8000);
        assert_eq!(IN_ALL_EVENTS, 0xfff);
        assert_eq!(IN_CLOSE, 0x18);
        assert_eq!(IN_MOVE, 0xc0);
        assert_eq!(IN_ONLYDIR, 0x0100_0000);
        assert_eq!(IN_DONT_FOLLOW, 0x0200_0000);
        assert_eq!(IN_EXCL_UNLINK, 0x0400_0000);
        assert_eq!(IN_MASK_CREATE, 0x1000_0000);
        assert_eq!(IN_MASK_ADD, 0x2000_0000);
        assert_eq!(IN_ISDIR, 0x4000_0000);
        assert_eq!(IN_ONESHOT, 0x8000_0000);
        assert_eq!(IN_NONBLOCK, 0x800);
        assert_eq!(IN_CLOEXEC, 0o200_0000);
        assert_eq!(EVENT_HEADER_SIZE, 16);
    }

    /// The padding rule, record by record: empty name pads to nothing,
    /// every other name gets its NUL and then zeros until the whole
    /// record lands on a multiple of 8.
    #[test]
    fn names_pad_to_a_multiple_of_eight() {
        assert_eq!(pad_name(b"").len(), 0);
        assert_eq!(record_len(b""), 16);
        // "a" + NUL = 2, next multiple of 8 from the header's 16 is 24:
        // the name field carries 8 bytes.
        assert_eq!(record_len(b"a"), 24);
        // 7 chars + NUL = 8 -> record 24, no extra padding needed.
        assert_eq!(record_len(b"abcdefg"), 24);
        // 8 chars + NUL = 9 -> the field goes to 16.
        assert_eq!(record_len(b"abcdefgh"), 32);
        let padded = pad_name(b"abc");
        assert_eq!(padded.len(), 8);
        assert_eq!(&padded[..3], b"abc");
        assert!(padded[3..].iter().all(|&b| b == 0));
    }

    /// One record's wire layout: the four header words little-endian and
    /// the padded name after, and `len` counting name+NUL+pad.
    #[test]
    fn a_record_is_header_then_padded_name() {
        let ev = child_event(7, IN_CREATE, 0, "ab");
        let mut buf = [0u8; 32];
        ev.write_to(&mut buf);
        assert_eq!(&buf[..4], &7i32.to_ne_bytes());
        assert_eq!(&buf[4..8], &IN_CREATE.to_ne_bytes());
        assert_eq!(&buf[8..12], &0u32.to_ne_bytes());
        assert_eq!(&buf[12..16], &8u32.to_ne_bytes(), "name field is 8 bytes");
        assert_eq!(&buf[16..18], b"ab");
        assert!(buf[18..24].iter().all(|&b| b == 0));
        assert_eq!(ev.len(), 24);

        // A nameless self-event is header-only.
        let ev = self_event(9, IN_DELETE_SELF);
        assert_eq!(ev.len(), 16);
    }

    /// The matching rule's four quadrants: mask bits match what they name
    /// and nothing else; a directory event also matches an `IN_ISDIR`
    /// filter watch; `IN_ISDIR` in a stored mask never matches a
    /// non-directory event of the same bit.
    #[test]
    fn a_watch_matches_by_event_bits_and_the_isdir_filter() {
        assert!(event_matches(IN_CREATE, IN_CREATE, false));
        assert!(!event_matches(IN_CREATE, IN_DELETE, false));
        // IN_ISDIR as filter: directory events only.
        assert!(event_matches(IN_CREATE | IN_ISDIR, IN_CREATE, true));
        assert!(event_matches(IN_ISDIR, IN_DELETE, true));
        assert!(!event_matches(IN_ISDIR, IN_DELETE, false));
        // The stored ISDIR bit never satisfies the event-bits test itself.
        assert!(!event_matches(IN_ISDIR, IN_ISDIR, false));
    }

    /// The host-side instance: reads of less than a header are EINVAL, an
    /// empty nonblocking read is EAGAIN, a pushed record comes back whole,
    /// and a record too long for the buffer stays queued rather than
    /// arriving cut.
    #[test]
    fn the_read_takes_whole_records_and_leaves_the_unfit_one() {
        let fd = instance(1, true);
        assert_eq!(
            fd.read_at(0, &mut [0u8; 8]).err(),
            Some(VfsError::Invalid),
            "shorter than a header is EINVAL"
        );
        assert_eq!(fd.read_at(0, &mut [0u8; 16]).err(), Some(VfsError::Again));

        fd.push(child_event(1, IN_CREATE, 0, "x"));
        fd.push(child_event(2, IN_MODIFY, 0, "a much longer name"));
        // A buffer that holds the first record only: the second is not
        // started, so it survives for the next read.
        let mut buf = [0u8; 32];
        assert_eq!(fd.read_at(0, &mut buf).unwrap(), 24);
        assert_eq!(&buf[..4], &1i32.to_ne_bytes());
        let mut buf = [0u8; 16];
        assert_eq!(
            fd.read_at(0, &mut buf).unwrap(),
            0,
            "32-byte record does not fit 16 - a short read of nothing"
        );
        // The record is still there for a bigger buffer.
        assert!(fd.pending());
        let mut buf = [0u8; 48];
        assert_eq!(fd.read_at(0, &mut buf).unwrap(), 40);
        assert!(!fd.pending());

        // Writes are refused EBADF, the read-only descriptor's answer.
        assert_eq!(fd.write_at(0, b"x").err(), Some(VfsError::BadFd));
    }

    /// Poll vocabulary: readable exactly when the queue is not empty,
    /// never writable.
    #[test]
    fn poll_reports_readability_and_never_writability() {
        let fd = instance(2, false);
        assert!(!fd.poll_ready().read);
        assert!(!fd.poll_ready().write);
        fd.push(self_event(1, IN_ATTRIB));
        assert!(fd.poll_ready().read);
        assert!(!fd.poll_ready().write);
    }

    /// The wd bookkeeping: the first watch descriptor is 1, the side map
    /// resolves both ways, and a forgotten wd answers none.
    #[test]
    fn wds_start_at_one_and_map_back_to_their_key() {
        let fd = instance(3, false);
        let wd = fd.alloc_wd();
        assert_eq!(wd, 1);
        fd.record_wd(wd, (0, 55));
        assert_eq!(fd.existing_wd((0, 55)), Some(1));
        assert_eq!(fd.existing_wd((0, 56)), None);
        assert_eq!(fd.forget_wd(wd), Some((0, 55)));
        assert_eq!(fd.forget_wd(wd), None);
        assert_eq!(fd.watch_count(), 0);
    }

    /// Overflow: a full queue drops new events, raises exactly one
    /// `IN_Q_OVERFLOW` report with wd -1, and does not raise a second one
    /// while the episode lasts.
    #[test]
    fn a_full_queue_reports_overflow_once() {
        let fd = instance(4, false);
        for i in 0..MAX_QUEUED_EVENTS as u32 {
            fd.push(self_event(i as i32, IN_MODIFY));
        }
        assert_eq!(
            critical_section::with(|cs| fd.state.borrow(cs).borrow().queue.len()),
            MAX_QUEUED_EVENTS
        );
        fd.push(self_event(1, IN_MODIFY));
        fd.push(self_event(1, IN_MODIFY));
        let q = critical_section::with(|cs| fd.state.borrow(cs).borrow().queue.clone());
        assert_eq!(q.len(), MAX_QUEUED_EVENTS, "dropped, never grown");
        assert_eq!(
            q.iter().filter(|e| e.mask == IN_Q_OVERFLOW).count(),
            1,
            "one overflow report per episode"
        );
        assert_eq!(
            q.iter().find(|e| e.mask == IN_Q_OVERFLOW).unwrap().wd,
            NO_WD
        );
    }

    /// The instance cap: descriptors cannot be conjured past it, and
    /// closing one frees its slot. Measured, not asserted to the exact
    /// number - other tests run beside this one and hold a few of their
    /// own, and the cap is a system-wide promise, not a per-test one.
    #[test]
    fn the_instance_cap_answers_and_frees() {
        let _g = lock_tests();
        let mut held: Vec<Arc<InotifyInstance>> = Vec::new();
        while let Some(fd) = InotifyInstance::try_new(5000 + held.len() as u64, false) {
            held.push(fd);
            assert!(held.len() <= MAX_INSTANCES, "the cap is the promise");
        }
        assert!(
            held.len() + 8 >= MAX_INSTANCES,
            "the refusals came from the cap, not from something else"
        );
        drop(held.pop());
        assert!(
            InotifyInstance::try_new(6001, false).is_some(),
            "a closed slot is free again"
        );
    }

    static TEST_INO: AtomicU32 = AtomicU32::new(1);
    fn next_test_ino() -> Ino {
        TEST_INO.fetch_add(1, Ordering::Relaxed) as Ino
    }

    /// A watched directory: a ramfs file, since watch keys come from real
    /// nodes and the emit tests ask the registry exactly what the syscall
    /// handlers would.
    fn watched_dir() -> Arc<dyn Inode> {
        crate::vfs::ramfs::RamFs::new()
            .root()
            .create("watched", NodeKind::Dir)
            .unwrap()
    }

    /// The wd lifecycle: new watches allocate from 1, the same instance
    /// re-adding the same key updates in place and keeps the number,
    /// `IN_MASK_CREATE` says EEXIST (as an outcome, which is where it
    /// becomes that errno), and `IN_MASK_ADD` merges.
    #[test]
    fn watches_dedup_per_instance_and_update_in_place() {
        let (node, _) = instance_node();
        let other = instance_node().0;
        let dir = watched_dir();
        let key = key_of(&dir);
        let owner = node.metadata().ino;

        assert_eq!(add_watch(&node, owner, key, IN_CREATE), AddOutcome::New(1));
        assert_eq!(
            add_watch(&node, owner, key, IN_DELETE),
            AddOutcome::Replaced(1)
        );
        assert_eq!(
            add_watch(&node, owner, key, IN_CREATE | IN_MASK_CREATE),
            AddOutcome::Exists(1)
        );
        assert_eq!(
            add_watch(&node, owner, key, IN_CREATE | IN_MASK_ADD),
            AddOutcome::Replaced(1)
        );
        // A second instance watching the same inode gets its own number.
        assert_eq!(
            add_watch(&other, other.metadata().ino, key, IN_CREATE),
            AddOutcome::New(1)
        );
        // And the stored mask after the merge carries both bits the first
        // two adds asked for, and not the replaced-only one: replace means
        // "IN_DELETE alone", merge added CREATE back.
        let seen = critical_section::with(|cs| {
            REGISTRY
                .borrow(cs)
                .borrow()
                .watches
                .iter()
                .find(|w| w.key == key && w.owner == owner)
                .map(|w| w.mask)
        });
        assert_eq!(seen, Some(IN_DELETE | IN_CREATE));
    }

    /// Emit fan-out: matching watches get the record (with `IN_ISDIR` on a
    /// directory event), non-matching masks and non-matching keys stay
    /// quiet, and `poll_ready` flips on the receiver.
    #[test]
    fn emit_delivers_to_matching_watches_only() {
        let (node, instance) = instance_node();
        let dir = watched_dir();
        let key = key_of(&dir);
        add_watch(&node, node.metadata().ino, key, IN_CREATE);

        emit(&dir, IN_DELETE, "gone"); // mask mismatch
        assert!(
            !instance.pending(),
            "a watched mask that does not match stays silent"
        );
        emit(&watched_dir(), IN_CREATE, "elsewhere"); // key mismatch
        assert!(
            !instance.pending(),
            "another inode's news is not this watch's"
        );

        emit(&dir, IN_CREATE, "new");
        assert!(instance.pending(), "the match lands");
        let mut buf = [0u8; 64];
        let n = instance.read_at(0, &mut buf).unwrap();
        assert!(n >= 24);
        assert_eq!(i32::from_ne_bytes(buf[..4].try_into().unwrap()), 1, "wd");
        let mask = u32::from_ne_bytes(buf[4..8].try_into().unwrap());
        assert_eq!(mask, IN_CREATE | IN_ISDIR, "directory events say so");
    }

    /// `IN_ONESHOT`: a delivered event spends the watch, and the spending
    /// is reported as the `IN_IGNORED` record right behind the event.
    #[test]
    fn a_oneshot_watch_is_spent_by_the_event_it_matches() {
        let (node, instance) = instance_node();
        let dir = watched_dir();
        let key = key_of(&dir);
        add_watch(&node, node.metadata().ino, key, IN_CREATE | IN_ONESHOT);

        emit(&dir, IN_CREATE, "x");
        // One read carries both halves of a spent watch's news: the
        // matched record and the IGNORED that retires it arrive together.
        let mut buf = [0u8; 64];
        let n = instance.read_at(0, &mut buf).unwrap();
        assert_eq!(n, 40, "24-byte event + 16-byte IGNORED");
        assert_eq!(
            u32::from_ne_bytes(buf[4..8].try_into().unwrap()),
            IN_CREATE | IN_ISDIR
        );
        assert_eq!(
            u32::from_ne_bytes(buf[28..32].try_into().unwrap()),
            IN_IGNORED
        );
        assert!(
            !critical_section::with(|cs| REGISTRY
                .borrow(cs)
                .borrow()
                .watches
                .iter()
                .any(|w| w.key == key && w.owner == node.metadata().ino)),
            "the spent watch is gone from the registry"
        );
    }

    /// The death pair for a watched inode: `IN_DELETE_SELF` then
    /// `IN_IGNORED`, and no watch left behind to catch a reused number.
    #[test]
    fn a_deleted_watch_target_reports_its_death_and_is_torn_down() {
        let (node, instance) = instance_node();
        let dir = watched_dir();
        let key = key_of(&dir);
        add_watch(&node, node.metadata().ino, key, IN_DELETE_SELF);

        notify_deleted(&dir);
        let mut buf = [0u8; 64];
        let n = instance.read_at(0, &mut buf).unwrap();
        assert_eq!(n, 32, "two nameless records");
        // The watched inode is a directory, and directory events say so -
        // `IN_ISDIR` rides along on the self-event too, as Linux sends it.
        assert_eq!(
            u32::from_ne_bytes(buf[4..8].try_into().unwrap()),
            IN_DELETE_SELF | IN_ISDIR
        );
        assert_eq!(
            u32::from_ne_bytes(buf[20..24].try_into().unwrap()),
            IN_IGNORED,
            "IGNORED at the second record's mask offset"
        );
        assert_eq!(instance.watch_count(), 0);
        assert!(!critical_section::with(|cs| REGISTRY
            .borrow(cs)
            .borrow()
            .watches
            .iter()
            .any(|w| w.key == key)));
    }

    /// `remove_watch` for an unknown wd answers false (the errno is the
    /// handler's EINVAL), and for a live one answers true with its IGNORED
    /// delivered and its registry entry gone.
    #[test]
    fn remove_watch_answers_by_whether_the_wd_was_live() {
        let (node, instance) = instance_node();
        let dir = watched_dir();
        let key = key_of(&dir);
        add_watch(&node, node.metadata().ino, key, IN_ATTRIB);

        assert!(!remove_watch(&instance, node.metadata().ino, 404));
        assert!(remove_watch(&instance, node.metadata().ino, 1));
        let mut buf = [0u8; 16];
        instance.read_at(0, &mut buf).unwrap();
        assert_eq!(
            u32::from_ne_bytes(buf[4..8].try_into().unwrap()),
            IN_IGNORED
        );
        assert!(!critical_section::with(|cs| REGISTRY
            .borrow(cs)
            .borrow()
            .watches
            .iter()
            .any(|w| w.key == key)));
    }

    /// Dead instances are swept: closing every reference to a node leaves
    /// tombstones the sweep removes.
    #[test]
    fn closed_instances_leave_no_tombstones_after_a_sweep() {
        let (node, instance) = instance_node();
        let dir = watched_dir();
        let key = key_of(&dir);
        let owner = node.metadata().ino;
        add_watch(&node, owner, key, IN_CREATE);
        drop(instance);
        drop(node);
        sweep_dead();
        assert!(!critical_section::with(|cs| REGISTRY
            .borrow(cs)
            .borrow()
            .watches
            .iter()
            .any(|w| w.key == key && w.owner == owner)));
    }
}
