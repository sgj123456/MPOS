//! The ext4 filesystem, read and write - and the ext2 one it grew out of,
//! which this same driver mounts by the rules ext4 lays down: an image
//! without the `extents` feature is a block-map filesystem, and one with
//! it is a tree, decided image by image and inode by inode.
//!
//! Everything runs through one design choice: the block cache is
//! *write-through*. Every modification lands on the disk before the call
//! returns, so the cache is the only state an [`Ext4Fs`] owns - dropping it
//! loses nothing, and a second mount over the same device sees every write.
//! That costs one extra write per allocation (counter blocks get rewritten
//! immediately) and buys crash-plain semantics with no dirty tracking, no
//! flush path and no journal - the right trade for a kernel whose storage
//! lives in a QEMU image.
//!
//! Allocation is first-fit: scan the group descriptors for a group with free
//! blocks or inodes, scan that group's bitmap for a zero bit, set it, and
//! push the decremented free counts back to disk at once. Correct on-disk
//! bookkeeping after every operation means the image stays inspectable with
//! the host's own ext2 tools after a run.
//!
//! Block mapping covers both shapes the format has. The old one is direct,
//! single-indirect and double-indirect; a file that would need triple gets
//! [`VfsError::FileTooBig`] rather than silent truncation. The new one is
//! ext4's extent tree, where a file is a handful of runs instead of a list of
//! blocks: four extents fit in the inode, and past that the tree moves into
//! blocks and splits, which is why that half of this module is recursive.
//! Removal is the mirror of both. What is deliberately absent: permission
//! checks (there is no identity to check against) and timestamps (preserved
//! when an inode is rewritten, never updated).
//!
//! An ext4 image is mounted when its features allow it. They are sorted into
//! three answers rather than two - features this driver can maintain,
//! features it can read but must not write (checksums it cannot recompute, an
//! unrecovered journal), and features it refuses outright because they change
//! what a block *means* - and the second kind costs a read-only mount instead
//! of a failed one: an image worth reading is worth reading.

use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use core::cell::RefCell;

use critical_section::Mutex;

use crate::blk::BlockDev;

use super::{FileSystem, Ino, Inode, Metadata, NodeKind, VfsError, VfsResult};

const MAGIC: u16 = 0xef53;
const ROOT_INO: u32 = 2;
/// The superblock is a fixed 1024 bytes at byte 1024, whatever the block size.
const SUPERBLOCK_AT: u64 = 1024;
const SUPERBLOCK_BYTES: usize = 1024;
const GROUP_DESC_BYTES: usize = 32;
/// The most of a descriptor this driver reads: the `64bit` layout is the
/// 32-byte ext2 one with the high halves appended, and nothing past that
/// changes how a block or an inode is found.
const GROUP_DESC_BYTES_MAX: usize = 64;

mod cache;
mod csum;
mod dir;
mod dirent;
mod dx;
mod extent;
mod hash;
mod inode;
mod mapping;
// The module files keep the names the split planned. `super` and `alloc` are
// words a `mod` declaration cannot bind - one reserved, the other shadowing
// the crate every `use alloc::` path starts from - so these two declarations
// reach the files under different names.
#[path = "alloc.rs"]
mod allocation;
#[path = "super.rs"]
mod super_block;
mod util;

pub(crate) use self::cache::Cache;
pub(crate) use self::dirent::{
    DIRENT_HEADER, dir_find, dir_init, dir_take, dir_try_insert, dir_walk, dirent_len, entry_name,
    entry_rec, trim_dir_tail, write_dirent,
};
pub(crate) use self::extent::{
    EXTENT_ENTRY, EXTENT_HEADER, EXTENT_INIT_MAX, EXTENT_MAX_DEPTH, EXTENT_ROOT_BYTES, ExtInsert,
    ExtSplit, ext_depth, ext_entries, ext_index, ext_init_header, ext_insert, ext_leaf, ext_len,
    ext_max, ext_ok, ext_put_root, ext_raw_len, ext_root, ext_set_entries, ext_write_index,
    ext_write_leaf,
};
pub(crate) use self::hash::dx_hash_name;
pub(crate) use self::inode::{
    DIRECT_BLOCKS, FAST_SYMLINK_MAX, FT_CHRDEV, FT_DIR, FT_REG, FT_SYMLINK, INODE_EXTENTS_FL,
    INODE_HUGE_FILE_FL, INODE_INDEX_FL, INODE_PARSED, InodeData, S_IFDIR, S_IFLNK, S_IFREG, now,
};
pub(crate) use self::super_block::{Group, Super};
pub(crate) use self::util::{le16, le32, set16, set32};
// The tests name these directly; no kernel path uses the bindings,
// so they are re-exported for the test build only.
#[cfg(test)]
pub(crate) use self::dx::dx_root_limit;
#[cfg(test)]
pub(crate) use self::extent::EXTENT_MAGIC;
#[cfg(test)]
pub(crate) use self::inode::INODE_SIZE_HIGH;
#[cfg(test)]
pub(crate) use self::super_block::{
    COMPAT_HAS_JOURNAL, COMPAT_ORPHAN_FILE, INCOMPAT_64BIT, INCOMPAT_EXTENTS, INCOMPAT_FILETYPE,
    ROCOMPAT_METADATA_CSUM, STATE_VALID_FS,
};

/// The parking half of a claim, split the way this tree's other
/// host-compiled waiters are split (`eventfd`'s, `mqueue`'s): on the
/// machine, a claim's wait is the scheduler's uninterruptible park, the
/// identity is the waiting task's, and a claim's end wakes the tasks
/// parked on it. On the host there is no scheduler and no second task,
/// so no claim is ever in flight behind another, and the doors answer
/// the degenerate forms - a wait whose condition holds on the first
/// look, a placeholder identity, a wake with nobody to wake.
/// The three places a reader parks on an ext4 claim, as `PARK_SITES` ids:
/// a slot's own claim, the fill/run scratch, and a set with no free way.
/// `/proc/<pid>/wchan` prints the number (`docs/notes/` §7 R.9).
pub(crate) const SLOT_WAIT: u8 = 7;
pub(crate) const SCRATCH_WAIT: u8 = 8;
pub(crate) const SET_WAIT: u8 = 9;

/// A reader that is waiting for the scratch **while it is itself inside a
/// claim's transfer** - the signature of the cache being re-entered from the
/// code path that holds it (a nested claim that can never be satisfied). It
/// is a park site of its own so one `wchan` answers "re-entry" instead of
/// leaving it to be inferred from a boot (`docs/notes/` §7 R.9).
pub(crate) const REENTRY_WAIT: u8 = 10;

/// Whether each task slot is inside a claim's device transfer right now, read
/// by the scratch wait to tell re-entry from ordinary contention. Machine-only:
/// the host build has no task table and never transfers.
#[cfg(target_os = "none")]
static IN_TRANSFER: [core::sync::atomic::AtomicBool; crate::sched::MAX_TASKS] =
    [const { core::sync::atomic::AtomicBool::new(false) }; crate::sched::MAX_TASKS];

#[cfg(target_os = "none")]
fn set_in_transfer(id: crate::sched::policy::TaskId, inside: bool) {
    if let Some(slot) = IN_TRANSFER.get(id.0) {
        slot.store(inside, core::sync::atomic::Ordering::Relaxed);
    }
}

#[cfg(target_os = "none")]
fn in_transfer(id: crate::sched::policy::TaskId) -> bool {
    IN_TRANSFER
        .get(id.0)
        .map(|slot| slot.load(core::sync::atomic::Ordering::Relaxed))
        .unwrap_or(false)
}

#[cfg(not(target_os = "none"))]
fn set_in_transfer(_id: crate::sched::policy::TaskId, _inside: bool) {}

#[cfg(not(target_os = "none"))]
fn in_transfer(_id: crate::sched::policy::TaskId) -> bool {
    false
}

/// Count a registration on the cache's claim-waiter list, for the heartbeat.
fn note_claim_registration() {
    #[cfg(target_os = "none")]
    crate::sched::note_claim_registration();
}

/// Count waiters handed back by a claim's end.
fn note_claim_wake(n: usize) {
    #[cfg(target_os = "none")]
    crate::sched::note_claim_wake(n);
    #[cfg(not(target_os = "none"))]
    let _ = n;
}

/// Count a fill claim taken / given back, for the heartbeat's lifecycle pair.
fn note_fill_begin() {
    #[cfg(target_os = "none")]
    {
        let me = claim_park::me();
        crate::sched::note_fill_begin(me.0, crate::sched::last_syscall(me.0));
    }
}
fn note_fill_end() {
    #[cfg(target_os = "none")]
    crate::sched::note_fill_end();
}
/// [`note_fill_begin`] for the run scratch.
fn note_run_begin() {
    #[cfg(target_os = "none")]
    crate::sched::note_run_begin();
}
fn note_run_end() {
    #[cfg(target_os = "none")]
    crate::sched::note_run_end();
}

/// Mirror both scratch flags into the scheduler's statics for the heartbeat,
/// called with the cache's answer after every take and every give-back.
fn mirror_scratch(fill: bool, run: bool) {
    #[cfg(target_os = "none")]
    crate::sched::note_ext4_scratch(fill, run);
    #[cfg(not(target_os = "none"))]
    {
        let _ = (fill, run);
    }
}

/// Record (or clear) the ext4 fill scratch's holder in the scheduler's two
/// statics, so the hung-boot heartbeat can name it (`docs/notes/` §7 R.9).
fn note_fill_owner(slot: Option<usize>) {
    #[cfg(target_os = "none")]
    crate::sched::note_fill_owner(slot);
    #[cfg(not(target_os = "none"))]
    let _ = slot;
}

/// [`note_fill_owner`] for the run scratch.
fn note_run_owner(slot: Option<usize>) {
    #[cfg(target_os = "none")]
    crate::sched::note_run_owner(slot);
    #[cfg(not(target_os = "none"))]
    let _ = slot;
}

/// The park site a scratch wait should carry: [`REENTRY_WAIT`] when the waiter
/// is itself mid-transfer, [`SCRATCH_WAIT`] otherwise.
fn scratch_site() -> u8 {
    if in_transfer(claim_park::me()) {
        REENTRY_WAIT
    } else {
        SCRATCH_WAIT
    }
}

mod claim_park {
    // `TaskId` is the claim's currency on both builds; the `sched` paths
    // exist only on the machine, and the bodies name them in full so the
    // host build imports nothing it cannot use.
    use crate::sched::policy::TaskId;

    pub(super) fn me() -> TaskId {
        #[cfg(target_os = "none")]
        return crate::sched::current();
        #[cfg(not(target_os = "none"))]
        return TaskId(0);
    }

    /// Wait for a claim to end. Uninterruptible on the machine for the
    /// reason every claim wait is: the task that would end it cannot be
    /// interrupted mid-transfer either, so the wait has no other end.
    ///
    /// `arm` runs **before every sleep**, not once: a waiter that is woken and
    /// sent back to sleep (a second claim took the scratch between its wake and
    /// its re-check) must be on the next drain's list too, or that drain's wake
    /// finds nobody. The boot measured exactly that - every claim wake hit, and
    /// the waiter slept again unregistered (`docs/notes/` §7 R.9).
    pub(super) fn wait(site: u8, cond: impl Fn() -> bool, arm: impl Fn()) {
        #[cfg(target_os = "none")]
        crate::sched::block_until_uninterruptible_armed(site, cond, arm);
        #[cfg(not(target_os = "none"))]
        {
            arm();
            while !cond() {
                core::hint::spin_loop();
            }
        }
    }

    pub(super) fn wake(ids: alloc::vec::Vec<TaskId>) {
        for id in ids {
            #[cfg(target_os = "none")]
            {
                let hit = crate::sched::wake(id);
                crate::sched::note_claim_wake_result(hit);
            }
            #[cfg(not(target_os = "none"))]
            let _ = id;
        }
    }
}

/// The filesystem state shared by every inode of one mount.
struct Core {
    dev: Arc<dyn BlockDev>,
    sb: Super,
    groups: u32,
    cache: Mutex<RefCell<Cache>>,
    inodes: Mutex<RefCell<BTreeMap<u32, Arc<dyn Inode>>>>,
    /// Whether this filesystem was mounted read-only. It lives here, on the
    /// one piece of state every inode of the mount shares, because "read-only"
    /// is a property of the mount and not of a file - and because it has to be
    /// the same answer for every path into it.
    read_only: core::sync::atomic::AtomicBool,
}

impl Core {
    /// Refuse a write to a filesystem mounted read-only.
    ///
    /// `mount -o ro` is a promise the filesystem keeps, not a hint: an
    /// initramfs mounts the root it has not yet trusted this way, and a
    /// kernel that accepted the flag and then wrote anyway would be lying
    /// about the one thing the caller asked for.
    fn writable(&self) -> VfsResult<()> {
        if self.read_only.load(core::sync::atomic::Ordering::Relaxed) {
            return Err(VfsError::ReadOnly);
        }
        Ok(())
    }

    fn set_read_only(&self, read_only: bool) {
        // A filesystem that must be read-only stays that way however it was
        // mounted: `mount -o remount,rw` cannot promise what the on-disk
        // format cannot keep.
        self.read_only.store(
            read_only || self.sb.must_read_only,
            core::sync::atomic::Ordering::Relaxed,
        );
    }

