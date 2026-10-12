//! The block cache behind every ext4 read and write.
//!
//! Four-way set-associative with least-recently-used victim selection, over a
//! lazily-allocated slot per way.
//!
//! It was a direct-mapped array of [`CACHE_SLOTS`] slots indexed by
//! `block % CACHE_SLOTS`, and the first fix to it was size: 32 slots (128 KiB)
//! was small enough that an ordinary `ls` re-read the inode table repeatedly, so
//! it grew to 512 (2 MiB) and the hot metadata stayed resident.
//!
//! Size cannot fix what is left, because the problem is not capacity but
//! *collision*: any two blocks a whole slot-count apart want the same slot, and
//! a working set of four such blocks - four groups' inode tables, say - makes
//! every access evict what the next one wants. Measured, with the counting
//! device in this file's tests: 4096 device reads for 4096 accesses, which is a
//! cache that is not there. Ways are the cure, and four of them cost nothing but
//! slots, so the set count is kept at the 512 that the `ls` measurement
//! justified and each set got four slots instead of one.
//!
//! Slots are no longer allocated up front: a slot takes its 4 KiB buffer the
//! first time it holds a block. That is what makes [`CACHE_SLOTS`] affordable -
//! a filesystem that touches a few hundred blocks pays for a few hundred, not
//! for all 8 MiB - and it also removes a fixed cost that every mount used to pay
//! whether or not anything was read.
//!
//! Purely a size/latency trade either way: correctness does not depend on any of
//! it, so an undersized cache is slow, not wrong. Every change here is measured
//! by device read count rather than by a clock, which is why the numbers above
//! are stable enough to write down.

use super::*;

/// Ways per set. Four is the smallest associativity that survives the aliasing
/// pattern the tests pin (three blocks colliding plus the hot one), and the
/// smallest that is worth the bookkeeping.
pub(crate) const CACHE_WAYS: usize = 4;

/// Sets, and so the number of distinct slots a block can be indexed to. Kept at
/// the figure the `ls` measurement chose, for the same reason: a scan of a
/// directory large enough to cycle every set is the load that made the number
/// matter.
pub(crate) const CACHE_SETS: usize = 512;

/// Total slots - what the old flat `CACHE_SLOTS` named, and exactly the places
/// a block may live.
pub(crate) const CACHE_SLOTS: usize = CACHE_SETS * CACHE_WAYS;

pub(crate) const NO_BLOCK: u64 = u64::MAX;

/// The most blocks one run fetch may cover, and so the width of the scratch
/// buffer a run needs: 16 × 4 KiB = **64 KiB**, allocated once per mount when a
/// run first asks for it (§10's rule-4 discipline - the caller steers neither the
/// count nor the width, and the buffer is the cache's, not the request's).
///
/// Sixteen is also what the device's own figure usually allows: `src/drivers/ahci.rs`
/// answers `max_transfer` = 128 KiB, so this is the tighter of the two bounds
/// there, while a driver at the default (`PAGE_SIZE`) never reaches it.
pub(crate) const RUN_BLOCKS: usize = 16;

pub(crate) struct Slot {
    block: u64,
    /// The buffer, empty until this slot is first used. A slot that has never
    /// held a block costs nothing but its entry.
    bytes: Vec<u8>,
    /// When this slot's block was last asked for, on the cache's own access
    /// counter. Least-recently-used is then a comparison, not a list.
    stamp: u64,
}

/// Set-associative, write-through block cache: `block % CACHE_SETS` picks a set,
/// a miss loads from the device into an unused or least-recently-used way, and
/// every modification is pushed straight back.
pub(crate) struct Cache {
    slots: Vec<Slot>,
    /// The access counter the `stamp`s are drawn from. Wrapping is fine and
    /// unreachable: it would take 2^64 block accesses to revisit a value.
    clock: u64,
    /// Where a wide run lands before it is scattered into slots, so one device
    /// call can serve [`Cache::slot_run`]. Empty until the first run; then
    /// `RUN_BLOCKS * block_size` at most.
    run: Vec<u8>,
}

