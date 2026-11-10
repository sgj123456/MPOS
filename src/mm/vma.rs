//! The process's virtual memory areas: which ranges are mapped, with what
//! permissions, and what backs each one.
//!
//! The page tables say what a virtual address *translates to*. They cannot say
//! what an address that translates to nothing is *supposed* to be - and with
//! demand paging that is the question every fault asks. This table is the
//! answer: a mapping exists (and may be populated on first touch) exactly when
//! a VMA covers the address, and the VMA carries the permissions the access is
//! checked against and the backing a page comes from.
//!
//! It lives on [`crate::sched::Mm`] rather than on the task because memory is
//! per *process*: a process is one thread group, its threads share one address
//! space, and Linux keeps `mm->mmap` in the same place.
//!
//! A `BTreeMap` keyed by `start` and not the sorted `Vec` this file used to
//! keep. The old rationale - "a process holds tens of these, not thousands, so
//! a slice is enough" - was measured wrong: a program mapping its way down the
//! arena (a sanitizer's shadow, a JIT's code cache, `t-mmap-count`) holds
//! thousands, and with a `Vec` every insert re-sorted and rebuilt the whole
//! list, so 20 480 one-page mappings did not finish in the gate's 120 s budget
//! (`docs/notes/` §10.11). The map answers the two hot questions - which VMA
//! covers this address, and where the highest hole below the cursor is - in
//! `log n`, keeps the same invariant the slice had (address order, no overlap,
//! abutting descriptions folded), and is still `Clone`-able for `fork`.
//! The decisions that matter - what a map/unmap/protect/split does to the set -
//! are pure arithmetic over ranges, so they stay pinned by this file's own host
//! tests rather than only from inside the machine.

use core::sync::atomic::{AtomicUsize, Ordering};

use alloc::collections::BTreeMap;
use alloc::sync::Arc;
use alloc::vec::Vec;

use crate::mm::vm::{MapFlags, PAGE_SIZE};

/// The object a frame-backed mapping aliases its pages from.
///
/// `mm` asks an object exactly three things, and this trait is those three and
/// nothing else: the frame behind one of its pages (what the fault path needs),
/// the `(device, inode)` pair `/proc/<pid>/maps` prints for a line, and whether
/// two mappings are of the *same* object (what lets two adjacent ranges
/// coalesce into the one line Linux prints).
///
/// It is deliberately **not** `vfs::Inode`. An address space is a memory
/// concept and a file is not; naming the VFS here is what made `mm` and `vfs` a
/// cycle (`ARCHITECTURE.md` V-13, whose own note says the file-backed mapping
/// "needs *a* mapped-object trait; `Inode` is the wrong one to name"). The VFS
/// answers this trait for its inodes through `crate::vfs::MappedInode`, which
/// is where the three answers come from - and a host test of *this* file can
/// supply a stand-in that knows nothing about files at all.
pub trait MappedObject {
    /// The frame holding the object's `index`-th page, or `None` if it has no
    /// such page - which is what makes a mapping of it grow lazily rather than
    /// being sized up front.
    fn frame_at(&self, index: u64) -> Option<usize>;

    /// The `(device, inode)` pair a mapping of this object reports. A device
    /// node answers the device it *is*; anything else answers `(0, 0)`, which is
    /// what this kernel's `stat` says for a filesystem and so keeps the two
    /// readers of that fact agreeing.
    fn identity(&self) -> ((u32, u32), u64);

    /// Whether the object's name is gone, so `/proc/<pid>/maps` appends the
    /// ` (deleted)` Linux prints for a mapping whose file was unlinked while it
    /// stayed mapped. Default `false`, for the same reason the VFS default is:
    /// an object that keeps no link count does not know, and a wrong suffix is
    /// worse than a missing one.
    fn name_is_gone(&self) -> bool {
        false
    }

    /// Which System V shared-memory segment this object is, for an object that
    /// is one.
    ///
    /// `shm_nattch` is not a field on a segment: it is how many address spaces
    /// map it, and a `fork` copies a mapping (Linux counts the child as an
    /// attachment, measured). So the answer has to be folded out of the VMA
    /// tables, and the fold needs to recognise a segment's object among every
    /// other frame-backed one. A `MappedObject` cannot be downcast, so the
    /// object says which segment it is instead - `None` for a memfd, a dma-buf,
    /// the framebuffer or a dumb buffer.
    fn shm_id(&self) -> Option<u64> {
        None
    }

    /// Whether `other` is this same object.
    ///
    /// Defaulted to identity, which is the strongest comparison a trait object
    /// can make without a downcast. For the four node types that are
    /// frame-backed here - a `memfd`, a dma-buf, the framebuffer and a DRM dumb
    /// buffer - the pair *is* unique per object, so this gives the same answer
    /// the `Arc::ptr_eq` it replaces did, expressed as a value rather than as an
    /// address. What the default costs is the distinction between two objects
    /// that report one identity, which would mean two inodes of one file; what
    /// it buys is that `mm` never sees a pointer it would have to interpret.
    fn same_as(&self, other: &dyn MappedObject) -> bool {
        self.identity() == other.identity()
    }

    /// Whether the kernel may bring one of this object's pages in *on the
    /// caller's behalf*, while a syscall validates a buffer.
    ///
    /// Default `false`, and the default is the load-bearing half: populating a
    /// frame-backed page calls into the node that owns the object, and the DRM
    /// ioctl path validates a caller's array **while holding the card's lock**,
    /// so a page that needed the card to answer a fault would be a lock taken
    /// twice. A dumb buffer, a dma-buf, the framebuffer and a shared memfd
    /// therefore stay `false`.
    ///
    /// A private mapping of a file is the one object that says `true`, because
    /// populating it reads the *filesystem* - which no syscall holds a lock
    /// across - and because a page of such a mapping is not something the
    /// process has failed to touch: Linux's `copy_to_user` faults it in exactly
    /// as a ring-3 access does, so refusing it here would answer `EFAULT` for
    /// memory the caller legally mapped (`docs/notes/` §10.15a, item 188).
    fn lazy_populatable(&self) -> bool {
        false
    }
}

/// What is behind a range's pages.
#[derive(Clone)]
pub enum VmaKind {
    /// Private anonymous memory: each page is a fresh zero frame on first
    /// touch, owned by the process alone.
    Anon,
    /// Frames the backing node owns and the mapping aliases: a `memfd` (which
    /// includes the unnamed one a shared anonymous mapping is made of), a
    /// dma-buf, the framebuffer, a DRM dumb buffer. `offset` is the *byte*
    /// offset into that object that `start` corresponds to, so the page a fault
    /// names is `offset + (virt - start)`, and a split or a move adjusts it
    /// with the address.
    ///
    /// `shared` is whether the caller asked for `MAP_SHARED`, and it is the
    /// mapping's own bit rather than the object's: a `memfd` mapped
    /// `MAP_PRIVATE` aliases the same frames but is *not* a shared mapping -
    /// Linux prints it `p`, and a write through it is a private fault that
    /// never reaches the object. It is what the fourth column of
    /// `/proc/<pid>/maps` reports, and the two are not derivable from each
    /// other: the object says what the pages *are*, this says what the caller
    /// asked for.
    Frames {
        object: Arc<dyn MappedObject>,
        offset: usize,
        shared: bool,
    },
}

impl VmaKind {
    /// Whether two kinds can be the two halves of one merged VMA. The address
    /// arithmetic that has to agree as well - abutment, and that the frame
    /// offsets continue each other - is checked by [`Vma::abuts`].
    fn same_object(&self, other: &VmaKind) -> bool {
        match (self, other) {
            (VmaKind::Anon, VmaKind::Anon) => true,
            (
                VmaKind::Frames {
                    object: a,
                    shared: sa,
                    ..
                },
                VmaKind::Frames {
                    object: b,
                    shared: sb,
                    ..
                },
            ) => a.same_as(b.as_ref()) && sa == sb,
            _ => false,
        }
    }
}

