//! Physical frame allocator: a buddy system, one zone per usable memory region.
//!
//! Two properties shape the design.
//!
//! First, it must run before the heap exists and must not itself need one, so
//! all metadata is carved out of the very memory it manages: a `u32` free-list
//! head per order, plus one state byte per frame.
//!
//! Second, the free-list links live *inside* the free blocks - next and prev,
//! eight bytes, and the smallest block is a 4 KiB page. That costs nothing in
//! memory and makes unlinking O(1), which is what merging needs: when two
//! buddies become one, the buddy is taken off its list on the spot. A list
//! therefore only ever holds blocks that are genuinely free of that order, and
//! every link in it can be trusted. (A singly linked list could not be repaired
//! like this: unlinking from the middle would be O(n), and leaving the merged
//! corpse linked - reading its link word later - would follow bytes its new
//! owner had since overwritten.)
//!
//! Frame state encoding: `state[f] == order + 1` means frame `f` is the head of
//! a free block of that order. `0` means allocated, or the interior of a larger
//! free block. Nothing else is stored, so the whole allocator is 3 integers
//! plus a pointer to its metadata.

use crate::boot::{BootInfo, MemRegion};

// Re-exported from the `mpos-vm` leaf rather than declared here: page size and
// the alignment arithmetic are part of what a `Mapping` *is*, so the machine
// layer - which is becoming a crate that cannot depend on this one - has to be
// able to name them (`NOTES.md` §14 S2b). The paths every caller already uses
// are unchanged.
pub use mpos_vm::{PAGE_SHIFT, PAGE_SIZE};
pub(crate) use mpos_vm::{align_down, align_up};

/// Largest block handed out, as a power of two in pages: 2^14 pages = 64 MiB.
/// Big enough that the kernel heap arena, a page-table pool, and a whole user
/// image read in for `execve` are single allocations - gcc's compiler proper
/// alone is 40 MB - and small enough that merging stays cheap. The free-list
/// indices are `u32`s, so the ceiling this could reach is four terabytes of
/// frames; the real bound is that a bigger order makes every search walk more
/// lists for a benefit nothing here has needed.
pub const MAX_ORDER: usize = 14;

/// One zone per usable memory region; QEMU and real firmware both stay far below.
pub const MAX_ZONES: usize = 32;

const ORDERS: usize = MAX_ORDER + 1;
const HEAD_BYTES: usize = ORDERS * size_of::<u32>();
/// Cache-line align the state array so it does not share lines with the heads.
const META_ALIGN: usize = 64;
const NIL: u32 = u32::MAX;

const STATE_OFF: usize = align_up(HEAD_BYTES, META_ALIGN);

/// Where a zone's per-frame reference counts begin, relative to `meta`. The
/// counts are `u16`, so the array starts on an even byte even when the state
/// array above it has an odd number of frames.
const fn rc_offset(frames: usize) -> usize {
    align_up(STATE_OFF + frames, core::mem::align_of::<u16>())
}

/// Bytes of metadata needed to manage `frames` frames: the free-list heads, one
/// state byte per frame, and one reference count per frame.
///
/// The reference counts live *here* rather than in a heap-allocated map on
/// purpose. The frame allocator runs before the heap exists and the heap grows
/// by calling into it, so any allocation the allocator itself performs would
/// re-enter the heap's own borrow and deadlock it. Carving the counts out of
/// the memory being managed keeps the allocator free of every other subsystem.
pub const fn metadata_bytes(frames: usize) -> usize {
    rc_offset(frames) + frames * core::mem::size_of::<u16>()
}

/// Buddy allocator over one contiguous run of frames.
///
/// Plain integers only, so `Buddy` is `Copy`, `Send`, `Sync` and
/// `const`-constructible, which is what lets the kernel keep the whole physical
/// memory manager in a single `static`.
#[derive(Clone, Copy)]
pub struct Buddy {
    /// Physical address of frame 0.
    pbase: usize,
    /// Added to a physical address to reach it from the kernel.
    hhdm: usize,
    /// Virtual address of the head array; the state array follows it.
    meta: usize,
    frames: usize,
    free_frames: usize,
}

impl Buddy {
    pub const fn empty() -> Self {
        Self {
            pbase: 0,
            hhdm: 0,
            meta: 0,
            frames: 0,
            free_frames: 0,
        }
    }

