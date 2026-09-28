//! `memfd_create(2)`: an anonymous, shareable, sealable memory buffer.
//!
//! The descriptor a Wayland client gets for a `wl_shm` pool is a memfd. The
//! client sizes it with `ftruncate`, maps it, draws into the mapping, and
//! hands the descriptor to the compositor over the unix socket's `SCM_RIGHTS`;
//! the compositor maps *the same descriptor* and must see those pixels. That
//! only works if the mapping is not a private copy - so this object owns real
//! physical pages, grows them on demand, and answers `mmap_frame` with them.
//! Every mapping of one memfd, in every process, lands on the same frames,
//! because `handle_mmap` maps the frame the inode hands it rather than copying
//! file bytes into a fresh one.
//!
//! It is also a plain readable/writable/truncatable file: `read`/`write`
//! reach the very same frames, so a tool that dumps a memfd sees what the
//! mappings wrote. Sealing is recorded and reported (libwayland seals pools
//! after sizing them); whether a particular seal is *enforced* against
//! shrink/grow/write is left advisory here, because this kernel has no reason
//! to refuse the writer of a buffer it just handed out, and a false `EPERM`
//! would stop a real compositor cold.

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::cell::RefCell;
use core::sync::atomic::{AtomicU32, AtomicUsize, Ordering};

use critical_section::Mutex;

use crate::mm::{PAGE_SIZE, alloc_frames, free_frames, phys_to_virt};
use crate::vfs::{Inode, Metadata, NodeKind, Ready, VfsError, VfsResult};

/// `MFD_CLOEXEC`: close the descriptor on exec, as `open(O_CLOEXEC)` does.
pub const MFD_CLOEXEC: usize = 0o001;
/// `MFD_ALLOW_SEALING`: let the owner add `F_SEAL_*` bits through `fcntl`.
pub const MFD_ALLOW_SEALING: usize = 0o002;

/// The `F_SEAL_*` bits, and the `fcntl` commands that read and set them.
pub const F_SEAL_SEAL: u32 = 0x01;
pub const F_SEAL_SHRINK: u32 = 0x02;
pub const F_SEAL_GROW: u32 = 0x04;
pub const F_SEAL_WRITE: u32 = 0x08;
pub const F_ADD_SEALS: usize = 1033;
pub const F_GET_SEALS: usize = 1034;
/// What `F_GET_SEALS` answers for a file sealing was never allowed on.
pub const F_SEAL_UNSUPPORTED: u32 = 0x02;

/// How far a memfd may grow - a cap so a wild `mmap` length cannot exhaust
/// the frame allocator. 65536 pages is 256 MiB, far past a shm pool.
const MAX_PAGES: usize = 1 << 16;

/// One anonymous shared buffer. The frames are the content; the size is how
/// much of them is part of the file (a mapping or a read sees zeros past it,
/// a write grows it).
pub struct Memfd {
    ino: u64,
    /// One physical page frame per page of the buffer, grown as needed.
    frames: Mutex<RefCell<Vec<usize>>>,
    size: AtomicUsize,
    seals: AtomicU32,
}

impl Memfd {
    /// A fresh, empty memfd. Without `MFD_ALLOW_SEALING` the buffer is born
    /// sealed, so no later `fcntl` can add seals to it.
    pub fn new(ino: u64, flags: usize) -> Arc<Memfd> {
        let seals = if flags & MFD_ALLOW_SEALING != 0 {
            0
        } else {
            F_SEAL_SEAL | F_SEAL_UNSUPPORTED
        };
        Arc::new(Memfd {
            ino,
            frames: Mutex::new(RefCell::new(Vec::new())),
            size: AtomicUsize::new(0),
            seals: AtomicU32::new(seals),
        })
    }

    /// The seal word `F_GET_SEALS` reports.
    pub fn seals(&self) -> u32 {
        self.seals.load(Ordering::Relaxed)
    }

    /// `F_ADD_SEALS`: fold more bits in. Refused once `F_SEAL_SEAL` is set,
    /// or if an unrecognised bit is asked for, as Linux's `!F_SEAL_USER`
    /// check answers `EINVAL`.
    pub fn add_seals(&self, want: u32) -> VfsResult<()> {
        const SEALABLE: u32 = F_SEAL_SEAL | F_SEAL_SHRINK | F_SEAL_GROW | F_SEAL_WRITE;
        let cur = self.seals.load(Ordering::Relaxed);
        if cur & F_SEAL_SEAL != 0 || want & !SEALABLE != 0 || want == 0 {
            return Err(VfsError::Invalid);
        }
        self.seals.store(cur | want, Ordering::Relaxed);
        Ok(())
    }

    /// Resize the buffer. Growing is lazy (frames appear on the next write or
    /// mapping); shrinking releases the tail frames that fall past the end.
    pub fn set_len(&self, len: usize) -> VfsResult<()> {
        self.size.store(len, Ordering::Relaxed);
        let keep = len.div_ceil(PAGE_SIZE).min(MAX_PAGES);
        let freed: Vec<usize> = critical_section::with(|cs| {
            let mut f = self.frames.borrow(cs).borrow_mut();
            if f.len() > keep {
                f.drain(keep..).collect()
            } else {
                Vec::new()
            }
        });
        for p in freed {
            if p != 0 {
                free_frames(p, 0);
            }
        }
        Ok(())
    }