/// A range of the address space whose pages were filled *eagerly*, recorded
/// for `/proc/<pid>/maps` to report.
///
/// Two things make these, and they are the same thing twice: `execve`, which
/// loads an image's bytes as it goes (`sched::load_user`), and a `MAP_PRIVATE`
/// mapping of an ordinary file, which reads the file's bytes in and keeps
/// nothing of the file afterwards (`handle_mmap`). Both fill every page at the
/// moment they map it, so no address in either ever faults.
///
/// That is why this is a separate type from [`Vma`] and not a kind of one.
/// A [`Vma`] exists to answer a fault, and there is no fault to answer here.
/// Recording these as `VmaKind::Anon` VMAs would *invent* one: a page that
/// `MADV_DONTNEED` had given back, or that a `munmap` had freed underneath a
/// stale address, would be silently refilled with zeros where the address
/// space rightly takes a fault - and a program's own code would come back as
/// blank bytes rather than as the bytes the file holds.
///
/// So these ranges are kept *beside* the table, where only the reader of
/// `/proc/<pid>/maps` looks. What that reader loses is the difference between
/// a mapping whose pages are the file's and one that is a copy of it, and a
/// file-backed mapping's `offset` and `inode` - none of which this kernel's
/// file mappings keep anywhere. That is a named gap in `NOTES.md` §7.4, not a
/// hidden one.
#[derive(Clone)]
pub struct MappedRange {
    pub start: usize,
    pub end: usize,
    pub prot: MapFlags,
    pub name: Option<Arc<str>>,
    /// `mseal(2)`'s bit, for the ranges that have no VMA - the program image,
    /// the loader's shared objects, the stack and a `MAP_PRIVATE` mapping of an
    /// ordinary file.
    ///
    /// Two descriptions can carry a mapping in this kernel and a seal has to
    /// live on whichever one of them describes the range, which is why the bit
    /// is on both [`Vma`] and [`MappedRange`] rather than on one: `mseal` over a
    /// program's text is the flagship use of the call, and in this kernel that
    /// text is an eager range.
    pub sealed: bool,
    /// Whether this range is pinned by `mlock`/`mlock2`/`mlockall` - Linux's
    /// `VM_LOCKED`.
    ///
    /// On both descriptions for the reason `sealed` gives, and it has to be
    /// stored rather than derived from "was `mlock` called": the accounting is
    /// per *page*, so a second `mlock` of an already-locked range must add
    /// nothing (`build/scratch/mlk/edge.c`), `munlock` of a sub-range must take
    /// off exactly that sub-range (`munlock(base+2P, 3P)` on an 8-page lock
    /// reads `VmLck 32 -> 20`, `build/scratch/mlk/acct2.c`), and three callers
    /// other than the two that set it read the bit: `VmLck` counts it, `smaps`
    /// prints it per mapping, and a destructive `madvise` on it is `EINVAL`
    /// (`build/scratch/mlk/dont.c`).
    pub locked: bool,
}

impl MappedRange {
    /// Whether `next` continues this range, so a segment list can be reported
    /// as the runs a reader expects rather than one line per ELF segment.
    fn abuts(&self, next: &MappedRange) -> bool {
        self.end == next.start
            && self.prot == next.prot
            && self.sealed == next.sealed
            && self.locked == next.locked
            && same_name(&self.name, &next.name)
    }
}

/// Fold a load's range list into the runs a maps line describes: adjacent
/// pieces with the same permissions and the same name are one range. An
/// image's text segment is usually two of them (`r--` then `r-x`) and its data
/// two more, which is exactly how Linux prints them.
///
/// ```
/// use mpos::mm::vma::{MappedRange, merge_ranges};
/// use mpos_vm::MapFlags;
///
/// let rw = MapFlags::READ | MapFlags::WRITE;
/// // Out of order, and the second continues the first: sorted, then folded.
/// let merged = merge_ranges(vec![
///     MappedRange { start: 0x2000, end: 0x3000, prot: rw, name: None, sealed: false, locked: false },
///     MappedRange { start: 0x1000, end: 0x2000, prot: rw, name: None, sealed: false, locked: false },
/// ]);
/// assert_eq!(merged.len(), 1);
/// assert_eq!((merged[0].start, merged[0].end), (0x1000, 0x3000));
/// // A permission change ends the run even when the addresses abut.
/// let rx = MapFlags::READ | MapFlags::EXEC;
/// let merged = merge_ranges(vec![
///     MappedRange { start: 0x1000, end: 0x2000, prot: rw, name: None, sealed: false, locked: false },
///     MappedRange { start: 0x2000, end: 0x3000, prot: rx, name: None, sealed: false, locked: false },
/// ]);
/// assert_eq!(merged.len(), 2);
/// // And so does a lock bit: a pinned page is its own run, because a
/// // `munlock` of the rest has to be able to take it back out.
/// let merged = merge_ranges(vec![
///     MappedRange { start: 0x1000, end: 0x2000, prot: rw, name: None, sealed: false, locked: false },
///     MappedRange { start: 0x2000, end: 0x3000, prot: rw, name: None, sealed: false, locked: true },
/// ]);
/// assert_eq!(merged.len(), 2);
/// ```
pub fn merge_ranges(mut ranges: Vec<MappedRange>) -> Vec<MappedRange> {
    ranges.sort_unstable_by_key(|r| r.start);
    let mut merged: Vec<MappedRange> = Vec::with_capacity(ranges.len());
    for range in ranges.drain(..) {
        match merged.last_mut() {
            Some(last) if last.abuts(&range) => last.end = range.end,
            _ => merged.push(range),
        }
    }
    merged
}

/// One page-granular range of the address space, `[start, end)`.
#[derive(Clone)]
pub struct Vma {
    pub start: usize,
    pub end: usize,
    /// The permissions `mmap`/`mprotect` last set: `READ`/`WRITE`/`EXEC` and,
    /// for a range reachable from ring 3 at all, `USER`. `PROT_NONE` is
    /// exactly the absence of `USER`.
    pub prot: MapFlags,
    pub kind: VmaKind,
    /// What this mapping is called, when it has a name: the path a file-backed
    /// mapping came from, or a bracketed convention like `[heap]`.
    ///
    /// Linux keeps the same fact as `vma->vm_file` plus its `[stack]`/`[heap]`
    /// conventions, and it is there for one reader: `/proc/<pid>/maps`. The
    /// name is *stored* rather than looked up because the lookup is a walk of
    /// the whole filesystem tree - `crate::vfs::File::display_name` says so -
    /// and a maps file with a mapping per page would do that walk once per
    /// line. `Arc<str>` and not `String` so that `Clone`, which every split
    /// does, copies a pointer.
    pub name: Option<Arc<str>>,
    /// `mseal(2)`'s bit: the range may not be unmapped, re-mapped, shrunk,
    /// re-protected or replaced, and a destructive `madvise` on it is refused
    /// unless the pages are writable or file-backed.
    ///
    /// It lives on the VMA rather than in a side table because every one of
    /// those operations already asks the VMA table what is there; a second
    /// structure would have to be kept in step with every `mmap`/`munmap`/
    /// `mremap`/`mprotect`, which is the kind of parallel record this
    /// repository has paid for before. It travels through `sliced`, so a seal
    /// over one page of a four-page mapping leaves the other three unsealed
    /// and a later split cannot lose the bit.
    pub sealed: bool,
    /// Whether `mlock`/`mlock2`/`mlockall` pinned this range - Linux's
    /// `VM_LOCKED`. See [`MappedRange::locked`] for why the bit is stored on
    /// both descriptions and what its three readers are.
    pub locked: bool,
}

impl Vma {
    pub fn new(start: usize, end: usize, prot: MapFlags, kind: VmaKind) -> Vma {
        assert!(start < end, "a VMA covers at least one byte");
        Vma {
            start,
            end,
            prot,
            kind,
            name: None,
            sealed: false,
            locked: false,
        }
    }

    /// The same mapping, with a name for `/proc/<pid>/maps` to print.
    pub fn named(mut self, name: impl Into<Option<Arc<str>>>) -> Vma {
        self.name = name.into();
        self
    }

    pub fn covers(&self, virt: usize) -> bool {
        self.start <= virt && virt < self.end
    }

    /// Whether an access of this kind is one the range permits. A read is
    /// implied by `USER` - every mapping this kernel makes sets `READ` when it
    /// sets `USER` - so the only question a fault asks beyond reachability is
    /// the write bit.
    pub fn permits(&self, write: bool) -> bool {
        self.prot.contains(MapFlags::USER) && (!write || self.prot.contains(MapFlags::WRITE))
    }

    /// The piece of this VMA that lies inside `[start, end)`, with its backing
    /// offset moved to match, or `None` when the two do not meet.
    fn sliced(&self, start: usize, end: usize) -> Option<Vma> {
        let start = start.max(self.start);
        let end = end.min(self.end);
        if start >= end {
            return None;
        }
        let kind = match &self.kind {
            VmaKind::Anon => VmaKind::Anon,
            VmaKind::Frames {
                object,
                offset,
                shared,
            } => VmaKind::Frames {
                object: Arc::clone(object),
                offset: offset + (start - self.start),
                shared: *shared,
            },
        };
        Some(Vma {
            start,
            end,
            prot: self.prot,
            kind,
            name: self.name.clone(),
            sealed: self.sealed,
            locked: self.locked,
        })
    }

