//! `io_uring_setup(2)`: the ring object a completion queue and a submission
//! queue share.
//!
//! The descriptor `io_uring_setup` answers is not a file: it is a *region of
//! memory* the caller maps three ways, plus the kernel that reads and writes
//! the same pages. Everything the ABI promises is in this layout, and the
//! layout is Linux's, byte for byte - `build/scratch/iouring/probe4.c` measured
//! it on Linux 7.2.11 (`io_uring_params` for `entries=8`: `sq_entries=8`,
//! `cq_entries=16`, `features=0x3ffff`, `sq_off.array=320`, `cq_off.cqes=64`,
//! `sq_off.head=0`, `cq_off.head=8`, and so on) - because the applications
//! this kernel is for (a liburing-based client, a wine/proton build) compute
//! every offset from these words rather than from a copy of this header.
//! Refusing to publish Linux's words would make the mapping unusable to them.
//!
//! **Frame-backed on purpose.** The pages are physical frames allocated here
//! and mapped into the caller's address space by `mmap`, which is what makes
//! the kernel's read of an SQE and the caller's write to it the *same memory* -
//! exactly the mechanism `Memfd` uses for a Wayland pool. The header page holds
//! both rings (Linux's `IORING_FEAT_SINGLE_MMAP` shape: the CQ ring lives inside
//! the same page range as the SQ ring, so a caller may map it once or twice),
//! and the SQE array has a page range of its own at Linux's magic offsets.
//!
//! **What this serves is the synchronous subset** (the scope the user accepted,
//! `docs/notes/06-process-management.md` §6.32): `NOP`, `READ`, `WRITE`,
//! `READV`, `WRITEV`, `FSYNC`, `CLOSE`. Every SQE is executed inside
//! `io_uring_enter` and its CQE posted before the call returns, which is a real
//! completion queue - just one whose completions are never deferred. A ring
//! built for `IORING_SETUP_SQPOLL`, `IOPOLL`, `CQE32` or the taskrun flags is
//! refused with `EOPNOTSUPP` rather than accepted and silently run
//! conventionally; `io_uring_register` is refused the same way. Both are named
//! gaps in §6.32.
//!
//! **Memory is bounded**, because §8.0's allocator panics on failure: entries
//! are capped at [`MAX_SQ_ENTRIES`] (Linux's own ceiling is 32768), and the
//! number of live rings at [`MAX_RINGS`]. A ring at the cap costs 26 pages
//! (10 for the ring, 16 for the SQEs), so the whole cap is under 7 MiB.

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::cell::RefCell;
use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use critical_section::Mutex;

use crate::mm::{PAGE_SIZE, alloc_frames, clone_frame_ref, free_frames, phys_to_virt};
use crate::vfs::{Inode, Metadata, NodeKind, Ready, VfsError, VfsResult};

use super::io_uring_abi::*;

/// One ring: the frames holding the shared pages and the waiters parked on it.
///
/// The frames are owned here and handed to every mapping with a clone of the
/// frame's reference, the same ownership rule a `Memfd` follows: the last
/// mapping and the descriptor can go in either order, and the frames are freed
/// only when both are gone.
pub struct IoUring {
    sq_entries: u32,
    cq_entries: u32,
    ring_frames: Vec<usize>,
    sqe_frames: Vec<usize>,
    waiters: Mutex<RefCell<Vec<crate::sched::TaskId>>>,
}

static LIVE_RINGS: AtomicUsize = AtomicUsize::new(0);
static NEXT_INO: AtomicU64 = AtomicU64::new(0x9700_0000);

impl IoUring {
    /// Build a ring of `entries` SQ slots and the doubled CQ that goes with
    /// it, zeroing every page (a head/tail of zero is the ABI's "empty") and
    /// publishing the masks and counts the caller reads back.
    pub fn build(entries: u32) -> Option<Arc<IoUring>> {
        let sq_entries = sq_entries_for(entries)?;
        let cq_entries = sq_entries.checked_mul(2)?;
        let ring_bytes = array_offset(cq_entries) + sq_entries as usize * 4;
        let sqe_bytes = sq_entries as usize * SQE_SIZE;
        let ring_pages = ring_bytes.div_ceil(PAGE_SIZE);
        let sqe_pages = sqe_bytes.div_ceil(PAGE_SIZE);

        // The cap is checked before a frame is taken, so a refused ring costs
        // no memory.
        if LIVE_RINGS.load(Ordering::Relaxed) >= MAX_RINGS {
            return None;
        }

        let mut ring_frames = Vec::new();
        for _ in 0..ring_pages {
            let Some(frame) = alloc_frames(0) else {
                free_list(&ring_frames);
                return None;
            };
            // The pages are shared with userland: everything starts at zero,
            // and a stale frame's last tenant must not be readable.
            unsafe { core::ptr::write_bytes(phys_to_virt(frame) as *mut u8, 0, PAGE_SIZE) };
            ring_frames.push(frame);
        }
        let mut sqe_frames = Vec::new();
        for _ in 0..sqe_pages {
            let Some(frame) = alloc_frames(0) else {
                free_list(&ring_frames);
                free_list(&sqe_frames);
                return None;
            };
            unsafe { core::ptr::write_bytes(phys_to_virt(frame) as *mut u8, 0, PAGE_SIZE) };
            sqe_frames.push(frame);
        }
        LIVE_RINGS.fetch_add(1, Ordering::Relaxed);
        let ring = Arc::new(IoUring {
            sq_entries,
            cq_entries,
            ring_frames,
            sqe_frames,
            waiters: Mutex::new(RefCell::new(Vec::new())),
        });
        // The counts and masks the caller reads: populated here, so the words
        // `params_words` publishes and the pages the caller maps agree.
        ring.write_u32(SQ_MASK, sq_entries - 1);
        ring.write_u32(CQ_MASK, cq_entries - 1);
        ring.write_u32(SQ_ENTRIES, sq_entries);
        ring.write_u32(CQ_ENTRIES, cq_entries);
        Some(ring)
    }

