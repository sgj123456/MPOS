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
//! **The cache does not hold its borrow across a device call.** A miss is a
//! *claim* now: the caller takes a slot out of eviction's reach, takes the
//! scratch the transfer reads into, releases the cache, parks on the device,
//! and lands the bytes back under a fresh borrow. The shape exists because the
//! device transfer parks its task - a borrow held across it is the
//! `RefCell already borrowed` the first parking transfer died of (measured,
//! 2026-10-25) - and because a second task asking for the same block while the
//! first is in flight must *wait for it* rather than read the device twice:
//! that is a buffer lock, Linux's `BH_Lock`, carried by the slot's own claim
//! state. The two scratch buffers - one fill, one run - are claimed the same
//! way, because they are cache-owned state a second claimant would clobber.
//!
//! Purely a size/latency trade either way: correctness does not depend on any of
//! it, so an undersized cache is slow, not wrong. Every change here is measured
//! by device read count rather than by a clock, which is why the numbers above
//! are stable enough to write down.

use super::*;
use crate::sched::TaskId;

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
    /// A claim, not a content state: this slot has been picked for `block` and
    /// its device transfer is in flight; `bytes` holds nothing usable until it
    /// ends. Eviction skips it, so the transfer lands where it was aimed, and
    /// a second task asking for the same block finds the claim and waits
    /// instead of reading the device twice - the buffer lock Linux carries as
    /// `BH_Lock`, in the only form a `RefCell` cache can serve it.
    loading: bool,
    /// Tasks parked on this slot's claim, in arrival order. Drained - and
    /// woken outside every lock - by whoever ends the claim. The list's bound
    /// is the task table's own: a task registers once per wait, and the table
    /// is the kernel's bound on tasks.
    waiters: Vec<TaskId>,
}

/// A one-block claim: the slot it will land in and the scratch the transfer
/// reads into. The scratch moves with the claim so the transfer happens with
/// no borrow of the cache anywhere in the caller's frame.
pub(crate) struct FillClaim {
    at: usize,
    offset: u64,
    scratch: Vec<u8>,
}

/// A run claim: `width` consecutive blocks, the slot each will land in, and
/// the run scratch the one transfer reads into. Every slot of the span is
/// claimed for the transfer's duration, ready ones included - a slot left
/// serving hits could be evicted before the scatter, and the bytes would
/// land on a stranger.
pub(crate) struct RunClaim {
    first: u64,
    width: usize,
    block_size: usize,
    offset: u64,
    /// The slot claimed for each block of the run.
    places: [usize; RUN_BLOCKS],
    scratch: Vec<u8>,
}

const NO_WAY: usize = usize::MAX;

/// Set-associative, write-through block cache: `block % CACHE_SETS` picks a set,
/// a miss takes a claim whose transfer the *caller* makes outside every borrow,
/// and every modification is pushed straight back.
pub(crate) struct Cache {
    slots: Vec<Slot>,
    /// The access counter the `stamp`s are drawn from. Wrapping is fine and
    /// unreachable: it would take 2^64 block accesses to revisit a value.
    clock: u64,
    /// Where a wide run lands before it is scattered into slots, so one device
    /// call can serve a run. Empty until the first run; then
    /// `RUN_BLOCKS * block_size` at most. Claimed by [`Cache::begin_run`]
    /// until its [`Cache::end_run`].
    run: Vec<u8>,
    /// Whether the run scratch is out with a claim.
    run_busy: bool,
    /// Where a single-block fill lands while the device works - claimed like
    /// the run scratch, for the same reason: it is cache-owned state a second
    /// claimant would clobber.
    fill: Vec<u8>,
    fill_busy: bool,
}

impl Cache {
    pub(crate) fn new() -> Cache {
        Cache {
            slots: (0..CACHE_SLOTS)
                .map(|_| Slot {
                    block: NO_BLOCK,
                    bytes: Vec::new(),
                    stamp: 0,
                    loading: false,
                    waiters: Vec::new(),
                })
                .collect(),
            clock: 0,
            run: Vec::new(),
            run_busy: false,
            fill: Vec::new(),
            fill_busy: false,
        }
    }

    /// The way of `block`'s set that holds it **ready** - a hit. A slot whose
    /// claim is still in flight names the block but holds no bytes yet, and a
    /// hit it is not.
    pub(crate) fn ready(&self, block: u64) -> Option<usize> {
        self.set_of(block)
            .find(|&at| self.slots[at].block == block && !self.slots[at].loading)
    }