    /// Whether `next` continues this VMA with nothing in between: same
    /// permissions, same object, and - for a frame-backed one - the offset
    /// where this range's own arithmetic puts it.
    ///
    /// The seal is part of the identity: folding a sealed piece into an
    /// unsealed neighbour would either lose the seal or spread it over pages
    /// nobody sealed, so two pieces that disagree about it stay two lines.
    fn abuts(&self, next: &Vma) -> bool {
        if self.end != next.start
            || self.prot != next.prot
            || self.sealed != next.sealed
            || self.locked != next.locked
            || !same_name(&self.name, &next.name)
        {
            return false;
        }
        if !self.kind.same_object(&next.kind) {
            return false;
        }
        match (&self.kind, &next.kind) {
            (VmaKind::Anon, VmaKind::Anon) => true,
            (VmaKind::Frames { offset: here, .. }, VmaKind::Frames { offset: there, .. }) => {
                here.checked_add(next.start - self.start) == Some(*there)
            }
            _ => false,
        }
    }
}

/// The three absolute addresses of a loaded program image that
/// `/proc/<pid>/stat` reports as `startcode`, `endcode` and `startstack`, plus
/// the two page books those addresses split.
///
/// One value rather than three fields threaded separately: the loader is the
/// only place that knows the addresses, and every construction site that takes
/// them has to take all three or a process would report a coherent-looking mix
/// of two programs. Host-compiled so `/proc`'s rendering can be unit-tested
/// against values that came from a real load.
///
/// What each means, and the measurements behind each, is
/// [`crate::sched::Mm`]'s `start_code`/`end_code`/`start_stack`.
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub struct ImageBounds {
    pub start_code: usize,
    pub end_code: usize,
    pub start_stack: usize,
}

/// Which of `/proc/<pid>/status`'s `Vm*` books one mapping is counted in.
///
/// Linux keeps three page counters on the `mm` - `exec_vm`, `stack_vm`,
/// `data_vm` - and adds a mapping to exactly one of them, in that order of
/// preference (`mm/mmap.c`'s `vm_stat_account`, which is an `if`/`else if`
/// chain). The three predicates are `mm/vma.h`'s:
///
/// ```text
/// is_exec_mapping   (flags & (VM_EXEC | VM_WRITE | VM_STACK)) == VM_EXEC
/// is_stack_mapping  (flags & VM_STACK) == VM_STACK
/// is_data_mapping   (flags & (VM_WRITE | VM_SHARED | VM_STACK)) == VM_WRITE
/// ```
///
/// Read out of the order they are tested in, those say: **exec is read-and-
/// execute and neither writable nor a stack; stack is a stack; data is private
/// and writable and not a stack.** Two consequences are the ones this kernel
/// would get wrong by describing a mapping more loosely than Linux does:
/// an `r|w|x` mapping is *data*, not exec (the write bit disqualifies it), and
/// a writable *stack* mapping is `VmStk`, not `VmData`.
///
/// This kernel has no `VM_STACK`: the one stack that exists is the loader's,
/// and it is named `[stack]` in the range table - which is what this reads.
/// `MAP_STACK` is accepted and changes nothing here, which is Linux's answer
/// too: a `MAP_STACK` anonymous mapping measured on Linux 7.2.14 moved
/// `VmData` and left `VmStk` alone (`build/scratch/vmlines/c.c`).
///
/// `shared` is [`VmaKind::Frames`]'s bit for a VMA, and always `false` for a
/// [`MappedRange`] - those are the loader's and a `MAP_PRIVATE` file copy's,
/// and both are private by construction.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VmBook {
    Exec,
    Stack,
    Data,
    /// Counted in none of the three: a shared mapping (`VmData` excludes it),
    /// a read-only private one (`VmData` wants write), a writable one that is
    /// not executable (`VmExe`/`VmLib` want exec).
    Other,
}

/// Which book `status` counts a mapping in, from the three facts Linux's
/// predicates read. See [`VmBook`] for the rules and where they come from.
pub fn vm_book(prot: MapFlags, shared: bool, name: Option<&str>) -> VmBook {
    let stack = name == Some("[stack]");
    let write = prot.contains(MapFlags::WRITE);
    let exec = prot.contains(MapFlags::EXEC);
    if exec && !write && !stack {
        VmBook::Exec
    } else if stack {
        VmBook::Stack
    } else if write && !shared && !stack {
        VmBook::Data
    } else {
        VmBook::Other
    }
}

/// Whether two mappings carry the same name. Compared by *string* rather than
/// by pointer, because the names of two mappings of one file are built by two
/// separate resolutions of it - and a missed merge is a maps file with two
/// lines where Linux has one, which is worse than the pointer comparison
/// saving a byte-by-byte walk of a path.
fn same_name(a: &Option<Arc<str>>, b: &Option<Arc<str>>) -> bool {
    match (a, b) {
        (None, None) => true,
        (Some(a), Some(b)) => a == b,
        _ => false,
    }
}

/// The most descriptions one process's table may hold: Linux's own default
/// `/proc/sys/vm/max_map_count`, 65 530.
///
/// Measured on Linux 7.2.11 (`build/scratch/mmapcount/p1.c`): with the count at
/// the limit, one more `mmap` of a page answers `ENOMEM` (errno 12); `munmap`
/// one and the next `mmap` succeeds. The count, not memory, is the gate - which
/// is why the limit belongs to the table and not to the frame allocator.
///
/// The number is Linux's *default*; this host has it tuned up to 1 048 576, so
/// the pin claims the shape the kernel ships rather than the host's setting
/// (`docs/notes/` §10.11). It is the *live* ceiling's starting value: on Linux
/// the number is policy, `/proc/sys/vm/max_map_count`'s contents, so a write to
/// that file moves the gate every later `mmap` asks (item 137) rather than
/// being a second number nothing reads.
pub const DEFAULT_MAX_MAP_COUNT: usize = 65_530;

/// The live ceiling: `/proc/sys/vm/max_map_count`.
///
/// Measured on Linux 7.2.11 first (`build/scratch/sysctl/p2.c`, as root, the
/// host's sysctl restored afterwards): a write of `1000` was accepted and the
/// next `mmap` past 1 000 descriptions answered `ENOMEM`; a write of `10`
/// *below* the count already held was accepted too and the mappings already
/// made kept working; `abc`, `-1` and `2147483648` were each `EINVAL` and left
/// the value alone. So the file is the policy and the table is its answer.
static MAX_MAP_COUNT: AtomicUsize = AtomicUsize::new(DEFAULT_MAX_MAP_COUNT);

/// The number of descriptions a process may hold, as the sysctl holds it.
pub fn max_map_count() -> usize {
    MAX_MAP_COUNT.load(Ordering::Relaxed)
}

/// Move the ceiling, from a write to `/proc/sys/vm/max_map_count`.
pub fn set_max_map_count(count: usize) {
    MAX_MAP_COUNT.store(count, Ordering::Relaxed);
}

/// The process's VMAs, keyed by `start` and never overlapping.
///
/// A `BTreeMap` and not the sorted `Vec` this was, for a reason a measurement
/// gave rather than a preference: the table's two hot paths are a
/// *predecessor* query and an *insert*. Every page fault asks which
/// description covers an address (`lookup`), and every `mmap` asks where the
/// highest hole below the cursor is and then puts a mapping there. With a
/// `Vec` the lookup was a linear scan, and an insert re-sorted the whole
/// vector and rebuilt it through `split_out` - so a program mapping its way
/// down the arena was quadratic: 20 480 one-page mappings did not finish in
/// the gate's 120 s budget on x86_64 (`docs/notes/` §10.11), and Linux's own
/// default `max_map_count` (65 530) was unreachable in a program's lifetime.
///
/// A `BTreeMap` answers the lookup in `log n`, inserts in `log n` and moves
/// nothing - which matters because the placement is *top-down*, so every new
/// mapping is inserted at the front of address order, where a `Vec` pays a
/// whole-vector move (65 530 of them). It iterates in address order for
/// `/proc/<pid>/maps`, and it is `Clone`-able for `fork` just as the slice
/// was.
///
/// The invariant is the old `tidy`'s, now kept locally: entries stand in
/// address order (the map's own order), two entries never overlap, and
/// adjacent ranges that describe the same thing are folded by [`Self::merge_at`]
/// rather than by re-sorting the table.
#[derive(Clone, Default)]
pub struct VmaTable {
    vmas: BTreeMap<usize, Vma>,
}

impl VmaTable {
    pub fn new() -> VmaTable {
        VmaTable {
            vmas: BTreeMap::new(),
        }
    }

    /// The VMA covering `virt`, if any.
    ///
    /// One predecessor query: entries never overlap, so the only candidate is
    /// the last one starting at or below `virt`.
    pub fn lookup(&self, virt: usize) -> Option<&Vma> {
        let (_, vma) = self.vmas.range(..=virt).next_back()?;
        vma.covers(virt).then_some(vma)
    }

    /// Whether any VMA covers `virt`.
    pub fn covers(&self, virt: usize) -> bool {
        self.lookup(virt).is_some()
    }