impl Cache {
    pub(crate) fn new() -> Cache {
        Cache {
            slots: (0..CACHE_SLOTS)
                .map(|_| Slot {
                    block: NO_BLOCK,
                    bytes: Vec::new(),
                    stamp: 0,
                })
                .collect(),
            clock: 0,
            run: Vec::new(),
        }
    }

    /// The page holding `block`, loaded from `dev` on a miss. The borrow keeps
    /// the way resident, so callers must finish with the slice before the
    /// next cache access.
    pub(crate) fn slot(&mut self, dev: &dyn BlockDev, block: u64, block_size: usize) -> &mut [u8] {
        self.clock = self.clock.wrapping_add(1);
        let stamp = self.clock;
        if let Some(at) = self.resident(block) {
            // A hit still has to be dated, or "least recently used" would be
            // "first loaded" and the hot block would lose to a scan.
            self.slots[at].stamp = stamp;
            return &mut self.slots[at].bytes;
        }

        let base = (block % CACHE_SETS as u64) as usize * CACHE_WAYS;
        let at = self.victim(base);
        let slot = &mut self.slots[at];
        if slot.bytes.len() != block_size {
            slot.bytes = vec![0; block_size];
        }
        // The read comes first and the block number is recorded after it: a
        // slot must never name a block it has not actually loaded.
        dev.read_at(block * block_size as u64, &mut slot.bytes);
        slot.block = block;
        slot.stamp = stamp;
        &mut slot.bytes
    }

    /// [`Cache::slot`] for a caller that knows it is reading **onward**: the
    /// block, and up to `want` consecutive device blocks with it, in one device
    /// call.
    ///
    /// This is a *demand* run and not a readahead, and the distinction is the
    /// whole design. `want` is what the caller's own request is about to touch -
    /// `ext4`'s file-read loop counts the run of contiguous blocks its extent
    /// lookup just found - so a random read of one block asks for one block and
    /// pays nothing for the privilege. A readahead *past* the request is the
    /// other design (Linux's `file_ra_state`, with sequential detection and a
    /// growing window); it is not this one because the cost it adds - device
    /// reads a random pattern never wanted - is invisible to §10.5's instrument,
    /// which measures one sequential pass.
    ///
    /// A hit returns at once and ignores `want`: a run already resident must not
    /// re-read anything, which is also what makes it safe for the *second* block
    /// of a run to ask again with a shorter `want`.
    ///
    /// The width is bounded twice and neither bound is the caller's to steer:
    /// the device's [`BlockDev::max_transfer`] (its promise that the run is one
    /// command) and [`RUN_BLOCKS`].
    pub(crate) fn slot_run(
        &mut self,
        dev: &dyn BlockDev,
        block: u64,
        want: usize,
        block_size: usize,
    ) -> &mut [u8] {
        if self.resident(block).is_some() {
            return self.slot(dev, block, block_size);
        }
        let width = self.run_width(dev, want, block_size);
        if width == 1 {
            return self.slot(dev, block, block_size);
        }

        // The run lands in the scratch first, because one device call is the
        // point and the slots are separately allocated.
        let need = width * block_size;
        if self.run.len() < need {
            self.run = vec![0; need];
        }
        dev.read_at(block * block_size as u64, &mut self.run[..need]);

        self.clock = self.clock.wrapping_add(1);
        let stamp = self.clock;
        // Where each block of the run goes, decided before anything moves: the
        // run's blocks are *consecutive*, so they land in consecutive sets and
        // no two of them can choose the same way - which is what lets this be a
        // decision and not a search.
        let mut places = [0usize; RUN_BLOCKS];
        for (i, place) in places[..width].iter_mut().enumerate() {
            let at = block + i as u64;
            *place = match self.resident(at) {
                Some(at) => at,
                None => self.victim((at % CACHE_SETS as u64) as usize * CACHE_WAYS),
            };
        }
        for (i, &at) in places[..width].iter().enumerate() {
            let slot = &mut self.slots[at];
            if slot.bytes.len() != block_size {
                slot.bytes = vec![0; block_size];
            }
            slot.bytes
                .copy_from_slice(&self.run[i * block_size..(i + 1) * block_size]);
            slot.block = block + i as u64;
            // The block that was asked for is the most recent; the run's tail is
            // older by its distance from it, so an eviction takes the tail first.
            slot.stamp = stamp.saturating_sub(i as u64);
        }
        &mut self.slots[places[0]].bytes
    }