    fn mount(dev: Arc<dyn BlockDev>) -> Result<Core, &'static str> {
        // The superblock sits at byte 1024 and is 1024 bytes long, so a device
        // shorter than two KiB cannot hold one - and reading past the end of a
        // real disk is a request the block driver answers by refusing the
        // mount loudly rather than quietly. Ask the question before the read.
        if dev.capacity_sectors() * 512 < SUPERBLOCK_AT + SUPERBLOCK_BYTES as u64 {
            return Err("device is too small to hold a filesystem");
        }
        // The superblock is read before the cache exists: the cache's slot
        // size is one of the things it decides.
        let mut bytes = [0u8; SUPERBLOCK_BYTES];
        dev.read_at(SUPERBLOCK_AT, &mut bytes);
        let sb = Super::parse(&bytes)?;
        // The format has just decided the mount's mode; say which half of
        // that decision the caller did not ask for. A journal this driver
        // does not maintain, and a volume whose clean-unmount marker never
        // arrived, are both readable - and writing either is what tomorrow's
        // fsck would have to undo.
        #[cfg(target_os = "none")]
        if sb.has_journal {
            crate::console::print_args(format_args!(
                "ext4: volume has a journal; mounted read-only until the journal is maintained\n"
            ));
        }
        let block_size = 1024usize << sb.log_block_size;
        // The device decides how large a filesystem may be. `s_blocks_count` is
        // disk data, and it is the bound every later block number is believed
        // against - so a volume claiming more blocks than the disk holds would
        // not just be lying about itself, it would quietly make every pointer
        // inside it pass a check that had stopped meaning anything. Linux
        // refuses at mount for the same reason.
        //
        // `SECTOR` is the unit `BlockDev` speaks in: the trait's offsets are
        // multiples of 512 bytes, whatever a device's own geometry says.
        const SECTOR: u64 = 512;
        sb.blocks_count
            .checked_mul(block_size as u64)
            .filter(|claimed| *claimed <= dev.capacity_sectors() * SECTOR)
            .ok_or("ext4 is larger than the device it sits on")?;
        // A volume that ends at or before its own first data block holds
        // nothing, and the group count below subtracts it.
        if sb.blocks_count <= u64::from(sb.first_data_block) {
            return Err("ext4 ends before its first data block");
        }
        let groups =
            (sb.blocks_count - sb.first_data_block as u64).div_ceil(sb.blocks_per_group as u64);
        let groups = u32::try_from(groups)
            .map_err(|_| "ext4 has more groups than this driver can address")?;
        let core = Core {
            // The mount starts in the mode the format can honour: a journal
            // nobody maintains and a volume that never recorded a clean
            // unmount are both readable, and a caller that asked for
            // read-write gets told no by the first write rather than by a
            // mount that refuses to see the disk.
            read_only: core::sync::atomic::AtomicBool::new(sb.must_read_only),
            dev,
            sb,
            groups,
            cache: Mutex::new(RefCell::new(Cache::new())),
            inodes: Mutex::new(RefCell::new(BTreeMap::new())),
        };
        // Checksummed volumes are verified at mount, the way the kernel's
        // initial-group-descriptor check is: a descriptor table whose own
        // numbers do not add up means the disk is not what it claims to be,
        // and no later read of a block pointer can be trusted through it.
        // (A descriptor or bitmap half left at zero is treated as
        // "not yet filled in" - lazy forms of `mke2fs` leave exactly that,
        // and every tool exempts it.)
        if core.sb.has_metadata_csum || core.sb.has_gdt_csum {
            core.verify_group_descriptors()?;
        }
        Ok(core)
    }

    fn block_size(&self) -> usize {
        1024 << self.sb.log_block_size
    }

    /// How much `i_blocks` grows for one block. Everywhere in the format it is
    /// counted in 512-byte sectors, except on an inode marked `huge_file`,
    /// which counts filesystem blocks - the same quantity in a different unit,
    /// and the one mistake that makes `stat` report a file eight times its
    /// size on a 4 KiB filesystem.
    /// Whether `block` is a block this volume has.
    ///
    /// The one question every block number read off the disk has to answer
    /// before it is used, and the reason it is asked where the number is
    /// *decoded* rather than where it is consumed is that only the decoding
    /// site knows what to do about a refusal: a directory entry's answer is
    /// "no such file", an extent's is "this tree is corrupt", the allocator's
    /// is "not this group". By the time a number reaches the cache it has
    /// usually passed through code with no error channel at all.
    fn block_is_live(&self, block: u64) -> bool {
        block < self.sb.blocks_count
    }

    fn block_units(&self, flags: u32) -> u32 {
        if flags & INODE_HUGE_FILE_FL != 0 {
            1
        } else {
            (self.block_size() / 512) as u32
        }
    }

    /// Whether an inode maps its data with an extent tree. It is the inode's
    /// own flag, not the filesystem's feature: an image converted from ext2
    /// can hold both shapes side by side, and the inode is the only thing
    /// that says which one it is.
    fn is_extent_mapped(&self, data: &InodeData) -> bool {
        data.flags & INODE_EXTENTS_FL != 0
    }

    // ---- cache primitives ----
    //
    // The rule every one of them keeps: **no borrow of the cache spans a
    // device call.** A transfer parks its task, and a borrow held across a
    // park is the `RefCell already borrowed` a second task dies of. A miss
    // is therefore a *claim*: the slot is taken out of eviction's reach, the
    // scratch is taken out of the cache, the borrow ends, the device works,
    // and the bytes land under a fresh borrow. A block someone else's claim
    // is already carrying is waited for, not read twice - the buffer lock
    // Linux carries as `BH_Lock`.

    /// Run `f` on `block`'s page, resident by the time it runs.
    ///
    /// [`Self::ensure_block`] is the whole of the work - the wait, the
    /// claim, the device call - and this is the copy-out under the borrow
    /// that follows it. Nothing between the two yields, so on one core the
    /// block is where the ensure left it.
    fn cached_block<R>(&self, block: u64, f: impl FnOnce(&[u8]) -> R) -> R {
        self.ensure_block(block);
        critical_section::with(|cs| {
            let mut cache = self.cache.borrow(cs).borrow_mut();
            let at = cache
                .ready(block)
                .expect("ensure_block left the block resident");
            // The hit is dated, or "least recently used" would be "first
            // loaded" and the hot block would lose to a scan - the read
            // count the cache's own tests pin starts with this line.
            cache.touch(at);
            f(cache.page(at))
        })
    }

    /// Make `block` resident, whatever it takes. The wait conditions read
    /// the cache through their own borrow, which is the ordinary case.
    fn ensure_block(&self, block: u64) {
        enum Step {
            Ready,
            /// Someone else's claim: park on it, naming the way it holds.
            Wait(usize),
            /// The fill scratch is out with another claim: park on that, with
            /// the site the reading needs (re-entry or plain contention).
            WaitScratch(u8),
            /// No free way in the block's set - every way is carrying a
            /// claim. Unreachable while the two scratches are all the claims
            /// there can be, and answered honestly anyway.
            WaitSet,
            /// Our claim: read the device into it, outside every borrow.
            Go(cache::FillClaim),
        }
        loop {
            let step = critical_section::with(|cs| {
                let mut cache = self.cache.borrow(cs).borrow_mut();
                cache.forget_abandoned();
                if cache.ready(block).is_some() {
                    return Step::Ready;
                }
                if let Some(at) = cache.loading(block) {
                    return Step::Wait(at);
                }
                if cache.fill_busy() {
                    return Step::WaitScratch(scratch_site());
                }
                match cache.begin_fill(block, self.block_size()) {
                    Some(claim) => {
                        cache.own_fill(claim_park::me(), claim.at());
                        note_fill_begin();
                        Step::Go(claim)
                    }
                    None => Step::WaitSet,
                }
            });
            match step {
                Step::Ready => return,
                Step::Wait(at) => {
                    claim_park::wait(
                        SLOT_WAIT,
                        || !self.with_cache(|cache| cache.loading(block).is_some()),
                        || self.with_cache_mut(|cache| cache.wait_on(at, claim_park::me())),
                    );
                }
                Step::WaitScratch(site) => {
                    claim_park::wait(
                        site,
                        || !self.with_cache(|cache| cache.fill_busy()),
                        || self.with_cache_mut(|cache| cache.wait_for_claim(claim_park::me())),
                    );
                }
                Step::WaitSet => {
                    claim_park::wait(
                        SET_WAIT,
                        || !self.with_cache(|cache| cache.set_in_flight(block)),
                        || self.with_cache_mut(|cache| cache.wait_for_claim(claim_park::me())),
                    );
                }
                Step::Go(mut claim) => {
                    let me = claim_park::me();
                    note_fill_owner(Some(me.0));
                    mirror_scratch(true, false);
                    set_in_transfer(me, true);
                    self.dev.read_at(claim.offset(), claim.scratch_mut());
                    set_in_transfer(me, false);
                    let woken = critical_section::with(|cs| {
                        let mut cache = self.cache.borrow(cs).borrow_mut();
                        cache.end_fill(claim)
                    });
                    note_fill_end();
                    note_fill_owner(None);
                    mirror_scratch(false, false);
                    // Outside every lock, the way every wake is handed.
                    claim_park::wake(woken);
                    return;
                }
            }
        }
    }

    /// Make the contiguous run starting at `block` resident in one device
    /// call where the width allows - [`Self::ensure_block`] for a run, with
    /// the same waits and the same rule. `want` is the caller's own demand;
    /// the width it names is clamped by the device and by the cache's
    /// scratch, and a clamped-to-one run *is* [`Self::ensure_block`].
    fn ensure_run(&self, block: u64, want: usize) {
        let width = self.with_cache(|cache| cache.run_width(&*self.dev, want, self.block_size()));
        if width <= 1 {
            self.ensure_block(block);
            return;
        }
        enum Step {
            Ready,
            /// Someone else's claim on the head block: park on it.
            Wait(usize),
            /// The run scratch is out with another claim: park on that.
            WaitScratch(u8),
            /// Some way of one of the run's sets is in flight: no free way
            /// for the span. Unreachable while two scratches are all the
            /// claims there are, and answered anyway.
            WaitSet,
            /// Our claim: one device call into its scratch, outside every
            /// borrow.
            Go(cache::RunClaim),
        }
        loop {
            let step = critical_section::with(|cs| {
                let mut cache = self.cache.borrow(cs).borrow_mut();
                cache.forget_abandoned();
                if cache.ready(block).is_some() {
                    return Step::Ready;
                }
                if let Some(at) = cache.loading(block) {
                    return Step::Wait(at);
                }
                if cache.run_busy() {
                    return Step::WaitScratch(scratch_site());
                }
                match cache.begin_run(&*self.dev, block, want, self.block_size()) {
                    Some(claim) => {
                        cache.own_run(claim_park::me(), claim.places());
                        note_run_begin();
                        Step::Go(claim)
                    }
                    None => Step::WaitSet,
                }
            });
            match step {
                Step::Ready => return,
                Step::Wait(at) => {
                    claim_park::wait(
                        SLOT_WAIT,
                        || !self.with_cache(|cache| cache.loading(block).is_some()),
                        || self.with_cache_mut(|cache| cache.wait_on(at, claim_park::me())),
                    );
                }
                Step::WaitScratch(site) => {
                    claim_park::wait(
                        site,
                        || !self.with_cache(|cache| cache.run_busy()),
                        || self.with_cache_mut(|cache| cache.wait_for_claim(claim_park::me())),
                    );
                }
                Step::WaitSet => {
                    claim_park::wait(
                        SET_WAIT,
                        || !self.with_cache(|cache| cache.set_in_flight(block)),
                        || self.with_cache_mut(|cache| cache.wait_for_claim(claim_park::me())),
                    );
                }
                Step::Go(mut claim) => {
                    let me = claim_park::me();
                    note_run_owner(Some(me.0));
                    mirror_scratch(false, true);
                    self.dev.read_at(claim.offset(), claim.scratch_mut());
                    let woken = critical_section::with(|cs| {
                        let mut cache = self.cache.borrow(cs).borrow_mut();
                        cache.end_run(claim)
                    });
                    note_run_end();
                    note_run_owner(None);
                    mirror_scratch(false, false);
                    // Outside every lock, the way every wake is handed.
                    claim_park::wake(woken);
                    return;
                }
            }
        }
    }

    /// The cache, for a read-only question asked outside the claim loops.
    fn with_cache<R>(&self, f: impl FnOnce(&cache::Cache) -> R) -> R {
        critical_section::with(|cs| f(&self.cache.borrow(cs).borrow()))
    }

    fn with_cache_mut<R>(&self, f: impl FnOnce(&mut cache::Cache) -> R) -> R {
        critical_section::with(|cs| f(&mut self.cache.borrow(cs).borrow_mut()))
    }

    fn read_bytes(&self, block: u64, off: usize, dst: &mut [u8]) {
        let block_size = self.block_size();
        assert!(
            off + dst.len() <= block_size,
            "read of {} bytes at {} runs past block {}",
            dst.len(),
            off,
            block
        );
        // The net under every decoding site. A block number that got this far
        // and is past the end of the volume means somewhere above read a
        // pointer the disk invented, and the somewhere that knows what to do
        // about that has already had its chance: `bmap_lookup` answers "hole",
        // `ext_load` answers `Corrupt`, the allocator skips the group. What
        // must not happen is the machine going down over a number, which is
        // what an `assert!` here would be - the same mistake the block driver
        // below would make of it, with a bare I/O error for an answer.
        debug_assert!(
            self.block_is_live(block),
            "ext2 block {block} is past the end of the filesystem ({} blocks)",
            self.sb.blocks_count
        );
        if !self.block_is_live(block) {
            dst.fill(0);
            return;
        }
        self.cached_block(block, |page| {
            dst.copy_from_slice(&page[off..off + dst.len()]);
        });
    }

    /// Read `run` **contiguous device blocks** beginning at `block` into `dst`,
    /// whose first `block_size - in_block` bytes are the tail of the first block.
    ///
    /// One device call when the device allows a run that wide (§10.5: the read
    /// path's own copies are why this exists - `read_bytes` per block is one
    /// device command per 4 KiB, and a 64 MiB sequential read is 16384 of them).
    /// Every block of the run is left **resident in the cache**, which is what
    /// makes the caller's next request for one of them a hit rather than a
    /// second read: the run is the caller's own demand, so nothing is fetched
    /// that was not asked for.
    fn read_run(&self, block: u64, run: usize, in_block: usize, dst: &mut [u8]) {
        let block_size = self.block_size();
        let head = (block_size - in_block).min(dst.len());
        // The head block and the whole of the contiguous run it asks for, in
        // one device call where the width allows - the claim made, the device
        // worked and the scatter landed before any of this runs, so every
        // block below is a hit.
        self.ensure_run(block, run);
        self.cached_block(block, |page| {
            dst[..head].copy_from_slice(&page[in_block..in_block + head]);
        });
        // The rest of the run: resident already where the run covered it, and
        // a one-block demand past its width, exactly as before.
        let mut at = head;
        for i in 1..run {
            if at >= dst.len() {
                break;
            }
            let take = block_size.min(dst.len() - at);
            self.cached_block(block + i as u64, |page| {
                dst[at..at + take].copy_from_slice(&page[..take]);
            });
            at += take;
        }
    }

    /// How far the **contiguous device run** beginning at `logical` goes, up to
    /// `cap` blocks. A walk of the extent tree in memory - `bmap_lookup` per
    /// block, no device read - so it is the *mapping* that decides whether a run
    /// is worth one call, and a hole or a jump to a different physical block ends
    /// it. A run of one is the answer for a file stored in scattered blocks, and
    /// the caller then behaves exactly as it did before this existed.
    fn contiguous_run(&self, data: &InodeData, logical: u64, cap: usize) -> usize {
        let Some(first) = self.bmap_lookup(data, logical) else {
            return 0;
        };
        let mut run = 1;
        while run < cap {
            match self.bmap_lookup(data, logical + run as u64) {
                Some(next) if next == first + run as u64 => run += 1,
                _ => break,
            }
        }
        run
    }

    /// The widest run this filesystem's device will load in one call - asked
    /// through the cache, because the bound is the device's own
    /// [`crate::blk::BlockDev::max_transfer`] clamped by the cache's scratch.
    fn run_width(&self) -> usize {
        let block_size = self.block_size();
        critical_section::with(|cs| {
            self.cache
                .borrow(cs)
                .borrow()
                .run_width(&*self.dev, usize::MAX, block_size)
        })
    }

    fn update_block(&self, block: u64, f: impl FnOnce(&mut [u8]) -> bool) -> bool {
        debug_assert!(
            self.block_is_live(block),
            "ext2 write to block {block}, past the end of the filesystem",
        );
        if !self.block_is_live(block) {
            // Nothing was read and nothing is written: a block the volume does
            // not have cannot be modified, and `false` is exactly what this
            // function already means when `f` declined to change anything. The
            // allocator's loop reads that as "not this group", which is the
            // right answer for a group whose descriptor pointed off the disk.
            return false;
        }
        let block_size = self.block_size();
        // Residency first, edit second - and nothing between them yields:
        // this task keeps the core from [`ensure_block`]'s return through the
        // borrow below, and on one core that is the whole guarantee that the
        // block is still where the ensure left it.
        self.ensure_block(block);
        let modified = critical_section::with(|cs| {
            let mut cache = self.cache.borrow(cs).borrow_mut();
            let at = cache
                .ready(block)
                .expect("ensure_block left the block resident");
            let page = cache.page_mut(at);
            if f(page) { Some(page.to_vec()) } else { None }
        });
        match modified {
            // `f` declined to modify.
            None => false,
            Some(bytes) => {
                // The device call, with no borrow of the cache anywhere in
                // this frame: the transfer may park, and a second task may
                // walk into the cache while it does. The disk holds every
                // modification before this returns, which is the whole of
                // write-through; what the seam between edit and write adds is
                // only a window in which a concurrent reader sees the new
                // cache bytes a few microseconds before the disk has them - a
                // window two writers of the same block were racing across
                // anyway.
                self.dev.write_at(block * block_size as u64, &bytes);
                true
            }
        }
    }

    fn write_bytes(&self, block: u64, off: usize, src: &[u8]) {
        self.update_block(block, |page| {
            page[off..off + src.len()].copy_from_slice(src);
            true
        });
    }

    fn read_inode(&self, ino: u32) -> InodeData {
        // An inode the volume cannot name reads as the zeroed record, which is
        // what an untouched table slot holds: no blocks, no links, size zero.
        // Every operation on it then fails the way an operation on an empty
        // file fails, instead of the way a read of a block outside the disk
        // fails.
        let Some((block, off)) = self.inode_pos(ino) else {
            return InodeData::parse(&[0u8; INODE_PARSED]);
        };
        let mut bytes = [0u8; INODE_PARSED];
        self.read_bytes(block, off, &mut bytes);
        InodeData::parse(&bytes)
    }

    fn write_inode(&self, ino: u32, data: &InodeData) {
        let Some((block, off)) = self.inode_pos(ino) else {
            return;
        };
        let mut bytes = [0u8; INODE_PARSED];
        data.store(&mut bytes);
        // An inode with extra space carries its own checksum - and every
        // field this driver just rewrote lives inside the fold, so the old
        // stored value is now a lie about these bytes. Recompute it from the
        // full on-disk record (its generation, its preserved tail), exactly
        // as the kernel's ext4_inode_csum_set would.
        let meta = self.sb.has_metadata_csum;
        let seed = self.sb.csum_seed;
        let isize_ = self.sb.inode_size as usize;
        self.update_block(block, |page| {
            page[off..off + INODE_PARSED].copy_from_slice(&bytes);
            if meta && isize_ > INODE_PARSED && off + isize_ <= page.len() {
                let rec = &mut page[off..off + isize_];
                // The checksum lives inside the extra area, so an inode that
                // arrives with none of it claimed (a lazy-initialized table
                // slot) claims the standard 32 bytes first - the same floor
                // mke2fs and the kernel give a fresh inode on a checksummed
                // volume, and the line M5's in-inode xattrs start above.
                if le16(rec, 128) == 0 {
                    let room = (isize_ - INODE_PARSED).min(32);
                    if room >= 4 {
                        set16(rec, 128, room as u16);
                    }
                }
                // Low half at 124 (a u16 in a word the format otherwise
                // leaves reserved), high half at 130 once the extra area
                // reaches that far - e2fsprogs' exact test.
                let (lo, hi) = self::csum::inode_csum(seed, ino, le32(rec, 100), rec);
                set16(rec, 124, lo);
                if le16(rec, 128) >= 4 {
                    set16(rec, 130, hi);
                }
            }
            true
        });
    }

    /// Whether any metadata checksum bookkeeping applies to this mount.
    fn csums_active(&self) -> bool {
        self.sb.has_metadata_csum || self.sb.has_gdt_csum
    }

    /// Stamp the leaf block's twelve-byte trailer: the fake dirent whose
    /// `rec_len` is 12 and whose `name_len` is the format's marker, with
    /// the covered-prefix checksum in its last four octets.
    /// Stamp an extent block's tail checksum. The tail sits at the end of the
    /// node's full capacity - header plus `eh_max` twelve-byte slots - and
    /// the last four of those bytes carry the answer, the fold running over
    /// everything before them. A block written this way needs no slot beyond
    /// its stated maximum, which is why the capacity, not the live count,
    /// places the tail. (The inode-embedded root is never stamped: at four
    /// entries its tail would land on `i_generation` itself, and neither
    /// mke2fs nor e2fsck treats that word as a checksum.)
    fn stamp_extent(&self, node: &mut [u8], ino: u32, genc: u32) {
        if !self.sb.has_metadata_csum {
            return;
        }
        let max = le16(node, 4) as usize;
        let tail = EXTENT_HEADER + max * EXTENT_ENTRY;
        if tail + 4 > node.len() {
            return;
        }
        let c = self::csum::ext_csum(self.sb.csum_seed, ino, genc, &node[..tail]);
        set32(node, tail, c);
    }

    /// Stamp the leaf block's twelve-byte trailer: the fake dirent at the
    /// end of the block - zero inode, `rec_len` 12, the name-length marker
    /// 0xDE00 - with the covered-prefix checksum in its last four octets.
    /// A walk sees the zero inode and steps over it; a checksum reads the
    /// bytes before it and folds the directory's own number and generation
    /// in place of what a real entry would have carried there.
    fn stamp_leaf_tail(&self, page: &mut [u8], dir_ino: u32, dir_gen: u32) {
        if !self.sb.has_metadata_csum {
            return;
        }
        let bs = page.len();
        page[bs - 12..bs - 8].fill(0);
        set16(page, bs - 8, 12);
        set16(page, bs - 6, 0xDE00);
        let c = self::csum::dir_csum(self.sb.csum_seed, dir_ino, dir_gen, page);
        set32(page, bs - 4, c);
    }

    /// How much of a leaf directory block entries may use: the whole block,
    /// or the block less its twelve-byte checksum trailer.
    pub(crate) fn leaf_limit(&self, block_size: usize) -> usize {
        if self.sb.has_metadata_csum {
            block_size - self::dirent::DIR_LEAF_TAIL
        } else {
            block_size
        }
    }

    /// Stamp an index block's tail: the entry array runs from the count/limit
    /// word for `limit` slots, and the tail is its last eight bytes - a
    /// reserved word, then the checksum folded over the directory's inode
    /// number and generation, the live array (count slots, the alias entry
    /// among them), the reserved word itself and a zero where the checksum
    /// will sit (the format folds its own absence in, then writes where it
    /// was).
    fn stamp_dx_tail(&self, page: &mut [u8], entries_at: usize, dir_ino: u32, dir_gen: u32) {
        if !self.sb.has_metadata_csum {
            return;
        }
        let limit = self::dx::dx_limit(page, entries_at);
        let count = self::dx::dx_count(page, entries_at);
        let tail = entries_at + limit * 8;
        let entries_end = entries_at + count * 8;
        if tail + 8 > page.len() || entries_end > tail {
            // An index that does not leave its tail eight bytes inside the
            // block is a tree this mount will not write a checksum into -
            // there is nowhere lawful to put it.
            return;
        }
        let head = [page[tail], page[tail + 1], page[tail + 2], page[tail + 3]];
        page[tail..tail + 4].fill(0);
        let c = self::csum::dx_csum(
            self.sb.csum_seed,
            dir_ino,
            dir_gen,
            page,
            entries_end,
            &head,
        );
        page[tail + 4..tail + 8].copy_from_slice(&c.to_le_bytes());
    }

    /// Walk the group descriptor table and confirm every filled-in checksum
    /// says what this mount's own arithmetic says. Zero halves mean
    /// "not written yet" and pass; a wrong one means the table lies about
    /// where its groups live, which is a mount to refuse rather than a
    /// read to hope through.
    fn verify_group_descriptors(&self) -> Result<(), &'static str> {
        let bs = self.block_size();
        let desc_size = self.sb.desc_size as usize;
        let mut page = vec![0u8; bs];
        for g in 0..self.groups {
            let at = g as usize * desc_size;
            let block = self.sb.first_data_block as u64 + 1 + (at / bs) as u64;
            let off = at % bs;
            if !self.block_is_live(block) {
                // The descriptor table is longer than the volume it describes.
                // The groups past the end cannot be read, so they cannot be
                // verified either; the walk stops there and the mount stands,
                // because an unreadable descriptor is no evidence that the
                // volume is bad - only that this one is out of reach.
                break;
            }
            self.dev.read_at(block * bs as u64, &mut page);
            let mut d = page[off..off + desc_size].to_vec();
            let stored = le16(&d, 30);
            if stored != 0 {
                let want = if self.sb.has_metadata_csum {
                    self::csum::gdt_csum(self.sb.csum_seed, g, &mut d)
                } else {
                    self::csum::gdt_csum16(&self.sb.uuid, g, &mut d)
                };
                if want != stored {
                    return Err("ext4 group descriptor checksum does not match");
                }
            }
            // The bitmap halves, when the descriptors point at bitmaps whose
            // checksums are filled: verify them too, so a silent bit-rot on
            // a bitmap block is caught at mount, not at the first free.
            let g0 = self.group(g);
            let blk_sum = le16(&d, 24) as u32
                | if desc_size >= 64 {
                    (le16(&d, 56) as u32) << 16
                } else {
                    0
                };
            if blk_sum != 0 && g0.block_bitmap != 0 && self.block_is_live(g0.block_bitmap) {
                self.dev.read_at(g0.block_bitmap * bs as u64, &mut page);
                let want = if self.sb.has_metadata_csum {
                    self::csum::bitmap_csum(self.sb.csum_seed, &page)
                } else {
                    u32::from(self::csum::bitmap_csum16(&self.sb.uuid, g, &page))
                };
                if want != blk_sum {
                    return Err("ext4 block bitmap checksum does not match");
                }
            }
        }
        Ok(())
    }

    /// Recompute everything a group's own bits depend on: the two bitmap
    /// checksum halves and the group descriptor checksum. Call it after any
    /// write that changed a bitmap or a descriptor's counts - the halves are
    /// stored in the descriptor, so one call leaves the group self-consistent.
    pub(crate) fn refresh_group_csums(&self, group: u32) {
        if !self.csums_active() {
            return;
        }
        let g = self.group(group);
        if !self.group_is_live(&g) {
            // A descriptor whose bitmaps are not on the disk has nothing to
            // checksum, and writing a checksum over the answer a read of an
            // absent block would have given is how a small piece of damage
            // becomes a volume that no longer mounts.
            return;
        }
        let bs = self.block_size();
        let mut blk = vec![0u8; bs];
        self.dev.read_at(g.block_bitmap * bs as u64, &mut blk);
        let ino_len = (self.sb.inodes_per_group as usize).div_ceil(8);
        let mut inob = vec![0u8; bs];
        self.dev.read_at(g.inode_bitmap * bs as u64, &mut inob);
        let (bsum, isum) = if self.sb.has_metadata_csum {
            (
                self::csum::bitmap_csum(self.sb.csum_seed, &blk),
                self::csum::bitmap_csum(self.sb.csum_seed, &inob[..ino_len]),
            )
        } else {
            (
                u32::from(self::csum::bitmap_csum16(&self.sb.uuid, group, &blk)),
                u32::from(self::csum::bitmap_csum16(
                    &self.sb.uuid,
                    group,
                    &inob[..ino_len],
                )),
            )
        };
        let desc_size = self.sb.desc_size as usize;
        let at = group as usize * desc_size;
        let desc_block = self.sb.first_data_block as u64 + 1 + (at / bs) as u64;
        let desc_off = at % bs;
        let meta = self.sb.has_metadata_csum;
        let seed = self.sb.csum_seed;
        let uuid = self.sb.uuid;
        self.update_block(desc_block, |page| {
            set16(page, desc_off + 24, bsum as u16);
            set16(page, desc_off + 26, isum as u16);
            if desc_size >= 64 {
                set16(page, desc_off + 56, (bsum >> 16) as u16);
                set16(page, desc_off + 58, (isum >> 16) as u16);
            }
            let d = &mut page[desc_off..desc_off + desc_size];
            let c = if meta {
                self::csum::gdt_csum(seed, group, d)
            } else {
                self::csum::gdt_csum16(&uuid, group, d)
            };
            set16(page, desc_off + 30, c);
            true
        });
    }

    /// Read `f`'s leaf directory block and, when the format asks for one,
    /// leave a valid trailer checksum behind in the fake twelve-byte tail at
    /// the block's end - the tail the dirent walk never names, whose
    /// `rec_len` of 12 and `name_len` of 0xDE00 are the format's own marks.
    /// Indexed blocks carry their tail where the index entries end, so the
    /// dx code fixes those at its own write sites; this is the leaf shape.
    fn update_dir_block(
        &self,
        block: u64,
        dir_ino: u32,
        dir_gen: u32,
        f: impl FnOnce(&mut [u8]) -> bool,
    ) -> bool {
        if !self.sb.has_metadata_csum {
            return self.update_block(block, f);
        }
        self.update_block(block, |page| {
            // An entry that still runs over the trailer's home gives the
            // overhang back first; then the edit, then the trailer whose
            // checksum covers whatever the block now holds.
            let trimmed = trim_dir_tail(page, page.len() - self::dirent::DIR_LEAF_TAIL);
            let wrote = f(page);
            if !wrote && !trimmed {
                return false;
            }
            self.stamp_leaf_tail(page, dir_ino, dir_gen);
            true
        })
    }
}