    /// Initialise a zone, seeding its free lists with the canonical power-of-two
    /// decomposition of `[0, frames)`.
    ///
    /// # Safety
    ///
    /// `meta` must point to at least `metadata_bytes(frames)` writable bytes that
    /// nothing else will use. `pbase` must be page-aligned and name `frames`
    /// frames of memory that nothing else will use, and `pbase + hhdm` must be
    /// mapped for the whole run. Both are written through immediately.
    pub unsafe fn init(meta: usize, pbase: usize, hhdm: usize, frames: usize) -> Self {
        let mut zone = Self {
            pbase,
            hhdm,
            meta,
            frames,
            free_frames: 0,
        };

        for order in 0..ORDERS {
            zone.set_head(order, NIL);
        }
        unsafe { core::ptr::write_bytes(zone.state_ptr(), 0, frames) };
        unsafe {
            core::ptr::write_bytes(
                zone.rc_ptr() as *mut u8,
                0,
                frames * core::mem::size_of::<u16>(),
            )
        };

        let mut frame = 0;
        while frame < frames {
            // Largest order that is both aligned here and still inside the zone.
            let mut order = 0;
            while order < MAX_ORDER
                && frame % (1 << (order + 1)) == 0
                && frame + (1 << (order + 1)) <= frames
            {
                order += 1;
            }
            zone.push(frame, order);
            frame += 1 << order;
        }

        zone
    }

    /// Allocate a block of `2^order` contiguous frames, aligned to its own size.
    /// Returns the physical address of its first frame.
    pub fn alloc(&mut self, order: usize) -> Option<usize> {
        if order > MAX_ORDER {
            return None;
        }

        let mut block = order;
        let frame = loop {
            if block > MAX_ORDER {
                return None;
            }
            match self.pop(block) {
                Some(frame) => break frame,
                None => block += 1,
            }
        };

        // Split the block down to the requested order, freeing the upper half at
        // each step. The halves are pushed largest-first, so the smallest ends up
        // at the head of its list and is reused before bigger blocks are broken.
        while block > order {
            block -= 1;
            self.push(frame + (1 << block), block);
        }

        let phys = self.pbase + (frame << PAGE_SHIFT);
        // A single page starts life owned by exactly one caller; a larger block
        // is never shared page-by-page, so its counts stay clear.
        if order == 0 {
            self.set_refcount(phys, 1);
        }
        Some(phys)
    }

    /// Return a block previously handed out by `alloc` with the same `order`.
    ///
    /// Merging is greedy: as long as the buddy is free and of the same order the
    /// two become one block, up to `MAX_ORDER`.
    pub fn free(&mut self, phys: usize, order: usize) {
        debug_assert!(order <= MAX_ORDER, "free of an order above MAX_ORDER");
        let mut frame = (phys - self.pbase) >> PAGE_SHIFT;
        debug_assert!(
            frame + (1 << order) <= self.frames,
            "free of a block outside this zone"
        );
        // A frame already sitting on a free list is a block two owners have
        // handed back, and the merge below cannot tell the second one from a
        // legitimate block: it links the same page into two lists, the next two
        // allocations of this order hand one page to both callers, and the
        // damage surfaces much later as some unrelated program reading another's
        // memory - a user fault, a corrupted page table, a machine that resets
        // with nothing to say about itself. So it is named here, where it is
        // still provable, in the build that actually ships: a `debug_assert` is
        // compiled out of a release kernel, and a release kernel is the one a
        // double free kills.
        if self.state(frame) != 0 {
            panic!(
                "double free of {phys:#x} (frame {frame}, state {}, order {order})",
                self.state(frame)
            );
        }

        // The block is no longer anybody's: clear the one count that mattered so
        // a later allocation of this page starts from a clean slate.
        if order == 0 {
            self.set_refcount(phys, 0);
        }

        let mut order = order;
        while order < MAX_ORDER {
            let buddy = frame ^ (1 << order);
            if buddy >= self.frames || self.state(buddy) as usize != order + 1 {
                break;
            }
            // The buddy is a genuine free block of this order, so take it off
            // the list before the pair becomes one block of the next.
            self.unlink(buddy, order);
            self.free_frames -= 1 << order;
            frame &= !(1 << order);
            order += 1;
        }

        self.push(frame, order);
    }

    pub fn contains(&self, phys: usize) -> bool {
        phys >= self.pbase && phys < self.pbase + (self.frames << PAGE_SHIFT)
    }