    /// The way whose in-flight claim is exactly this block, if any: the slot a
    /// second asker of the same block parks on.
    pub(crate) fn loading(&self, block: u64) -> Option<usize> {
        self.set_of(block)
            .find(|&at| self.slots[at].block == block && self.slots[at].loading)
    }

    /// Whether any way of `block`'s set has a claim in flight - the answer a
    /// claimant gets when its set has no free way at all, and the condition
    /// it then waits on.
    pub(crate) fn set_in_flight(&self, block: u64) -> bool {
        self.set_of(block).any(|at| self.slots[at].loading)
    }

    /// Whether the fill scratch is out with a claim.
    pub(crate) fn fill_busy(&self) -> bool {
        self.fill_busy
    }

    /// Whether the run scratch is out with a claim.
    pub(crate) fn run_busy(&self) -> bool {
        self.run_busy
    }

    /// Date a hit: without this, "least recently used" would be "first loaded"
    /// and the hot block would lose to a scan.
    pub(crate) fn touch(&mut self, at: usize) {
        self.clock = self.clock.wrapping_add(1);
        self.slots[at].stamp = self.clock;
    }

    /// A ready slot's bytes, for the caller that has just re-checked
    /// [`Cache::ready`] - the copy-out half of a read.
    pub(crate) fn page(&self, at: usize) -> &[u8] {
        &self.slots[at].bytes
    }

    /// A ready slot's bytes, mutable, for the write-through edit: `f` runs on
    /// this under the borrow, and the copy that travels to the device is
    /// taken out of the borrow before the device call.
    pub(crate) fn page_mut(&mut self, at: usize) -> &mut [u8] {
        &mut self.slots[at].bytes
    }

    /// Park `me` on the claim at `at`. The caller found the slot through
    /// [`Cache::loading`]; whoever ends the claim drains the list and wakes
    /// it, outside every lock.
    pub(crate) fn wait_on(&mut self, at: usize, me: TaskId) {
        self.slots[at].waiters.push(me);
    }

    /// Claim a miss of one `block`: pick a way no claim uses, mark it, take
    /// the fill scratch out, and hand the whole claim back. `None` answers
    /// "not now" - the set has no free way, or the scratch is out - and the
    /// caller re-derives from the top after waiting on whichever it was.
    pub(crate) fn begin_fill(&mut self, block: u64, block_size: usize) -> Option<FillClaim> {
        if self.fill_busy {
            return None;
        }
        let base = (block % CACHE_SETS as u64) as usize * CACHE_WAYS;
        let at = self.victim(base)?;
        self.clock = self.clock.wrapping_add(1);
        let slot = &mut self.slots[at];
        if slot.bytes.len() != block_size {
            slot.bytes = vec![0; block_size];
        }
        slot.block = block;
        slot.loading = true;
        if self.fill.len() < block_size {
            self.fill = vec![0; block_size];
        }
        self.fill_busy = true;
        Some(FillClaim {
            at,
            offset: block * block_size as u64,
            scratch: core::mem::take(&mut self.fill),
        })
    }

    /// The claim's transfer is done: land the scratch's bytes in the slot it
    /// was aimed at, end the claim, put the scratch back, and hand back the
    /// tasks parked on the claim - the caller wakes them outside every lock.
    pub(crate) fn end_fill(&mut self, claim: FillClaim) -> Vec<TaskId> {
        // The read came first and the block number is recorded with the bytes:
        // a slot must never name a block it has not actually loaded.
        self.clock = self.clock.wrapping_add(1);
        let slot = &mut self.slots[claim.at];
        debug_assert!(slot.loading, "a fill ended on a slot nobody claimed");
        slot.bytes.copy_from_slice(&claim.scratch);
        slot.loading = false;
        slot.stamp = self.clock;
        self.fill = claim.scratch;
        self.fill_busy = false;
        core::mem::take(&mut slot.waiters)
    }