    /// The table in address order - what `/proc/<pid>/maps` prints.
    pub fn iter(&self) -> impl Iterator<Item = &Vma> + '_ {
        self.vmas.values()
    }

    pub fn is_empty(&self) -> bool {
        self.vmas.is_empty()
    }

    pub fn len(&self) -> usize {
        self.vmas.len()
    }

    /// Put `vma` down, taking its whole range: whatever a previous VMA held
    /// inside `[vma.start, vma.end)` is removed (and split where it only
    /// partly overlaps), exactly as `MAP_FIXED` replaces what it covers.
    ///
    /// Adjacent ranges that describe the same object with the same permissions
    /// are then folded back together, so a heap grown a page at a time is one
    /// VMA and not a thousand.
    pub fn insert(&mut self, vma: Vma) {
        self.split_out(vma.start, vma.end);
        self.place(vma);
    }

    /// Claim a run of pages the caller has just *placed* - mapped eagerly or
    /// re-mapped after a move - next to descriptions that may still be lazy.
    ///
    /// [`insert`] is `MAP_FIXED`'s take-the-range semantics: folding a new run
    /// into an adjacent existing one goes through [`VmaTable::split_out`],
    /// which takes the old VMA out and puts it back, and the page-table half
    /// of that re-insert path frees every page inside the old description's
    /// range first. For a lazily-mapped neighbour whose frames have never
    /// arrived there is nothing in the tables to free and no frame to give
    /// anybody - but the *description* would be rewritten by the merge, and a
    /// caller like `brk`'s growth loop cannot tell "the hole beside me" from
    /// "somebody else's untouched mapping" without learning the machine's
    /// page-table internals (`NOTES.md` §15.18's measurement). So this puts
    /// the run down without touching anything already in the table; two plain
    /// anonymous runs that abut are still folded, because for `Anon` the merge
    /// is a statement about identity and not about placement.
    pub fn insert_present(&mut self, vma: Vma) {
        let anon = matches!(vma.kind, VmaKind::Anon);
        let key = vma.start;
        // The run is described as *placed*, which says nothing about what was
        // described there before: an overlapping description is superseded, and
        // leaving it in the table is how one range came to be printed twice by
        // `/proc/<pid>/maps` (measured on a guest: the same `rw-s` span above
        // its own line, so a first-match reader took the one with no object -
        // `docs/notes/` §6.68). Clipping here keeps the table a cover rather
        // than a stack, and drops no page: the caller has already dealt with
        // the frames of whatever it maps over, exactly as `MAP_FIXED` does
        // through `munmap` before it gets here.
        self.split_out(vma.start, vma.end);
        self.vmas.insert(key, vma);
        if anon {
            self.merge_at(key);
        }
    }

    /// Drop every VMA (or every part of one) inside `[start, end)`.
    pub fn remove_range(&mut self, start: usize, end: usize) {
        self.split_out(start, end);
    }

    /// Take the VMAs inside `[start, end)` out and hand them back, clipped to
    /// the range, **without** disturbing their pages.
    ///
    /// [`VmaTable::take_range`] is what `munmap`-style paths use, where the
    /// descriptions leaving the table also lose their frames. `mremap`'s move
    /// needs the other half only: the pages travel with the mapping and are
    /// un-mapped explicitly afterwards, so a description being lifted out must
    /// not free what it describes - a `PROT_NONE` page taken this way was
    /// previously handed back to the allocator while its PTE stayed live,
    /// which is exactly the shape §15.18 found on the stack region.
    pub fn lift_range(&mut self, start: usize, end: usize) -> Vec<Vma> {
        self.split_out(start, end)
    }

    /// Take the VMAs inside `[start, end)` out and hand them back, clipped to
    /// the range: what `mremap` needs to carry a mapping's backing with it.
    pub fn take_range(&mut self, start: usize, end: usize) -> Vec<Vma> {
        self.split_out(start, end)
    }

    /// `mseal(2)`: mark everything inside `[start, end)` sealed.
    ///
    /// Measured on Linux 7.2.11 (`build/scratch/mseal/probe4.c`), because the
    /// two edge answers are not the ones a reader guesses:
    ///
    /// * a span that is **not fully mapped** is `ENOMEM` - `mseal` refuses to
    ///   seal a hole, since a hole can be mapped later and the seal would then
    ///   have promised something it could not keep;
    /// * sealing an already-sealed range is `0`, not an error, so a caller may
    ///   repeat itself.
    ///
    /// The range is split at its ends exactly like `mprotect`'s, which is what
    /// makes "seal one page of four" leave three pages usable.
    pub fn seal_range(&mut self, start: usize, end: usize) -> Result<(), isize> {
        if start >= end {
            return Ok(());
        }
        // Every byte of the span has to be somebody's address. `gaps` answers
        // the holes, and the check is on the span as given - a hole *inside* it
        // refuses the whole call, which is the measured answer.
        if !self.gaps(start, end).is_empty() {
            return Err(crate::err::ENOMEM);
        }
        let mut inside = self.split_out(start, end);
        for vma in &mut inside {
            vma.sealed = true;
        }
        self.retake(inside);
        Ok(())
    }

    /// Mark whatever the table covers inside `[start, end)` sealed, splitting at
    /// the ends and **without** the "the whole span must be mapped" refusal.
    ///
    /// That refusal belongs to the caller that can see every description of a
    /// mapping: a span over the program image is fully mapped yet names no VMA,
    /// so the table alone cannot tell "nothing is there" from "it is another
    /// description's". [`VmaTable::seal_range`] is the whole-call form a table
    /// with no eager ranges beside it can answer; this is the half that marks.
    pub fn seal_present(&mut self, start: usize, end: usize) {
        if start >= end {
            return;
        }
        let mut inside = self.split_out(start, end);
        for vma in &mut inside {
            vma.sealed = true;
        }
        self.retake(inside);
    }

    /// Whether any part of `[start, end)` is sealed. Every operation `mseal`
    /// protects asks this *before* it changes anything, and the answer refuses
    /// the whole call rather than the sealed slice of it: a `munmap` that
    /// covers a sealed page and three unsealed ones is `EPERM` in full, which
    /// is what the host measured.
    pub fn sealed_within(&self, start: usize, end: usize) -> bool {
        if start >= end {
            // An empty span covers no byte, so no byte of it is sealed. The
            // `mseal`-protected operations all treat an empty range as "change
            // nothing", and a guard that answered `true` here would refuse a
            // `munmap(addr, 0)` that Linux accepts.
            return false;
        }
        self.overlapping(start, end).any(|v| v.sealed)
    }

    /// Whether the VMA covering `virt` is sealed. `madvise` asks per range
    /// because its rule is about the pages an advice would destroy.
    pub fn sealed_at(&self, virt: usize) -> bool {
        self.lookup(virt).is_some_and(|v| v.sealed)
    }

    /// Rewrite the permissions of everything already inside `[start, end)`.
    /// A range nothing is mapped in is left alone: `mprotect` changes what is
    /// there, it does not create.
    pub fn set_prot(&mut self, start: usize, end: usize, prot: MapFlags) {
        let mut inside = self.split_out(start, end);
        if inside.is_empty() {
            return;
        }
        for vma in &mut inside {
            vma.prot = prot;
        }
        self.retake(inside);
    }

    /// `mlock`/`munlock`: mark or unmark every description inside
    /// `[start, end)` as pinned, splitting at the ends exactly like
    /// `mprotect`'s so that locking one page of four leaves three unlocked.
    ///
    /// The bit is what `munlock` of a *sub*-range subtracts from: measured on
    /// a current host (`build/scratch/mlk/acct2.c`), 8 pages locked read
    /// `VmLck 32 kB` and `munlock(base + 2 pages, 3 pages)` leaves exactly
    /// `20 kB` - so the accounting is a count of marked pages, never a running
    /// total the caller hands in.
    pub fn set_locked(&mut self, start: usize, end: usize, locked: bool) {
        let mut inside = self.split_out(start, end);
        if inside.is_empty() {
            return;
        }
        for vma in &mut inside {
            vma.locked = locked;
        }
        self.retake(inside);
    }

    /// How many pages this table has marked as pinned - half of
    /// [`crate::sched::Mm`]'s `locked_pages`, the other half being the eager
    /// ranges.
    pub fn locked_pages(&self) -> usize {
        self.vmas
            .values()
            .filter(|vma| vma.locked)
            .map(|vma| (vma.end - vma.start).div_ceil(PAGE_SIZE))
            .sum()
    }

    /// How many pinned pages of this table lie inside `[start, end)`.
    ///
    /// The subtraction `do_mlock` makes before it compares against
    /// `RLIMIT_MEMLOCK`: a range already locked is not re-charged, which is why
    /// locking the same 8 pages twice reads `VmLck 32 kB` both times
    /// (`build/scratch/mlk/acct.c`).
    pub fn locked_pages_within(&self, start: usize, end: usize) -> usize {
        self.vmas
            .values()
            .filter(|vma| vma.locked)
            .map(|vma| {
                (vma.end
                    .min(end)
                    .saturating_sub(vma.start.max(start))
                    .div_ceil(PAGE_SIZE))
            })
            .filter(|pages| *pages > 0)
            .sum()
    }

    /// Mark every description as pinned - `mlockall(MCL_CURRENT)`.
    pub fn lock_all(&mut self) {
        for vma in self.vmas.values_mut() {
            vma.locked = true;
        }
    }

    /// Unpin every description without touching the geometry - what `fork`
    /// does to a copied table (`dup_mmap`'s `vma_clear_flags_mask(tmp,
    /// VMA_LOCKED_MASK)`) and what `munlockall` does here.
    pub fn clear_locks(&mut self) {
        for vma in self.vmas.values_mut() {
            vma.locked = false;
        }
    }

    /// `PR_SET_VMA_ANON_NAME`: give every anonymous mapping in `[start, end)`
    /// this name, or take theirs away with `None`.
    ///
    /// Measured on a current host (`.workbuddy/probe_vma_name.c`), because
    /// every half of the shape is a decision:
    ///
    /// * the range is **split** at its ends like `mprotect`'s, so naming one page
    ///   of a four-page mapping leaves two lines with the second named;
    /// * a range nothing is mapped in is `ENOMEM` (not `ENOENT`), and a range a
    ///   **file** mapping sits in is `EBADF` - the one errno a reader would not
    ///   guess from the man page's silence;
    /// * clearing the name folds the pieces back into one line, which is what
    ///   [`Self::merge_at`] does when two abutting pieces agree on the name.
    ///
    /// The name is stored already bracketed (`[anon:x]`, or `[anon:]` for an
    /// empty name, both as the host prints them), because the one reader is
    /// `/proc/<pid>/maps` and its name column is this string.
    pub fn set_anon_name(
        &mut self,
        start: usize,
        end: usize,
        name: Option<Arc<str>>,
    ) -> Result<(), isize> {
        let mut inside = self.split_out(start, end);
        if inside.is_empty() {
            return Err(crate::err::ENOMEM);
        }
        if inside.iter().any(|vma| !matches!(vma.kind, VmaKind::Anon)) {
            // Put back what the split took before refusing: the caller's range
            // is unchanged, which is what an error means here.
            self.retake(inside);
            return Err(crate::err::EBADF);
        }
        for vma in &mut inside {
            vma.name = name.clone();
        }
        self.retake(inside);
        Ok(())
    }

    /// The parts of `[start, end)` that no VMA covers, lowest first.
    ///
    /// This is what a heap growth asks before it extends: a page inside the
    /// range that something else already owns - musl's `PROT_NONE` guard over
    /// the first page of a fresh heap - must not be claimed by the growth.
    ///
    /// The sweep walks the overlapping descriptions in address order, which is
    /// the map's own order, so it cannot report one hole twice. (A `Vec` had to
    /// be sorted first, and an unsorted table broke the sweep rather than
    /// merely slowing it: a VMA that started above `at` yet ended below it
    /// pulled `at` backwards and the next pass reported the same hole again -
    /// how one mmap placement came to claim its gap twice with two overlapping
    /// descriptions, `NOTES.md` §15.18.)
    pub fn gaps(&self, start: usize, end: usize) -> Vec<(usize, usize)> {
        let mut gaps = Vec::new();
        let mut at = start;
        for vma in self.overlapping(start, end) {
            if vma.start > at {
                gaps.push((at, vma.start.min(end)));
            }
            at = at.max(vma.end);
            if at >= end {
                break;
            }
        }
        if at < end {
            gaps.push((at, end));
        }
        gaps
    }

    /// The entries that overlap `[start, end)`, in address order.
    ///
    /// The predecessor comes first when it reaches past `start`: `range` only
    /// yields keys at or above `start`, and an overlapping entry may begin
    /// below it.
    fn overlapping(&self, start: usize, end: usize) -> impl Iterator<Item = &Vma> + '_ {
        let first = self
            .vmas
            .range(..start)
            .next_back()
            .map(|(_, vma)| vma)
            .filter(move |vma| vma.end > start);
        first
            .into_iter()
            .chain(self.vmas.range(start..end).map(|(_, vma)| vma))
    }

    /// Put a run down in address order and fold it into whatever it continues.
    fn place(&mut self, vma: Vma) {
        let key = vma.start;
        self.vmas.insert(key, vma);
        self.merge_at(key);
    }

    /// Put split pieces back: each may now continue a neighbour, so each is
    /// merged on the way in.
    fn retake(&mut self, inside: Vec<Vma>) {
        for vma in inside {
            self.place(vma);
        }
    }

    /// Fold the entry at `key` into its neighbours for as long as they
    /// describe the same thing.
    ///
    /// This is the old `tidy`'s merge, one entry at a time: the table is
    /// already in address order, so only the two entries beside the one just
    /// written can have started to abut. The loop repeats because folding
    /// downward can make the survivor abut the next entry up.
    fn merge_at(&mut self, mut key: usize) {
        loop {
            let Some(vma) = self.vmas.get(&key) else {
                return;
            };
            let (start, end) = (vma.start, vma.end);
            // Copy the answers out before touching the map: `abuts` borrows
            // both entries and the fold below needs the map mutably.
            let below = self
                .vmas
                .range(..start)
                .next_back()
                .map(|(&k, prev)| (k, prev.abuts(vma)));
            let above = self
                .vmas
                .range(end..)
                .next()
                .map(|(&k, next)| (k, vma.abuts(next)));
            if let Some((below_key, true)) = below {
                let end = self.vmas[&key].end;
                self.vmas.remove(&key);
                if let Some(prev) = self.vmas.get_mut(&below_key) {
                    prev.end = end;
                }
                key = below_key;
                continue;
            }
            if let Some((above_key, true)) = above {
                let above_end = self.vmas[&above_key].end;
                self.vmas.remove(&above_key);
                if let Some(cur) = self.vmas.get_mut(&key) {
                    cur.end = above_end;
                }
                continue;
            }
            return;
        }
    }

    /// Take the pieces of every VMA inside `[start, end)` out of the table and
    /// return them; what is left stays, split at the boundaries as required.
    ///
    /// The no-overlap case - an `mmap` placement into a hole - is the common
    /// one and returns before allocating anything.
    fn split_out(&mut self, start: usize, end: usize) -> Vec<Vma> {
        if start >= end {
            return Vec::new();
        }
        let keys: Vec<usize> = self.overlapping(start, end).map(|vma| vma.start).collect();
        if keys.is_empty() {
            return Vec::new();
        }
        let mut taken = Vec::with_capacity(keys.len());
        for key in keys {
            let Some(vma) = self.vmas.remove(&key) else {
                continue;
            };
            if let Some(left) = vma.sliced(vma.start, start) {
                self.vmas.insert(left.start, left);
            }
            if let Some(middle) = vma.sliced(start, end) {
                taken.push(middle);
            }
            if let Some(right) = vma.sliced(end, vma.end) {
                self.vmas.insert(right.start, right);
            }
        }
        taken.sort_unstable_by_key(|vma| vma.start);
        taken
    }
}