    pub fn sq_entries(&self) -> u32 {
        self.sq_entries
    }

    pub fn cq_entries(&self) -> u32 {
        self.cq_entries
    }

    /// The number of SQEs submitted and not yet consumed, as the caller's
    /// tail and the kernel's head say.
    pub fn sq_pending(&self) -> usize {
        self.sq_tail().wrapping_sub(self.sq_head()) as usize
    }

    /// Room left in the CQ ring for another completion.
    pub fn cq_room(&self) -> usize {
        self.cq_entries as usize - self.cq_pending()
    }

    /// Completions posted and not yet reaped.
    pub fn cq_pending(&self) -> usize {
        self.cq_tail().wrapping_sub(self.cq_head()) as usize
    }

    pub fn sq_head(&self) -> u32 {
        self.read_u32(SQ_HEAD)
    }

    pub fn sq_tail(&self) -> u32 {
        self.read_u32(SQ_TAIL)
    }

    pub fn cq_head(&self) -> u32 {
        self.read_u32(CQ_HEAD)
    }

    pub fn cq_tail(&self) -> u32 {
        self.read_u32(CQ_TAIL)
    }

    pub fn set_sq_head(&self, value: u32) {
        self.write_u32(SQ_HEAD, value);
    }

    /// The raw 64-byte SQE at the submission position `pos` (a position in the
    /// caller's tail/head numbering, not an array slot): the SQ array's word
    /// at that position names the SQE slot.
    pub fn sqe_at(&self, pos: u32) -> [u8; SQE_SIZE] {
        let slot = pos % self.sq_entries;
        let array = array_offset(self.cq_entries) + slot as usize * 4;
        let index = self.read_u32(array) as usize;
        let mut out = [0u8; SQE_SIZE];
        let Some(frame) = self.sqe_frames.get(index * SQE_SIZE / PAGE_SIZE) else {
            return out;
        };
        let at = index * SQE_SIZE % PAGE_SIZE;
        unsafe {
            core::ptr::copy_nonoverlapping(
                (phys_to_virt(*frame) + at) as *const u8,
                out.as_mut_ptr(),
                SQE_SIZE,
            )
        };
        out
    }

    /// Post one completion, in the slot the caller's CQ tail names, and wake
    /// whoever is parked on this ring. A full ring drops the record instead of
    /// growing - the caller had `cq_room` answers before it submitted, and the
    /// submit loop never consumes an SQE it cannot complete.
    pub fn post(&self, user_data: u64, res: i32) {
        if self.cq_room() == 0 {
            return;
        }
        let tail = self.cq_tail();
        let slot = (tail % self.cq_entries) as usize;
        let at = CQES + slot * CQE_SIZE;
        self.write_bytes(at, &user_data.to_le_bytes());
        self.write_bytes(at + 8, &res.to_le_bytes());
        self.write_bytes(at + 12, &0u32.to_le_bytes());
        self.write_u32(CQ_TAIL, tail.wrapping_add(1));
        self.wake_waiters();
    }

    /// Park until a completion is available. `true` means one arrived; `false`
    /// means a signal interrupted the park. Only a *second* task entering the
    /// same ring can make one arrive, which is Linux's shape as well - a ring
    /// with nothing submitted and nothing pending blocks until somebody posts.
    pub fn wait_for_completion(&self) -> bool {
        let task = crate::sched::current();
        critical_section::with(|cs| self.waiters.borrow(cs).borrow_mut().push(task));
        let end = crate::sched::block_until_interruptible(|| self.cq_pending() > 0);
        critical_section::with(|cs| {
            let mut waiters = self.waiters.borrow(cs).borrow_mut();
            waiters.retain(|t| *t != task);
        });
        !matches!(end, crate::sched::WaitEnd::Interrupted)
    }

    fn wake_waiters(&self) {
        let waiting: Vec<crate::sched::TaskId> =
            critical_section::with(|cs| self.waiters.borrow(cs).borrow().clone());
        for task in waiting {
            crate::sched::wake(task);
        }
    }

    fn page(&self, index: usize) -> Option<usize> {
        self.ring_frames.get(index).copied()
    }

