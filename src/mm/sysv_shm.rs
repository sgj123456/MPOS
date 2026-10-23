//! System V shared memory: the segment table, and the frames a segment's pages
//! are.
//!
//! A segment is not a file and not an address-space range: it is a *memfd-like
//! object with a name that is a number* - `shmid` - which any process holding a
//! number may attach to its own address space, and whose pages every attachment
//! aliases. That is why the object lives here rather than in `vfs`: nothing opens
//! it by path, `mmap` is not its door, and the one thing a mapping of it needs is
//! what [`MappedObject`] already asks for - the frame behind a page, the
//! `(dev, inode)` pair a maps line prints, and identity.
//!
//! The registry is one table with the id scheme [`crate::mm::ipc_id`] spells out.
//! What the table *is* to a caller - keys, permissions, `IPC_SET`, `IPC_RMID`
//! marking a segment dead while its attachments keep it alive - is Linux's, and
//! measured before any of it was written (`docs/notes/` §6.13). The three facts
//! that shaped this file:
//!
//! * `IPC_STAT`'s `shm_segsz` is the size **asked for**, not the page-rounded one
//!   (a 100-byte segment reports 100 and maps one page);
//! * `IPC_RMID` does not destroy an attached segment: the number stays valid and
//!   a further `shmat` succeeds until the last detach, after which the number is
//!   `EINVAL` - so "removed" is a flag on the object, not a removal from the
//!   table;
//! * a `fork` **counts as an attachment** (the child reads `shm_nattch` one
//!   higher and the count falls back when it exits), which is why the count is a
//!   question asked of the address spaces rather than a number this object keeps.
//!
//! The last one is the reason there is no `nattch` field here at all: a field
//! would be wrong the moment a child is cloned or a mapping is split, and the
//! truth is a fold over every VMA that names the segment. `sched` answers it
//! (`shm_attachment_count`) because the address spaces are its to walk; this
//! module stays below `sched` and never asks.
//!
//! **The table is per IPC namespace**: one slot space each, keyed by the
//! namespace number `sched::current_ipc_ns` answers (`None` for the machine's
//! own), because the number a segment is known by restarts at 0 in a fresh
//! namespace (measured, `.workbuddy/probe_ipcns.c`) and because `SHMALL` is a
//! per-namespace allowance. A number from one space never names an object of
//! another; the reaper finds a segment by *identity* because a number is only
//! unique inside one (`docs/notes/` §6.16).

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::cell::RefCell;
use core::sync::atomic::{AtomicBool, Ordering};

use critical_section::Mutex;

use crate::mm::ipc_id;
use crate::mm::pframe::{PAGE_SIZE, align_up};

/// `IPC_PRIVATE`, the key that always makes a fresh segment.
pub const IPC_PRIVATE: i32 = 0;

/// The smallest and largest segment size a `shmget` may ask for.
///
/// Linux's `SHMMIN` is 1 and its `SHMMAX` is a sysctl whose default is
/// effectively "the address space". A bound is not optional here (AGENTS.md's
/// worst-case rule: the allocator panics on failure, so anything a caller can
/// steer must be capped) and a small one is a legitimate *configuration*
/// difference - containers routinely lower `shmmax` - as opposed to a shape
/// difference, so this kernel names its own and reports it through `IPC_INFO`:
/// 64 MiB. It is below the machine's 2 GiB of RAM, which is the point.
pub const SHMMAX: usize = 64 << 20;

/// The total a table's segments may claim, in pages: `SHMALL` (128 MiB).
///
/// Charged at *creation* from the size asked for, exactly as Linux's `shm_tot`
/// is, so a process cannot promise itself more memory than exists by creating
/// many large segments and touching one page of each.
pub const SHMALL: usize = 32_768;

/// How many segments may exist at once: Linux's own `SHMMNI`.
pub const SHMMNI: usize = 4_096;