#[cfg(all(test, not(target_os = "none")))]
mod tests {
    use super::*;
    // The host build's `MMAP_BASE` is the x86_64 one, which is what the
    // placement arithmetic below is measured against (`mm::elf`'s doc).
    use crate::mm::elf::MMAP_BASE;

    /// A stand-in backing object.
    ///
    /// It knows nothing about files, which is the point of the seam: the tests
    /// care about *identity* - which object a mapping is of - and identity is
    /// now something this file defines rather than something it borrows from
    /// the VFS. Before V-13 was closed these tests built a fake `vfs::Inode`,
    /// which is exactly the dependency the trait exists to remove.
    struct Fake {
        ino: u64,
    }

    impl MappedObject for Fake {
        fn frame_at(&self, _index: u64) -> Option<usize> {
            None
        }

        fn identity(&self) -> ((u32, u32), u64) {
            ((0, 0), self.ino)
        }
    }

    fn anon(start: usize, end: usize, prot: MapFlags) -> Vma {
        Vma::new(start, end, prot, VmaKind::Anon)
    }

    /// One backing object, so that two VMAs over it can be the *same* object
    /// the way two mappings of one descriptor are.
    fn fake(ino: u64) -> Arc<dyn MappedObject> {
        Arc::new(Fake { ino })
    }

    fn frames(
        start: usize,
        end: usize,
        prot: MapFlags,
        object: &Arc<dyn MappedObject>,
        offset: usize,
    ) -> Vma {
        Vma::new(
            start,
            end,
            prot,
            VmaKind::Frames {
                object: Arc::clone(object),
                offset,
                shared: true,
            },
        )
    }

    const RW: MapFlags = MapFlags::READ.union(MapFlags::WRITE).union(MapFlags::USER);