    fn frames_len(&self) -> usize {
        critical_section::with(|cs| self.frames.borrow(cs).borrow().len())
    }

    /// The frame of page `idx`, or 0 when the page is not allocated yet (a
    /// read of such a page sees zeros). Never grows the buffer.
    fn page(&self, idx: usize) -> usize {
        critical_section::with(|cs| {
            self.frames
                .borrow(cs)
                .borrow()
                .get(idx)
                .copied()
                .unwrap_or(0)
        })
    }

    /// The frame of page `idx`, allocating (and zeroing) pages up to it.
    /// Returns 0 if the page is past the cap or memory is gone. Frames are
    /// allocated outside the lock, so the frame allocator's own critical
    /// section never nests inside this node's borrow.
    fn ensure_page(&self, idx: usize) -> usize {
        if idx >= MAX_PAGES {
            return 0;
        }
        loop {
            let len = self.frames_len();
            if idx < len {
                return self.page(idx);
            }
            let frame = match alloc_frames(0) {
                Some(f) => f,
                None => return 0,
            };
            unsafe {
                core::ptr::write_bytes(phys_to_virt(frame) as *mut u8, 0, PAGE_SIZE);
            }
            critical_section::with(|cs| self.frames.borrow(cs).borrow_mut().push(frame));
        }
    }

    /// The kernel-visible address of a page's frame, or a run of zeros when
    /// the page is not allocated, for reading a byte span out of the buffer.
    fn read_span(&self, pos: usize, buf: &mut [u8]) {
        let pidx = pos / PAGE_SIZE;
        let off = pos % PAGE_SIZE;
        let frame = self.page(pidx);
        if frame == 0 {
            buf.fill(0);
        } else {
            unsafe {
                core::ptr::copy_nonoverlapping(
                    (phys_to_virt(frame) + off) as *const u8,
                    buf.as_mut_ptr(),
                    buf.len(),
                );
            }
        }
    }
}

impl Inode for Memfd {
    fn metadata(&self) -> Metadata {
        Metadata {
            ino: self.ino,
            kind: NodeKind::File,
            size: self.size.load(Ordering::Relaxed) as u64,
        }
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> VfsResult<usize> {
        let size = self.size.load(Ordering::Relaxed);
        let start = offset as usize;
        if start >= size {
            return Ok(0);
        }
        let n = buf.len().min(size - start);
        let mut done = 0;
        while done < n {
            let pos = start + done;
            let chunk = (PAGE_SIZE - pos % PAGE_SIZE).min(n - done);
            self.read_span(pos, &mut buf[done..done + chunk]);
            done += chunk;
        }
        Ok(n)
    }

    fn write_at(&self, offset: u64, buf: &[u8]) -> VfsResult<usize> {
        let start = offset as usize;
        if (start + buf.len()).div_ceil(PAGE_SIZE) > MAX_PAGES {
            return Err(VfsError::FileTooBig);
        }
        let mut done = 0;
        while done < buf.len() {
            let pos = start + done;
            let chunk = (PAGE_SIZE - pos % PAGE_SIZE).min(buf.len() - done);
            let frame = self.ensure_page(pos / PAGE_SIZE);
            if frame == 0 {
                return Err(VfsError::NoSpace);
            }
            unsafe {
                core::ptr::copy_nonoverlapping(
                    buf.as_ptr().add(done),
                    (phys_to_virt(frame) + pos % PAGE_SIZE) as *mut u8,
                    chunk,
                );
            }
            done += chunk;
        }
        let end = start + buf.len();
        self.size.fetch_max(end, Ordering::Relaxed);
        Ok(buf.len())
    }

    fn truncate(&self, len: u64) -> VfsResult<()> {
        self.set_len(len as usize)
    }

    fn poll_ready(&self) -> Ready {
        Ready::ALL
    }

    /// Page `index` of the buffer, as a physical frame `mmap` maps directly -
    /// the mechanism by which two processes mapping one memfd share bytes.
    /// A mapping past the truncated size grows the frame store, so the tail
    /// of a mapping is always real, never a private copy.
    ///
    /// `clone_frame_ref` hands the mapping process its *own* reference: the
    /// frame the inode allocated is owned by the inode (released in `Drop`),
    /// and every process that maps it must hold a count the process reaper
    /// will drop when it exits - else that exit frees a frame the inode and
    /// the other mappers still use. This is what the pinned framebuffer gets
    /// by being pinned; here the same safety comes from the refcount.
    fn mmap_frame(&self, index: u64) -> Option<usize> {
        let frame = self.ensure_page(index as usize);
        (frame != 0).then(|| {
            crate::mm::clone_frame_ref(frame);
            frame
        })
    }

    /// A memfd is the archetypal frame-backed node: every mapping of it must
    /// alias the same pages, not copy them.
    fn frame_backed(&self) -> bool {
        true
    }

    fn as_memfd(&self) -> Option<&Memfd> {
        Some(self)
    }
}

impl Drop for Memfd {
    fn drop(&mut self) {
        // The last descriptor closed: hand every page back to the allocator.
        // A buffer still mapped by someone is still alive here, so this only
        // runs when the memfd truly has no owners left.
        let frames: Vec<usize> =
            critical_section::with(|cs| core::mem::take(&mut *self.frames.borrow(cs).borrow_mut()));
        for p in frames {
            if p != 0 {
                free_frames(p, 0);
            }
        }
    }
}