    pub fn frames(&self) -> usize {
        self.frames
    }

    pub fn free_frames(&self) -> usize {
        self.free_frames
    }

    fn push(&mut self, frame: usize, order: usize) {
        let head = self.head(order);
        self.set_next(frame, head);
        self.set_prev(frame, NIL);
        if head != NIL {
            self.set_prev(head as usize, frame as u32);
        }
        self.set_head(order, frame as u32);
        self.set_state(frame, (order + 1) as u8);
        self.free_frames += 1 << order;
    }

    /// Remove a block from the front of `order`'s free list.
    fn pop(&mut self, order: usize) -> Option<usize> {
        let head = self.head(order);
        if head == NIL {
            return None;
        }
        let frame = head as usize;
        self.unlink(frame, order);
        self.free_frames -= 1 << order;
        Some(frame)
    }

    /// Take a block off `order`'s free list. O(1), because the lists are
    /// doubly linked; the caller has checked the block is on the list.
    fn unlink(&mut self, frame: usize, order: usize) {
        let next = self.next_of(frame);
        let prev = self.prev_of(frame);
        if prev == NIL {
            self.set_head(order, next);
        } else {
            self.set_next(prev as usize, next);
        }
        if next != NIL {
            self.set_prev(next as usize, prev);
        }
        self.set_state(frame, 0);
    }

    fn frame_virt(&self, frame: usize) -> usize {
        self.pbase + self.hhdm + (frame << PAGE_SHIFT)
    }

    fn head(&self, order: usize) -> u32 {
        unsafe { (self.meta as *const u32).add(order).read() }
    }

    fn set_head(&self, order: usize, head: u32) {
        unsafe { (self.meta as *mut u32).add(order).write(head) };
    }

    fn state_ptr(&self) -> *mut u8 {
        unsafe { (self.meta as *mut u8).add(STATE_OFF) }
    }

    /// The zone's per-frame reference counts, indexed by frame number.
    fn rc_ptr(&self) -> *mut u16 {
        unsafe { (self.meta as *mut u8).add(rc_offset(self.frames)) as *mut u16 }
    }

    fn frame_of(&self, phys: usize) -> usize {
        (phys - self.pbase) >> PAGE_SHIFT
    }

    /// How many owners a single page has. Pages the zone does not own read as
    /// zero, which is also what a never-shared page reads as before its count
    /// is seeded.
    pub fn refcount(&self, phys: usize) -> u16 {
        if !self.contains(phys) {
            return 0;
        }
        unsafe { self.rc_ptr().add(self.frame_of(phys)).read() }
    }

    /// Write a frame's reference count outright. Used to seed a fresh page at
    /// one and to clear it when the page goes back to the free lists.
    pub fn set_refcount(&mut self, phys: usize, count: u16) {
        if !self.contains(phys) {
            return;
        }
        unsafe { self.rc_ptr().add(self.frame_of(phys)).write(count) };
    }

    /// Record that one more owner now holds `phys` - what a fork does with every
    /// page it shares with its child.
    pub fn share(&mut self, phys: usize) {
        if !self.contains(phys) {
            return;
        }
        let at = unsafe { self.rc_ptr().add(self.frame_of(phys)) };
        let count = unsafe { at.read() };
        unsafe { at.write(count.saturating_add(1)) };
    }

    /// Drop one owner. Answers `true` when the last one is gone and the frame
    /// may be returned to the buddy allocator, `false` while others hold it.
    pub fn unshare(&mut self, phys: usize) -> bool {
        if !self.contains(phys) {
            // Not ours to track: report it as free so the caller's second half
            // still hands it back rather than leaking it.
            return true;
        }
        let at = unsafe { self.rc_ptr().add(self.frame_of(phys)) };
        let count = unsafe { at.read() };
        if count > 1 {
            unsafe { at.write(count - 1) };
            false
        } else {
            unsafe { at.write(0) };
            true
        }
    }

    fn state(&self, frame: usize) -> u8 {
        unsafe { self.state_ptr().add(frame).read() }
    }

    fn set_state(&self, frame: usize, state: u8) {
        unsafe { self.state_ptr().add(frame).write(state) };
    }