    /// [`Cache::begin_fill`] for a caller that knows it is reading **onward**:
    /// the block, and up to `want` consecutive device blocks with it, in one
    /// transfer. `None` answers "not now": some block of the run's span has no
    /// free way, or the run scratch is out - the caller waits and re-derives.
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
    /// The width is bounded twice and neither bound is the caller's to steer:
    /// the device's [`BlockDev::max_transfer`] (its promise that the run is one
    /// command) and [`RUN_BLOCKS`].
    pub(crate) fn begin_run(
        &mut self,
        dev: &dyn BlockDev,
        block: u64,
        want: usize,
        block_size: usize,
    ) -> Option<RunClaim> {
        let width = self.run_width(dev, want, block_size);
        let need = width * block_size;
        if self.run.len() < need {
            self.run = vec![0; need];
        }
        self.clock = self.clock.wrapping_add(1);
        let stamp = self.clock;
        // Where each block of the run goes, decided before anything moves: the
        // run's blocks are *consecutive*, so they land in consecutive sets and
        // no two of them can choose the same way - which is what lets this be a
        // decision and not a search.
        let mut places = [NO_WAY; RUN_BLOCKS];
        for (i, place) in places[..width].iter_mut().enumerate() {
            let at = block + i as u64;
            *place = match self.ready(at) {
                Some(at) => at,
                None => self.victim((at % CACHE_SETS as u64) as usize * CACHE_WAYS)?,
            };
        }
        let scratch = core::mem::take(&mut self.run);
        self.run_busy = true;
        // Mark every slot the run will land in, ready ones included: a slot
        // left serving hits could be evicted by another task's claim before
        // the scatter, and the transfer's bytes would land on a stranger.
        // Claiming it costs a hit for the transfer's own latency - the same
        // wait a second asker of a claimed block takes - and nothing else.
        for (i, &at) in places[..width].iter().enumerate() {
            let slot = &mut self.slots[at];
            if slot.bytes.len() != block_size {
                slot.bytes = vec![0; block_size];
            }
            slot.block = block + i as u64;
            slot.loading = true;
            // The block that was asked for is the most recent; the run's tail is
            // older by its distance from it, so an eviction takes the tail first.
            slot.stamp = stamp.saturating_sub(i as u64);
        }
        Some(RunClaim {
            first: block,
            width,
            block_size,
            offset: block * block_size as u64,
            places,
            scratch,
        })
    }

    /// The run's transfer is done: scatter the scratch into the claimed
    /// slots, end their claims, put the scratch back, and hand back everyone
    /// who parked on any of them.
    pub(crate) fn end_run(&mut self, claim: RunClaim) -> Vec<TaskId> {
        let mut woken = Vec::new();
        for (i, &at) in claim.places[..claim.width].iter().enumerate() {
            let slot = &mut self.slots[at];
            debug_assert!(slot.loading, "a run landed on a slot nobody claimed");
            debug_assert_eq!(slot.block, claim.first + i as u64);
            let from = i * claim.block_size;
            slot.bytes
                .copy_from_slice(&claim.scratch[from..from + claim.block_size]);
            slot.loading = false;
            woken.append(&mut slot.waiters);
        }
        self.run = claim.scratch;
        self.run_busy = false;
        woken
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

    /// The `CACHE_WAYS` slots of `block`'s set.
    fn set_of(&self, block: u64) -> impl Iterator<Item = usize> {
        let base = (block % CACHE_SETS as u64) as usize * CACHE_WAYS;
        base..base + CACHE_WAYS
    }

    /// Which way of this set loses its block: one that has never held one, or
    /// else the one whose block was asked for longest ago - and never a way
    /// whose claim is in flight, because a transfer's bytes must land where
    /// they were aimed. `None`: every way of the set is in flight, and the
    /// claimant waits for one to end.
    fn victim(&self, base: usize) -> Option<usize> {
        for way in 0..CACHE_WAYS {
            if self.slots[base + way].block == NO_BLOCK {
                return Some(base + way);
            }
        }
        let mut victim = None;
        for way in 0..CACHE_WAYS {
            let at = base + way;
            if self.slots[at].loading {
                continue;
            }
            victim = match victim {
                None => Some(at),
                Some(best) if self.slots[at].stamp < self.slots[best].stamp => Some(at),
                other => other,
            };
        }
        victim
    }
}

impl FillClaim {
    /// Where the transfer reads from: the block's own byte offset on the
    /// device.
    pub(crate) fn offset(&self) -> u64 {
        self.offset
    }

    /// The buffer the transfer reads into. It belongs to the cache; the claim
    /// holds it so the device works on memory no borrow names.
    pub(crate) fn scratch_mut(&mut self) -> &mut [u8] {
        &mut self.scratch
    }
}

impl RunClaim {
    /// Where the one transfer reads from: the run's first byte.
    pub(crate) fn offset(&self) -> u64 {
        self.offset
    }