// ---- the VFS seam ----

/// An ext2 filesystem mounted through the VFS.
pub struct Ext4Fs {
    core: Arc<Core>,
}

impl FileSystem for Ext4Fs {
    fn root(&self) -> Arc<dyn Inode> {
        self.inode_of(ROOT_INO)
    }

    /// What `/proc/mounts` calls this filesystem. The driver is one, the name
    /// is not: an image that maps files with extents is what every other
    /// operating system calls ext4, and reporting it as ext2 tells `mount`
    /// and anything reading that file a lie about the disk underneath.
    fn name(&self) -> &'static str {
        if self.core.sb.has_extents {
            "ext4"
        } else {
            "ext2"
        }
    }

    fn read_only(&self) -> bool {
        self.core
            .read_only
            .load(core::sync::atomic::Ordering::Relaxed)
    }

    fn set_read_only(&self, read_only: bool) {
        self.core.set_read_only(read_only);
    }

    fn statfs(&self) -> Option<crate::vfs::FsStats> {
        // The free counters live on the group descriptors, the same place
        // every allocation bumps them (`bump_free`), so a df right after a
        // write storm reads what is actually on the disk.
        let mut free_blocks = 0u64;
        let mut free_inodes = 0u64;
        for g in 0..self.core.groups {
            let desc = self.core.group(g);
            free_blocks += desc.free_blocks as u64;
            free_inodes += desc.free_inodes as u64;
        }
        Some(crate::vfs::FsStats {
            bsize: self.core.block_size() as u64,
            blocks: self.core.sb.blocks_count,
            bfree: free_blocks,
            bavail: free_blocks,
            files: self.core.sb.inodes_count as u64,
            ffree: free_inodes,
        })
    }
}

impl Ext4Fs {
    /// Mount the filesystem on `dev`, validating the superblock. The device
    /// must already be quiescent; all state lives on the disk plus the
    /// write-through cache.
    pub fn mount(dev: Arc<dyn BlockDev>) -> Result<Ext4Fs, &'static str> {
        Ok(Ext4Fs {
            core: Arc::new(Core::mount(dev)?),
        })
    }

    pub fn inode_of(&self, ino: u32) -> Arc<dyn Inode> {
        critical_section::with(|cs| {
            let mut inodes = self.core.inodes.borrow(cs).borrow_mut();
            if let Some(cached) = inodes.get(&ino) {
                return cached.clone();
            }
            let inode = Arc::new(Ext4Ino {
                fs: self.core.clone(),
                ino,
            });
            inodes.insert(ino, inode.clone());
            inode
        })
    }
}

/// One inode of one ext2 mount: a pure handle. The on-disk header is read
/// through the cache on every operation, so two handles to the same inode
/// can never disagree, and there is nothing here to go stale.
pub struct Ext4Ino {
    fs: Arc<Core>,
    ino: u32,
}

impl Ext4Ino {
    fn wrap(&self, ino: u32) -> Arc<dyn Inode> {
        critical_section::with(|cs| {
            let mut inodes = self.fs.inodes.borrow(cs).borrow_mut();
            if let Some(cached) = inodes.get(&ino) {
                return cached.clone();
            }
            let inode = Arc::new(Ext4Ino {
                fs: self.fs.clone(),
                ino,
            });
            inodes.insert(ino, inode.clone());
            inode
        })
    }

    fn require_dir(&self) -> VfsResult<()> {
        if self.fs.read_inode(self.ino).kind() == NodeKind::Dir {
            Ok(())
        } else {
            Err(VfsError::NotDir)
        }
    }
}

impl Inode for Ext4Ino {
    fn metadata(&self) -> Metadata {
        let data = self.fs.read_inode(self.ino);
        let kind = data.kind();
        Metadata {
            ino: self.ino as Ino,
            kind,
            // The directory's inode size - the byte length of its dirent
            // blocks - is what Linux's own ext2 reports, and what `ls -l`
            // prints. Zeroing it made every directory read as empty of
            // content as a stat could suggest.
            size: data.size,
        }
    }

    /// The number this handle was built with: free, and the half of the
    /// process-exit key that must not become a device read.
    fn number(&self) -> u64 {
        self.ino as u64
    }

    fn mode(&self) -> u32 {
        (self.fs.read_inode(self.ino).mode as u32) & 0o7777
    }

    /// Every ext4 inode carries a mode, so a zero here is `chmod 000` rather
    /// than "nobody said" - the distinction a permission check turns on.
    fn keeps_modes(&self) -> bool {
        true
    }

    fn owner(&self) -> (u32, u32) {
        let data = self.fs.read_inode(self.ino);
        (data.uid as u32, data.gid as u32)
    }

    /// Store the owner, `chown`'s half of the model. The inode keeps sixteen
    /// bits of each id, so a larger one is kept as its low half rather than
    /// dropped: a filesystem that cannot hold the whole number should still
    /// answer with something the caller recognises.
    fn set_owner(&self, uid: u32, gid: u32) {
        let mut data = self.fs.read_inode(self.ino);
        data.uid = uid as u16;
        data.gid = gid as u16;
        self.fs.write_inode(self.ino, &data);
    }

    /// Store the permission bits - the low twelve - on the inode.
    ///
    /// This is what makes a package install real. `apk` writes each extracted
    /// file's mode through `open(O_CREAT, mode)` and through `chmod`, and a
    /// kernel that accepts both and stores neither hands every binary back as
    /// 0644: a setuid helper like `doas` or `sudo` then refuses to run at all -
    /// "not installed setuid" - and an executable script comes out
    /// non-executable, which is the same gate OpenRC's service check is built
    /// on. The bits above the twelve are the node's *kind*, which the inode
    /// already carries and this must not disturb.
    fn set_mode(&self, mode: u32) {
        let mut data = self.fs.read_inode(self.ino);
        data.mode = (data.mode & !0o7777) | (mode as u16 & 0o7777);
        self.fs.write_inode(self.ino, &data);
    }

    /// The inode's attribute flags: `i_flags` off the on-disk record, which is
    /// the word `lsattr` prints and the one `FS_IOC_GETFLAGS` returns. Every
    /// file this driver creates carries the extent bit, so `lsattr` shows the
    /// `e` that tells a reader the blocks are a tree.
    fn flags(&self) -> VfsResult<u32> {
        Ok(self.fs.read_inode(self.ino).flags)
    }

    /// Move them, refusing anything this kernel cannot keep.
    ///
    /// The word is *persisted* rather than remembered: a flag that a remount
    /// forgot would be a promise that lasted until the next boot, and the
    /// on-disk inode is where Linux keeps it too. The three bits the driver
    /// owns - the extent tree, the directory index, the block-counting unit -
    /// are carried over untouched by
    /// [`apply_flag_change`](crate::vfs::apply_flag_change), because they
    /// describe a layout already chosen rather than a promise a caller can make.
    ///
    /// The capability the two privileged flags need is checked by the caller,
    /// `syscall/linux/fs.rs`: it is a fact about the *caller*, and no
    /// filesystem can know it.
    fn set_flags(&self, flags: u32) -> VfsResult<()> {
        // A read-only mount refuses the write, as it refuses every other one.
        self.fs.writable()?;
        let mut data = self.fs.read_inode(self.ino);
        data.flags = crate::vfs::apply_flag_change(
            data.flags,
            flags,
            INODE_EXTENTS_FL | INODE_INDEX_FL | INODE_HUGE_FILE_FL,
        )?;
        self.fs.write_inode(self.ino, &data);
        Ok(())
    }

    /// Set the access and modification clocks, `utimensat`'s whole job. The
    /// change clock moves with the metadata, so it is stamped with the current
    /// time here rather than taken from the caller - exactly what Linux does,
    /// and what makes `ls -lc` show the touch and not the file's age.
    fn set_times(&self, atime: u64, mtime: u64) -> VfsResult<()> {
        let mut data = self.fs.read_inode(self.ino);
        data.atime = atime as u32;
        data.mtime = mtime as u32;
        data.ctime = now();
        self.fs.write_inode(self.ino, &data);
        Ok(())
    }

    /// The file's last write, off the on-disk inode. This is the number apk
    /// judges a cached index by, and the one `make` compares against its
    /// sources.
    fn mtime(&self) -> u64 {
        self.fs.read_inode(self.ino).mtime as u64
    }

    /// The read and metadata-change clocks, off the on-disk inode. ext2 keeps
    /// all three; a `stat` that reported mtime for them threw away the ctime
    /// the disk already had.
    fn atime(&self) -> u64 {
        self.fs.read_inode(self.ino).atime as u64
    }

    fn ctime(&self) -> u64 {
        self.fs.read_inode(self.ino).ctime as u64
    }

    /// `i_links_count`: how many names reach this inode. This is the whole
    /// point of a hard link as `stat` sees it - after `ln a b`, both names
    /// report two, and the file is only freed when the count reaches zero.
    fn nlink(&self) -> u64 {
        self.fs.read_inode(self.ino).links as u64
    }

    fn lookup(&self, name: &str) -> VfsResult<Arc<dyn Inode>> {
        self.require_dir()?;
        let dir = self.fs.read_inode(self.ino);
        let child = self.fs.dir_lookup(&dir, name)?.ok_or(VfsError::NoEntry)?;
        Ok(self.wrap(child))
    }

    /// `O_TMPFILE`: an inode with no name in it, born here and written whole
    /// before anything links it in. [`Inode::new_inode`] gives a new file one
    /// link for the name it is about to get, and this one gets no name - so
    /// the count is set back to zero, and [`Inode::link`] is what gives it
    /// the one it ends up living by.
    fn create_tmpfile(&self) -> VfsResult<Arc<dyn Inode>> {
        self.fs.writable()?;
        self.require_dir()?;
        let child = self.fs.new_inode(NodeKind::File, self.ino)?;
        let mut data = self.fs.read_inode(child);
        data.links = 0;
        self.fs.write_inode(child, &data);
        Ok(self.wrap(child))
    }

    /// Create a name in this directory.
    ///
    /// A device node is refused, and with it the number [`Inode::create`]
    /// carries: this layout keeps no `i_rdev`, so a device node made here would
    /// be a number this filesystem has nowhere to put rather than a device with
    /// one. `docs/uapi/` §5.1 item 71 names that as the honest half - the nodes
    /// a memory filesystem makes carry the pair, and this one makes none.
    fn create(
        &self,
        name: &str,
        kind: NodeKind,
        rdev: Option<(u32, u32)>,
    ) -> VfsResult<Arc<dyn Inode>> {
        self.fs.writable()?;
        if rdev.is_some() {
            return Err(VfsError::Unsupported);
        }
        if matches!(
            kind,
            NodeKind::CharDev | NodeKind::Symlink | NodeKind::Socket
        ) {
            // A character device is a driver's business, a link is made of a
            // target rather than a kind, and a socket's S_IFSOCK needs an
            // on-disk inode type this layout does not carry - none of them
            // are files this filesystem can invent from a name alone. The
            // sockets this kernel serves live on tmpfs, where `bind` has a
            // node kind to plant.
            return Err(VfsError::Unsupported);
        }
        self.require_dir()?;
        let mut dir = self.fs.read_inode(self.ino);
        if self.fs.dir_lookup(&dir, name)?.is_some() {
            return Err(VfsError::Exists);
        }
        let ftype = if kind == NodeKind::Dir {
            FT_DIR
        } else {
            FT_REG
        };
        let child = self.fs.new_inode(kind, self.ino)?;

        if kind == NodeKind::Dir {
            // The child's ".." is a link to us, on top of its own ".".
            dir.links += 1;
        }
        self.fs.dir_add(&mut dir, self.ino, name, child, ftype)?;
        self.fs.write_inode(self.ino, &dir);

        Ok(self.wrap(child))
    }

    /// Remove `name` from this directory, freeing what it named if that was
    /// the last name for it.
    ///
    /// Directories are refused: their link count is not just their name in
    /// us but their own "." and every child's "..", so taking one apart is
    /// the other half of an `rmdir` this filesystem does not have.
    fn unlink(&self, name: &str) -> VfsResult<()> {
        self.fs.writable()?;
        self.require_dir()?;
        let mut dir = self.fs.read_inode(self.ino);
        // Looked up before anything is touched, so a refusal leaves the
        // directory exactly as it was.
        let child = self.fs.dir_lookup(&dir, name)?.ok_or(VfsError::NoEntry)?;
        let mut data = self.fs.read_inode(child);
        if data.kind() == NodeKind::Dir {
            return Err(VfsError::IsDir);
        }
        // A directory entry naming an inode whose link count is already zero is
        // a damaged filesystem: ext2 never writes that state, and something that
        // did not finish - an install killed partway, a tool that wrote the
        // directory before the inode table - left it. Checked here, before
        // anything is touched, for the same reason the refusal above is: this
        // must not take a good directory entry away on its way to saying no.
        //
        // `data.links -= 1` on a zero is not a wrong answer, it is no answer:
        // in a debug build it stops the machine, and in a release build it
        // wraps to 65535 and leaks every block the inode maps - so a caller that
        // could have been told `EIO` instead finds out much later, and through
        // something else's corruption.
        if data.links == 0 {
            return Err(VfsError::Corrupt);
        }
        self.fs.dir_remove(&mut dir, self.ino, name)?;
        self.fs.write_inode(self.ino, &dir);

        data.links -= 1;
        if data.links == 0 {
            self.fs.free_inode(child);
        } else {
            self.fs.write_inode(child, &data);
        }
        Ok(())
    }

    /// Remove the empty directory `name`.
    ///
    /// The accounting is why this cannot be [`Self::unlink`] with a flag.
    /// A directory's link count is not one name but three kinds of them: its
    /// parent's entry, its own `.`, and each of its children's `..`. So an
    /// empty one still holds two, its parent holds one on its behalf - that is
    /// the `+1` [`Self::create`] gave it - and removing it has to give that
    /// one back. Everything is checked before anything is written, so a
    /// refusal leaves both directories exactly as they were.
    fn rmdir(&self, name: &str) -> VfsResult<()> {
        self.fs.writable()?;
        self.require_dir()?;
        let mut dir = self.fs.read_inode(self.ino);
        let child = self.fs.dir_lookup(&dir, name)?.ok_or(VfsError::NoEntry)?;
        let data = self.fs.read_inode(child);
        if data.kind() != NodeKind::Dir {
            return Err(VfsError::NotDir);
        }
        // `.` and `..` are entries of every directory and are not contents.
        // Anything else is something to lose, so the call stops here.
        let holds_something = self
            .fs
            .dir_entries(&data)?
            .iter()
            .any(|(entry, _)| entry != "." && entry != "..");
        if holds_something {
            return Err(VfsError::NotEmpty);
        }

        self.fs.dir_remove(&mut dir, self.ino, name)?;
        dir.links -= 1;
        self.fs.write_inode(self.ino, &dir);
        // The child's own two links - its name here and its `.` - went with
        // it, so nothing is left to decrement.
        self.fs.free_inode(child);
        Ok(())
    }

    /// Make a symbolic link called `name` holding `target`.
    ///
    /// The target is stored, not resolved: a link may point at a path that does
    /// not exist yet, and POSIX says making one is still allowed. What the
    /// format decides is *where* the text goes - inside the inode when it fits
    /// the footprint of the block pointers, in a data block when it does not -
    /// and that choice is invisible from outside, which is why `readlink` reads
    /// both the same way. Every package in a distribution that ships versioned
    /// libraries is built out of these: `libfoo.so.1` is a link to
    /// `libfoo.so.1.2.0`, and a filesystem that cannot make one cannot install
    /// a package.
    fn symlink(&self, name: &str, target: &str) -> VfsResult<Arc<dyn Inode>> {
        self.fs.writable()?;
        self.require_dir()?;
        let mut dir = self.fs.read_inode(self.ino);
        if self.fs.dir_lookup(&dir, name)?.is_some() {
            return Err(VfsError::Exists);
        }
        let child = self.fs.new_inode(NodeKind::Symlink, self.ino)?;
        let mut data = self.fs.read_inode(child);
        let bytes = target.as_bytes();
        if bytes.len() as u32 <= FAST_SYMLINK_MAX {
            // A fast link: the target *is* the block-pointer area, so no block
            // is ever allocated and `i_blocks` stays zero - which is the very
            // thing that says the target lives in the inode.
            let mut raw = [0u8; FAST_SYMLINK_MAX as usize];
            raw[..bytes.len()].copy_from_slice(bytes);
            for (i, ptr) in data.block.iter_mut().enumerate() {
                *ptr = u32::from_le_bytes(raw[i * 4..i * 4 + 4].try_into().expect("four bytes"));
            }
        } else {
            // Too long for the inode, so it reads like a file's contents - and
            // `readlink` reads it back that way.
            let (block, allocated) = self.fs.bmap_ensure(&mut data, self.ino, 0)?;
            if allocated {
                data.blocks += self.fs.block_units(data.flags);
            }
            self.fs.write_bytes(block, 0, bytes);
        }
        data.size = bytes.len() as u64;
        self.fs.write_inode(child, &data);

        self.fs
            .dir_add(&mut dir, self.ino, name, child, FT_SYMLINK)?;
        self.fs.write_inode(self.ino, &dir);
        Ok(self.wrap(child))
    }