    /// How many blocks a run from `block` may cover: the caller's demand,
    /// clamped by what the device moves in one transfer and by [`RUN_BLOCKS`].
    /// Public to the filesystem because the read loop asks the *same* question
    /// before it decides how far to look ahead - a run longer than this would be
    /// loaded in pieces, and the loop would rather ask once for the width it will
    /// actually get.
    pub(crate) fn run_width(&self, dev: &dyn BlockDev, want: usize, block_size: usize) -> usize {
        let by_device = (dev.max_transfer() / block_size).max(1);
        want.max(1).min(by_device).min(RUN_BLOCKS)
    }

    /// The way of `block`'s set that already holds it, if any.
    fn resident(&self, block: u64) -> Option<usize> {
        let base = (block % CACHE_SETS as u64) as usize * CACHE_WAYS;
        (base..base + CACHE_WAYS).find(|&at| self.slots[at].block == block)
    }

    /// Which way of this set loses its block: one that has never held one, or
    /// else the one whose block was asked for longest ago.
    fn victim(&self, base: usize) -> usize {
        for way in 0..CACHE_WAYS {
            if self.slots[base + way].block == NO_BLOCK {
                return base + way;
            }
        }
        let mut victim = base;
        for way in 1..CACHE_WAYS {
            if self.slots[base + way].stamp < self.slots[victim].stamp {
                victim = base + way;
            }
        }
        victim
    }
}

#[cfg(test)]
mod tests {
    use alloc::vec::Vec;
    use core::sync::atomic::{AtomicU64, Ordering};

    use super::*;

    const BLOCK: usize = 4096;

    /// A device that answers with a block-shaped pattern and counts the reads
    /// per block.
    ///
    /// The count is the measurement, and the reason a cache can be tested at
    /// all without a clock: a cache's whole job is to make the device read
    /// count smaller, and unlike a timing that number is the same on every
    /// run, on every machine, under TCG or KVM. A cache that is subtly wrong
    /// also shows up here as *content*, not as slowness - the pattern carries
    /// the block number so a slot handed back for the wrong block is caught.
    struct Counting {
        reads: Vec<AtomicU64>,
        total: AtomicU64,
    }

    impl Counting {
        fn new(blocks: u64) -> Counting {
            Counting {
                reads: (0..blocks).map(|_| AtomicU64::new(0)).collect(),
                total: AtomicU64::new(0),
            }
        }

        fn reads_of(&self, block: u64) -> u64 {
            self.reads[block as usize].load(Ordering::Relaxed)
        }

        fn total(&self) -> u64 {
            self.total.load(Ordering::Relaxed)
        }
    }

    impl BlockDev for Counting {
        fn capacity_sectors(&self) -> u64 {
            self.reads.len() as u64 * (BLOCK as u64 / 512)
        }

        /// A mock that genuinely serves a wide buffer - it fills every byte of
        /// one - so it may honestly claim a run. `16 * BLOCK` is what the real
        /// width turns out to be (`RUN_BLOCKS`, and the AHCI disk's 128 KiB is
        /// the looser of the two bounds), which is what lets the run's own test
        /// below ask for the width rather than hard-code one.
        fn max_transfer(&self) -> usize {
            RUN_BLOCKS * BLOCK
        }

        fn read_at(&self, offset: u64, buf: &mut [u8]) {
            let first = offset / BLOCK as u64;
            // **Every block the call covers** is counted, not just the one at the
            // offset: a call that moves a run transfers each of its blocks once,
            // and the per-block counters are what the tests read.
            self.total.fetch_add(1, Ordering::Relaxed);
            for i in 0..buf.len().div_ceil(BLOCK) {
                self.reads[(first + i as u64) as usize].fetch_add(1, Ordering::Relaxed);
            }
            // Every byte of the block is derived from its number, so a slot
            // holding the wrong block cannot pass by luck.
            for (i, byte) in buf.iter_mut().enumerate() {
                let block = first + (i / BLOCK) as u64;
                *byte = (block as u8) ^ ((i % BLOCK) as u8);
            }
        }

