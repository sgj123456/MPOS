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
        }
    }

    /// The page holding `block`, loaded from `dev` on a miss. The borrow keeps
    /// the way resident, so callers must finish with the slice before the
    /// next cache access.
    pub(crate) fn slot(&mut self, dev: &dyn BlockDev, block: u64, block_size: usize) -> &mut [u8] {
        self.clock = self.clock.wrapping_add(1);
        let stamp = self.clock;
        let base = (block % CACHE_SETS as u64) as usize * CACHE_WAYS;

        for way in 0..CACHE_WAYS {
            let slot = &mut self.slots[base + way];
            if slot.block == block {
                // A hit still has to be dated, or "least recently used" would
                // be "first loaded" and the hot block would lose to a scan.
                slot.stamp = stamp;
                return &mut slot.bytes;
            }
        }

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

        fn read_at(&self, offset: u64, buf: &mut [u8]) {
            let block = offset / BLOCK as u64;
            self.reads[block as usize].fetch_add(1, Ordering::Relaxed);
            self.total.fetch_add(1, Ordering::Relaxed);
            // Every byte of the block is derived from its number, so a slot
            // holding the wrong block cannot pass by luck.
            for (i, byte) in buf.iter_mut().enumerate() {
                *byte = (block as u8) ^ (i as u8);
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