    #[test]
    fn lookup_finds_the_covering_area_and_nothing_else() {
        let mut table = VmaTable::new();
        table.insert(anon(0x1000, 0x3000, RW));
        assert!(table.covers(0x1000));
        assert!(table.covers(0x2fff));
        assert!(!table.covers(0x3000), "the end is not covered");
        assert!(!table.covers(0x0fff), "the start is not covered");
    }

    #[test]
    fn a_later_insert_replaces_exactly_its_own_range() {
        let mut table = VmaTable::new();
        table.insert(anon(0x1000, 0x5000, RW));
        table.insert(anon(0x2000, 0x3000, MapFlags::READ | MapFlags::USER));

        assert_eq!(
            table.len(),
            3,
            "the middle page is its own, and splits the rest"
        );
        assert_eq!(table.lookup(0x1fff).unwrap().prot, RW);
        assert_eq!(
            table.lookup(0x2000).unwrap().prot,
            MapFlags::READ | MapFlags::USER
        );
        assert_eq!(table.lookup(0x4000).unwrap().prot, RW);
    }

    #[test]
    fn adjacent_areas_of_one_object_merge_and_dissimilar_ones_do_not() {
        let object = fake(7);
        let mut table = VmaTable::new();
        table.insert(frames(0x1000, 0x2000, RW, &object, 0));
        table.insert(frames(0x2000, 0x3000, RW, &object, 0x1000));
        assert_eq!(table.len(), 1, "the same object, offset continuing");
        assert_eq!(table.iter().next().unwrap().end, 0x3000);

        // A different object, the same permissions.
        table.insert(frames(0x3000, 0x4000, RW, &fake(8), 0x2000));
        assert_eq!(table.len(), 2, "a different object is a different area");

        // The same object, but the offset does not continue.
        let mut table = VmaTable::new();
        table.insert(frames(0x1000, 0x2000, RW, &object, 0));
        table.insert(frames(0x2000, 0x3000, RW, &object, 0x9000));
        assert_eq!(table.len(), 2, "a gap in the object is a gap in the list");
    }

    #[test]
    fn one_object_is_recognised_across_two_handles_to_it() {
        // The rule V-13 changed, pinned as the rule it is now: two mappings are
        // of one object when the object *says so*, not when their `Arc`s are the
        // same pointer. Two `Arc`s with the same identity are two handles to one
        // thing - which is what a `memfd` reached through two descriptors looks
        // like - and they merge; two identities do not, however equal the offsets
        // line up. The old pointer comparison could not be asked across the trait
        // boundary, and this is the fact it was standing in for.
        let mut table = VmaTable::new();
        table.insert(frames(0x1000, 0x2000, RW, &fake(7), 0));
        table.insert(frames(0x2000, 0x3000, RW, &fake(7), 0x1000));
        assert_eq!(
            table.len(),
            1,
            "one object through two handles is one area, as Linux prints it"
        );

        let mut table = VmaTable::new();
        table.insert(frames(0x1000, 0x2000, RW, &fake(7), 0));
        table.insert(frames(0x2000, 0x3000, RW, &fake(8), 0x1000));
        assert_eq!(table.len(), 2, "and two objects are two areas");
    }

    #[test]
    fn a_split_keeps_the_frame_offset_with_its_address() {
        let object = fake(7);
        let mut table = VmaTable::new();
        table.insert(frames(0x1000, 0x4000, RW, &object, 0x8000));

        // Replace the middle page: the two halves must each keep the offset
        // their own start corresponds to.
        table.insert(anon(0x2000, 0x3000, MapFlags::READ | MapFlags::USER));

        let low = table.lookup(0x1000).unwrap();
        let high = table.lookup(0x3000).unwrap();
        match (&low.kind, &high.kind) {
            (VmaKind::Frames { offset: lo, .. }, VmaKind::Frames { offset: hi, .. }) => {
                assert_eq!(*lo, 0x8000);
                assert_eq!(*hi, 0xa000, "the upper half moved with its address");
            }
            _ => panic!("the halves are no longer frame-backed"),
        }
    }

    #[test]
    fn a_shared_bit_the_two_halves_do_not_agree_on_is_two_areas() {
        let object = fake(7);
        let mut table = VmaTable::new();
        table.insert(frames(0x1000, 0x2000, RW, &object, 0));
        // The same object and the same offset continuation, but asked for
        // privately: a `memfd` mapped `MAP_PRIVATE` aliases the frames and is
        // still not the mapping its neighbour is.
        table.insert(Vma::new(
            0x2000,
            0x3000,
            RW,
            VmaKind::Frames {
                object: Arc::clone(&object),
                offset: 0x1000,
                shared: false,
            },
        ));
        assert_eq!(table.len(), 2, "a private mapping is its own area");
    }

    #[test]
    fn a_name_splits_an_area_and_a_shared_one_folds_it_back() {
        let mut table = VmaTable::new();
        table.insert(anon(0x1000, 0x2000, RW).named(Arc::from("/bin/one")));
        table.insert(anon(0x2000, 0x3000, RW).named(Arc::from("/bin/two")));

        assert_eq!(table.len(), 2, "two names, two lines");

        // The same name reaches back over the split, because the two halves
        // describe one thing: this is what makes a file mapped in two pieces
        // by one caller print as one line, as Linux prints it.
        table.insert(anon(0x2000, 0x3000, RW).named(Arc::from("/bin/one")));
        assert_eq!(table.len(), 1);
        assert_eq!(table.iter().next().unwrap().end, 0x3000);

        let mut named_and_not = VmaTable::new();
        named_and_not.insert(anon(0x1000, 0x2000, RW).named(Arc::from("/bin/one")));
        named_and_not.insert(anon(0x2000, 0x3000, RW));
        assert_eq!(
            named_and_not.len(),
            2,
            "a name and no name are not one area"
        );
    }

    #[test]
    fn a_split_keeps_the_name_on_both_halves() {
        let mut table = VmaTable::new();
        table.insert(anon(0x1000, 0x4000, RW).named(Arc::from("[heap]")));
        table.insert(anon(0x2000, 0x3000, MapFlags::READ | MapFlags::USER));

        let low = table.lookup(0x1000).unwrap();
        let high = table.lookup(0x3000).unwrap();
        assert_eq!(low.name.as_deref(), Some("[heap]"));
        assert_eq!(high.name.as_deref(), Some("[heap]"));
        assert_eq!(table.lookup(0x2000).unwrap().name, None);
    }

    #[test]
    fn remove_range_leaves_the_ends_and_takes_the_middle() {
        let mut table = VmaTable::new();
        table.insert(anon(0x1000, 0x6000, RW));
        table.remove_range(0x2000, 0x5000);

        assert_eq!(table.len(), 2, "the two ends survive as separate areas");
        assert_eq!(table.lookup(0x1000).unwrap().end, 0x2000);
        assert_eq!(table.lookup(0x5000).unwrap().start, 0x5000);
        assert!(!table.covers(0x3000));
    }

    #[test]
    fn take_range_hands_back_what_it_removed() {
        let object = fake(7);
        let mut table = VmaTable::new();
        table.insert(frames(0x1000, 0x5000, RW, &object, 0x1000));
        let taken = table.take_range(0x2000, 0x4000);
        assert_eq!(taken.len(), 1);
        assert_eq!((taken[0].start, taken[0].end), (0x2000, 0x4000));
        match &taken[0].kind {
            VmaKind::Frames { offset, .. } => {
                assert_eq!(*offset, 0x2000, "the offset came with the piece")
            }
            _ => panic!("the piece lost its backing"),
        }
        assert_eq!(table.len(), 2, "the rest stayed behind");
    }

    #[test]
    fn set_prot_touches_only_what_is_already_there() {
        let mut table = VmaTable::new();
        table.insert(anon(0x1000, 0x3000, RW));
        table.set_prot(0x0000, 0x8000, MapFlags::READ | MapFlags::USER);

        assert_eq!(table.len(), 1);
        assert_eq!(
            table.lookup(0x2000).unwrap().prot,
            MapFlags::READ | MapFlags::USER
        );

        // Nothing is created by protecting a range nothing lives in.
        let mut empty = VmaTable::new();
        empty.set_prot(0x1000, 0x2000, RW);
        assert!(empty.is_empty());
    }

    #[test]
    fn set_prot_splits_so_the_untouched_part_keeps_its_bits() {
        let mut table = VmaTable::new();
        table.insert(anon(0x1000, 0x5000, RW));
        table.set_prot(0x2000, 0x3000, MapFlags::READ | MapFlags::USER);

        assert_eq!(table.len(), 3);
        assert_eq!(table.lookup(0x1000).unwrap().prot, RW);
        assert_eq!(
            table.lookup(0x2000).unwrap().prot,
            MapFlags::READ | MapFlags::USER
        );
        assert_eq!(table.lookup(0x4000).unwrap().prot, RW);
    }