    /// The scratch the transfer reads into.
    pub(crate) fn scratch_mut(&mut self) -> &mut [u8] {
        &mut self.scratch
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

    /// One demand of a single block, the shape the filesystem runs it: a hit
    /// is dated and answered; a miss takes a claim, reads the device into the
    /// claim's scratch **outside the cache**, and lands the bytes. The slot
    /// index the block is ready at comes back.
    fn demand(cache: &mut Cache, dev: &Counting, block: u64) -> usize {
        if let Some(at) = cache.ready(block) {
            cache.touch(at);
            return at;
        }
        let mut claim = cache.begin_fill(block, BLOCK).expect("the fill claims");
        dev.read_at(claim.offset(), claim.scratch_mut());
        cache.end_fill(claim);
        cache.ready(block).expect("the fill left the block ready")
    }

    /// One demand of a run: the head block, plus as many of its successors as
    /// one transfer covers - a hit answers at once, a single-block width
    /// falls to [`demand`], and a real run is one device call whose scratch
    /// is scattered back under a fresh borrow.
    fn demand_run(cache: &mut Cache, dev: &Counting, block: u64, want: usize) -> usize {
        if let Some(at) = cache.ready(block) {
            cache.touch(at);
            return at;
        }
        if cache.run_width(dev, want, BLOCK) <= 1 {
            return demand(cache, dev, block);
        }
        let claim = cache
            .begin_run(dev, block, want, BLOCK)
            .expect("the run claims");
        let mut claim = claim;
        dev.read_at(claim.offset(), claim.scratch_mut());
        cache.end_run(claim);
        cache.ready(block).expect("the run left the block ready")
    }

    #[test]
    fn a_hit_returns_the_blocks_own_bytes() {
        let (mut cache, dev) = cache_and_device(4096);
        let want: Vec<u8> = (0..BLOCK).map(|i| (7u8) ^ (i as u8)).collect();
        demand(&mut cache, &dev, 7);
        let at = demand(&mut cache, &dev, 7);
        let got = cache.page(at);
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
                demand(&mut cache, &dev, block);
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
            demand(&mut cache, &dev, block);
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
        demand(&mut cache, &dev, HOT);
        for block in 100..2148u64 {
            demand(&mut cache, &dev, block);
            demand(&mut cache, &dev, HOT);
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
    /// `demand_run` would miss), and a run that re-read what it already held
    /// would show as more calls than the scan has runs.
    #[test]
    fn a_sequential_scan_through_runs_costs_one_call_per_run() {
        let (mut cache, dev) = cache_and_device(8192);
        let width = cache.run_width(&dev, usize::MAX, BLOCK);
        assert!(width > 1, "a run of one is not a run: width {width}");
        for block in (0..2048u64).step_by(width) {
            demand_run(&mut cache, &dev, block, width);
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
        demand_run(&mut cache, &dev, 100, width);
        assert_eq!(dev.total(), 1, "the run itself");
        for i in 0..width as u64 {
            let want: Vec<u8> = (0..BLOCK).map(|b| ((100 + i) as u8) ^ (b as u8)).collect();
            assert_eq!(
                cache.page(cache.ready(100 + i).expect("the run's block is ready")),
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
        demand_run(&mut cache, &dev, 500, 1);
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
            demand_run(&mut cache, &dev, block, 1);
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
            demand(&mut cache, &dev, block);
        }
        // Touch the last three again, so the first is the least recent.
        for &block in &set[1..4] {
            demand(&mut cache, &dev, block);
        }
        let before = dev.total();

        // The fifth block of the set takes exactly one way, and it takes the
        // first block's.
        demand(&mut cache, &dev, set[4]);
        assert_eq!(dev.total(), before + 1, "the fifth block's own load");
        assert_eq!(dev.reads_of(set[0]), 1, "the evicted block is re-read once");
        for &block in &set[1..4] {
            assert_eq!(dev.reads_of(block), 1, "a block that stayed is re-read");
        }
    }

    // ---- the claim machinery itself ----
    //
    // The claims exist so the device call can run with no borrow of the cache
    // anywhere in the caller's frame - the shape a *parking* transfer needs -
    // and so a second asker of the same block waits for the claim instead of
    // reading the device twice. These tests hold claims open across calls the
    // way a parked transfer would.

    /// A claim in flight is not a hit: a `ready` for its block answers
    /// nothing until the claim ends, and a second asker finds the claim
    /// through `loading` and can park on it. The claim is for a block
    /// nobody holds - which is the only claim the ensure loops take, a miss
    /// being their only reason to claim.
    #[test]
    fn a_claim_in_flight_is_not_a_hit() {
        let (mut cache, dev) = cache_and_device(8192);
        let claim = cache.begin_fill(7, BLOCK).expect("the fill claims");
        assert!(
            cache.ready(7).is_none(),
            "an in-flight claim answered as a hit"
        );
        assert_eq!(
            cache.loading(7),
            Some(claim.at),
            "the claim is not findable"
        );
        cache.end_fill(claim);
        assert!(
            cache.ready(7).is_some(),
            "the claim's own block, after it ends"
        );
        assert_eq!(
            dev.total(),
            0,
            "the claims themselves asked the device for nothing"
        );
    }

    /// The fill scratch is one: a second claim while the first is open is
    /// refused, and the refused claimant re-derives once the scratch is back.
    #[test]
    fn the_fill_scratch_serves_one_claim_at_a_time() {
        let (mut cache, dev) = cache_and_device(8192);
        let claim = cache.begin_fill(7, BLOCK).expect("the first claim");
        assert!(
            cache.fill_busy(),
            "the scratch did not go out with its claim"
        );
        assert!(
            cache.begin_fill(8, BLOCK).is_none(),
            "a second claim took the scratch the first is holding"
        );
        cache.end_fill(claim);
        assert!(!cache.fill_busy(), "the scratch did not come back");
        assert!(
            cache.begin_fill(8, BLOCK).is_some(),
            "the refused claimant, re-derived"
        );
        dev.total();
    }

    /// A task that arrives while another's claim is in flight registers on
    /// the claim, and the claim's end hands the waiters back - the
    /// buffer-lock half of the shape, in the list the waker walks.
    #[test]
    fn a_waiter_on_a_claim_is_released_by_its_end() {
        let (mut cache, dev) = cache_and_device(8192);
        let claim = cache.begin_fill(7, BLOCK).expect("the claim");
        // The second asker finds the claim in flight, not a hit, and parks.
        assert!(cache.ready(7).is_none());
        let at = cache.loading(7).expect("the claim is findable");
        cache.wait_on(at, TaskId(42));
        let woken = cache.end_fill(claim);
        assert_eq!(
            woken,
            alloc::vec![TaskId(42)],
            "the parked task, handed back"
        );
        assert_eq!(
            dev.total(),
            0,
            "the waiter's arrival asked the device for nothing"
        );
    }

    /// The victim search must never hand out a way whose claim is in flight:
    /// a transfer's bytes have to land where they were aimed. A run claims
    /// the ways beside an open fill claim, and its slot for that set is a
    /// different way.
    #[test]
    fn a_claim_takes_its_way_out_of_the_victims_reach() {
        let (mut cache, dev) = cache_and_device(8192);
        let fill = cache.begin_fill(7, BLOCK).expect("the fill claim");
        let width = cache.run_width(&dev, usize::MAX, BLOCK);
        let run = cache
            .begin_run(&dev, 7, width, BLOCK)
            .expect("the run claims");
        assert_ne!(
            run.places[0], fill.at,
            "the run's way for block 7 is the fill claim's own way"
        );
        cache.end_run(run);
        cache.end_fill(fill);
    }

    /// A run claims **every** way its span touches for its own duration -
    /// one left serving hits could be evicted before the scatter, and the
    /// transfer's bytes would land on a stranger.
    #[test]
    fn a_run_claims_every_way_its_span_touches() {
        let (mut cache, dev) = cache_and_device(8192);
        let width = cache.run_width(&dev, usize::MAX, BLOCK);
        demand(&mut cache, &dev, 101);
        let run = cache
            .begin_run(&dev, 100, width, BLOCK)
            .expect("the run claims");
        for i in 0..width as u64 {
            assert!(
                cache.ready(100 + i).is_none(),
                "block {} answered as a hit under the run's claim",
                100 + i
            );
            assert!(
                cache.loading(100 + i).is_some(),
                "block {} is not out of eviction's reach",
                100 + i
            );
        }
        cache.end_run(run);
        for i in 0..width as u64 {
            assert!(
                cache.ready(100 + i).is_some(),
                "block {} after the run",
                100 + i
            );
        }
        // One read: the demand for 101 the test itself made. The claim and
        // the scatter are cache work alone.
        assert_eq!(
            dev.total(),
            1,
            "the claims themselves asked the device for nothing"
        );
    }
}