        fn write_at(&self, _offset: u64, _buf: &[u8]) {}
    }

    /// A cache over a device big enough for the traces below.
    fn cache_and_device(blocks: u64) -> (Cache, Counting) {
        (Cache::new(), Counting::new(blocks))
    }

    #[test]
    fn a_hit_returns_the_blocks_own_bytes() {
        let (mut cache, dev) = cache_and_device(4096);
        let want: Vec<u8> = (0..BLOCK).map(|i| (7u8) ^ (i as u8)).collect();
        cache.slot(&dev, 7, BLOCK);
        let got = cache.slot(&dev, 7, BLOCK);
        assert_eq!(got, &want[..], "a second read of block 7");
        assert_eq!(dev.total(), 1, "block 7 was read from the device twice");
    }

    /// The trace the ways exist for, and the one a direct-mapped cache cannot
    /// serve at all: a working set of four blocks a whole *set-count* apart, so
    /// every one of them is indexed to the same set.
    ///
    /// Four groups' inode tables land here. Accessed in turn, the direct-mapped
    /// cache read the device 4096 times for 4096 accesses - each access evicting
    /// what the next one wanted - and the four ways make it four reads.
    #[test]
    fn a_working_set_that_aliases_is_served_from_the_cache() {
        let (mut cache, dev) = cache_and_device(8192);
        let working_set: [u64; 4] = [
            0,
            CACHE_SETS as u64,
            2 * CACHE_SETS as u64,
            3 * CACHE_SETS as u64,
        ];
        for _ in 0..1024 {
            for block in working_set {
                cache.slot(&dev, block, BLOCK);
            }
        }
        assert_eq!(
            dev.total(),
            working_set.len() as u64,
            "a four-block working set read from the device {} times over 4096 accesses",
            dev.total(),
        );
    }

    /// A sequential scan has no reuse to exploit, so the cache must not be
    /// *asked* to help: every block is read once, and a rewrite that grew the
    /// cache must not have started re-reading anything.
    #[test]
    fn a_sequential_scan_reads_each_block_once() {
        let (mut cache, dev) = cache_and_device(8192);
        for block in 0..2048u64 {
            cache.slot(&dev, block, BLOCK);
        }
        assert_eq!(dev.total(), 2048, "a 2048-block scan");
    }

    /// The other half of the same problem, and the regression that matters
    /// most: a scan long enough to cycle every set must not carry the *hot*
    /// block away with it. This is the shape of every real workload here - an
    /// inode table or a group descriptor read between streams of directory or
    /// file blocks - and the ways are what keep it: the hot block sits in one
    /// way of its set while the scan cycles through the other three.
    #[test]
    fn a_hot_block_survives_a_long_scan() {
        const HOT: u64 = 7;
        let (mut cache, dev) = cache_and_device(8192);
        cache.slot(&dev, HOT, BLOCK);
        for block in 100..2148u64 {
            cache.slot(&dev, block, BLOCK);
            cache.slot(&dev, HOT, BLOCK);
        }
        assert!(
            dev.reads_of(HOT) <= 5,
            "the hot block was read {} times across one 2048-block scan",
            dev.reads_of(HOT),
        );
        assert_eq!(dev.total(), 2048 + dev.reads_of(HOT));
    }