/// `SHM_LOCKED`, the mode bit `SHM_LOCK` sets and `SHM_UNLOCK` clears.
///
/// Measured off `IPC_STAT` on a current host: after `SHM_LOCK` the mode goes
/// from `0600` to `02600`, i.e. this bit - it is *not* the `01000` some older
/// headers spell, which is why it is named from the measurement rather than
/// from a man page.
pub const SHM_LOCKED: u32 = 0o2000;

/// The permission triads of an `IPC_SET`-able mode.
pub const MODE_MASK: u32 = 0o777;

/// Why a segment operation was refused. The syscall layer maps these onto
/// errnos; the numbers themselves are its business, not this object's.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ShmError {
    /// The key names nothing and `IPC_CREAT` was not asked for.
    NoEntry,
    /// The key names something and `IPC_CREAT|IPC_EXCL` was asked for.
    Exists,
    /// The size is outside `SHMMIN..=SHMMAX`.
    BadSize,
    /// An existing keyed segment is smaller than the size the caller named:
    /// `shmget` may not grow one, and Linux answers `EINVAL` rather than the
    /// existing id.
    TooSmall,
    /// The table, or its page allowance, is full. Linux spells both `ENOSPC`.
    Full,
}

/// What a segment is, apart from its pages: everything `IPC_STAT` reports and
/// `IPC_SET` may change.
#[derive(Clone, Copy)]
pub struct Perm {
    /// The key the segment was made with, which is `IPC_PRIVATE` for one that
    /// has no name.
    pub key: i32,
    /// The id that names it, kept here so a caller holding an `Arc` need not
    /// also carry the number.
    pub id: u64,
    /// Current owner, which `IPC_SET` may hand to somebody else.
    pub uid: u32,
    pub gid: u32,
    /// The creator, which no `IPC_SET` moves.
    pub cuid: u32,
    pub cgid: u32,
    /// Permission triads plus the bits above them - `SHM_LOCKED` here.
    pub mode: u32,
    /// The generation this id was built from; `IPC_STAT`'s `seq` field.
    pub seq: u16,
    /// The size asked for, in bytes - not the page-rounded mapping length.
    pub size: usize,
    /// The task that made it.
    pub cpid: u32,
}

/// The three clocks a segment reports, in seconds since the Unix epoch, and the
/// task that last touched one of them.
#[derive(Clone, Copy, Default)]
pub struct Times {
    pub atime: i64,
    pub dtime: i64,
    pub ctime: i64,
    pub lpid: u32,
}

/// One segment.
pub struct Segment {
    perm: Mutex<RefCell<Perm>>,
    times: Mutex<RefCell<Times>>,
    /// The frames of the pages that have been touched, `0` for a page that has
    /// not. Grown to the index asked for, so a caller faulting the last page of
    /// a large segment costs one frame and one `Vec` growth, not the whole run.
    frames: Mutex<RefCell<Vec<usize>>>,
    /// `IPC_RMID` has been asked for. The segment lives until its last
    /// attachment goes, and a number that names it stays valid until then.
    destroyed: AtomicBool,
}

impl Segment {
    /// A copy of the immutable-ish facts, for a handler that wants to read them
    /// without holding the table's lock across a user copy.
    pub fn perm(&self) -> Perm {
        critical_section::with(|cs| *self.perm.borrow(cs).borrow())
    }

    /// The id that names this segment.
    pub fn id(&self) -> u64 {
        self.perm().id
    }

    /// The byte size asked for, unrounded.
    pub fn size(&self) -> usize {
        self.perm().size
    }

    /// The mapped length: `size` rounded up to whole pages.
    pub fn mapped_len(&self) -> usize {
        align_up(self.perm().size.max(1), PAGE_SIZE)
    }

    /// The pages a mapping of it covers.
    pub fn pages(&self) -> usize {
        self.mapped_len() / PAGE_SIZE
    }

    pub fn times(&self) -> Times {
        critical_section::with(|cs| *self.times.borrow(cs).borrow())
    }