    /// Hard-link `target` under `name`: the directory gains an entry naming
    /// an inode that already exists, and that inode's link count goes up.
    /// The one rule ext2 enforces where Linux enforces it too: no second
    /// name for a directory, or the tree would grow cycles it cannot walk.
    fn link(&self, name: &str, target: &Arc<dyn Inode>) -> VfsResult<()> {
        self.fs.writable()?;
        self.require_dir()?;
        let mut dir = self.fs.read_inode(self.ino);
        if self.fs.dir_lookup(&dir, name)?.is_some() {
            return Err(VfsError::Exists);
        }
        let child = target.metadata().ino as u32;
        let mut data = self.fs.read_inode(child);
        if data.kind() == NodeKind::Dir {
            return Err(VfsError::Invalid);
        }
        let ftype = match data.kind() {
            NodeKind::File => FT_REG,
            NodeKind::CharDev => FT_CHRDEV,
            NodeKind::Symlink => FT_SYMLINK,
            _ => FT_REG,
        };
        self.fs.dir_add(&mut dir, self.ino, name, child, ftype)?;
        self.fs.write_inode(self.ino, &dir);
        data.links += 1;
        self.fs.write_inode(child, &data);
        Ok(())
    }

    fn read_dir(&self) -> VfsResult<Vec<(String, NodeKind)>> {
        self.require_dir()?;
        let dir = self.fs.read_inode(self.ino);
        self.fs.dir_entries(&dir)
    }

    /// Where this link points, spelled exactly as the filesystem stored it.
    ///
    /// Alpine is built out of these: `/bin`, `/sbin`, `/usr/bin` and `/lib`
    /// hold a few hundred names, all of them links to one busybox or one
    /// loader. Resolution is what follows them; this is what says where.
    fn readlink(&self) -> VfsResult<String> {
        let data = self.fs.read_inode(self.ino);
        if data.kind() != NodeKind::Symlink {
            return Err(VfsError::Invalid);
        }
        let size = data.size as usize;
        if data.is_fast_symlink() {
            // No block was ever allocated: the target is the bytes of
            // i_block, read back as the text it was written as.
            let mut raw = Vec::with_capacity(size);
            for ptr in data.block.iter() {
                raw.extend_from_slice(&ptr.to_le_bytes());
            }
            raw.truncate(size);
            return String::from_utf8(raw).map_err(|_| VfsError::Invalid);
        }
        // A target too long for the inode is in data blocks and reads like a
        // file's contents - but through the block map directly, because
        // `read_at` refuses anything that is not a regular file.
        let block_size = self.fs.block_size() as u64;
        let mut raw = vec![0u8; size];
        let mut pos = 0usize;
        while pos < size {
            let logical = pos as u64 / block_size;
            let in_block = pos % block_size as usize;
            let n = (size - pos).min(block_size as usize - in_block);
            let Some(block) = self.fs.bmap_lookup(&data, logical) else {
                return Err(VfsError::Invalid);
            };
            self.fs.read_bytes(block, in_block, &mut raw[pos..pos + n]);
            pos += n;
        }
        String::from_utf8(raw).map_err(|_| VfsError::Invalid)
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> VfsResult<usize> {
        // `mut` for one reason: a read may move the access time (see the
        // `relatime` block at the end), and nothing else here touches the inode.
        let mut data = self.fs.read_inode(self.ino);
        let kind = data.kind();
        if kind != NodeKind::File {
            // A directory is a directory; a link is a name, and reading
            // through one is a caller that skipped resolution. Neither may
            // reach the block pointers - a link keeps its target *there*, so
            // reading it as data asks the device for a block number spelled
            // out of the link's own text.
            return Err(if kind == NodeKind::Dir {
                VfsError::IsDir
            } else {
                VfsError::Invalid
            });
        }
        let block_size = self.fs.block_size() as u64;
        let end = data.size.min(offset + buf.len() as u64);
        if offset >= end {
            return Ok(0);
        }
        let mut done = 0;
        let mut pos = offset;
        // How wide a run this device will load in one call - asked once, since
        // it depends on the device and the block size and not on where the read
        // is. `1` is a device that answers the page default, and then every
        // branch below is the per-block read this loop has always been.
        let width = self.fs.run_width();
        while pos < end {
            let logical = pos / block_size;
            let in_block = (pos % block_size) as usize;
            let n = ((end - pos) as usize).min(block_size as usize - in_block);
            match self.fs.bmap_lookup(&data, logical) {
                Some(block) => {
                    // The demand run: how many blocks this *request* still wants,
                    // and how far they are contiguous on the device. A read of
                    // one block asks for one block and gains nothing, which is
                    // the point - the width is the caller's, not a prefetch's.
                    let want = ((end - pos) as usize)
                        .div_ceil(block_size as usize)
                        .max(1)
                        .min(width);
                    let run = self.fs.contiguous_run(&data, logical, want);
                    let take = (run * block_size as usize - in_block).min((end - pos) as usize);
                    self.fs
                        .read_run(block, run, in_block, &mut buf[done..done + take]);
                    done += take;
                    pos += take as u64;
                }
                None => {
                    buf[done..done + n].fill(0);
                    done += n;
                    pos += n as u64;
                }
            }
        }
        // `relatime` - Linux's default, and the only shape of "update the
        // access time on read" that is not a device write per read. The access
        // time moves only when the recorded one has nothing to say since: it
        // predates the inode's own change time (so something has happened to
        // the file since it was last looked at), or it is more than a day old.
        // A read that is the second look at the same state therefore costs
        // nothing, which is what keeps this off the hot path §10.5 is about.
        if done > 0 {
            let stamp = now();
            const DAY: u32 = 86_400;
            // The inode's own `A` flag is asked first and it is not the same
            // question as the mount's `noatime`: this one is a promise about
            // *this file*, made by whoever set it, and it holds wherever the
            // filesystem happens to be mounted. A flag nothing reads would be a
            // flag that did nothing.
            if data.flags & crate::vfs::FS_NOATIME_FL == 0
                && (data.atime < data.ctime || data.atime + DAY < stamp)
            {
                data.atime = stamp;
                self.fs.write_inode(self.ino, &data);
            }
        }
        Ok(done)
    }

    fn write_at(&self, offset: u64, buf: &[u8]) -> VfsResult<usize> {
        self.fs.writable()?;
        let mut data = self.fs.read_inode(self.ino);
        let kind = data.kind();
        if kind != NodeKind::File {
            // The same refusal as a read, for the same reason: a link's block
            // pointers are its target, and writing "data" there would rewrite
            // where the link points.
            return Err(if kind == NodeKind::Dir {
                VfsError::IsDir
            } else {
                VfsError::Invalid
            });
        }
        if offset + buf.len() as u64 > u32::MAX as u64 {
            return Err(VfsError::FileTooBig);
        }
        let block_size = self.fs.block_size();
        let mut done = 0;
        let mut pos = offset;
        while done < buf.len() {
            let logical = pos / block_size as u64;
            let in_block = (pos % block_size as u64) as usize;
            let n = (block_size - in_block).min(buf.len() - done);
            let (block, allocated) = self.fs.bmap_ensure(&mut data, self.ino, logical)?;
            let src = &buf[done..done + n];
            self.fs.update_block(block, |page| {
                page[in_block..in_block + n].copy_from_slice(src);
                true
            });
            if allocated {
                data.blocks += self.fs.block_units(data.flags);
            }
            done += n;
            pos += n as u64;
        }
        data.size = data.size.max(offset + buf.len() as u64);
        // A write is what mtime is for, and ctime rides with it: the inode's
        // metadata - its size - changed too.
        data.mtime = now();
        data.ctime = data.mtime;
        self.fs.write_inode(self.ino, &data);
        Ok(done)
    }

    /// Resize, with the three cases done differently because they are
    /// different jobs.
    ///
    /// Growing changes the size and nothing else. A block that was never
    /// allocated reads as zeros - [`Self::read_at`] fills a hole - so there is
    /// no data to write.
    ///
    /// Shrinking has to give the tail's blocks back to the bitmaps. This
    /// driver can free *all* of an inode's blocks but has no "free the ones
    /// past N", so the prefix is held aside, the whole mapping is dropped, and
    /// the prefix is written back. Wasteful, and obviously correct, which is
    /// the right trade at this size. Zero - the common case, since it is what
    /// `O_TRUNC` and a replacing `rename` ask for - has no prefix to save and
    /// costs nothing extra.
    fn truncate(&self, len: u64) -> VfsResult<()> {
        self.fs.writable()?;
        let mut data = self.fs.read_inode(self.ino);
        match data.kind() {
            NodeKind::File => {}
            NodeKind::Dir => return Err(VfsError::IsDir),
            // A link's block pointers are its target: there is nothing here
            // to resize, and truncating it would rewrite where it points.
            _ => return Err(VfsError::Invalid),
        }
        let size = data.size;
        if len == size {
            return Ok(());
        }
        if len > size {
            data.size = len;
            self.fs.write_inode(self.ino, &data);
            return Ok(());
        }
        // Shrinking. The kept bytes never move - only the blocks past the new
        // end are given back - so a truncate is bounded by the tail, not the
        // file. Try the direct tail-free first; it covers the extent-mapped
        // files a real image (and `apk`) uses.
        let block_size = self.fs.block_size() as u64;
        let keep_blocks = len.div_ceil(block_size);
        if let Some(freed) = self.fs.free_blocks_after(&mut data, keep_blocks) {
            // Zero the bytes past the new end inside the last kept block, so a
            // later grow does not resurrect the truncated tail.
            let tail = (len % block_size) as usize;
            if tail > 0
                && let Some(block) = self.fs.bmap_lookup(&data, keep_blocks - 1)
            {
                self.fs.update_block(block, |page| {
                    for b in &mut page[tail..] {
                        *b = 0;
                    }
                    true
                });
            }
            data.blocks = data
                .blocks
                .saturating_sub((freed as u32).saturating_mul(self.fs.block_units(data.flags)));
            data.size = len;
            data.mtime = now();
            data.ctime = data.mtime;
            self.fs.write_inode(self.ino, &data);
            return Ok(());
        }
        // A mapping the tail-free does not cover (a deep extent tree, or a
        // block map): the buffering path. It reads the kept prefix into memory -
        // slow and heavy for a large file, but correct, and rare.
        let keep = if len == 0 {
            Vec::new()
        } else {
            let mut prefix = vec![0u8; len as usize];
            self.read_at(0, &mut prefix)?;
            prefix
        };
        self.fs.free_blocks_of(&data);
        self.fs.reset_mapping(&mut data);
        data.blocks = 0;
        data.size = 0;
        data.mtime = now();
        data.ctime = data.mtime;
        self.fs.write_inode(self.ino, &data);
        if !keep.is_empty() {
            self.write_at(0, &keep)?;
        }
        Ok(())
    }
}

#[cfg(all(test, not(target_os = "none")))]
mod tests {
    use super::*;
    use alloc::string::String;
    use alloc::vec;
    // The host test build links std, which the kernel itself never sees.
    extern crate std;

    /// An in-memory disk for host tests: a `Vec<u8>` behind the house lock.
    struct MemDev {
        bytes: Mutex<RefCell<Vec<u8>>>,
    }

    impl MemDev {
        fn new(blocks: usize) -> MemDev {
            MemDev {
                bytes: Mutex::new(RefCell::new(vec![0u8; blocks * 1024])),
            }
        }
    }

    impl BlockDev for MemDev {
        fn capacity_sectors(&self) -> u64 {
            critical_section::with(|cs| self.bytes.borrow(cs).borrow().len() as u64 / 512)
        }

        fn read_at(&self, offset: u64, buf: &mut [u8]) {
            critical_section::with(|cs| {
                let bytes = self.bytes.borrow(cs).borrow();
                let at = offset as usize;
                buf.copy_from_slice(&bytes[at..at + buf.len()]);
            });
        }

        fn write_at(&self, offset: u64, buf: &[u8]) {
            critical_section::with(|cs| {
                let mut bytes = self.bytes.borrow(cs).borrow_mut();
                let at = offset as usize;
                bytes[at..at + buf.len()].copy_from_slice(buf);
            });
        }
    }

    fn put16(bytes: &mut [u8], at: usize, v: u16) {
        bytes[at..at + 2].copy_from_slice(&v.to_le_bytes());
    }

    fn put32(bytes: &mut [u8], at: usize, v: u32) {
        bytes[at..at + 4].copy_from_slice(&v.to_le_bytes());
    }

    // Test image layout: block 0 boot, 1 superblock, 2 group descriptors,
    // 3 block bitmap, 4 inode bitmap, 5.. inode table (128-byte inodes),
    // then the root directory, then data. The table's length follows the
    // inode count, so the big-disk fixtures can hold enough inodes to fill
    // a directory index.
    const TABLE: usize = 5 * 1024;

    /// A minimal 1024-byte-block, one-group ext2 with an empty root
    /// directory. Enough structure to mount, and to hand-place files into.
    /// `total_blocks` sizes the disk; the small default keeps the original
    /// 128-block, 64-inode shape, the big ones grow the inode table to match
    /// the tests that fill a directory index.
    fn format_sized(total_blocks: usize) -> MemDev {
        let inodes = if total_blocks <= 128 { 64 } else { 1200 };
        let inode_blocks = inodes * 128 / 1024;
        let root_dir_block = 5 + inode_blocks;
        let used_blocks = root_dir_block; // blocks 1..=root_dir_block
        let dev = MemDev::new(total_blocks);
        critical_section::with(|cs| {
            let mut disk = dev.bytes.borrow(cs).borrow_mut();
            let disk = &mut *disk;
            let sb = 1024;
            put32(disk, sb, inodes as u32); // inodes_count
            put32(disk, sb + 4, total_blocks as u32); // blocks_count
            put32(disk, sb + 12, (total_blocks - used_blocks) as u32); // free_blocks_count
            put32(disk, sb + 16, (inodes - 2) as u32); // free_inodes_count
            put32(disk, sb + 20, 1); // first_data_block
            put32(disk, sb + 24, 0); // log_block_size: 1024
            put32(disk, sb + 28, 0); // fragments == blocks
            put32(disk, sb + 32, 8192); // blocks_per_group
            put32(disk, sb + 36, 8192); // frags_per_group
            put32(disk, sb + 40, inodes as u32); // inodes_per_group
            put16(disk, sb + 56, MAGIC);
            put16(disk, sb + 58, 0x0001); // s_state: cleanly unmounted
            put16(disk, sb + 58, 1); // clean
            put32(disk, sb + 76, 1); // rev 1
            put32(disk, sb + 84, 11); // first_ino
            put16(disk, sb + 88, 128); // inode_size
            put32(disk, sb + 96, INCOMPAT_FILETYPE);

            put32(disk, 2 * 1024, 3); // block bitmap
            put32(disk, 2 * 1024 + 4, 4); // inode bitmap
            put32(disk, 2 * 1024 + 8, 5); // inode table
            // The three counters are packed u16s at 12, 14 and 16 - free
            // blocks, free inodes, used directories. Getting the middle one
            // wrong does not corrupt anything visibly: the allocator reads a
            // group with "no free inodes", skips it, and every create then
            // answers ENOSPC on a filesystem that is almost empty.
            put16(disk, 2 * 1024 + 12, (total_blocks - used_blocks) as u16); // free blocks
            put16(disk, 2 * 1024 + 14, (inodes - 2) as u16); // free inodes
            put16(disk, 2 * 1024 + 16, 1); // used dirs (the root)

            // Blocks 1..=root_dir_block are metadata and the root directory.
            // On a 1 KiB filesystem block 0 lives outside every group, so
            // bitmap bit b is block b + 1.
            for bit in 0..used_blocks {
                disk[3 * 1024 + bit / 8] |= 1 << (bit % 8);
            }
            disk[4 * 1024] = 0b0000_0011; // inodes 1 (bad) and 2 (root)

            let root = TABLE + 128; // inode 2, index 1
            put16(disk, root, S_IFDIR | 0o755);
            put32(disk, root + 4, 1024); // size
            put16(disk, root + 26, 2); // links
            put32(disk, root + 28, 2); // blocks in sectors
            put32(disk, root + 40, root_dir_block as u32);

            // "." and "..", the latter owning the rest of the block.
            write_dirent(disk, root_dir_block * 1024, 2, dirent_len(1), ".", FT_DIR);
            write_dirent(
                disk,
                root_dir_block * 1024 + dirent_len(1),
                2,
                1024 - dirent_len(1),
                "..",
                FT_DIR,
            );
        });
        dev
    }

    fn format() -> MemDev {
        format_sized(128)
    }

    /// The raw word at `offset` in the on-disk inode `ino`.
    fn inode_word(dev: &MemDev, ino: u32, offset: usize) -> u32 {
        critical_section::with(|cs| {
            let disk = dev.bytes.borrow(cs).borrow();
            le32(&disk, TABLE + (ino as usize - 1) * 128 + offset)
        })
    }

    /// Convert an empty directory (nothing but "." and "..") into a one-leaf
    /// htree index, the shape a Linux conversion leaves behind: block 0
    /// becomes the index root and a fresh leaf - allocated by the caller at
    /// `leaf_block` - takes the (here: no) entries.
    fn make_indexed(dev: &MemDev, dir_ino: u32, leaf_block: u32) {
        critical_section::with(|cs| {
            let mut disk = dev.bytes.borrow(cs).borrow_mut();
            let disk = &mut *disk;
            let at = TABLE + (dir_ino as usize - 1) * 128;
            let base = le32(disk, at + 40) as usize * 1024;
            let parent = le32(disk, base + 12); // ".." keeps its inode

            // Inode: the flag goes on, the leaf joins the directory, and
            // block[1] names it.
            let flags = le32(disk, at + 32) | INODE_INDEX_FL;
            put32(disk, at + 32, flags);
            let size = le32(disk, at + 4) + 1024;
            put32(disk, at + 4, size);
            let sectors = le32(disk, at + 28) + 2;
            put32(disk, at + 28, sectors);
            put32(disk, at + 44, leaf_block);

            // The leaf: one hole covering the whole block.
            let leaf = leaf_block as usize * 1024;
            put16(disk, leaf + 4, 1024);

            // Block 0 becomes the index root.
            write_dirent(disk, base, dir_ino, dirent_len(1), ".", FT_DIR);
            write_dirent(
                disk,
                base + dirent_len(1),
                parent,
                1024 - dirent_len(1),
                "..",
                FT_DIR,
            );
            put32(disk, base + 24, 0); // reserved_zero
            disk[base + 28] = 1; // half_md4
            disk[base + 29] = 8; // info_length
            disk[base + 30] = 0; // indirect_levels
            disk[base + 31] = 0; // unused flags
            put16(disk, base + 32, dx_root_limit(1024, 8) as u16); // limit
            put16(disk, base + 34, 1); // count: the alias only
            put32(disk, base + 36, 1); // entry[0].block = the leaf

            // Bookkeeping: the leaf block is in use now.
            disk[3 * 1024 + (leaf_block as usize - 1) / 8] |= 1 << ((leaf_block - 1) % 8);
            for free in [2 * 1024 + 12, 1024 + 12] {
                let n = le16(disk, free) - 1;
                put16(disk, free, n);
            }
        });
    }

    /// Place a file by hand: inode, data block, both bitmaps, and a root
    /// directory entry carved out of the last entry's slack. Bypasses the
    /// filesystem entirely - the point is to read back host-written bytes.
    fn seed_file(dev: &MemDev, ino: u32, name: &str, data_block: u32, data: &[u8]) {
        critical_section::with(|cs| {
            let mut disk = dev.bytes.borrow(cs).borrow_mut();
            let disk = &mut *disk;

            let at = TABLE + (ino as usize - 1) * 128;
            put16(disk, at, S_IFREG | 0o644);
            put32(disk, at + 4, data.len() as u32);
            put16(disk, at + 26, 1);
            put32(disk, at + 28, 2);
            put32(disk, at + 40, data_block);
            disk[data_block as usize * 1024..data_block as usize * 1024 + data.len()]
                .copy_from_slice(data);

            disk[4 * 1024 + (ino as usize - 1) / 8] |= 1 << ((ino - 1) % 8);
            // Bitmap bit b is block b + 1 on this image (see format()).
            disk[3 * 1024 + (data_block as usize - 1) / 8] |= 1 << ((data_block - 1) % 8);
            for at in [2 * 1024 + 12, 1024 + 12] {
                let n = le16(disk, at) - 1;
                put16(disk, at, n);
            }
            for at in [2 * 1024 + 16, 1024 + 16] {
                let n = le16(disk, at) - 1;
                put16(disk, at, n);
            }

            // Walk to the entry that owns the end of the root block and take
            // its slack. The root's block comes from the root inode - the
            // big fixtures lay the table out differently.
            let root_block = le32(disk, TABLE + 128 + 40) as usize * 1024;
            let root = &mut disk[root_block..];
            let mut last = 0;
            loop {
                let rec = le16(root, last + 4) as usize;
                if last + rec >= 1024 {
                    break;
                }
                last += rec;
            }
            let own = dirent_len(root[last + 6] as usize);
            assert!(
                le16(root, last + 4) as usize >= own + dirent_len(name.len()),
                "no slack left for the seed"
            );
            set16(root, last + 4, own as u16);
            let rest = le16(root, last + 4) as usize - own;
            write_dirent(root, last + own, ino, rest, name, FT_REG);
        });
    }