    fn read_u32(&self, offset: usize) -> u32 {
        let Some(frame) = self.page(offset / PAGE_SIZE) else {
            return 0;
        };
        let at = phys_to_virt(frame) + offset % PAGE_SIZE;
        unsafe { core::ptr::read_unaligned(at as *const u32) }
    }

    fn write_u32(&self, offset: usize, value: u32) {
        let Some(frame) = self.page(offset / PAGE_SIZE) else {
            return;
        };
        let at = phys_to_virt(frame) + offset % PAGE_SIZE;
        unsafe { core::ptr::write_unaligned(at as *mut u32, value) }
    }

    fn write_bytes(&self, offset: usize, bytes: &[u8]) {
        for (i, byte) in bytes.iter().enumerate() {
            let pos = offset + i;
            let Some(frame) = self.page(pos / PAGE_SIZE) else {
                return;
            };
            let at = phys_to_virt(frame) + pos % PAGE_SIZE;
            unsafe { core::ptr::write_unaligned(at as *mut u8, *byte) };
        }
    }

    /// A mapping of one of the three regions, checked before a frame is handed
    /// out: an offset that is not one of Linux's magic ones, or a length past
    /// the region, is `EINVAL` rather than a fault on the first touch.
    pub fn mmap_validate(&self, offset: u64, len: u64) -> VfsResult<()> {
        let ring_pages = self.ring_frames.len() as u64;
        let sqe_pages = self.sqe_frames.len() as u64;
        let region = |pages: u64| pages * PAGE_SIZE as u64;
        match offset {
            IORING_OFF_SQ_RING | IORING_OFF_CQ_RING => {
                if len > region(ring_pages) {
                    return Err(VfsError::Invalid);
                }
            }
            IORING_OFF_SQES => {
                if len > region(sqe_pages) {
                    return Err(VfsError::Invalid);
                }
            }
            _ => return Err(VfsError::Invalid),
        }
        Ok(())
    }

    /// The page at `index` of the object, as `mmap` numbers it: the SQ mapping
    /// starts at 0, the CQ mapping at `IORING_OFF_CQ_RING / PAGE_SIZE` (the
    /// same pages: one mapping can cover both, Linux's `SINGLE_MMAP`), and the
    /// SQE array at `IORING_OFF_SQES / PAGE_SIZE`.
    fn frame_at(&self, index: u64) -> Option<usize> {
        let page_size = PAGE_SIZE as u64;
        let cq_first = IORING_OFF_CQ_RING / page_size;
        let sqe_first = IORING_OFF_SQES / page_size;
        let (frames, page) = if index < self.ring_frames.len() as u64 {
            (&self.ring_frames, index)
        } else if index >= cq_first && index - cq_first < self.ring_frames.len() as u64 {
            (&self.ring_frames, index - cq_first)
        } else if index >= sqe_first && index - sqe_first < self.sqe_frames.len() as u64 {
            (&self.sqe_frames, index - sqe_first)
        } else {
            return None;
        };
        let frame = *frames.get(page as usize)?;
        // The mapping gets its own reference; the inode keeps the one it took
        // when the frame was allocated, and `Drop` returns it.
        clone_frame_ref(frame);
        Some(frame)
    }
}

impl Inode for IoUring {
    fn metadata(&self) -> Metadata {
        Metadata {
            ino: NEXT_INO.fetch_add(1, Ordering::Relaxed),
            kind: NodeKind::Anon { mode: 0o600 },
            size: 0,
        }
    }

    fn fd_name(&self) -> Option<alloc::string::String> {
        Some(alloc::string::String::from("anon_inode:[io_uring]"))
    }

    /// The ring is a mapped region, not a byte stream: reading or writing the
    /// descriptor is Linux's `EINVAL`, not a copy of the headers.
    fn read_at(&self, _offset: u64, _buf: &mut [u8]) -> VfsResult<usize> {
        Err(VfsError::Invalid)
    }

    fn write_at(&self, _offset: u64, _buf: &[u8]) -> VfsResult<usize> {
        Err(VfsError::Invalid)
    }

    fn poll_ready(&self) -> Ready {
        Ready::NONE
    }

    fn mmap_validate(&self, offset: u64, len: u64) -> VfsResult<()> {
        IoUring::mmap_validate(self, offset, len)
    }

    fn mmap_frame(&self, index: u64) -> Option<usize> {
        self.frame_at(index)
    }

    fn frame_backed(&self) -> bool {
        true
    }

    #[cfg(target_os = "none")]
    fn as_io_uring(&self) -> Option<&IoUring> {
        Some(self)
    }
}

impl Drop for IoUring {
    fn drop(&mut self) {
        // The last mapping and the last descriptor are both gone: the pages
        // go back. A mapping holds a reference to this inode, so this only
        // runs when no page table still points at one of the frames.
        LIVE_RINGS.fetch_sub(1, Ordering::Relaxed);
        let ring = core::mem::take(&mut self.ring_frames);
        free_list(&ring);
        let sqes = core::mem::take(&mut self.sqe_frames);
        free_list(&sqes);
    }
}

fn free_list(frames: &[usize]) {
    for frame in frames {
        if *frame != 0 {
            free_frames(*frame, 0);
        }
    }
}