    /// Record an attach: `shm_atime` and the attaching task.
    pub fn touch_attach(&self, pid: u32, now: i64) {
        critical_section::with(|cs| {
            let mut t = self.times.borrow(cs).borrow_mut();
            t.atime = now;
            t.lpid = pid;
        });
    }

    /// Record a detach: `shm_dtime` and the detaching task.
    pub fn touch_detach(&self, pid: u32, now: i64) {
        critical_section::with(|cs| {
            let mut t = self.times.borrow(cs).borrow_mut();
            t.dtime = now;
            t.lpid = pid;
        });
    }

    /// Record a change - `IPC_SET`, `SHM_LOCK`/`SHM_UNLOCK`: `shm_ctime`.
    pub fn touch_change(&self, now: i64) {
        critical_section::with(|cs| {
            self.times.borrow(cs).borrow_mut().ctime = now;
        });
    }

    /// Rewrite the parts of the permission record an `IPC_SET` owns: the owner,
    /// the group and the triads, leaving the creator and every bit above the
    /// triads (the locked bit included) alone.
    ///
    /// That split is Linux's `ipc_update_perm`, and it is why an `IPC_SET` that
    /// carries `mode = 0666` over a locked segment does not silently unlock it.
    pub fn set_perm(&self, uid: u32, gid: u32, mode: u32) {
        critical_section::with(|cs| {
            let mut p = self.perm.borrow(cs).borrow_mut();
            p.uid = uid;
            p.gid = gid;
            p.mode = (p.mode & !MODE_MASK) | (mode & MODE_MASK);
        });
    }

    /// Set or clear `SHM_LOCKED`.
    pub fn set_locked(&self, locked: bool) {
        critical_section::with(|cs| {
            let mut p = self.perm.borrow(cs).borrow_mut();
            if locked {
                p.mode |= SHM_LOCKED;
            } else {
                p.mode &= !SHM_LOCKED;
            }
        });
    }

    /// Take the segment's key away: `IPC_RMID` does this at once, before the
    /// segment itself goes, so the name is free for a new segment while the old
    /// one is still attached. Measured on a current host: `IPC_STAT` after the
    /// `IPC_RMID` reports `key = 0` (`IPC_PRIVATE`) while `nattch` is unchanged.
    pub fn clear_key(&self) {
        critical_section::with(|cs| self.perm.borrow(cs).borrow_mut().key = IPC_PRIVATE);
    }

    /// Mark the segment for destruction. `false` means it already was.
    pub fn destroy(&self) -> bool {
        !self.destroyed.swap(true, Ordering::Relaxed)
    }

    /// Whether `IPC_RMID` has been asked for and the last attachment has gone -
    /// the state in which the table may drop it and a number that named it is
    /// `EINVAL` for ever.
    pub fn is_reapable(&self) -> bool {
        self.destroyed.load(Ordering::Relaxed)
    }

    /// How many of the segment's pages have a frame - `shm_rss`, and the number
    /// `/proc/sysvipc/shm` prints.
    ///
    /// Counted from the frame list rather than kept as a counter, because a
    /// `madvise(MADV_DONTNEED)` on an attachment drops a page and a fault brings
    /// it back; a counter would drift where this cannot.
    pub fn resident_pages(&self) -> usize {
        critical_section::with(|cs| {
            self.frames
                .borrow(cs)
                .borrow()
                .iter()
                .filter(|frame| **frame != 0)
                .count()
        })
    }
}