    fn mounted(dev: Arc<dyn BlockDev>) -> Ext4Fs {
        Ext4Fs::mount(dev).expect("the test image mounts")
    }

    fn read_all(inode: &dyn Inode, len: usize) -> Vec<u8> {
        let mut buf = vec![0u8; len];
        let n = inode.read_at(0, &mut buf).expect("read_at works");
        buf.truncate(n);
        buf
    }

    #[test]
    fn mount_rejects_bytes_that_are_not_ext2() {
        assert!(Ext4Fs::mount(Arc::new(MemDev::new(128))).is_err());
    }

    /// Two superblock words say where the format's own bookkeeping starts, and
    /// both are consumed by subtraction on the way there: the recorded position
    /// of the superblock is the distance from its block's start to byte 1024,
    /// and the first inode group 0's allocator may offer is `s_first_ino` less
    /// one. A volume that names a block past the one holding its own
    /// superblock - the pair (1, 4096), or any number above 1 - or that claims
    /// to reserve nothing, is therefore not an unusual layout but an underflow,
    /// and `Super::parse` answers it as the refusal it is rather than as a
    /// mount that panics on its first read.
    #[test]
    fn a_superblock_that_underflows_its_own_geometry_is_refused() {
        for (at, value, what) in [
            (20, 2u32, "a first data block past the superblock's own"),
            (20, u32::MAX, "a first data block no block size reaches"),
            (84, 0u32, "a volume that reserves no inodes at all"),
        ] {
            let dev = format();
            critical_section::with(|cs| {
                let mut disk = dev.bytes.borrow(cs).borrow_mut();
                put32(&mut disk, 1024 + at, value);
            });
            assert!(
                Ext4Fs::mount(Arc::new(dev)).is_err(),
                "{what} mounted anyway"
            );
        }
    }

    /// The device is what makes `s_blocks_count` a bound worth trusting: every
    /// block number read later is checked against it, so a volume that claims
    /// more blocks than the disk holds does not merely describe itself
    /// wrongly - it makes every check that would catch a lying pointer agree
    /// with the lie. Refused at mount, the way Linux refuses it.
    #[test]
    fn a_volume_larger_than_its_device_is_refused() {
        let dev = format();
        // The fixture's device is 128 blocks of 1024 bytes; claim a thousand
        // times that, which is the shape of a number a broken or invented
        // superblock carries.
        critical_section::with(|cs| {
            let mut disk = dev.bytes.borrow(cs).borrow_mut();
            put32(&mut disk, 1024 + 4, 128 * 1000);
        });
        assert!(
            Ext4Fs::mount(Arc::new(dev)).is_err(),
            "a filesystem wider than its disk mounted"
        );
    }

    /// The same claim in the *high* half of a 64-bit `s_blocks_count`: the
    /// multiplication that turns blocks into bytes is the check, and it has to
    /// survive a number that overflows it rather than wrapping into the range
    /// the device can hold.
    #[test]
    fn a_block_count_that_overflows_its_own_byte_count_is_refused() {
        let dev = format();
        ext4_feature(&dev, 1024 + 96, INCOMPAT_64BIT);
        critical_section::with(|cs| {
            let mut disk = dev.bytes.borrow(cs).borrow_mut();
            put32(&mut disk, 1024 + 4, u32::MAX);
            put32(&mut disk, 1024 + 336, u32::MAX); // the high half of the count
        });
        assert!(
            Ext4Fs::mount(Arc::new(dev)).is_err(),
            "a block count whose byte count overflows mounted"
        );
    }
    /// values contain spaces and `=` (`--background=cyan`), consumed in many
    /// small chunked `read_at` calls and split on tabs. This is the exact byte
    /// path a corrupted theme read would ride through, so pinning it rules the
    /// filesystem in or out as the source of a garbled line.
    #[test]
    fn a_theme_shaped_file_reads_back_intact_under_chunked_reads() {
        let dev = Arc::new(format());
        let fs = Ext4Fs::mount(dev).expect("mount");
        let root = fs.root();
        let file = root
            .create("default.theme", NodeKind::File, None)
            .expect("create");
        let mut body: Vec<u8> = Vec::new();
        for name in [
            "fish_color_normal",
            "fish_color_search_match",
            "fish_pager_color_progress",
        ] {
            body.extend_from_slice(name.as_bytes());
            body.push(b'\t');
            body.extend_from_slice(b"brwhite --background=cyan --bold\n");
        }
        assert_eq!(file.write_at(0, &body).unwrap(), body.len());

        // Read it back the way a reader with a small buffer does.
        let mut got: Vec<u8> = Vec::new();
        let mut off = 0u64;
        loop {
            let mut chunk = [0u8; 13]; // deliberately not a block or line size
            let n = file.read_at(off, &mut chunk).unwrap();
            if n == 0 {
                break;
            }
            got.extend_from_slice(&chunk[..n]);
            off += n as u64;
        }
        assert_eq!(got, body, "the bytes come back exactly");

        // Split like `read -t`: the tab field before the value must be intact.
        let text = String::from_utf8_lossy(&got);
        let first_line = text.lines().next().unwrap();
        let toks: Vec<&str> = first_line.split('\t').collect();
        assert_eq!(toks[0], "fish_color_normal");
        assert_eq!(toks[1], "brwhite --background=cyan --bold");
    }

    #[test]
    fn a_seeded_file_and_a_hole_read_back_exactly() {
        let dev = Arc::new(format());
        seed_file(
            &dev,
            11,
            "hello.txt",
            14,
            b"hello from the host-written image\n",
        );
        // A sparse second file: first block unmapped, data in the second.
        critical_section::with(|cs| {
            let mut disk = dev.bytes.borrow(cs).borrow_mut();
            let disk = &mut *disk;
            let at = TABLE + 11 * 128; // inode 12
            put16(disk, at, S_IFREG | 0o644);
            put32(disk, at + 4, 2048);
            put16(disk, at + 26, 1);
            put32(disk, at + 28, 2);
            put32(disk, at + 44, 15); // i_block[1] only: block 0 is a hole
            disk[15 * 1024..15 * 1024 + 4].copy_from_slice(b"tail");
            disk[4 * 1024 + 1] |= 1 << ((12 - 1) % 8);
            disk[3 * 1024 + 14 / 8] |= 1 << (14 % 8); // bit 14 is block 15

            // hello.txt owns the block end; shrink it and insert behind it.
            let root = &mut disk[13 * 1024..];
            set16(root, 24 + 4, dirent_len(9) as u16);
            write_dirent(
                root,
                24 + dirent_len(9),
                12,
                1024 - 24 - dirent_len(9),
                "sparse",
                FT_REG,
            );
        });

        let fs = mounted(dev);
        let root = fs.root();
        let hello = root.lookup("hello.txt").expect("seeded file is found");
        assert_eq!(hello.metadata().size, 34);
        assert_eq!(
            read_all(&*hello, 64),
            b"hello from the host-written image\n".to_vec()
        );

        let sparse = root.lookup("sparse").expect("sparse file is found");
        let bytes = read_all(&*sparse, 2048);
        assert_eq!(bytes.len(), 2048);
        assert!(bytes[..1024].iter().all(|&b| b == 0), "the hole is zeros");
        assert_eq!(&bytes[1024..1028], b"tail");
        assert_eq!(root.lookup("missing").err(), Some(VfsError::NoEntry));
    }

    /// The byte pattern the QEMU gate writes to the real disk, mirrored here.
    fn pattern(i: usize) -> u8 {
        (i * 31 + 7) as u8
    }

    #[test]
    fn writes_span_direct_and_indirect_and_survive_a_remount() {
        const LEN: usize = 20 * 1024; // 12 direct blocks + 8 through an indirect
        let dev = Arc::new(format());
        let fs = mounted(dev.clone());
        let root = fs.root();
        let out = root
            .create("out.bin", NodeKind::File, None)
            .expect("create");
        let bytes: Vec<u8> = (0..LEN).map(pattern).collect();
        assert_eq!(out.write_at(0, &bytes).unwrap(), LEN);
        // Rewriting a middle chunk in place must not disturb its neighbours.
        assert_eq!(out.write_at(5 * 1024, b"MIDDLE").unwrap(), 6);

        drop(out);
        drop(root);
        drop(fs);

        // A fresh mount over the same device: cold cache, the disk is truth.
        let fs2 = mounted(dev);
        let back = fs2
            .root()
            .lookup("out.bin")
            .expect("the file is on the disk");
        assert_eq!(back.metadata().size as usize, LEN);
        let read_back = read_all(&*back, LEN);
        assert_eq!(&read_back[5 * 1024..5 * 1024 + 6], b"MIDDLE");
        for (i, &b) in read_back.iter().enumerate() {
            if (5 * 1024..5 * 1024 + 6).contains(&i) {
                continue;
            }
            assert_eq!(b, pattern(i), "byte {i} (block {})", i / 1024);
        }
    }

    #[test]
    fn directory_growth_uses_new_blocks_when_slack_runs_out() {
        let dev = Arc::new(format());
        let fs = mounted(dev);
        let root = fs.root();
        // Names long enough that a handful fill one block: real length
        // align4(8 + 60) = 68, so a block holds ~14 entries before a second
        // directory block must be allocated.
        let names: Vec<String> = (0..40)
            .map(|i| alloc::format!("f{i:02}-{}", "x".repeat(54)))
            .collect();
        for name in &names {
            root.create(name, NodeKind::File, None)
                .unwrap_or_else(|e| panic!("create {name}: {e:?}"));
        }
        for name in &names {
            root.lookup(name)
                .unwrap_or_else(|e| panic!("lookup {name}: {e:?}"));
        }

        assert_eq!(root.lookup(&names[0]).unwrap().metadata().ino, 11);
        let root_data = fs.core.read_inode(ROOT_INO);
        assert!(root_data.size > 1024, "the directory grew");
        assert!(
            root_data.block[1] != 0,
            "a second directory block was mapped"
        );
    }

    #[test]
    fn truncating_to_zero_leaves_no_tail_and_frees_the_blocks() {
        let dev = Arc::new(format());
        assert_eq!(free_counts(&dev), (115, 62));
        let fs = mounted(dev.clone());
        let root = fs.root();
        let file = root.create("log", NodeKind::File, None).expect("create");
        // Three blocks' worth, so the tail is real blocks rather than bytes
        // inside the block the head lives in.
        let long = vec![b'x'; 3 * 1024];
        file.write_at(0, &long).unwrap();
        assert_eq!(free_counts(&dev), (115 - 3, 61), "three data blocks");

        file.truncate(0).expect("truncate to zero");
        assert_eq!(file.metadata().size, 0);
        assert_eq!(read_all(&*file, 16).len(), 0, "an empty file reads empty");
        assert_eq!(free_counts(&dev), (115, 61), "the blocks came back");

        // The inode survived - only the contents went - so the same name can
        // be written again without having to be created again.
        assert_eq!(file.write_at(0, b"fresh").unwrap(), 5);
        assert_eq!(read_all(&*file, 16), b"fresh");
    }

    #[test]
    fn set_times_stamps_the_access_and_modification_clocks() {
        let dev = Arc::new(format());
        let fs = mounted(dev.clone());
        let root = fs.root();
        let file = root.create("stamp", NodeKind::File, None).expect("create");
        let ctime_before = file.ctime();
        file.set_times(111, 222).expect("set_times");
        assert_eq!(file.atime(), 111, "st_atime took the caller's value");
        assert_eq!(file.mtime(), 222, "st_mtime took the caller's value");
        // ctime tracks the metadata change, not either word the caller set -
        // which is what makes `ls -lc` show the touch, not the file's age.
        assert_ne!(file.ctime(), ctime_before, "ctime moved with the change");
        assert_ne!(file.ctime(), 111);
        assert_ne!(file.ctime(), 222);
    }

    #[test]
    fn a_read_bumps_atime_once_and_relatime_leaves_the_rest_alone() {
        // Linux's default mount is `relatime`: a read moves the access time only
        // when the recorded one predates the inode's change time, or is a day
        // old. The rule is not decoration - the access time lives in the inode,
        // so a read that always updates it is a read that always writes to the
        // device, which is the read path §10.5 exists to make faster.
        let dev = Arc::new(format());
        let fs = mounted(dev.clone());
        let root = fs.root();
        let file = root
            .create("watched", NodeKind::File, None)
            .expect("create");
        file.write_at(0, b"payload").unwrap();
        let stale = file.atime();
        assert!(
            stale < file.ctime(),
            "the write moved ctime past an atime nothing has read since"
        );

        // The first read sees a file that has changed since it was last looked
        // at, so the stamp moves.
        assert_eq!(read_all(&*file, 16), b"payload");
        let first = file.atime();
        assert!(first > stale, "a read after a change bumps atime");

        // The second read sees nothing new: same inode, no change in between, so
        // neither the stamp nor the disk is touched.
        assert_eq!(read_all(&*file, 16), b"payload");
        assert_eq!(file.atime(), first, "a second read moves nothing");

        // A write re-opens the gap, and the next read answers it again.
        file.write_at(0, b"PAYLOAD").unwrap();
        assert_eq!(read_all(&*file, 16), b"PAYLOAD");
        assert!(
            file.atime() > first,
            "a read after a fresh write bumps again"
        );
    }

    #[test]
    fn truncating_a_prefix_keeps_the_prefix_and_drops_the_rest() {
        let dev = Arc::new(format());
        let fs = mounted(dev.clone());
        let root = fs.root();
        let file = root.create("cut", NodeKind::File, None).expect("create");
        file.write_at(0, &vec![b'a'; 2500]).unwrap();
        file.write_at(2500, b"tail that goes away").unwrap();

        file.truncate(2500).expect("truncate the tail off");
        assert_eq!(file.metadata().size, 2500);
        let bytes = read_all(&*file, 4096);
        assert_eq!(bytes.len(), 2500);
        assert!(bytes.iter().all(|&b| b == b'a'), "the prefix is intact");
        // The tail's blocks went back to the bitmaps, and the prefix's were
        // reallocated: the file still occupies what it holds, no more.
        assert_eq!(free_counts(&dev), (115 - 3, 61));
    }

    #[test]
    fn truncating_upward_grows_a_hole_that_reads_as_zeros() {
        let fs = mounted(Arc::new(format()));
        let root = fs.root();
        let file = root.create("grow", NodeKind::File, None).expect("create");
        file.write_at(0, b"head").unwrap();
        file.truncate(4096).expect("grow");

        assert_eq!(file.metadata().size, 4096);
        let bytes = read_all(&*file, 8192);
        assert_eq!(bytes.len(), 4096);
        assert_eq!(&bytes[..4], b"head");
        assert!(
            bytes[4..].iter().all(|&b| b == 0),
            "the new space is a hole"
        );
    }

    #[test]
    fn truncate_refuses_what_is_not_a_file() {
        let fs = mounted(Arc::new(format()));
        let root = fs.root();
        let dir = root.create("d", NodeKind::Dir, None).expect("create dir");
        assert_eq!(dir.truncate(0).err(), Some(VfsError::IsDir));
        assert_eq!(root.truncate(0).err(), Some(VfsError::IsDir));
    }

    #[test]
    fn a_symlink_puts_its_target_where_it_fits_and_reads_it_back() {
        let dev = Arc::new(format());
        assert_eq!(free_counts(&dev), (115, 62));
        let fs = mounted(dev.clone());
        let root = fs.root();

        // A target that fits the inode's block-pointer area lives there, and
        // costs nothing but the inode - which is the format's fast link, and
        // what `libfoo.so.1 -> libfoo.so.1.2.0` is in every package ever built.
        let fast = root.symlink("short", "/bin/busybox").expect("fast link");
        assert_eq!(fast.metadata().kind, NodeKind::Symlink);
        assert_eq!(fast.readlink().unwrap(), "/bin/busybox");
        assert_eq!(fast.metadata().size, 12);
        assert_eq!(free_counts(&dev), (115, 61), "an inode, but no data block");

        // One that does not fit gets a block, and reads back identically - the
        // choice is invisible from outside, which is the point.
        let long = "/usr/libexec/some/very/long/path/that/does/not/fit/anywhere/busybox";
        assert!(long.len() > 60, "the long case has to be long");
        let slow = root.symlink("long", long).expect("slow link");
        assert_eq!(slow.readlink().unwrap(), long);
        assert_eq!(slow.metadata().size, long.len() as u64);
        assert_eq!(free_counts(&dev), (114, 60), "and one block for the target");

        // The name is taken; the target does not have to exist.
        assert_eq!(
            root.symlink("short", "/elsewhere").err(),
            Some(VfsError::Exists)
        );

        // A cold mount is the only witness that this reached the disk.
        drop(fast);
        drop(slow);
        drop(root);
        drop(fs);
        let fs2 = mounted(dev);
        let root2 = fs2.root();
        assert_eq!(
            root2.lookup("short").unwrap().readlink().unwrap(),
            "/bin/busybox"
        );
        assert_eq!(root2.lookup("long").unwrap().readlink().unwrap(), long);

        // And a link is not a file: writing through one, or resizing it, would
        // rewrite where it points.
        let link = root2.lookup("long").unwrap();
        assert_eq!(link.write_at(0, b"x").err(), Some(VfsError::Invalid));
        assert_eq!(link.truncate(0).err(), Some(VfsError::Invalid));
        assert_eq!(
            link.read_at(0, &mut [0u8; 4]).err(),
            Some(VfsError::Invalid)
        );
    }

    #[test]
    fn unlinking_a_symlink_gives_back_only_what_it_took() {
        let dev = Arc::new(format());
        assert_eq!(free_counts(&dev), (115, 62));
        let fs = mounted(dev.clone());
        let root = fs.root();

        // A fast link's "block pointers" are the text of its target. Freeing
        // them as if they were block numbers takes blocks out of whatever else
        // is on the filesystem - or, when the text is not a plausible number,
        // walks off the group table and panics. `/bin/busybox` reads as one.
        root.symlink("fast", "/bin/busybox").expect("fast link");
        assert_eq!(free_counts(&dev), (115, 61), "an inode, and nothing else");
        root.unlink("fast").expect("unlink the fast link");
        assert_eq!(
            free_counts(&dev),
            (115, 62),
            "the inode came back and no block was ever touched"
        );

        // The same for a link whose target needed a block: that block *is* its
        // own, and does come back.
        let long = "/usr/libexec/some/very/long/path/that/does/not/fit/in/an/inode/busybox";
        root.symlink("slow", long).expect("slow link");
        assert_eq!(free_counts(&dev), (114, 61), "the block and the inode");
        root.unlink("slow").expect("unlink the slow link");
        assert_eq!(free_counts(&dev), (115, 62), "both came back");
    }

    #[test]
    fn rmdir_takes_an_empty_directory_and_puts_the_books_back() {
        let dev = Arc::new(format());
        assert_eq!(free_counts(&dev), (115, 62));
        let fs = mounted(dev.clone());
        let root = fs.root();
        let root_links = fs.core.read_inode(ROOT_INO).links;

        let dir = root.create("d", NodeKind::Dir, None).expect("create dir");
        // A new subdirectory is a name in its parent *and* a `..` pointing
        // back at it, so the parent gains a link as well as the child.
        assert_eq!(fs.core.read_inode(ROOT_INO).links, root_links + 1);
        assert_eq!(free_counts(&dev), (114, 61), "one block and one inode");

        // Not empty, and it must say so rather than take what is inside with
        // it: that is the whole difference between `rmdir` and a recursive
        // delete wearing the wrong name.
        let inner = dir
            .create("inner", NodeKind::File, None)
            .expect("create inner");
        inner.write_at(0, b"x").unwrap();
        assert_eq!(root.rmdir("d").err(), Some(VfsError::NotEmpty));
        assert!(root.lookup("d").is_ok(), "the directory is still there");

        // Emptying it is the caller's job, one name at a time.
        dir.unlink("inner").expect("unlink what was inside");
        root.rmdir("d").expect("rmdir the empty directory");
        assert_eq!(
            fs.core.read_inode(ROOT_INO).links,
            root_links,
            "the parent's link came back"
        );
        assert_eq!(
            free_counts(&dev),
            (115, 62),
            "and so did the child's block and inode"
        );

        // A file is not a directory, and a name that is not there is not one.
        root.create("f", NodeKind::File, None).expect("create file");
        assert_eq!(root.rmdir("f").err(), Some(VfsError::NotDir));
        assert_eq!(root.rmdir("gone").err(), Some(VfsError::NoEntry));
    }