    /// A free block's links live in its own first eight bytes - next, then
    /// prev - which is always available: the smallest block is a 4 KiB page.
    fn next_of(&self, frame: usize) -> u32 {
        unsafe { (self.frame_virt(frame) as *const u32).read() }
    }

    fn prev_of(&self, frame: usize) -> u32 {
        unsafe { ((self.frame_virt(frame) as *const u32).add(1)).read() }
    }

    fn set_next(&self, frame: usize, next: u32) {
        unsafe { (self.frame_virt(frame) as *mut u32).write(next) };
    }

    fn set_prev(&self, frame: usize, prev: u32) {
        unsafe { ((self.frame_virt(frame) as *mut u32).add(1)).write(prev) };
    }
}

/// The kernel's physical memory: every usable region, as a set of buddy zones.
pub struct Pmm {
    zones: [Buddy; MAX_ZONES],
    zone_count: usize,
}

impl Pmm {
    pub const fn empty() -> Self {
        Self {
            zones: [Buddy::empty(); MAX_ZONES],
            zone_count: 0,
        }
    }

    /// Which zone owns `phys`, if any. Zones are few (one per usable region), so
    /// the linear scan is cheaper than any index would be.
    fn zone_index(&self, phys: usize) -> Option<usize> {
        (0..self.zone_count).find(|&index| self.zones[index].contains(phys))
    }

    /// The reference count of a single page. Returns 0 for pages no zone owns,
    /// including the interior of a multi-page block, which is never COW-shared.
    pub fn refcount(&self, phys: usize) -> u32 {
        self.zone_index(phys)
            .map_or(0, |index| self.zones[index].refcount(phys) as u32)
    }

    /// Increment the reference count of a single page. Called when fork shares
    /// a frame with the child. The page must have been previously allocated.
    pub fn clone_frame_ref(&mut self, phys: usize) {
        if let Some(index) = self.zone_index(phys) {
            self.zones[index].share(phys);
        }
    }

    /// Decrement the reference count of a single page. Returns `true` when the
    /// count reached zero and the frame should be returned to the buddy
    /// allocator, `false` when other owners still hold it.
    pub fn free_frame_ref(&mut self, phys: usize) -> bool {
        self.zone_index(phys)
            .is_none_or(|index| self.zones[index].unshare(phys))
    }

    /// Build a zone per allocatable region of the memory map.
    ///
    /// # Safety
    ///
    /// `info` must describe live memory, and `info.hhdm_offset` must translate
    /// all of it. Every allocatable region is written through during setup.
    pub unsafe fn init(&mut self, info: &BootInfo) {
        self.zones = [Buddy::empty(); MAX_ZONES];
        self.zone_count = 0;

        for region in info.regions() {
            if self.zone_count == MAX_ZONES {
                break;
            }
            if !region.kind.is_allocatable() {
                continue;
            }
            if let Some(zone) = unsafe { Self::zone_for(*region, info.hhdm_offset) } {
                self.zones[self.zone_count] = zone;
                self.zone_count += 1;
            }
        }
    }

    /// Carve a zone out of `region`, taking its metadata from the front.
    ///
    /// The metadata size depends on the frame count, which depends on the
    /// metadata size. Size it for the largest frame count the region could hold
    /// and accept the slack: that is one page of waste, not a fixed point.
    unsafe fn zone_for(region: MemRegion, hhdm: usize) -> Option<Buddy> {
        let start = align_up(region.base, PAGE_SIZE);
        let end = align_down(region.end(), PAGE_SIZE);
        if start >= end {
            return None;
        }

        let max_frames = (end - start) >> PAGE_SHIFT;
        let meta_frames = align_up(metadata_bytes(max_frames), PAGE_SIZE) >> PAGE_SHIFT;
        if meta_frames >= max_frames {
            return None;
        }

        let meta = start + hhdm;
        let pbase = start + (meta_frames << PAGE_SHIFT);
        Some(unsafe { Buddy::init(meta, pbase, hhdm, max_frames - meta_frames) })
    }

    pub fn alloc(&mut self, order: usize) -> Option<usize> {
        self.zones[..self.zone_count]
            .iter_mut()
            .find_map(|zone| zone.alloc(order))
    }

    pub fn free(&mut self, phys: usize, order: usize) {
        match self.zone_of(phys) {
            Some(zone) => zone.free(phys, order),
            None => panic!("free of {phys:#x}: address is not in any zone"),
        }
    }