#[cfg(target_os = "none")]
impl crate::mm::vma::MappedObject for Segment {
    /// The frame behind page `index`, allocated and zeroed the first time it is
    /// asked for and aliased by every attachment afterwards.
    ///
    /// Lazy rather than eager: `shmget` promises an address range and charges
    /// `SHMALL` for it, and a process that maps a megabyte and touches one page
    /// should cost one frame. The returned frame carries the extra reference the
    /// fault path takes and drops when the mapping goes, exactly as a memfd's
    /// does.
    fn frame_at(&self, index: u64) -> Option<usize> {
        let idx = usize::try_from(index).ok()?;
        if idx >= self.pages() {
            return None;
        }
        critical_section::with(|cs| {
            let mut frames = self.frames.borrow(cs).borrow_mut();
            if idx >= frames.len() {
                frames.resize(idx + 1, 0);
            }
            if frames[idx] == 0 {
                let frame = crate::mm::alloc_frames(0)?;
                // A page nobody has written reads as zeros: a fresh segment is
                // not whatever the frame's last owner left in it.
                unsafe {
                    core::ptr::write_bytes(crate::mm::phys_to_virt(frame) as *mut u8, 0, PAGE_SIZE)
                };
                frames[idx] = frame;
            }
            let frame = frames[idx];
            crate::mm::clone_frame_ref(frame);
            Some(frame)
        })
    }

    /// The pair `/proc/<pid>/maps` prints. Measured off a current host: a
    /// segment's line carries `00:01` and the **shmid** as its inode, which is
    /// what a reader correlating a mapping with `ipcs` needs.
    fn identity(&self) -> ((u32, u32), u64) {
        ((0, 1), self.id())
    }

    /// Which segment this object is - the answer `sched` folds into an
    /// attachment count without downcasting a trait object.
    fn shm_id(&self) -> Option<u64> {
        Some(self.id())
    }
}

#[cfg(target_os = "none")]
impl Drop for Segment {
    fn drop(&mut self) {
        // The last reference (a table entry or a VMA) is gone: every page comes
        // back to the allocator. A mapping still alive would be a reference, so
        // this only runs when nothing can reach the frames.
        let frames: Vec<usize> =
            critical_section::with(|cs| core::mem::take(&mut *self.frames.borrow(cs).borrow_mut()));
        for frame in frames {
            if frame != 0 {
                crate::mm::free_frames(frame, 0);
            }
        }
    }
}

/// One namespace's slot space.
///
/// The number a segment is known by restarts at 0 in a fresh IPC namespace
/// (measured, `.workbuddy/probe_ipcns.c`), which only a slot vector per space can
/// produce - so a space is what the table holds, keyed by the namespace. `pages`
/// is the space's `SHMALL` allowance, per namespace because Linux's limit is.
struct Space {
    ns: Option<u64>,
    /// The live segments, by slot. A slot that has been used keeps its
    /// generation even after the segment goes, so a number from the last
    /// occupant never names the next.
    slots: Vec<Option<Arc<Segment>>>,
    /// The generation of each slot, used when a slot is filled again.
    seqs: Vec<u16>,
    /// Pages claimed by the live segments, against [`SHMALL`].
    pages: usize,
}

impl Space {
    const fn new(ns: Option<u64>) -> Space {
        Space {
            ns,
            slots: Vec::new(),
            seqs: Vec::new(),
            pages: 0,
        }
    }

    fn slot(&self, index: u64) -> Option<&Arc<Segment>> {
        self.slots.get(index as usize)?.as_ref()
    }

    fn find_key(&self, key: i32) -> Option<Arc<Segment>> {
        self.slots
            .iter()
            .flatten()
            .find(|seg| seg.perm().key == key && !seg.is_reapable())
            .cloned()
    }

    /// The first free slot's index, growing the vectors as far as `SHMMNI`.
    fn free_slot(&mut self) -> Option<u64> {
        if let Some(index) = self.slots.iter().position(|slot| slot.is_none()) {
            return Some(index as u64);
        }
        if self.slots.len() >= SHMMNI {
            return None;
        }
        self.slots.push(None);
        self.seqs.push(0);
        Some((self.slots.len() - 1) as u64)
    }

    /// Give `slot` a segment, keeping the slot's generation for the number.
    fn install(&mut self, slot: u64, segment: Arc<Segment>) {
        self.pages += segment.pages();
        self.slots[slot as usize] = Some(segment);
    }