    #[test]
    fn a_write_past_eof_leaves_zeros_in_the_gap() {
        let dev = Arc::new(format());
        let fs = mounted(dev);
        let root = fs.root();
        let file = root.create("sparse", NodeKind::File, None).expect("create");
        assert_eq!(file.write_at(5000, b"abc").unwrap(), 3);
        assert_eq!(file.metadata().size, 5003);

        let bytes = read_all(&*file, 5010);
        assert_eq!(bytes.len(), 5003);
        assert!(bytes[..5000].iter().all(|&b| b == 0), "the gap is zeros");
        assert_eq!(&bytes[5000..], b"abc");
    }

    #[test]
    fn creating_twice_is_exists_and_directories_get_dot_entries() {
        let dev = Arc::new(format());
        let fs = mounted(dev.clone());
        let root = fs.root();
        let dir = root.create("d", NodeKind::Dir, None).expect("create dir");
        assert_eq!(
            root.create("d", NodeKind::Dir, None).err(),
            Some(VfsError::Exists)
        );

        let inner = dir
            .create("f", NodeKind::File, None)
            .expect("create in dir");
        assert_eq!(inner.write_at(0, b"nested").unwrap(), 6);
        assert_eq!(dir.metadata().kind, NodeKind::Dir);
        // A directory's size is the byte length of its dirent blocks - what
        // Linux's own ext2 reports and `ls -l` prints. A fresh directory
        // holds "." and ".." in its one first block, so the size is the
        // block size. Zeroing it (as this test once asserted) made every
        // directory read as empty of content as a stat could suggest.
        assert_eq!(dir.metadata().size, fs.core.block_size() as u64);

        // "." and ".." resolve to the right inodes, through the real parser.
        assert_eq!(
            dir.lookup(".").expect(".").metadata().ino,
            dir.metadata().ino
        );
        assert_eq!(
            dir.lookup("..").expect("..").metadata().ino,
            root.metadata().ino
        );

        // The nested file is reachable from the root on a fresh mount, which
        // also proves the directory metadata hit the disk.
        drop(inner);
        drop(dir);
        drop(root);
        drop(fs);
        let fs2 = mounted(dev);
        let found = fs2.root().lookup("d").unwrap().lookup("f").unwrap();
        let mut buf = [0u8; 6];
        assert_eq!(found.read_at(0, &mut buf).unwrap(), 6);
        assert_eq!(&buf, b"nested");
    }

    #[test]
    fn read_dir_walks_every_entry_including_the_dots() {
        let fs = mounted(Arc::new(format()));
        let root = fs.root();
        root.create("note", NodeKind::File, None)
            .expect("create file");
        root.create("docs", NodeKind::Dir, None)
            .expect("create dir");

        let listed = root.read_dir().expect("list the root");
        // "." and ".." come with the block, as POSIX readdir reports them.
        assert_eq!(listed.len(), 4, "got {listed:?}");
        assert!(listed.contains(&(String::from("."), NodeKind::Dir)));
        assert!(listed.contains(&(String::from(".."), NodeKind::Dir)));
        assert!(listed.contains(&(String::from("note"), NodeKind::File)));
        assert!(listed.contains(&(String::from("docs"), NodeKind::Dir)));

        // A file lists nothing.
        let file = root.lookup("note").unwrap();
        assert_eq!(file.read_dir().unwrap_err(), VfsError::NotDir);
    }

    /// The image's own free counts, as the superblock reports them.
    fn free_counts(dev: &MemDev) -> (u32, u32) {
        critical_section::with(|cs| {
            let disk = dev.bytes.borrow(cs).borrow();
            (le32(&disk, 1024 + 12), le32(&disk, 1024 + 16))
        })
    }

    #[test]
    fn unlink_gives_the_name_the_blocks_and_the_inode_back() {
        const LEN: usize = 20 * 1024; // 12 direct blocks + 8 through one indirect
        let dev = Arc::new(format());
        assert_eq!(free_counts(&dev), (115, 62));

        let fs = mounted(dev.clone());
        let root = fs.root();
        let out = root
            .create("out.bin", NodeKind::File, None)
            .expect("create");
        let bytes: Vec<u8> = (0..LEN).map(pattern).collect();
        assert_eq!(out.write_at(0, &bytes).unwrap(), LEN);
        assert_eq!(free_counts(&dev), (115 - 21, 61), "20 data, one indirect");

        root.unlink("out.bin").expect("unlink");
        assert_eq!(
            free_counts(&dev),
            (115, 62),
            "every block and the inode went back to the bitmaps"
        );

        drop(out);
        drop(root);
        drop(fs);
        // A cold mount is the only witness that can tell the disk from the
        // cache of the mount that did the writing.
        let fs2 = mounted(dev);
        assert_eq!(
            fs2.root().lookup("out.bin").err(),
            Some(VfsError::NoEntry),
            "the name left the disk"
        );
    }

    #[test]
    fn a_removed_name_can_be_created_again() {
        let dev = Arc::new(format());
        let fs = mounted(dev);
        let root = fs.root();
        let file = root
            .create("scratch", NodeKind::File, None)
            .expect("create");
        assert_eq!(file.write_at(0, b"first").unwrap(), 5);
        root.unlink("scratch").expect("unlink");

        // What a second boot over the same image does: the create must not
        // answer Exists, and the inode it is handed must be a clean one.
        let again = root
            .create("scratch", NodeKind::File, None)
            .expect("create the same name again");
        assert_eq!(again.metadata().size, 0, "the reused inode starts empty");
        assert_eq!(read_all(&*again, 16), Vec::new());
    }

    #[test]
    fn unlink_refuses_directories_and_unknown_names() {
        let fs = mounted(Arc::new(format()));
        let root = fs.root();
        root.create("d", NodeKind::Dir, None).expect("create dir");
        assert_eq!(root.unlink("d").err(), Some(VfsError::IsDir));
        assert_eq!(root.unlink("nope").err(), Some(VfsError::NoEntry));

        // Neither refusal touched anything.
        root.lookup("d").expect("the directory is still there");
        assert!(
            root.read_dir()
                .expect("list the root")
                .contains(&(String::from("d"), NodeKind::Dir))
        );
    }

    #[test]
    fn directory_entry_removal_hands_the_space_to_the_entry_before() {
        let mut page = [0u8; 64];
        write_dirent(&mut page, 0, 2, 12, ".", FT_DIR);
        write_dirent(&mut page, 12, 2, 24, "..", FT_DIR);
        write_dirent(&mut page, 36, 7, 28, "gone", FT_REG);

        let mut found = None;
        assert!(dir_take(&mut page, "gone", &mut found));
        assert_eq!(found, Some(7));
        // ".." grew over the gap; "." never moved.
        assert_eq!(le16(&page, 16), 52);
        assert_eq!(le16(&page, 4), 12);
        let mut found = None;
        assert!(!dir_take(&mut page, "gone", &mut found), "it is gone");

        // The first entry of a block has nothing before it, so it becomes a
        // hole: the zero inode a walk skips and a later insert reuses.
        let mut found = None;
        assert!(dir_take(&mut page, ".", &mut found));
        assert_eq!(found, Some(2));
        assert_eq!(le32(&page, 0), 0);
        let mut seen = Vec::new();
        dir_walk(&page, |ino, _ft, name| {
            seen.push((ino, String::from_utf8(name.to_vec()).expect("utf8")));
            true
        });
        assert_eq!(seen, vec![(2u32, String::from(".."))]);
    }

    #[test]
    fn directory_entry_insertion_carves_slack_exactly() {
        // 64 bytes: "." (12) then ".." owning the rest (52). Entries insert
        // into live entries' slack, and each new entry's record spans what
        // is left, exactly as ext2 lays them out.
        let mut page = [0u8; 64];
        write_dirent(&mut page, 0, 2, dirent_len(1), ".", FT_DIR);
        write_dirent(
            &mut page,
            dirent_len(1),
            2,
            64 - dirent_len(1),
            "..",
            FT_DIR,
        );

        {
            let lim = page.len();
            assert!(dir_try_insert(&mut page, lim, "newly", 7, FT_REG));
        }
        assert_eq!(le32(&page, 24), 7, "inserted behind ..");
        assert_eq!(le16(&page, 28), 64 - 24, "its record owns the rest");
        assert_eq!(page[30], 5, "name_len");
        assert_eq!(le16(&page, 16), 12, ".. shrank to its real length");

        // A second one shrinks the first inserted entry instead.
        {
            let lim = page.len();
            assert!(dir_try_insert(&mut page, lim, "ab", 8, FT_REG));
        }
        assert_eq!(le32(&page, 40), 8);
        assert_eq!(le16(&page, 44), 24, "the new entry owns what remained");
        assert_eq!(le16(&page, 28), 16, "newly shrank to its real length");

        // Nothing left but "ab"'s 12-byte slack: a 20-byte entry cannot fit.
        {
            let lim = page.len();
            assert!(!dir_try_insert(&mut page, lim, "toolongname", 9, FT_REG));
        }
        // But a 12-byte one fits exactly, taking the whole remainder.
        {
            let lim = page.len();
            assert!(dir_try_insert(&mut page, lim, "cd", 9, FT_REG));
        }
        assert_eq!(le16(&page, 44), 12, "ab shrank to its real length");
        assert_eq!(le32(&page, 52), 9);
        assert_eq!(le16(&page, 56), 12, "cd owns what ab's record gave up");
    }

    /// apk's install, replayed in the order that broke it: a package lays
    /// every file down under a `.apk.<digest>` temp name and commits them
    /// one by one, and within a directory the `.so` symlink sorts before the
    /// real library it points at. So the symlink's rename runs while its
    /// target still lives under its own temp name - resolving the link would
    /// go looking for the target's real name and die with ENOENT, which is
    /// exactly what `apk add eza` showed: every regular file committed,
    /// every symlink failed. A rename must move the name un-followed.
    #[test]
    fn an_apk_style_symlink_commit_resolves() {
        let fs = mounted(Arc::new(format()));
        let vfs = alloc::sync::Arc::new(crate::vfs::mount::MountTable::new());
        vfs.mount_root(alloc::sync::Arc::new(fs));

        let root = vfs.resolve("/").expect("root");
        root.create("usr", NodeKind::Dir, None).expect("usr");
        let usr = root.lookup("usr").expect("usr");
        usr.create("lib", NodeKind::Dir, None).expect("lib");
        let lib = usr.lookup("lib").expect("lib");

        let tmpname = ".apk.0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        let bodytmp = ".apk.1111111111111111111111111111111111111111111111111111111111111111";

        // Extract: the symlink first - dangling, because the library under
        // it has not been laid down yet - then the library's own temp file.
        lib.symlink(tmpname, "libllhttp.so.9.4.1")
            .expect("symlinkat the temp name");
        let body = lib
            .create(bodytmp, NodeKind::File, None)
            .expect("the real library's temp file");
        crate::vfs::File::new(body.clone())
            .write(&vec![0x42u8; 63088])
            .expect("write the library body");

        // Commit the symlink first, as the db order does. What renameat2
        // must do: look the old name up un-followed, read the link's own
        // target text, and lay the same link under the real name.
        let old = lib.lookup(tmpname).expect("the temp link is there");
        assert_eq!(old.metadata().kind, NodeKind::Symlink);
        assert_eq!(old.readlink().expect("readlink"), "libllhttp.so.9.4.1");
        lib.symlink("libllhttp.so.9.4", "libllhttp.so.9.4.1")
            .expect("rename lays the link under its real name");
        lib.unlink(tmpname).expect("the temp name goes");

        // Commit the library itself.
        lib.link("libllhttp.so.9.4.1", &body)
            .expect("rename the body");
        lib.unlink(bodytmp).expect("the body's temp name goes");

        // The end state apk expects: the library body, plus a link chain
        // that resolves to it.
        let lib9 = lib.lookup("libllhttp.so.9.4").expect("the .so.9.4 link");
        assert_eq!(lib9.metadata().kind, NodeKind::Symlink);
        assert_eq!(lib9.readlink().expect("readlink"), "libllhttp.so.9.4.1");
        let real = lib.lookup("libllhttp.so.9.4.1").expect("the library");
        assert_eq!(real.metadata().kind, NodeKind::File);
        assert_eq!(real.metadata().size, 63088);
    }

    // ---- the directory hash tree ----

    #[test]
    fn dx_hash_matches_the_reference_implementation() {
        // Goldens from e2fsprogs 1.47.4's debugfs `dx_hash`, half_md4 salted
        // with this seed - the one /usr/lib of the real image carries. The
        // third name is full of bytes above 127, the one place the signed and
        // unsigned flavors of the algorithm part ways.
        let seed = [0x0450_54d6, 0x9e4e_2290, 0xe5a0_1da8, 0xbc0d_2838];
        assert_eq!(
            dx_hash_name(b"libpsx.so.2", 1, seed, false),
            Some(0x2d8a_cbda)
        );
        assert_eq!(
            dx_hash_name(b"modules-load.d", 1, seed, false),
            Some(0x26d5_4824)
        );
        assert_eq!(
            dx_hash_name(&[0xff, 0x80, 0x7f, 0x01, b'a', b'b'], 1, seed, false),
            Some(0xbbed_4542)
        );
        assert_eq!(
            dx_hash_name(&[0xff, 0x80, 0x7f, 0x01, b'a', b'b'], 1, seed, true),
            Some(0xf8d3_c940)
        );
        // The other two algorithms, from the same source. The legacy hash
        // takes no seed; TEA does.
        assert_eq!(
            dx_hash_name(b"libpsx.so.2", 0, [0; 4], false),
            Some(0xd417_b560)
        );
        assert_eq!(
            dx_hash_name(b"libpsx.so.2", 2, seed, false),
            Some(0x9b01_6660)
        );
    }

    /// A directory made indexed by [`make_indexed`], plus its on-disk inode
    /// number and the device, for the raw reads the tree checks need.
    fn indexed_dir(total_blocks: usize) -> (Arc<MemDev>, Ext4Fs, Arc<dyn Inode>, u32, u32) {
        let dev = Arc::new(format_sized(total_blocks));
        let ino = {
            let fs = mounted(dev.clone());
            fs.root()
                .create("d", NodeKind::Dir, None)
                .expect("mkdir")
                .metadata()
                .ino as u32
        };
        // The leaf goes right after the block the directory already owns -
        // on this little image that first block is always block 14, and a
        // leaf dropped on it would be the root indexing itself.
        let leaf_block = inode_word(&dev, ino, 40) + 1;
        make_indexed(&dev, ino, leaf_block);
        // A fresh mount: the block cache is write-through, and the first
        // mount's slots still hold the blocks the raw surgery just rewrote
        // underneath it.
        let fs = mounted(dev.clone());
        let dir = fs.root().lookup("d").expect("the directory");
        (dev, fs, dir, ino, leaf_block)
    }

    #[test]
    fn an_indexed_directory_takes_an_insert() {
        let (dev, _fs, dir, ino, _leaf_block) = indexed_dir(128);

        // The apk shape that started all this: a versioned library under a
        // symlink chain, laid into /usr/lib the way symlinkat does.
        dir.symlink("libllhttp.so.9.4", "libllhttp.so.9.4.1")
            .expect("insert into the indexed directory");
        let link = dir
            .lookup("libllhttp.so.9.4")
            .expect("the link resolves through the index");
        assert_eq!(link.readlink().expect("readlink"), "libllhttp.so.9.4.1");

        // The entry landed in the leaf the index names, not in the root
        // block's dead slack, and the index still counts its alias only.
        let root_block = inode_word(&dev, ino, 40);
        let leaf = inode_word(&dev, ino, 44) as usize * 1024;
        critical_section::with(|cs| {
            let disk = dev.bytes.borrow(cs).borrow();
            assert_eq!(
                dir_find(&disk[leaf..leaf + 1024], "libllhttp.so.9.4").is_some(),
                true
            );
            assert_eq!(
                dir_find(
                    &disk[root_block as usize * 1024..root_block as usize * 1024 + 1024],
                    "libllhttp.so.9.4"
                ),
                None
            );
            assert_eq!(le16(&disk, root_block as usize * 1024 + 34), 1); // count
        });
    }

    #[test]
    fn a_growing_indexed_directory_splits_leaves_and_stays_consistent() {
        let (_dev, fs, dir, ino, _leaf) = indexed_dir(512);

        let names: Vec<String> = (0..200)
            .map(|i| alloc::format!("entry-{i:04}-padpadpad"))
            .collect();
        for name in &names {
            dir.create(name, NodeKind::File, None).expect("insert");
        }
        for name in &names {
            dir.lookup(name).expect("every name is findable");
        }

        // The tree agrees with itself: for every name, the leaf the index
        // probe names for its hash is the leaf the name actually lives in -
        // which is exactly the property a Linux lookup depends on.
        let core = &fs.core;
        let data = core.read_inode(ino);
        let blocks = data.size as u64 / 1024;
        assert!(blocks > 2, "the test must have forced leaf splits");
        for name in &names {
            let hash = dx_hash_name(name.as_bytes(), 1, [0; 4], false).expect("hash");
            let path = core.dx_probe(&data, hash).expect("probe");
            let mut home = None;
            for l in 0..blocks {
                let block = core.bmap_lookup(&data, l).expect("mapped");
                let mut page = vec![0u8; 1024];
                core.read_bytes(block, 0, &mut page);
                if dir_find(&page, name).is_some() {
                    home = Some(l);
                    break;
                }
            }
            assert_eq!(home, Some(path.leaf), "{name} lives outside its leaf");
        }
    }

    #[test]
    fn a_full_root_index_answers_enospc_like_linux() {
        let (_dev, _fs, dir, _ino, _leaf) = indexed_dir(512);

        // 100-byte names: 108-byte entries, nine to a 1 KiB leaf. The root's
        // index limit at this block size is 124 entries, so the tree tops
        // out at 124 leaves and the next split is refused - with ENOSPC,
        // exactly what Linux ("directory index full") answers here.
        let name = |i: usize| alloc::format!("n{:0>99}", i);
        let mut inserted = 0;
        loop {
            match dir.create(&name(inserted), NodeKind::File, None) {
                Ok(_) => inserted += 1,
                Err(VfsError::NoSpace) => break,
                Err(e) => panic!("unexpected error at {inserted}: {e:?}"),
            }
        }
        assert!(inserted > 500, "only {inserted} entries before ENOSPC");
        // The refusal is clean: everything already inserted is intact, and
        // the refusal repeats.
        for i in 0..inserted {
            dir.lookup(&name(i)).expect("still there");
        }
        assert!(matches!(
            dir.create(&name(inserted), NodeKind::File, None),
            Err(VfsError::NoSpace)
        ));
    }

    #[test]
    fn removing_from_an_indexed_directory_leaves_the_index_alone() {
        let (dev, _fs, dir, ino, _leaf) = indexed_dir(512);

        let names: Vec<String> = (0..40)
            .map(|i| alloc::format!("victim-{i:03}-padding"))
            .collect();
        for name in &names {
            dir.create(name, NodeKind::File, None).expect("insert");
        }
        let root_block = inode_word(&dev, ino, 40);
        let count_before = critical_section::with(|cs| {
            let disk = dev.bytes.borrow(cs).borrow();
            le16(&disk, root_block as usize * 1024 + 34) as usize
        });
        assert!(count_before > 1, "the inserts must have split leaves");

        // Half the names go. A removal never touches the index - it cannot
        // move an entry between leaves - so the root's count is untouched.
        for name in names.iter().take(20) {
            dir.unlink(name).expect("unlink");
        }
        let count_after = critical_section::with(|cs| {
            let disk = dev.bytes.borrow(cs).borrow();
            le16(&disk, root_block as usize * 1024 + 34) as usize
        });
        assert_eq!(count_after, count_before);
        for name in names.iter().skip(20) {
            dir.lookup(name).expect("survivor");
        }
        for name in names.iter().take(20) {
            assert!(dir.lookup(name).is_err(), "{name} should be gone");
        }
    }

