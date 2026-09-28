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
use crate::vfs::Inode;

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
    Frames {
        inode: Arc<dyn Inode>,
        offset: usize,
    },
}

impl VmaKind {
    /// Whether two kinds can be the two halves of one merged VMA. The address
    /// arithmetic that has to agree as well - abutment, and that the frame
    /// offsets continue each other - is checked by [`Vma::abuts`].
    fn same_object(&self, other: &VmaKind) -> bool {
        match (self, other) {
            (VmaKind::Anon, VmaKind::Anon) => true,
            (VmaKind::Frames { inode: a, .. }, VmaKind::Frames { inode: b, .. }) => {
                Arc::ptr_eq(a, b)
            }
            _ => false,
        }
    }
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
}

impl Vma {
    pub fn new(start: usize, end: usize, prot: MapFlags, kind: VmaKind) -> Vma {
        assert!(start < end, "a VMA covers at least one byte");
        Vma {
            start,
            end,
            prot,
            kind,
        }
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
            VmaKind::Frames { inode, offset } => VmaKind::Frames {
                inode: Arc::clone(inode),
                offset: offset + (start - self.start),
            },
        };
        Some(Vma {
            start,
            end,
            prot: self.prot,
            kind,
        })
    }

    /// Whether `next` continues this VMA with nothing in between: same
    /// permissions, same object, and - for a frame-backed one - the offset
    /// where this range's own arithmetic puts it.
    fn abuts(&self, next: &Vma) -> bool {
        if self.end != next.start || self.prot != next.prot {
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
    use crate::vfs::{Metadata, NodeKind};

    /// A stand-in backing object: the tests care about *identity* (which Arc is
    /// which), which is the whole of what `Frames` merging is about.
    struct Fake {
        ino: u64,
    }

    impl Inode for Fake {
        fn metadata(&self) -> Metadata {
            Metadata {
                ino: self.ino,
                kind: NodeKind::File,
                size: 0,
            }
        }
    }

    fn anon(start: usize, end: usize, prot: MapFlags) -> Vma {
        Vma::new(start, end, prot, VmaKind::Anon)
    }

    /// One backing object, so that two VMAs over it can be the *same* object
    /// the way two mappings of one descriptor are.
    fn fake(ino: u64) -> Arc<dyn Inode> {
        Arc::new(Fake { ino })
    }

    fn frames(
        start: usize,
        end: usize,
        prot: MapFlags,
        inode: &Arc<dyn Inode>,
        offset: usize,
    ) -> Vma {
        Vma::new(
            start,
            end,
            prot,
            VmaKind::Frames {
                inode: Arc::clone(inode),
                offset,
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