    fn remove(&mut self, slot: u64) -> Option<Arc<Segment>> {
        let index = slot as usize;
        let segment = self.slots.get_mut(index)?.take()?;
        self.pages = self.pages.saturating_sub(segment.pages());
        // The next occupant of this slot is a different object, even if the
        // number's slot part is the same.
        if let Some(seq) = self.seqs.get_mut(index) {
            *seq = ipc_id::next_generation(*seq);
        }
        Some(segment)
    }
}

/// Every namespace's slot space, the machine's own made on first use.
///
/// A namespace that dies leaves its space behind - nothing here is told when a
/// namespace loses its last task - so its segments stay until the machine ends,
/// bounded by `SHMMNI` and `SHMALL` per space and by how many namespaces were
/// ever made (`docs/notes/` §6.16).
struct Table {
    spaces: Vec<Space>,
}

impl Table {
    const fn empty() -> Table {
        Table { spaces: Vec::new() }
    }

    fn space(&self, ns: Option<u64>) -> Option<&Space> {
        self.spaces.iter().find(|space| space.ns == ns)
    }

    fn space_mut(&mut self, ns: Option<u64>) -> &mut Space {
        if let Some(index) = self.spaces.iter().position(|space| space.ns == ns) {
            return &mut self.spaces[index];
        }
        self.spaces.push(Space::new(ns));
        self.spaces.last_mut().expect("just pushed")
    }

    /// Where the object itself sits: which space, and which slot. By identity
    /// rather than by number, because a number is only unique inside one space -
    /// this is what [`reap`] needs, since it holds the segment.
    fn find_segment(&self, segment: &Arc<Segment>) -> Option<(usize, u64)> {
        for (space_index, space) in self.spaces.iter().enumerate() {
            for (slot, held) in space.slots.iter().enumerate() {
                if let Some(held) = held
                    && Arc::ptr_eq(held, segment)
                {
                    return Some((space_index, slot as u64));
                }
            }
        }
        None
    }
}

static TABLE: Mutex<RefCell<Table>> = Mutex::new(RefCell::new(Table::empty()));

/// Find a live segment by the number that names it in namespace `ns`, checking
/// the generation.
pub fn lookup(ns: Option<u64>, id: i64) -> Option<Arc<Segment>> {
    let (slot, seq) = ipc_id::decode(id)?;
    critical_section::with(|cs| {
        let table = TABLE.borrow(cs).borrow();
        let space = table.space(ns)?;
        // A slot whose generation differs names an object this one replaced.
        if space.seqs.get(slot as usize).copied()? != seq {
            return None;
        }
        space.slot(slot).cloned()
    })
}

/// The segment in namespace `ns`'s table slot `index`, for `SHM_STAT`/
/// `SHM_STAT_ANY`, whose argument is an index `ipcs` walks rather than a number
/// a caller holds.
pub fn by_slot(ns: Option<u64>, index: u64) -> Option<Arc<Segment>> {
    critical_section::with(|cs| TABLE.borrow(cs).borrow().space(ns)?.slot(index).cloned())
}

/// Every segment a reader in namespace `ns` may see, in slot order: what
/// `/proc/sysvipc/shm` walks.
pub fn all(ns: Option<u64>) -> Vec<Arc<Segment>> {
    critical_section::with(|cs| {
        TABLE
            .borrow(cs)
            .borrow()
            .space(ns)
            .map(|space| space.slots.iter().flatten().cloned().collect())
            .unwrap_or_default()
    })
}

/// The highest slot index a live segment of `ns` occupies, which is the
/// `IPC_INFO` and `SHM_INFO` return value `ipcs` walks (Linux's `ipc_get_maxid`).
pub fn highest_slot(ns: Option<u64>) -> u64 {
    critical_section::with(|cs| {
        let table = TABLE.borrow(cs).borrow();
        match table
            .space(ns)
            .and_then(|space| space.slots.iter().rposition(|slot| slot.is_some()))
        {
            Some(index) => index as u64,
            None => 0,
        }
    })
}