    #[test]
    fn a_corrupt_index_degrades_to_a_linear_directory() {
        let (dev, _fs, _dir, ino, _leaf) = indexed_dir(128);
        // Scramble the index: a hash version nothing implements.
        let root_block = inode_word(&dev, ino, 40) as usize * 1024;
        critical_section::with(|cs| {
            let mut disk = dev.bytes.borrow(cs).borrow_mut();
            disk[root_block + 28] = 9;
        });
        // A fresh mount, so the fs reads the scrambled block rather than the
        // cached copy from before the surgery.
        let dir = {
            let fs = mounted(dev.clone());
            fs.root().lookup("d").expect("the directory")
        };

        dir.create("after", NodeKind::File, None)
            .expect("a bad index does not stop inserts");
        dir.lookup("after").expect("the name is there");
        // The flag went with the fallback, and the directory is linear now.
        assert_eq!(inode_word(&dev, ino, 32) & INODE_INDEX_FL, 0);
    }

    #[test]
    fn a_full_index_node_splits_when_the_root_has_room() {
        let (dev, _fs, _dir, ino, _leaf) = indexed_dir(512);

        // Hand-build a two-level tree: root -> node -> leaf, the shape ext4
        // with `largedir` makes and the only way an intermediate level comes
        // to exist at all. The node is full: 127 entries, its exact limit.
        // Every entry hashes 0x10 and points at the one leaf, whose eight
        // 120-byte entries leave no slack.
        let dir_block = inode_word(&dev, ino, 40) as usize;
        let (node_block, leaf_block) = (dir_block + 2, dir_block + 3);
        put32_wrapper(&dev, ino, 44, node_block as u32); // block[1] = node
        put32_wrapper(&dev, ino, 48, leaf_block as u32); // block[2] = leaf
        critical_section::with(|cs| {
            let mut disk = dev.bytes.borrow(cs).borrow_mut();
            let base = dir_block * 1024;
            // Inode: two more blocks.
            let iat = TABLE + (ino as usize - 1) * 128;
            put32(&mut disk, iat + 4, 3 * 1024);
            put32(&mut disk, iat + 28, 6);
            // Root info: one intermediate level below the root.
            disk[base + 30] = 1;
            // Node: fake dirent over the whole block; limit 126; count 126 -
            // the slot count a real volume records, with the eight-byte
            // tail reserved out of the entry array.
            let node = node_block * 1024;
            put16(&mut disk, node + 4, 1024);
            put16(&mut disk, node + 8, 126); // limit
            put16(&mut disk, node + 10, 126); // count
            // Index entries hold LOGICAL block numbers: the leaf is
            // block[2], logical 2.
            put32(&mut disk, node + 12, 2); // alias block: the leaf
            for j in 1..126usize {
                put32(&mut disk, node + 8 + j * 8, 0x10); // hash
                put32(&mut disk, node + 8 + j * 8 + 4, 2);
            }
            // Leaf: eight maximal entries, no slack, no holes.
            let leaf = leaf_block * 1024;
            for i in 0..8u32 {
                let at = leaf + i as usize * 128;
                put32(&mut disk, at, 100 + i); // a plausible inode
                put16(&mut disk, at + 4, 128);
                disk[at + 6] = 120;
                disk[at + 7] = FT_REG;
                for (j, b) in disk[at + 8..at + 8 + 120].iter_mut().enumerate() {
                    *b = if j < 3 { b'0' + i as u8 } else { b'x' };
                }
            }
            for b in [node_block, leaf_block] {
                disk[3 * 1024 + (b - 1) / 8] |= 1 << ((b - 1) % 8);
            }
            for free in [2 * 1024 + 12, 1024 + 12] {
                let n = le16(&disk, free) - 1;
                put16(&mut disk, free, n);
            }
        });
        // A fresh mount, past the cached blocks the surgery rewrote.
        let fs = mounted(dev.clone());
        let core = &fs.core;
        let dir = fs.root().lookup("d").expect("the directory");

        // The name hashes past every 0x10 entry of the node, so the probe
        // lands in the last leaf - full - with the node above it full too,
        // and the root (one entry, limit 124) holding the only slot. The
        // insert has to split the node, restart, and split the leaf.
        dir.create("new-name", NodeKind::File, None)
            .expect("the insert goes through the two splits");
        dir.lookup("new-name").expect("the name is findable");

        // Root gained the sibling node; the node's halves split 63/63; the
        // leaf gained a sibling; the directory grew by two blocks.
        let data = core.read_inode(ino);
        let base = data.block[0] as usize * 1024;
        let (root_count, node_count) = critical_section::with(|cs| {
            let disk = dev.bytes.borrow(cs).borrow();
            (
                le16(&disk, base + 34),
                le16(&disk, data.block[1] as usize * 1024 + 8 + 2),
            )
        });
        assert_eq!(root_count, 2);
        assert_eq!(node_count, 63);
        assert_eq!(data.size, 5 * 1024);
        // And the filler entries of the original leaf are all still there.
        let mut found = 0;
        for l in 1..data.size as u64 / 1024 {
            let block = core.bmap_lookup(&data, l).expect("mapped");
            let mut page = vec![0u8; 1024];
            core.read_bytes(block, 0, &mut page);
            let mut at = 0;
            while at + 8 <= page.len() {
                let rec = le16(&page, at + 4) as usize;
                if rec < 8 {
                    break;
                }
                if le32(&page, at) != 0 && page[at + 6] != 0 {
                    found += 1;
                }
                at += rec;
            }
        }
        assert_eq!(found, 9); // eight fillers plus the new name
    }

    /// The test fixture's disk writes: one raw word into inode `ino`.
    fn put32_wrapper(dev: &MemDev, ino: u32, offset: usize, value: u32) {
        critical_section::with(|cs| {
            let mut disk = dev.bytes.borrow(cs).borrow_mut();
            put32(&mut disk, TABLE + (ino as usize - 1) * 128 + offset, value);
        });
    }

    // ---- ext4 ----
    //
    // The same one-group, 1 KiB-block skeleton as above with the four things
    // ext4 changes written into it by hand: 64-byte group descriptors with
    // high halves, 256-byte inodes, files mapped by extent trees, and a file
    // whose size needs its high word. Nothing here is produced by mke2fs,
    // which is the point - a fixture built from the spec cannot agree with a
    // bug in the reader.

    const EXT4_INODE_SIZE: usize = 256;
    const EXT4_INODES: u32 = 32;
    const EXT4_TABLE: usize = 5 * 1024;
    const EXT4_INODE_BLOCKS: usize = EXT4_INODES as usize * EXT4_INODE_SIZE / 1024;
    const EXT4_ROOT_BLOCK: u32 = (5 + EXT4_INODE_BLOCKS) as u32;
    /// Where "hello"'s two-block extent starts.
    const EXT4_HELLO_BLOCK: u32 = EXT4_ROOT_BLOCK + 2;
    const EXT4_USED: usize = EXT4_ROOT_BLOCK as usize + 2;

    fn ext4_inode_at(ino: u32) -> usize {
        EXT4_TABLE + (ino as usize - 1) * EXT4_INODE_SIZE
    }

    /// Write an extent header into an inode's i_block, the way a filesystem
    /// that has been sitting on a disk has it.
    fn ext4_put_header(disk: &mut [u8], ino: u32, entries: u16, depth: u16) {
        let at = ext4_inode_at(ino) + 40;
        put16(disk, at, EXTENT_MAGIC);
        put16(disk, at + 2, entries);
        put16(disk, at + 4, 4);
        put16(disk, at + 6, depth);
        put32(disk, at + 8, 0);
    }

    fn ext4_put_extent(disk: &mut [u8], ino: u32, i: usize, first: u32, len: u16, start: u64) {
        let at = ext4_inode_at(ino) + 40 + EXTENT_HEADER + i * EXTENT_ENTRY;
        put32(disk, at, first);
        put16(disk, at + 4, len);
        put16(disk, at + 6, (start >> 32) as u16);
        put32(disk, at + 8, start as u32);
    }

    fn format_ext4() -> MemDev {
        let total_blocks = 128usize;
        let dev = MemDev::new(total_blocks);
        critical_section::with(|cs| {
            let mut guard = dev.bytes.borrow(cs).borrow_mut();
            let disk = &mut *guard;
            let sb = 1024;
            put32(disk, sb, EXT4_INODES); // inodes_count
            put32(disk, sb + 4, total_blocks as u32); // blocks_count_lo
            put32(disk, sb + 12, (total_blocks - 1 - EXT4_USED) as u32);
            put32(disk, sb + 16, EXT4_INODES - 12); // inodes 1..12 are taken
            put32(disk, sb + 20, 1); // first_data_block
            put32(disk, sb + 24, 0); // log_block_size: 1024
            put32(disk, sb + 28, 0);
            put32(disk, sb + 32, 8192); // blocks_per_group
            put32(disk, sb + 36, 8192);
            put32(disk, sb + 40, EXT4_INODES); // inodes_per_group
            put16(disk, sb + 56, MAGIC);
            put16(disk, sb + 58, 0x0001); // s_state: cleanly unmounted
            put16(disk, sb + 58, 1); // clean
            put32(disk, sb + 76, 1); // rev 1
            put32(disk, sb + 84, 11); // first_ino
            put16(disk, sb + 88, EXT4_INODE_SIZE as u16);
            // compat: no journal, so nothing to replay. incompat: filetype,
            // extents, 64bit. ro_compat: sparse_super, large_file, dir_nlink,
            // extra_isize - every one of them is a thing this driver reads.
            put32(disk, sb + 92, 0);
            put32(
                disk,
                sb + 96,
                INCOMPAT_FILETYPE | INCOMPAT_EXTENTS | INCOMPAT_64BIT,
            );
            put32(disk, sb + 100, (1 << 0) | (1 << 1) | (1 << 5) | (1 << 6));
            put16(disk, sb + 254, 64); // desc_size
            put32(disk, sb + 336, 0); // blocks_count_hi

            // The 64-byte descriptor: the ext2 fields, then the high halves
            // of all three pointers and of both free counts.
            put32(disk, 2 * 1024, 3); // block bitmap
            put32(disk, 2 * 1024 + 4, 4); // inode bitmap
            put32(disk, 2 * 1024 + 8, 5); // inode table
            put16(disk, 2 * 1024 + 12, (total_blocks - 1 - EXT4_USED) as u16);
            put16(disk, 2 * 1024 + 14, (EXT4_INODES - 12) as u16);
            put16(disk, 2 * 1024 + 16, 1); // used dirs: the root
            put32(disk, 2 * 1024 + 32, 0); // block_bitmap_hi
            put32(disk, 2 * 1024 + 36, 0); // inode_bitmap_hi
            put32(disk, 2 * 1024 + 40, 0); // inode_table_hi
            put16(disk, 2 * 1024 + 44, 0); // free_blocks_hi
            put16(disk, 2 * 1024 + 46, 0); // free_inodes_hi

            // Blocks 1..=13 (metadata and the root directory) plus hello's
            // two. Bit b is block b + 1: block 0 is outside the group.
            for block in 1..=EXT4_ROOT_BLOCK {
                disk[3 * 1024 + (block - 1) as usize / 8] |= 1 << ((block - 1) % 8);
            }
            for block in EXT4_HELLO_BLOCK..EXT4_HELLO_BLOCK + 2 {
                disk[3 * 1024 + (block - 1) as usize / 8] |= 1 << ((block - 1) % 8);
            }
            put16(disk, 4 * 1024, 0x0fff); // inodes 1..12

            // Root: extent-mapped, one extent holding its only block.
            let root = ext4_inode_at(2);
            put16(disk, root, S_IFDIR | 0o755);
            put32(disk, root + 4, 1024);
            put16(disk, root + 26, 2);
            put32(disk, root + 28, 2);
            put32(disk, root + 32, INODE_EXTENTS_FL);
            ext4_put_header(disk, 2, 1, 0);
            ext4_put_extent(disk, 2, 0, 0, 1, EXT4_ROOT_BLOCK as u64);
            let base = EXT4_ROOT_BLOCK as usize * 1024;
            write_dirent(disk, base, 2, dirent_len(1), ".", FT_DIR);
            write_dirent(disk, base + dirent_len(1), 2, dirent_len(1), "..", FT_DIR);
            write_dirent(
                disk,
                base + 2 * dirent_len(1),
                11,
                1024 - 2 * dirent_len(1),
                "hello",
                FT_REG,
            );

            // "hello": eleven bytes in the first block of a two-block extent
            // and four more in its second, which is what an extent is - a run
            // of blocks, mapped by one entry.
            let hello = ext4_inode_at(11);
            put16(disk, hello, S_IFREG | 0o644);
            put32(disk, hello + 4, 1500);
            put16(disk, hello + 26, 1);
            put32(disk, hello + 28, 4);
            put32(disk, hello + 32, INODE_EXTENTS_FL);
            ext4_put_header(disk, 11, 1, 0);
            ext4_put_extent(disk, 11, 0, 0, 2, EXT4_HELLO_BLOCK as u64);
            disk[EXT4_HELLO_BLOCK as usize * 1024..][..11].copy_from_slice(b"hello world");
            disk[(EXT4_HELLO_BLOCK + 1) as usize * 1024..][..4].copy_from_slice(b"XXXX");

            // Inode 12: a file whose size is 4 GiB, all of it in the high
            // word at 108 - the word ext2 reads as i_dir_acl.
            let big = ext4_inode_at(12);
            put16(disk, big, S_IFREG | 0o644);
            put32(disk, big + 4, 0);
            put32(disk, big + INODE_SIZE_HIGH, 1);
            put16(disk, big + 26, 1);
            put32(disk, big + 32, INODE_EXTENTS_FL);
            ext4_put_header(disk, 12, 0, 0);
        });
        dev
    }

    /// Set one feature word in the ext4 fixture's superblock before mounting.
    /// A volume that ends up with `metadata_csum` also gets the superblock's
    /// own checksum recomputed, because a mount now verifies it - a fixture
    /// that flipped the bit silently is a fixture that mounts read-only.
    fn ext4_feature(dev: &MemDev, at: usize, bits: u32) {
        critical_section::with(|cs| {
            let mut disk = dev.bytes.borrow(cs).borrow_mut();
            let was = le32(&disk, at);
            put32(&mut disk, at, was | bits);
            if le32(&disk, 1024 + 100) & ROCOMPAT_METADATA_CSUM != 0 {
                let c = self::csum::sb_csum(&disk[1024..1024 + 1020]);
                put32(&mut disk, 1024 + 1020, c);
            }
        });
    }

    #[test]
    fn an_extent_of_exactly_32768_blocks_is_a_run_and_not_a_hole() {
        // `ee_len` is not a length with a flag beside it: ext4 keeps the
        // unwritten bit *in* the length, where at most 32768 means a written
        // run of that many blocks and anything larger means an unwritten run
        // of `ee_len - 32768`. So 32768 - the longest run one entry can
        // describe, and the shape the allocator produces for any file big
        // enough to need capping - is a *written* extent.
        //
        // Testing the top bit instead makes exactly that extent a zero-length
        // hole, and a hole reads as zeros: 128 MiB of a shared library came
        // back blank that way, which is what "Exec format error" from a
        // `dlopen` of it looks like from userland.
        let dev = format_ext4();
        critical_section::with(|cs| {
            let mut disk = dev.bytes.borrow(cs).borrow_mut();
            ext4_put_header(&mut disk, 11, 1, 0);
            ext4_put_extent(
                &mut disk,
                11,
                0,
                0,
                EXTENT_INIT_MAX,
                EXT4_HELLO_BLOCK as u64,
            );
        });
        let fs = mounted(Arc::new(dev));
        let hello = fs.root().lookup("hello").expect("hello is there");
        assert_eq!(read_all(&*hello, 11), b"hello world".to_vec());
    }

    #[test]
    fn an_extent_longer_than_that_is_the_unwritten_one() {
        // The other side of the same rule: 32769 is not a written run of
        // 32769 blocks - it is an *unwritten* run of one, and its block reads
        // as zeros however much data the disk behind it happens to hold.
        let dev = format_ext4();
        critical_section::with(|cs| {
            let mut disk = dev.bytes.borrow(cs).borrow_mut();
            ext4_put_header(&mut disk, 11, 1, 0);
            ext4_put_extent(
                &mut disk,
                11,
                0,
                0,
                EXTENT_INIT_MAX + 1,
                EXT4_HELLO_BLOCK as u64,
            );
        });
        let fs = mounted(Arc::new(dev));
        let hello = fs.root().lookup("hello").expect("hello is there");
        assert_eq!(read_all(&*hello, 11), vec![0u8; 11]);
    }

    #[test]
    fn the_unwritten_bit_is_the_length_and_nothing_else() {
        // The rule itself, at the boundaries: written up to and including
        // 32768, unwritten above it with the run one shorter than the field.
        assert_eq!(ext_len(0), (0, false));
        assert_eq!(ext_len(1), (1, false));
        assert_eq!(ext_len(EXTENT_INIT_MAX - 1), (32767, false));
        assert_eq!(ext_len(EXTENT_INIT_MAX), (32768, false));
        assert_eq!(ext_len(EXTENT_INIT_MAX + 1), (1, true));
        assert_eq!(ext_len(u16::MAX), (32767, true));
        // And a field written back is the field it was.
        for raw in [
            0u16,
            1,
            EXTENT_INIT_MAX - 1,
            EXTENT_INIT_MAX,
            EXTENT_INIT_MAX + 1,
            u16::MAX,
        ] {
            let (len, unwritten) = ext_len(raw);
            assert_eq!(ext_raw_len(len, unwritten), raw, "{raw} round-trips");
        }
    }

    #[test]
    fn truncating_a_file_with_a_hole_in_it_keeps_the_hole() {
        // The length field *is* the flag, so anything that copies an extent
        // entry has to copy it as the field it was. A truncate that rewrote a
        // hole as a plain length would turn it into a written run, and the
        // file would then read whatever the block's last owner left on the
        // disk.
        let dev = format_ext4();
        critical_section::with(|cs| {
            let mut disk = dev.bytes.borrow(cs).borrow_mut();
            ext4_put_header(&mut disk, 11, 1, 0);
            // Two blocks, unwritten: `ee_len` is the length plus 32768.
            ext4_put_extent(
                &mut disk,
                11,
                0,
                0,
                2 + EXTENT_INIT_MAX,
                EXT4_HELLO_BLOCK as u64,
            );
        });
        let fs = mounted(Arc::new(dev));
        let hello = fs.root().lookup("hello").expect("hello is there");
        assert_eq!(read_all(&*hello, 11), vec![0u8; 11]);

        // Shrink it, so the entry is rewritten to the head of the run...
        hello.truncate(1).expect("truncate");
        assert_eq!(read_all(&*hello, 1), vec![0u8; 1]);
        // ...and the head is still marked unwritten.
        let data = fs.core.read_inode(11);
        let root = ext_root(&data);
        let (_first, raw_len, _start) = ext_leaf(&root, 0);
        assert_eq!(ext_len(raw_len), (1, true));
    }

    #[test]
    fn an_ext4_image_mounts_and_reads_through_its_extent_tree() {
        let fs = mounted(Arc::new(format_ext4()));
        let root = fs.root();
        let names: Vec<String> = root
            .read_dir()
            .expect("readdir")
            .into_iter()
            .map(|(name, _)| name)
            .collect();
        assert!(names.contains(&String::from("hello")), "{names:?}");
        let hello = root.lookup("hello").expect("hello is there");
        assert_eq!(read_all(&*hello, 11), b"hello world".to_vec());
        // The second block of the run, four hundred and sixty-one bytes past
        // the end of the first block's data: one extent entry maps both, and
        // a lookup that only ever looked at the first would read a hole.
        let mut past = [0u8; 4];
        assert_eq!(hello.read_at(1024, &mut past).expect("read"), 4);
        assert_eq!(&past, b"XXXX");
    }