    #[test]
    fn gaps_report_what_growth_may_claim() {
        let mut table = VmaTable::new();
        table.insert(anon(0x1000, 0x2000, MapFlags::READ | MapFlags::USER));
        table.insert(anon(0x3000, 0x4000, RW));

        assert_eq!(
            table.gaps(0x1000, 0x4000),
            alloc::vec![(0x2000, 0x3000)],
            "only the hole between the two"
        );
        assert_eq!(table.gaps(0x0000, 0x2000), alloc::vec![(0x0000, 0x1000)]);
        assert!(table.gaps(0x1000, 0x2000).is_empty());
    }

    #[test]
    fn gaps_are_one_hole_per_hole_whatever_order_the_runs_arrive() {
        // The sweep walks the descriptions in address order and moves its
        // cursor forward only. Arrival order must not matter: a *file-backed*
        // run claimed beside an existing one through `insert_present` arrives
        // later than a lower address, and a table that reported the same hole
        // twice would let a caller that claims every gap it is handed place two
        // overlapping descriptions over one range (`NOTES.md` §15.18). The map
        // keeps the order the `Vec` had to be sorted back into.
        let object = fake(7);
        let mut table = VmaTable::new();
        // The higher address first, through the door that does not split.
        table.insert_present(frames(0x3000, 0x4000, RW, &object, 0));
        table.insert_present(anon(0x1000, 0x2000, RW));

        assert_eq!(
            table.gaps(0x1000, 0x4000),
            alloc::vec![(0x2000, 0x3000)],
            "one hole, reported once, whatever order the runs arrived in"
        );
    }

    /// The first placement `free_range` makes for a process whose mmap cursor
    /// still sits at the base: downward from `MMAP_BASE`, one page, taking the
    /// highest run that fits below it. This is that walk, verbatim - the loop
    /// lives beside it in `src/syscall/linux/proc.rs::free_range`, and replaying
    /// it here (rather than re-deriving its arithmetic) is what lets a test pin
    /// the *answer* instead of a transcription of it (`NOTES.md` §15.18).
    ///
    /// Verbatim in the two places the step is easy to get wrong, both of them
    /// the difference between this and a walk that never leaves the page under
    /// the cursor. The clamp opens it: a cursor above the base descends from the
    /// base. And each pass asks for the whole of `floor..cursor`, not
    /// `floor..floor + len` - `sched::vma_gaps(virt, len)` takes a *length* and
    /// asks `Mm::vma_gaps(virt, virt + len)`, so a length handed to
    /// [`VmaTable::gaps`], whose second argument is an end, would ask about
    /// `floor..len`, an address range of a few pages near zero.
    fn free_range_walk(table: &VmaTable, len: usize, cursor: usize) -> Option<usize> {
        let page = 0x1000usize;
        let mut top = cursor.min(MMAP_BASE);
        loop {
            let floor = top.checked_sub(len)? & !(page - 1);
            let window = table.gaps(floor, floor + cursor.saturating_sub(floor));
            for (hole_start, hole_end) in window {
                let start = hole_start.next_multiple_of(page);
                if start.checked_add(len).is_some_and(|end| end <= hole_end) {
                    return Some(start);
                }
            }
            top = floor;
        }
    }

    /// `src/syscall/linux/proc.rs::handle_mmap_impl`'s placement rules,
    /// replayed against the real table for the sequence `t-mmap` runs: the
    /// arena's lazy `PROT_NONE` span, then a hint over an address the arena
    /// already describes, and a `MAP_FIXED_NOREPLACE` onto one. Each rule is
    /// one arm of that match; this pins what the arms answer, so a
    /// kernel-side change to either predicate says so here rather than in a
    /// guest log (`NOTES.md` §15.18).
    #[test]
    fn mmap_placement_rules_over_a_lazy_arena() {
        let page = 0x1000usize;
        // The arena `t-mmap` reserves, put where the kernel really puts it: the
        // walk places every fresh mapping *below* the base, so a 2 MiB arena is
        // at `MMAP_BASE - 2 MiB` and the very first placement is made on top of
        // it.
        let arena = MMAP_BASE - 0x20_0000;
        let mut table = VmaTable::new();
        // The fixture: a private PROT_NONE span nobody has touched. Every
        // address in it is claimed by a description with no frames behind it.
        table.insert(Vma::new(arena, MMAP_BASE, MapFlags::empty(), VmaKind::Anon));

        // A hint inside the arena is not free of descriptions, so
        // `handle_mmap_impl` declines it and relocates - `vma_range_free` is
        // false over this range, which is the only thing the arm asks. The
        // *walk* is the arena's other half: a lazy hole has no frames either,
        // and the span is skipped because of the descriptions, not the pages
        // (measured against real Linux: a hint inside a lazily-mapped span is
        // relocated, exactly as one over a populated page is).
        let hint = arena + 0x2_00;
        assert!(table.covers(hint));
        assert!(table.gaps(hint, hint + page).is_empty());

        // `t-mmap`'s busy-hint fixture: a page taken `MAP_FIXED` inside the
        // arena, exactly as the test's owner does. The hint itself stays a
        // description-only claim, because `MAP_FIXED` takes the range and
        // records it fresh - what a hint over it answers is the relocation
        // above, not an error.
        table.insert(Vma::named(
            Vma::new(arena + 0x22_00, arena + 0x23_00, RW, VmaKind::Anon),
            None::<alloc::sync::Arc<str>>,
        ));
        assert!(table.covers(arena + 0x22_00));

        // NOREPLACE's gate is the same description question the hint asks:
        // the second half of the arena is still covered, even untouched.
        assert!(table.covers(arena + 0x30_00));

        // The relocation walk, replayed faithfully against this table. The
        // cursor a fresh process brings is the base itself, and the page under
        // it is the top of the arena: described, and with no frame behind it.
        assert!(
            table.gaps(MMAP_BASE - page, MMAP_BASE).is_empty(),
            "no gap within one page of the cursor"
        );
        // So the walk is turned away by the descriptions alone, and the whole
        // span - 512 pages of nothing but `PROT_NONE` claims - is skipped: the
        // first placement lands below the arena rather than inside it. A walk
        // that asked the *frames* would have answered `MMAP_BASE - page`, the
        // arena's own top page, and that is the collision §15.18 measured
        // (`NOTES.md` §15.18).
        assert_eq!(
            free_range_walk(&table, page, MMAP_BASE),
            Some(arena - page),
            "the first placement goes below the untouched arena, not into it"
        );
        // And the cursor follows it down: the next placement continues from the
        // bottom of the last one, which is the direction the region grows in.
        assert_eq!(
            free_range_walk(&table, page, arena - page),
            Some(arena - 2 * page),
            "the next placement goes just below the previous one"
        );
    }
    /// `mseal(2)` on a span that is fully mapped: the covered pages become
    /// sealed and the mapping splits so the pages outside stay usable -
    /// measured on Linux 7.2.11 (`build/scratch/mseal/probe7.c`): a four-page
    /// mapping whose first page is sealed still gives up pages 1..4 to
    /// `munmap`, and a `munmap` that touches the sealed page is `EPERM`.
    #[test]
    fn a_seal_splits_the_area_at_its_ends_and_marks_only_what_it_covers() {
        let mut table = VmaTable::new();
        table.insert(anon(0x1000, 0x5000, RW));

        assert_eq!(table.seal_range(0x2000, 0x4000), Ok(()));
        assert_eq!(table.len(), 3, "the two unsealed ends are their own areas");
        assert!(table.sealed_at(0x2000));
        assert!(table.sealed_at(0x3fff));
        assert!(!table.sealed_at(0x1000), "the head was not asked for");
        assert!(!table.sealed_at(0x4000), "the tail was not asked for");
        assert!(table.sealed_within(0x3fff, 0x4000));
        assert!(!table.sealed_within(0x4000, 0x5000));
        assert!(
            !table.sealed_within(0x3000, 0x3000),
            "an empty span names no sealed page"
        );
    }

    /// A span with a hole in it is refused in full: `mseal` may not promise
    /// anything about an address nothing owns, because a later `mmap` there
    /// would inherit a seal nobody asked for. The measured answer is `ENOMEM`
    /// (`build/scratch/mseal/probe4.c`), the same one an unmapped address gives.
    #[test]
    fn a_seal_over_a_hole_is_enomem_and_changes_nothing() {
        let mut table = VmaTable::new();
        table.insert(anon(0x1000, 0x3000, RW));

        assert_eq!(
            table.seal_range(0x1000, 0x4000),
            Err(crate::err::ENOMEM),
            "the hole at 0x3000..0x4000 refuses the whole call"
        );
        assert_eq!(table.len(), 1, "a refused seal does not split anything");
        assert!(!table.sealed_at(0x1000));
        assert_eq!(table.seal_range(0x1000, 0x3000), Ok(()));
        assert!(table.sealed_at(0x1fff));
    }