/// How many segments are live in `ns` and how many pages they claim, for
/// `SHM_INFO`.
pub fn totals(ns: Option<u64>) -> (usize, usize) {
    critical_section::with(|cs| {
        let table = TABLE.borrow(cs).borrow();
        let Some(space) = table.space(ns) else {
            return (0, 0);
        };
        (space.slots.iter().flatten().count(), space.pages)
    })
}

/// The facts a new segment is made from, grouped because they travel together.
pub struct New {
    pub key: i32,
    pub size: usize,
    pub mode: u32,
    pub uid: u32,
    pub gid: u32,
    pub cpid: u32,
    pub now: i64,
}

/// Make a segment in `ns`, or answer the live one `key` already names **there**.
///
/// `flags` is the caller's `shmflg`: the two `IPC_*` creation bits are read here,
/// and the `SHM_*` bits are the caller's business. The returned `created` says
/// which happened - the handler needs it only to know whether to report the new
/// number as a creation. A key is namespace-scoped: the same key inside a fresh
/// namespace makes a new segment numbered from 0 (measured, §6.16).
///
/// The checks are in Linux's order, which is load-bearing for the errno a caller
/// branches on: an existing key with `IPC_EXCL` is `EEXIST` before any size or
/// permission question, and a size larger than the existing segment is `EINVAL`
/// rather than "the segment you asked for does not exist".
pub fn get_or_create(
    ns: Option<u64>,
    new: New,
    create: bool,
    exclusive: bool,
) -> Result<(Arc<Segment>, bool), ShmError> {
    if new.key != IPC_PRIVATE {
        if let Some(existing) = find_any(ns, new.key) {
            if create && exclusive {
                return Err(ShmError::Exists);
            }
            if new.size > existing.size() {
                return Err(ShmError::TooSmall);
            }
            return Ok((existing, false));
        }
        if !create {
            return Err(ShmError::NoEntry);
        }
    }
    create_segment(ns, new).map(|segment| (segment, true))
}

/// Like [`lookup`], but by key. A destroyed segment is deliberately *not* found:
/// measured on a current host, `IPC_RMID` clears the key to `IPC_PRIVATE` the
/// moment it marks the segment, so a `shmget` naming that key creates a fresh
/// segment (and Linux's own does - the dying one keeps only its id).
fn find_any(ns: Option<u64>, key: i32) -> Option<Arc<Segment>> {
    critical_section::with(|cs| {
        TABLE
            .borrow(cs)
            .borrow()
            .space(ns)
            .and_then(|space| space.find_key(key))
    })
}

/// Make a segment unconditionally.
fn create_segment(ns: Option<u64>, new: New) -> Result<Arc<Segment>, ShmError> {
    if new.size < 1 || new.size > SHMMAX {
        return Err(ShmError::BadSize);
    }
    let pages = align_up(new.size, PAGE_SIZE) / PAGE_SIZE;
    critical_section::with(|cs| {
        let mut table = TABLE.borrow(cs).borrow_mut();
        let space = table.space_mut(ns);
        if space.pages + pages > SHMALL {
            return Err(ShmError::Full);
        }
        let slot = space.free_slot().ok_or(ShmError::Full)?;
        let seq = space.seqs[slot as usize];
        let id = ipc_id::build(slot, seq);
        let segment = Arc::new(Segment {
            perm: Mutex::new(RefCell::new(Perm {
                key: new.key,
                id,
                uid: new.uid,
                gid: new.gid,
                cuid: new.uid,
                cgid: new.gid,
                mode: new.mode & MODE_MASK,
                seq,
                size: new.size,
                cpid: new.cpid,
            })),
            times: Mutex::new(RefCell::new(Times {
                ctime: new.now,
                ..Times::default()
            })),
            frames: Mutex::new(RefCell::new(Vec::new())),
            destroyed: AtomicBool::new(false),
        });
        space.install(slot, Arc::clone(&segment));
        Ok(segment)
    })
}