    #[test]
    fn a_64bit_descriptor_carries_its_high_halves() {
        let dev = format_ext4();
        // Put the inode table past the 4 GiB mark, which only the
        // descriptor's high half can say.
        critical_section::with(|cs| {
            let mut disk = dev.bytes.borrow(cs).borrow_mut();
            put32(&mut disk, 2 * 1024 + 40, 1);
        });
        let fs = mounted(Arc::new(dev));
        assert_eq!(fs.core.group(0).inode_table, (1u64 << 32) + 5);
    }

    #[test]
    fn a_files_size_is_the_two_words_ext4_splits_it_into() {
        let dev = Arc::new(format_ext4());
        let fs = mounted(dev.clone());
        assert_eq!(fs.inode_of(12).metadata().size, 1u64 << 32);
        // And a directory's word at 108 is not the top of a size: it is
        // i_dir_acl, and handing it back as one would make every directory
        // exabytes long.
        critical_section::with(|cs| {
            let mut disk = dev.bytes.borrow(cs).borrow_mut();
            put32(&mut disk, ext4_inode_at(2) + INODE_SIZE_HIGH, 7);
        });
        assert_eq!(fs.inode_of(2).metadata().size, 1024);
    }

    #[test]
    fn an_extent_tree_grows_past_the_four_extents_the_inode_holds() {
        let fs = mounted(Arc::new(format_ext4()));
        let core = &fs.core;
        let file = fs
            .root()
            .create("grown", NodeKind::File, None)
            .expect("the file is created");
        // Six extents nowhere near each other - near ones would merge into a
        // run - so the inode's four are outgrown: the fifth has to split it
        // and the root has to become an index over two leaves rather than a
        // leaf itself.
        for l in [10u64, 20, 30, 40, 50, 60] {
            let page = vec![b'a' + (l / 10) as u8; 1024];
            assert_eq!(file.write_at(l * 1024, &page).expect("write"), 1024);
        }
        let data = core.read_inode(file.metadata().ino as u32);
        assert!(
            data.flags & INODE_EXTENTS_FL != 0,
            "a file born on an extent filesystem stays extent-mapped"
        );
        let root = ext_root(&data);
        assert_eq!(ext_depth(&root), 1, "the root became an index");
        assert_eq!(ext_entries(&root), 2, "over the two halves it split into");
        for l in [10u64, 20, 30, 40, 50, 60] {
            let mut page = vec![0u8; 1024];
            assert_eq!(file.read_at(l * 1024, &mut page).expect("read"), 1024);
            assert!(
                page.iter().all(|b| *b == b'a' + (l / 10) as u8),
                "block {l} reads back what was written"
            );
        }
    }

    #[test]
    fn adjacent_blocks_join_the_extent_they_land_beside() {
        let fs = mounted(Arc::new(format_ext4()));
        let core = &fs.core;
        let file = fs
            .root()
            .create("run", NodeKind::File, None)
            .expect("the file is created");
        // Six blocks written one at a time, and the allocator hands them out
        // in a row: five entries saying so would be a list where one run
        // belongs, and the first thing a reference fsck says about it is
        // "narrower".
        for l in 0..6u64 {
            let page = vec![b'a' + l as u8; 1024];
            assert_eq!(file.write_at(l * 1024, &page).expect("write"), 1024);
        }
        let data = core.read_inode(file.metadata().ino as u32);
        let root = ext_root(&data);
        assert_eq!(ext_depth(&root), 0, "one run, no tree");
        assert_eq!(ext_entries(&root), 2, "block 0 apart, blocks 1..5 as a run");
        let (_first, len, _start) = ext_leaf(&root, 1);
        assert_eq!(len, 5, "five blocks, one extent");
        for l in 0..6u64 {
            let mut page = vec![0u8; 1024];
            assert_eq!(file.read_at(l * 1024, &mut page).expect("read"), 1024);
            assert!(
                page.iter().all(|b| *b == b'a' + l as u8),
                "block {l} reads back what was written"
            );
        }
    }

    #[test]
    fn a_split_that_lands_in_the_half_that_was_kept_still_indexes_both() {
        let fs = mounted(Arc::new(format_ext4()));
        let file = fs
            .root()
            .create("sparse", NodeKind::File, None)
            .expect("the file is created");
        // Four extents nowhere near each other, then a fifth that belongs
        // *before* all of them. The split has to hand the new one to the half
        // that stayed and still index the half that moved out - under the
        // block the moved half starts at, not the one that took the entry.
        for l in [10u64, 20, 30, 40] {
            let page = vec![b'a' + (l / 10) as u8; 1024];
            file.write_at(l * 1024, &page).expect("write");
        }
        file.write_at(5 * 1024, &vec![b'z'; 1024]).expect("write");
        for (l, want) in [(5u64, b'z'), (10, b'b'), (20, b'c'), (30, b'd'), (40, b'e')] {
            let mut page = vec![0u8; 1024];
            assert_eq!(file.read_at(l * 1024, &mut page).expect("read"), 1024);
            assert!(
                page.iter().all(|b| *b == want),
                "block {l} reads back what was written"
            );
        }
    }

    #[test]
    fn every_block_of_an_extent_tree_comes_back_on_truncate() {
        let dev = Arc::new(format_ext4());
        let before = mounted(dev.clone()).statfs().expect("statfs").bfree;
        let fs = mounted(dev.clone());
        let file = fs
            .root()
            .create("grown", NodeKind::File, None)
            .expect("the file is created");
        // Six extents nowhere near each other, so the tree has to split and
        // grow: six data blocks, the sibling the split made, and the block
        // the root's own entries moved into when it became an index.
        for l in [10u64, 20, 30, 40, 50, 60] {
            let page = vec![b'a' + (l / 10) as u8; 1024];
            file.write_at(l * 1024, &page).expect("write");
        }
        let spent = before - mounted(dev.clone()).statfs().expect("statfs").bfree;
        assert_eq!(spent, 8, "the tree costs its own blocks, and no more");
        file.truncate(0).expect("truncate");
        // A fresh mount: nothing of it is in this one's cache.
        let after = mounted(dev).statfs().expect("statfs").bfree;
        assert_eq!(after, before, "every block came back");
        assert_eq!(read_all(&*file, 32).len(), 0, "and the file is empty");
    }

    #[test]
    fn truncate_of_an_extent_file_gives_back_only_the_tail() {
        // A contiguous file is one depth-0 extent - the shape `apk` installs,
        // and the case the bounded tail-free covers. Shrinking must free just
        // the blocks past the new end and leave the kept bytes untouched, with
        // no allocation proportional to the file.
        let dev = Arc::new(format_ext4());
        let fs = mounted(dev.clone());
        let file = fs
            .root()
            .create("tail", NodeKind::File, None)
            .expect("create");
        for l in 0..5u64 {
            let page = vec![b'a' + l as u8; 1024];
            file.write_at(l * 1024, &page).expect("write");
        }
        let after_write = mounted(dev.clone()).statfs().expect("statfs").bfree;
        file.truncate(2 * 1024).expect("truncate to two blocks");
        assert_eq!(file.metadata().size, 2 * 1024, "the size shrank");
        let after_trunc = mounted(dev.clone()).statfs().expect("statfs").bfree;
        assert_eq!(
            after_trunc - after_write,
            3,
            "only the three tail blocks came back"
        );
        // The kept bytes are intact, and the file now ends at two blocks.
        let kept = read_all(&*file, 4096);
        assert_eq!(kept.len(), 2 * 1024);
        assert!(kept.iter().all(|&b| b == b'a' || b == b'b'));
    }

    #[test]
    fn a_feature_this_driver_cannot_read_is_refused_at_mount() {
        let dev = format_ext4();
        // ENCRYPT: an inode's block pointers are ciphertext until somebody
        // supplies a key, which is not a thing to read past.
        ext4_feature(&dev, 1024 + 96, 1 << 16);
        assert!(
            Ext4Fs::mount(Arc::new(dev)).is_err(),
            "a filesystem is refused rather than misread"
        );
    }

    #[test]
    fn a_checksummed_volume_writes_and_leaves_its_checksums_true() {
        // The retirement of the read-only gate, pinned: a `metadata_csum`
        // image is writable, every write recomputes what it touched, and a
        // *fresh mount* - whose verifier recomputes from the bytes alone -
        // accepts the result. A stale checksum would fail that second mount
        // exactly the way the kernel's would.
        let dev = Arc::new(format_ext4());
        ext4_feature(&dev, 1024 + 100, ROCOMPAT_METADATA_CSUM);
        let fs = mounted(dev.clone());
        assert!(!fs.read_only(), "checksums we maintain mean writes");
        let file = fs
            .root()
            .create("summed", NodeKind::File, None)
            .expect("create");
        file.write_at(0, b"checksum payload").expect("write");
        drop(fs);
        let again = mounted(dev);
        assert_eq!(
            read_all(&*again.root().lookup("summed").expect("found"), 16),
            b"checksum payload".to_vec(),
            "the file and its bookkeeping survive a remount"
        );
    }

    #[test]
    fn a_volume_with_a_journal_mounts_read_only_and_stays_that_way() {
        // `has_journal` is a *compat* bit - the old table never even read
        // word 92 - and a journal nobody maintains means today's bypassing
        // write is what tomorrow's Linux replay scribbles over. Read-only
        // until the journal milestone is the honest answer.
        let dev = format_ext4();
        ext4_feature(&dev, 1024 + 92, COMPAT_HAS_JOURNAL);
        let fs = mounted(Arc::new(dev));
        assert!(fs.read_only(), "a journaled volume is not ours to write");
        fs.set_read_only(false);
        assert!(fs.read_only(), "and the refusal does not wash off");
        assert_eq!(
            read_all(&*fs.root().lookup("hello").expect("hello"), 11),
            b"hello world".to_vec(),
            "a volume worth reading is read"
        );
    }

    #[test]
    fn a_volume_never_marked_cleanly_unmounted_mounts_read_only() {
        let dev = Arc::new(format_ext4());
        // Clear EXT2_VALID_FS from s_state: the marker a crash leaves behind.
        critical_section::with(|cs| {
            let mut disk = dev.bytes.borrow(cs).borrow_mut();
            disk[1024 + 58] = 0;
        });
        let fs = mounted(dev.clone());
        assert!(fs.read_only(), "mid-repair is not a state to write through");
        critical_section::with(|cs| {
            let mut disk = dev.bytes.borrow(cs).borrow_mut();
            disk[1024 + 58] = STATE_VALID_FS as u8;
        });
        assert!(
            !mounted(dev).read_only(),
            "the refusal belongs to the marker, not to the volume"
        );
    }

    #[test]
    fn the_modern_compat_table_tolerates_orphan_file_and_refuses_the_unknown() {
        // A default 2026 mke2fs image sets compat 0x103c - journal, attr,
        // resize, dir_index, orphan_file. Refusing the last one would refuse
        // every modern volume over a bit this driver simply never writes.
        let dev = format_ext4();
        ext4_feature(&dev, 1024 + 92, COMPAT_ORPHAN_FILE);
        let fs = mounted(Arc::new(dev));
        assert!(!fs.read_only(), "orphan_file is a bit we leave alone");
        // The same word with an un-named bit is a format we have not read
        // the meaning of: refusal, not guesswork.
        let dev = format_ext4();
        ext4_feature(&dev, 1024 + 92, 1 << 7);
        assert!(
            Ext4Fs::mount(Arc::new(dev)).is_err(),
            "an unknown compat bit refuses the mount"
        );
    }

    /// A real file as a block device, for the one test that holds ground
    /// truth: an image the host's own mke2fs made.
    struct FileDev {
        file: std::fs::File,
        sectors: u64,
    }

    impl BlockDev for FileDev {
        fn capacity_sectors(&self) -> u64 {
            self.sectors
        }

        fn read_at(&self, offset: u64, buf: &mut [u8]) {
            use std::io::{Read, Seek, SeekFrom};
            let mut file = &self.file;
            file.seek(SeekFrom::Start(offset)).expect("seek");
            file.read_exact(buf).expect("read");
        }

        fn write_at(&self, offset: u64, buf: &[u8]) {
            use std::io::{Seek, SeekFrom, Write};
            let mut file = &self.file;
            file.seek(SeekFrom::Start(offset)).expect("seek");
            file.write_all(buf).expect("write");
        }
    }

    /// Read a large file through this driver and write it to
    /// `MPOS_BIGFILE_OUT`, so the host can hash-compare it against `debugfs`
    /// ground truth. A large-file/extent-tree read slip corrupts a high offset
    /// only - exactly what small-file round-trips cannot catch.
    ///
    /// The subject is `usr/lib/libcrypto.so.3`: 4.5 MB, so its high offsets ride
    /// the extent tree, and it is a file **both** machines' package lists
    /// guarantee, because `apk-tools` links it. The fixture used to be
    /// `usr/bin/fish` - a program an image can stop carrying, and one this
    /// image did, which makes an instrument whose subject has left the machine
    /// one that fails for a reason that is not the driver's.
    #[test]
    fn a_large_file_reads_through_the_driver_byte_for_byte() {
        let Ok(img) = std::env::var("MPOS_EXT4_IMG") else {
            return;
        };
        let Ok(out) = std::env::var("MPOS_BIGFILE_OUT") else {
            return;
        };
        let file = std::fs::File::options()
            .read(true)
            .write(true)
            .open(&img)
            .expect("image opens");
        let sectors = file.metadata().expect("metadata").len() / 512;
        let dev: Arc<dyn BlockDev> = Arc::new(FileDev { file, sectors });
        let fs = mounted(dev);
        let bin = fs
            .root()
            .lookup("usr")
            .expect("usr")
            .lookup("lib")
            .expect("lib")
            .lookup("libcrypto.so.3")
            .expect("libcrypto.so.3");
        let size = bin.metadata().size as usize;
        assert!(size > 1 << 20, "libcrypto is a multi-megabyte binary");
        // Read it in small chunks, the way a reader with a modest buffer does.
        let mut got: Vec<u8> = Vec::with_capacity(size);
        let mut off = 0u64;
        while (got.len()) < size {
            let mut chunk = vec![0u8; 4096];
            let n = bin.read_at(off, &mut chunk).expect("read_at");
            assert!(n > 0, "progress at offset {off}");
            got.extend_from_slice(&chunk[..n]);
            off += n as u64;
        }
        std::fs::write(&out, &got).expect("write out");
    }

    /// Mount an image made by the host's mke2fs and read what it wrote - the
    /// fixture above is built from the spec and cannot disagree with it, so
    /// the real thing is the only check that the spec was read right. Skipped
    /// unless `MPOS_EXT4_IMG` names an image; `.workbuddy/verify-ext4.sh`
    /// makes one and runs this against it.
    #[test]
    fn an_image_the_host_mke2fs_made_mounts_and_reads() {
        let Ok(path) = std::env::var("MPOS_EXT4_IMG") else {
            return;
        };
        let file = std::fs::File::options()
            .read(true)
            .write(true)
            .open(&path)
            .expect("the image opens");
        let sectors = file.metadata().expect("metadata").len() / 512;
        let dev: Arc<dyn BlockDev> = Arc::new(FileDev { file, sectors });
        let fs = mounted(dev);
        let names: Vec<String> = fs
            .root()
            .read_dir()
            .expect("readdir")
            .into_iter()
            .map(|(name, _)| name)
            .collect();
        assert!(names.contains(&String::from("hello.txt")), "{names:?}");
        let hello = fs.root().lookup("hello.txt").expect("hello.txt");
        assert_eq!(
            read_all(&*hello, 64),
            b"hello from mke2fs\n".to_vec(),
            "the file mke2fs wrote reads back"
        );
        let deep = fs
            .root()
            .lookup("sub")
            .expect("the directory")
            .lookup("deep.txt")
            .expect("deep.txt");
        assert_eq!(read_all(&*deep, 64), b"deep enough\n".to_vec());
    }

    /// Write to a real mke2fs image through the driver and read it back. The
    /// script that runs this then hands the same image to the host's debugfs,
    /// which is the referee: whatever this kernel wrote, the reference tools
    /// must be able to read.
    #[test]
    fn writes_to_a_real_image_read_back_in_the_driver() {
        let Ok(path) = std::env::var("MPOS_EXT4_IMG") else {
            return;
        };
        let file = std::fs::File::options()
            .read(true)
            .write(true)
            .open(&path)
            .expect("the image opens");
        let sectors = file.metadata().expect("metadata").len() / 512;
        let dev: Arc<dyn BlockDev> = Arc::new(FileDev { file, sectors });
        let fs = mounted(dev);
        assert!(
            !fs.read_only(),
            "the image without metadata_csum is writable"
        );

        // Six blocks, one extent each - the fifth and sixth force a split.
        let grown = fs
            .root()
            .create("grown", NodeKind::File, None)
            .expect("create");
        for l in 0..6u64 {
            let page = vec![b'a' + l as u8; 1024];
            assert_eq!(grown.write_at(l * 1024, &page).expect("write"), 1024);
        }
        for l in 0..6u64 {
            let mut page = vec![0u8; 1024];
            assert_eq!(grown.read_at(l * 1024, &mut page).expect("read"), 1024);
            assert!(page.iter().all(|b| *b == b'a' + l as u8), "block {l}");
        }

        // An overwrite inside an extent mke2fs made: in place, no new blocks.
        let hello = fs.root().lookup("hello.txt").expect("hello.txt");
        assert_eq!(
            hello
                .write_at(0, b"CHANGED BY THE KERNEL\n")
                .expect("overwrite"),
            22
        );
        assert_eq!(read_all(&*hello, 64), b"CHANGED BY THE KERNEL\n".to_vec());
    }

    /// The attribute flags are the on-disk `i_flags`, and going through
    /// `set_flags` really does write them down.
    ///
    /// This is the pin the guest cannot give: the gate boots a ramfs, so the
    /// only place an ext4 inode's flags can be *persisted* and read back is an
    /// image the host's `mke2fs` made. Three things are checked, and each is one
    /// the other tests would not catch - that a fresh file reports the extent
    /// bit `lsattr` prints as `e`, that a flag set through the VFS round-trips
    /// off the disk, and that the bit the driver owns is *not* the caller's to
    /// clear, because clearing it would not turn the extent tree back into a
    /// block map.
    #[test]
    fn the_inode_flags_are_the_ones_on_the_disk() {
        let Ok(path) = std::env::var("MPOS_EXT4_IMG") else {
            return;
        };
        let file = std::fs::File::options()
            .read(true)
            .write(true)
            .open(&path)
            .expect("the image opens");
        let sectors = file.metadata().expect("metadata").len() / 512;
        let dev: Arc<dyn BlockDev> = Arc::new(FileDev { file, sectors });
        let fs = mounted(dev);

        let flagged = fs
            .root()
            .create("flagged", NodeKind::File, None)
            .expect("create");
        let fresh = flagged.flags().expect("a file on ext4 has flags");
        assert_eq!(
            fresh & crate::vfs::FS_EXTENT_FL,
            crate::vfs::FS_EXTENT_FL,
            "every file this driver makes is extent-mapped, and that is the `e`              `lsattr` prints"
        );
        assert_eq!(
            flagged.read_at(0, &mut [0u8; 8]).expect("read"),
            0,
            "and a fresh file has no bytes"
        );

        // The bits `chattr +A +d` set, read back off the *disk* and not out of
        // a cache: the image is mounted a second time, from a fresh device
        // handle and with an empty block cache, and the file is looked up
        // there. Anything the first mount only remembered would be lost, which
        // is exactly what a flag stored in memory rather than in `i_flags`
        // would do - and `i_flags` is where Linux keeps it too.
        let want = fresh | crate::vfs::FS_NOATIME_FL | crate::vfs::FS_NODUMP_FL;
        flagged.set_flags(want).expect("an allowed change");
        let reopened = std::fs::File::options()
            .read(true)
            .write(true)
            .open(&path)
            .expect("the image opens again");
        let sectors = reopened.metadata().expect("metadata").len() / 512;
        let dev: Arc<dyn BlockDev> = Arc::new(FileDev {
            file: reopened,
            sectors,
        });
        let again = mounted(dev);
        let on_disk = again.root().lookup("flagged").expect("the file is there");
        assert_eq!(on_disk.flags().expect("flags"), want);

        // And the driver's own bit cannot be moved, on either side of the
        // operation.
        assert_eq!(
            flagged.set_flags(want & !crate::vfs::FS_EXTENT_FL).err(),
            Some(VfsError::Unsupported)
        );
        assert_eq!(flagged.flags().expect("flags"), want, "and nothing changed");
    }
}