    pub fn contains(&self, phys: usize) -> bool {
        self.zones[..self.zone_count]
            .iter()
            .any(|zone| zone.contains(phys))
    }

    fn zone_of(&mut self, phys: usize) -> Option<&mut Buddy> {
        self.zones[..self.zone_count]
            .iter_mut()
            .find(|zone| zone.contains(phys))
    }

    pub fn zone_count(&self) -> usize {
        self.zone_count
    }

    pub fn total_frames(&self) -> usize {
        self.zones[..self.zone_count]
            .iter()
            .map(Buddy::frames)
            .sum()
    }

    pub fn free_frames(&self) -> usize {
        self.zones[..self.zone_count]
            .iter()
            .map(Buddy::free_frames)
            .sum()
    }
}

#[cfg(all(test, not(target_os = "none")))]
mod tests {
    use super::*;
    use crate::boot::{MAX_MEM_REGIONS, MemKind};
    use alloc::vec;
    use alloc::vec::Vec;

    /// A zone backed by host memory. `usize` elements so that both buffers are
    /// aligned enough for the `u32` links the allocator writes into them.
    struct Zone {
        buddy: Buddy,
        _meta: Vec<usize>,
        _mem: Vec<usize>,
    }

    fn words(bytes: usize) -> usize {
        bytes.div_ceil(size_of::<usize>())
    }

    impl Zone {
        fn new(frames: usize) -> Zone {
            let mut meta = vec![0usize; words(metadata_bytes(frames))];
            let mut mem = vec![0usize; words(frames << PAGE_SHIFT)];
            // Moving the Vecs does not move their buffers, so these stay valid.
            let buddy = unsafe {
                Buddy::init(
                    meta.as_mut_ptr() as usize,
                    mem.as_mut_ptr() as usize,
                    0,
                    frames,
                )
            };
            Zone {
                buddy,
                _meta: meta,
                _mem: mem,
            }
        }
    }

    #[test]
    fn seeding_accounts_for_every_frame() {
        // 100 is deliberately not a power of two: the decomposition has to leave
        // a ragged tail of smaller blocks and still add up exactly.
        for frames in [1, 2, 3, 16, 100, 1023] {
            let zone = Zone::new(frames);
            assert_eq!(zone.buddy.frames(), frames);
            assert_eq!(zone.buddy.free_frames(), frames);
        }
    }

    #[test]
    fn allocations_are_disjoint() {
        let mut zone = Zone::new(64);
        let mut seen: Vec<(usize, usize)> = Vec::new();
        for order in [0, 0, 1, 2, 0, 3] {
            let phys = zone.buddy.alloc(order).expect("zone is not exhausted");
            let size = PAGE_SIZE << order;
            for other in &seen {
                assert!(
                    phys + size <= other.0 || other.0 + other.1 <= phys,
                    "{phys:#x}+{size:#x} overlaps {other:#?}"
                );
            }
            // A block must be aligned to its own size, relative to the zone base.
            assert_eq!(
                (phys - zone.buddy.pbase) % size,
                0,
                "misaligned order {order}"
            );
            seen.push((phys, size));
        }
        assert_eq!(zone.buddy.free_frames(), 64 - (1 + 1 + 2 + 4 + 1 + 8));
    }

    #[test]
    fn exhausting_a_zone_then_refilling_it_loses_nothing() {
        let mut zone = Zone::new(100);
        let mut blocks = Vec::new();
        while let Some(phys) = zone.buddy.alloc(0) {
            blocks.push(phys);
        }
        assert_eq!(blocks.len(), 100);
        assert_eq!(zone.buddy.free_frames(), 0);
        assert!(zone.buddy.alloc(0).is_none());

        for phys in blocks {
            zone.buddy.free(phys, 0);
        }
        assert_eq!(zone.buddy.free_frames(), 100);
    }

    #[test]
    fn freed_buddies_merge_back_into_a_larger_block() {
        let mut zone = Zone::new(4);
        let a = zone.buddy.alloc(0).unwrap();
        let b = zone.buddy.alloc(0).unwrap();
        assert_eq!(b, a + PAGE_SIZE, "expected the halves of one block");

        zone.buddy.free(a, 0);
        zone.buddy.free(b, 0);
        assert_eq!(zone.buddy.free_frames(), 4);

        // Only a merge can produce this: the order-1 request lands exactly where
        // the pair started. Without a merge it would have to come from the far
        // half of the zone instead.
        assert_eq!(zone.buddy.alloc(1), Some(a));
        assert_eq!(zone.buddy.alloc(1), Some(a + 2 * PAGE_SIZE));
        assert_eq!(zone.buddy.alloc(1), None);
    }