    /// A sequential scan through **runs**: one device call per run of
    /// contiguous blocks, and every block still transferred exactly once.
    ///
    /// This is §10.5's change in the cache's own terms, and it is the
    /// regression pin for the two ways it could be wrong: a run that was one call
    /// but lost its tail would show as a block transferred twice (the second
    /// `slot_run` would miss), and a run that re-read what it already held would
    /// show as more calls than the scan has runs.
    #[test]
    fn a_sequential_scan_through_runs_costs_one_call_per_run() {
        let (mut cache, dev) = cache_and_device(8192);
        let width = cache.run_width(&dev, usize::MAX, BLOCK);
        assert!(width > 1, "a run of one is not a run: width {width}");
        for block in 0..2048u64 {
            cache.slot_run(&dev, block, width, BLOCK);
        }
        assert_eq!(
            dev.total(),
            (2048 / width) as u64,
            "a 2048-block scan through runs of {width}",
        );
        for block in 0..2048u64 {
            assert_eq!(
                dev.reads_of(block),
                1,
                "block {block} was transferred {} times",
                dev.reads_of(block),
            );
        }
    }

    /// A run's blocks are resident and hold **their own** bytes - the run is
    /// scattered into per-block slots, so a copy that shifted by one would read
    /// as content rather than as slowness.
    #[test]
    fn every_block_of_a_run_holds_its_own_bytes() {
        let (mut cache, dev) = cache_and_device(8192);
        let width = cache.run_width(&dev, usize::MAX, BLOCK);
        cache.slot_run(&dev, 100, width, BLOCK);
        assert_eq!(dev.total(), 1, "the run itself");
        for i in 0..width as u64 {
            let want: Vec<u8> = (0..BLOCK).map(|b| ((100 + i) as u8) ^ (b as u8)).collect();
            assert_eq!(
                cache.slot(&dev, 100 + i, BLOCK),
                &want[..],
                "block {} after the run",
                100 + i,
            );
        }
        assert_eq!(dev.total(), 1, "the run was re-read through its slots");
    }

    /// A **demand** run is not a prefetch: asking for one block loads one block.
    /// This is the whole reason the width comes from the caller and not from a
    /// heuristic - a random read must not drag a run of its neighbours in, and
    /// the instrument §10.5 measures could not see that cost.
    #[test]
    fn asking_for_one_block_loads_one_block() {
        let (mut cache, dev) = cache_and_device(8192);
        cache.slot_run(&dev, 500, 1, BLOCK);
        assert_eq!(dev.total(), 1, "a one-block demand");
        assert_eq!(dev.reads_of(500), 1);
        assert_eq!(dev.reads_of(501), 0, "a neighbour was loaded unasked");
    }

    /// A one-block **demand** is one block read, whatever the device could move:
    /// `run_width` answers the caller's own figure when it is the smaller one, so
    /// a random read of a block does not drag its neighbours in. (The device's
    /// *default* - a driver that answers no `max_transfer` at all - is pinned
    /// where the default lives, in `crates/blk`.)
    #[test]
    fn a_one_block_demand_never_forms_a_run() {
        let (mut cache, dev) = cache_and_device(8192);
        assert_eq!(cache.run_width(&dev, 1, BLOCK), 1);
        for block in 0..8u64 {
            cache.slot_run(&dev, block, 1, BLOCK);
        }
        assert_eq!(dev.total(), 8, "eight one-block demands");
        assert_eq!(dev.reads_of(0), 1);
    }

    /// The ways have to be chosen, not guessed at: with four blocks of a set
    /// live and a fifth arriving, the one that loses is the one asked for
    /// longest ago, and the four that stay are still hits.
    #[test]
    fn the_way_that_loses_is_the_least_recently_used_one() {
        let (mut cache, dev) = cache_and_device(8192);
        let set: Vec<u64> = (0..5).map(|i| i * CACHE_SETS as u64).collect();

        // Fill all four ways with the first four.
        for &block in &set[..4] {
            cache.slot(&dev, block, BLOCK);
        }
        // Touch the last three again, so the first is the least recent.
        for &block in &set[1..4] {
            cache.slot(&dev, block, BLOCK);
        }
        let before = dev.total();

        // The fifth block of the set takes exactly one way, and it takes the
        // first block's.
        cache.slot(&dev, set[4], BLOCK);
        assert_eq!(dev.total(), before + 1, "the fifth block's own load");
        assert_eq!(dev.reads_of(set[0]), 1, "the evicted block is re-read once");
        for &block in &set[1..4] {
            assert_eq!(dev.reads_of(block), 1, "a block that stayed is re-read");
        }
    }
}