    /// Sealing twice is not an error: a caller that seals its whole image and
    /// then seals one page again gets `0` both times (measured).
    #[test]
    fn sealing_a_sealed_page_again_answers_ok() {
        let mut table = VmaTable::new();
        table.insert(anon(0x1000, 0x3000, RW));
        assert_eq!(table.seal_range(0x1000, 0x3000), Ok(()));
        assert_eq!(table.seal_range(0x1000, 0x3000), Ok(()));
        assert_eq!(table.seal_range(0x2000, 0x3000), Ok(()));
        assert!(table.sealed_at(0x1000));
        assert!(table.sealed_at(0x2000));
    }

    /// A sealed piece stays a piece: it must not fold back into an unsealed
    /// neighbour, or the seal would either vanish or spread over pages nobody
    /// sealed. That is why `abuts` compares the bit.
    #[test]
    fn a_sealed_piece_does_not_fold_into_an_unsealed_neighbour() {
        let mut table = VmaTable::new();
        // Two insertions that would merge without the seal.
        table.insert(anon(0x1000, 0x2000, RW));
        table.insert(anon(0x2000, 0x3000, RW));
        assert_eq!(table.len(), 1, "the unsealed pair is one area");

        let mut table = VmaTable::new();
        table.insert(anon(0x1000, 0x2000, RW));
        table.insert(anon(0x2000, 0x3000, RW));
        assert_eq!(table.seal_range(0x1000, 0x2000), Ok(()));
        assert_eq!(
            table.len(),
            2,
            "the sealed page and its neighbour are two areas"
        );
        assert!(table.sealed_at(0x1000));
        assert!(!table.sealed_at(0x2000));
    }

    /// The table's entries with their keys, for the invariant test above.
    fn table_keys(table: &VmaTable) -> Vec<(usize, Vma)> {
        table
            .vmas
            .iter()
            .map(|(&key, vma)| (key, vma.clone()))
            .collect()
    }

    const PAGE: usize = 4096;

    /// The table's shape invariant, checked after every mutation of a
    /// pseudo-random sequence: each entry's key is its own `start`, entries
    /// stand in address order without overlap, and each one is described once.
    ///
    /// The invariant is what `/proc/<pid>/maps` reads: the file prints one line
    /// per entry, so a table holding two descriptions of one range prints that
    /// range twice - which happened on a guest, intermittently, and could not be
    /// reproduced (`docs/notes/` §6.68). This pins the invariant the renderer
    /// assumes against the mutation shapes the table actually sees - `insert`
    /// (with its split-and-merge), `insert_present` (which must not fold a lazy
    /// neighbour), and `remove_range`.
    #[test]
    fn every_description_is_keyed_by_its_own_start_and_stands_alone() {
        let object = fake(7);
        let mut table = VmaTable::new();
        let mut seed = 0x1234_5678_9abc_def0u64;
        let mut next = |bound: usize| {
            seed = seed
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            ((seed >> 33) as usize) % bound
        };

        for step in 0..4000 {
            let page = next(16) + 1;
            let start = MMAP_BASE + page * PAGE;
            let pages = next(6) + 1;
            let end = start + pages * PAGE;
            let op = next(4);
            let cut = start + next(pages + 1) * PAGE;
            match op {
                0 => table.insert(anon(start, end, RW)),
                1 => table.insert(frames(start, end, RW, &object, page * PAGE)),
                2 => table.insert_present(anon(start, end, RW)),
                _ => table.remove_range(start, cut),
            }
            let mut last_end = 0;
            for (key, vma) in self::table_keys(&table) {
                assert_eq!(key, vma.start, "an entry's key is not its own start");
                assert!(vma.start < vma.end, "an entry describes an empty range");
                if vma.start < last_end {
                    panic!(
                        "two descriptions overlap at step {step} (op {op}, {start:x}-{end:x}, cut {cut:x}): {:?}",
                        self::table_keys(&table)
                            .iter()
                            .map(|(k, v)| (*k, v.start, v.end))
                            .collect::<alloc::vec::Vec<_>>()
                    );
                }
                last_end = vma.end;
            }
        }
    }

    /// Lifting a range out and putting the pieces back describes the same
    /// address space: what `mremap` and `mseal`'s split-and-retake do.
    #[test]
    fn a_lift_and_retake_round_trip_keeps_the_cover() {
        let mut table = VmaTable::new();
        table.insert(anon(MMAP_BASE, MMAP_BASE + 4 * PAGE, RW));
        let taken = table.lift_range(MMAP_BASE + PAGE, MMAP_BASE + 3 * PAGE);
        assert_eq!(taken.len(), 1);
        assert_eq!(
            table.gaps(MMAP_BASE, MMAP_BASE + 4 * PAGE),
            alloc::vec![(MMAP_BASE + PAGE, MMAP_BASE + 3 * PAGE)]
        );
        for vma in taken {
            table.insert_present(vma);
        }
        assert!(table.gaps(MMAP_BASE, MMAP_BASE + 4 * PAGE).is_empty());
    }

    /// The three books `status` counts into, against the mapping shapes Linux
    /// was measured on (`build/scratch/vmlines/b.c`, `c.c`, `f.c`).
    #[test]
    /// The pin is a bit on the description, and the count is recounted from
    /// the bits - so locking a range twice charges it once, and a `munlock` of
    /// a sub-range takes off exactly that sub-range's pages.
    ///
    /// The numbers are Linux's, measured on 7.2.14 with a 32-page limit
    /// (`build/scratch/mlk/acct2.c`): 8 locked pages read `VmLck 32 kB`, a
    /// `munlock` of pages 2..5 takes it to `20 kB`, and the same `munlock`
    /// again leaves it at `20`.
    #[test]
    fn locking_a_range_charges_its_pages_once_and_unlocking_takes_them_off() {
        let mut table = VmaTable::new();
        let base = 0x1000_0000;
        table.insert(Vma::new(
            base,
            base + 64 * 4096,
            MapFlags::READ | MapFlags::WRITE,
            VmaKind::Anon,
        ));
        assert_eq!(table.locked_pages(), 0, "nothing is pinned to begin with");
        table.set_locked(base, base + 8 * 4096, true);
        assert_eq!(table.locked_pages(), 8, "8 pages at 4 KiB each is 32 kB");
        table.set_locked(base, base + 8 * 4096, true);
        assert_eq!(
            table.locked_pages(),
            8,
            "locking the same 8 pages again charges nothing"
        );
        table.set_locked(base + 2 * 4096, base + 5 * 4096, false);
        assert_eq!(
            table.locked_pages(),
            5,
            "a sub-range munlock takes off exactly its own 3 pages"
        );
        table.set_locked(base + 2 * 4096, base + 5 * 4096, false);
        assert_eq!(
            table.locked_pages(),
            5,
            "and unlocking them again is a no-op"
        );
        assert_eq!(
            table.locked_pages_within(base, base + 8 * 4096),
            5,
            "the pages already pinned inside a range are the ones do_mlock subtracts"
        );
        table.clear_locks();
        assert_eq!(
            table.locked_pages(),
            0,
            "fork's copy starts with no page pinned and no accounting"
        );
    }

    fn a_mappings_book_is_the_one_linuxs_three_predicates_pick() {
        let rw = MapFlags::READ | MapFlags::WRITE;
        let rx = MapFlags::READ | MapFlags::EXEC;
        let rwx = MapFlags::READ | MapFlags::WRITE | MapFlags::EXEC;
        let ro = MapFlags::READ;
        // Private writable memory is data; a private read-only mapping is none
        // of the three (it moved no `Vm*` line at all).
        assert_eq!(vm_book(rw, false, None), VmBook::Data);
        assert_eq!(vm_book(ro, false, None), VmBook::Other);
        // Executable and not writable is exec, whatever backs it: Linux moved
        // `VmLib` for an anonymous `PROT_READ|PROT_EXEC` mapping.
        assert_eq!(vm_book(rx, false, None), VmBook::Exec);
        // ...and writable beats executable, so `rwx` is data, not exec.
        assert_eq!(vm_book(rwx, false, None), VmBook::Data);
        // A shared mapping is in neither data nor exec: `VmData` wants private,
        // and the measurements moved `RssShmem` instead. `VmExe`/`VmLib` have
        // no shared rule at all - a shared `r-x` mapping of a memfd moved
        // `VmLib` (`build/scratch/vmlines/b.c`), which is what the exec
        // predicate's mask (`EXEC|WRITE|STACK`) says by omitting `SHARED`.
        assert_eq!(vm_book(rw, true, None), VmBook::Other);
        assert_eq!(vm_book(rx, true, None), VmBook::Exec);
        // The stack is its own book, and it does not also count as data even
        // though it is private and writable.
        assert_eq!(vm_book(rw, false, Some("[stack]")), VmBook::Stack);
        assert_eq!(vm_book(rwx, false, Some("[stack]")), VmBook::Stack);
        // A name that is not the stack's decides nothing.
        assert_eq!(vm_book(rw, false, Some("/lib/libc.so")), VmBook::Data);
    }
}