    #[test]
    fn merging_cascades_all_the_way_up() {
        let mut zone = Zone::new(1 << MAX_ORDER);
        assert_eq!(zone.buddy.alloc(MAX_ORDER), Some(zone.buddy.pbase));
        assert!(zone.buddy.alloc(0).is_none());
        zone.buddy.free(zone.buddy.pbase, MAX_ORDER);
        assert_eq!(zone.buddy.free_frames(), 1 << MAX_ORDER);
    }

    #[test]
    fn stale_list_entries_are_skipped_not_handed_out() {
        let mut zone = Zone::new(8);
        let held: Vec<usize> = (0..4).map(|_| zone.buddy.alloc(0).unwrap()).collect();

        // Freeing held[1] links frame 1 onto the order-0 list; freeing held[0]
        // then merges the pair, leaving that link behind as a corpse.
        zone.buddy.free(held[1], 0);
        zone.buddy.free(held[0], 0);
        assert_eq!(zone.buddy.free_frames(), 6);

        assert_eq!(zone.buddy.alloc(0), Some(held[0]));
        assert_eq!(zone.buddy.alloc(0), Some(held[1]));
        assert_eq!(zone.buddy.free_frames(), 4);
    }

    #[test]
    fn orders_above_the_maximum_are_refused() {
        let mut zone = Zone::new(8);
        assert!(zone.buddy.alloc(MAX_ORDER + 1).is_none());
        assert_eq!(zone.buddy.free_frames(), 8);
    }

    #[test]
    fn a_region_smaller_than_its_metadata_is_rejected() {
        // Metadata is sized for the largest frame count the region could hold and
        // rounded up to whole pages, so a single page cannot hold both.
        let mut backing = vec![0usize; words(2 << PAGE_SHIFT)];
        let region = MemRegion {
            base: align_up(backing.as_mut_ptr() as usize, PAGE_SIZE),
            length: PAGE_SIZE,
            kind: MemKind::Usable,
        };
        assert!(unsafe { Pmm::zone_for(region, 0) }.is_none());
    }

    #[test]
    fn the_pmm_only_takes_allocatable_regions() {
        // One page of slack so the page-aligned region fits inside the buffer.
        let mut backing = vec![0usize; words(1025 << PAGE_SHIFT)];
        let base = align_up(backing.as_mut_ptr() as usize, PAGE_SIZE);

        let mut info = BootInfo {
            hhdm_offset: 0,
            kernel_phys_base: 0,
            kernel_virt_base: 0,
            bootloader_name: "host-test",
            bootloader_version: "0",
            initramfs: None,
            cmdline: "",
            // No loader on the host, so no canary anywhere. Zero is the value
            // that says so, and the check skips on it.
            boot_stack_base: 0,
            // Nor a firmware handing over a device tree: that is a fact about a
            // machine, and this test builds its own memory map instead.
            dtb: None,
            regions: [MemRegion {
                base: 0,
                length: 0,
                kind: MemKind::Reserved,
            }; MAX_MEM_REGIONS],
            region_count: 0,
        };
        info.regions[0] = MemRegion {
            base,
            length: 1024 << PAGE_SHIFT,
            kind: MemKind::Usable,
        };
        info.regions[1] = MemRegion {
            base: 0x1_0000_0000,
            length: 64 << PAGE_SHIFT,
            kind: MemKind::KernelAndModules,
        };
        info.region_count = 2;

        let mut pmm = Pmm::empty();
        unsafe { pmm.init(&info) };

        assert_eq!(pmm.zone_count(), 1);
        // One page of the region went to metadata.
        assert_eq!(pmm.total_frames(), 1023);
        assert_eq!(pmm.free_frames(), 1023);

        let phys = pmm.alloc(3).expect("an 8-frame block");
        assert!(pmm.contains(phys));
        assert_eq!(pmm.free_frames(), 1023 - 8);
        pmm.free(phys, 3);
        assert_eq!(pmm.free_frames(), 1023);

        // The kernel image region must not have been touched.
        assert!(!pmm.contains(0x1_0000_0000));
    }
}