/// Drop a segment from its table: only once it is destroyed and nothing has it
/// attached.
///
/// `attachments` is the count the caller folded out of the address spaces -
/// `sched::shm_attachment_count` - passed in rather than asked for, because
/// `mm` sits below `sched` and may not name it. The slot is found by **object**,
/// not by number: the number is only unique inside one namespace, and this holds
/// the segment itself.
pub fn reap(segment: &Arc<Segment>, attachments: usize) -> bool {
    if attachments != 0 || !segment.is_reapable() {
        return false;
    }
    critical_section::with(|cs| {
        let mut table = TABLE.borrow(cs).borrow_mut();
        let Some((space_index, slot)) = table.find_segment(segment) else {
            return false;
        };
        // The generation check: a slot that has already been reused (by a
        // `shmget` racing this reap on another thread - impossible on one core
        // with interrupts off, but the table must not depend on that) is not
        // this segment's to clear.
        let Some((_, seq)) = ipc_id::decode(segment.id() as i64) else {
            return false;
        };
        let space = &mut table.spaces[space_index];
        if space
            .slot(slot)
            .is_some_and(|live| Arc::ptr_eq(live, segment) && live.perm().seq == seq)
        {
            space.remove(slot);
            return true;
        }
        false
    })
}

/// Drop the slot space a **dead** IPC namespace kept, for the reason
/// [`crate::sched::sysv_msg::forget`] gives.
///
/// Dropping the segments' table references cannot lose their pages: a segment
/// mapped into some address space is kept alive by that mapping's own `Arc`
/// (which is what [`Segment`]'s `Drop` relies on), so the frames come back
/// exactly when the last user of them goes - Linux's `shm_file` refcount
/// doing the same job.
pub fn forget(ns: Option<u64>) {
    critical_section::with(|cs| {
        let mut table = TABLE.borrow(cs).borrow_mut();
        if let Some(at) = table.spaces.iter().position(|space| space.ns == ns) {
            // Marked destroyed so that a `shmget` cannot name one of them again
            // through a reused number, and so the table's own sweep would skip
            // them if a later space ever claimed the same slots.
            for segment in table.spaces[at].slots.iter().flatten() {
                segment.destroy();
            }
            table.spaces.remove(at);
        }
    });
}

/// Every destroyed segment still in the table, for a sweep that computes their
/// attachment counts and reaps the ones at zero.
pub fn destroyed() -> Vec<Arc<Segment>> {
    critical_section::with(|cs| {
        // Every space, not the reader's: a segment marked for destruction in a
        // namespace nobody is standing in still has frames to give back, and the
        // sweep is the machine's.
        TABLE
            .borrow(cs)
            .borrow()
            .spaces
            .iter()
            .flat_map(|space| space.slots.iter().flatten())
            .filter(|segment| segment.is_reapable())
            .cloned()
            .collect()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The limits are not decoration: `IPC_INFO` reports them, `SHMALL` is what
    /// keeps a caller from promising itself more frames than exist, and
    /// `SHMMAX` is the one size a single segment may be. A change to any of the
    /// three is a change to what a program measuring the machine sees.
    #[test]
    fn the_limits_are_what_the_calls_report() {
        assert_eq!(SHMMNI, 4_096);
        assert_eq!(SHMALL, 32_768);
        assert!(SHMMAX < 128 << 20);
        assert!(SHMMAX as usize >= 1 << 20);
        // `SHM_LOCKED` sits above the triads, so `MODE_MASK` cannot erase it -
        // the split `set_perm` keeps.
        assert_eq!(SHM_LOCKED & MODE_MASK, 0);
    }

    /// The page arithmetic `IPC_STAT`'s size and the mapping's length disagree
    /// about, deliberately: the size is what was asked for, the mapping is whole
    /// pages.
    #[test]
    fn a_size_between_pages_maps_one_page_and_reports_itself() {
        assert_eq!(align_up(100, PAGE_SIZE), PAGE_SIZE);
        assert_eq!(align_up(PAGE_SIZE, PAGE_SIZE), PAGE_SIZE);
        assert_eq!(align_up(PAGE_SIZE + 1, PAGE_SIZE), 2 * PAGE_SIZE);
    }
}
