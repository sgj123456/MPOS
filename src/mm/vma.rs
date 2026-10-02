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
//! A sorted `Vec` and not a tree, for the same reason [`crate::vfs::locks`] and
//! [`crate::vfs::inotify`] are what they are: a process holds tens of these,
//! not thousands, and a plain slice keeps the type `Clone`-able (so `fork` is a
//! copy) and testable on the host with no page tables anywhere in sight. The
//! decisions that matter - which VMA covers an address, and what a
//! map/unmap/protect/split does to the list - are pure arithmetic over ranges,
//! so they are pinned by this file's own host tests rather than only from
//! inside the machine.

use alloc::sync::Arc;
use alloc::vec::Vec;

use crate::mm::vm::MapFlags;

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
/// file mappings keep anywhere. That is a named gap in `PLAN.md` §7.4, not a
/// hidden one.
#[derive(Clone)]
pub struct MappedRange {
    pub start: usize,
    pub end: usize,
    pub prot: MapFlags,
    pub name: Option<Arc<str>>,
}

impl MappedRange {
    /// Whether `next` continues this range, so a segment list can be reported
    /// as the runs a reader expects rather than one line per ELF segment.
    fn abuts(&self, next: &MappedRange) -> bool {
        self.end == next.start && self.prot == next.prot && same_name(&self.name, &next.name)
    }
}

/// Fold a load's range list into the runs a maps line describes: adjacent
/// pieces with the same permissions and the same name are one range. An
/// image's text segment is usually two of them (`r--` then `r-x`) and its data
/// two more, which is exactly how Linux prints them.
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
        })
    }

    /// Whether `next` continues this VMA with nothing in between: same
    /// permissions, same object, and - for a frame-backed one - the offset
    /// where this range's own arithmetic puts it.
    fn abuts(&self, next: &Vma) -> bool {
        if self.end != next.start || self.prot != next.prot || !same_name(&self.name, &next.name) {
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

/// The process's VMAs, sorted by `start` and never overlapping.
#[derive(Clone, Default)]
pub struct VmaTable {
    vmas: Vec<Vma>,
}

impl VmaTable {
    pub fn new() -> VmaTable {
        VmaTable { vmas: Vec::new() }
    }

    /// The VMA covering `virt`, if any.
    pub fn lookup(&self, virt: usize) -> Option<&Vma> {
        self.vmas.iter().find(|v| v.covers(virt))
    }

    /// Whether any VMA covers `virt`.
    pub fn covers(&self, virt: usize) -> bool {
        self.lookup(virt).is_some()
    }

    pub fn iter(&self) -> core::slice::Iter<'_, Vma> {
        self.vmas.iter()
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
        self.vmas.push(vma);
        self.tidy();
    }

    /// Drop every VMA (or every part of one) inside `[start, end)`.
    pub fn remove_range(&mut self, start: usize, end: usize) {
        self.split_out(start, end);
    }

    /// Take the VMAs inside `[start, end)` out and hand them back, clipped to
    /// the range: what `mremap` needs to carry a mapping's backing with it.
    pub fn take_range(&mut self, start: usize, end: usize) -> Vec<Vma> {
        self.split_out(start, end)
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
        self.vmas.append(&mut inside);
        self.tidy();
    }

    /// The parts of `[start, end)` that no VMA covers, lowest first.
    ///
    /// This is what a heap growth asks before it extends: a page inside the
    /// range that something else already owns - musl's `PROT_NONE` guard over
    /// the first page of a fresh heap - must not be claimed by the growth.
    pub fn gaps(&self, start: usize, end: usize) -> Vec<(usize, usize)> {
        let mut gaps = Vec::new();
        let mut at = start;
        for vma in self.vmas.iter().filter(|v| v.end > start && v.start < end) {
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

    /// Take the pieces of every VMA inside `[start, end)` out of the table and
    /// return them; what is left stays, split at the boundaries as required.
    fn split_out(&mut self, start: usize, end: usize) -> Vec<Vma> {
        if start >= end {
            return Vec::new();
        }
        let mut taken = Vec::new();
        let mut kept = Vec::with_capacity(self.vmas.len() + 1);
        for vma in self.vmas.drain(..) {
            // Whatever of this VMA is below `start`...
            if let Some(left) = vma.sliced(vma.start, start) {
                kept.push(left);
            }
            // ...whatever is inside the range, which is what the caller wants...
            if let Some(inside) = vma.sliced(start, end) {
                taken.push(inside);
            }
            // ...and whatever is above `end`.
            if let Some(right) = vma.sliced(end, vma.end) {
                kept.push(right);
            }
        }
        self.vmas = kept;
        taken
    }

    /// Sort by address and fold neighbours that describe the same thing.
    fn tidy(&mut self) {
        self.vmas.sort_unstable_by_key(|v| v.start);
        let mut merged: Vec<Vma> = Vec::with_capacity(self.vmas.len());
        for vma in self.vmas.drain(..) {
            match merged.last_mut() {
                Some(last) if last.abuts(&vma) => last.end = vma.end,
                _ => merged.push(vma),
            }
        }
        self.vmas = merged;
    }
}

#[cfg(all(test, not(target_os = "none")))]
mod tests {
    use super::*;

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
}
