//! dma-buf, the buffer-sharing descriptor: GEM handles by another door.
//!
//! A dma-buf is how a buffer crosses a process or device boundary on Linux:
//! one side's object is named by a descriptor the other side `mmap`s and
//! draws into - a GBM/EGL stack on the same machine, or another process over
//! `SCM_RIGHTS`. mpos has one producer of them, the DRM card's PRIME export,
//! and one consumer that matters: Mesa's software GBM path, where llvmpipe
//! renders into the dumb buffer behind the descriptor through its mapping
//! while the compositor imports the same object back to scan it out.
//!
//! v1 is *guest-internal* sharing, and named as such: every dma-buf is plain
//! guest RAM (the only real DMA in the machine is the GPU's own scanout), so
//! `DMA_BUF_IOCTL_SYNC` is accepted as the no-op a coherent buffer needs and
//! nothing more - no cross-device attach, no implicit synchronization to
//! hold a writer back with. What *is* real is the ownership discipline: the
//! pages are the exporter's, this object holds one reference per page (taken
//! at export), and every mapping takes its own reference through
//! `clone_frame_ref` - the same rule the memfd and the dumb buffers live by,
//! so a buffer dies exactly when its last descriptor and mapping do, and
//! never while a scanout still points into it.

use alloc::vec::Vec;
use core::cell::RefCell;
use core::sync::atomic::{AtomicU64, Ordering};

use critical_section::Mutex;

use crate::mm::{clone_frame_ref, free_frames};
use crate::vfs::{Inode, Metadata, NodeKind, Ready};

/// Numbers for descriptors that have no name to be numbered by; `fstat` is
/// the only reporter, and the DRM card's identity table is the only reader.
static NEXT_INO: AtomicU64 = AtomicU64::new(0x9300);

/// One exported buffer: the geometry the importing side needs to make a
/// handle of it, and the pages themselves.
///
/// The frames are *references*: the exporting handle holds its own, this
/// object holds one per page (cloned for it at export), and every mapping
/// takes one more - so closing the last descriptor leaves the pages alive
/// until the last mapping goes too, and destroying the exporting handle does
/// not pull them out from under a live dma-buf.
pub struct DmaBuf {
    ino: u64,
    width: u32,
    height: u32,
    pitch: u32,
    size: usize,
    frames: Mutex<RefCell<Vec<usize>>>,
}

impl DmaBuf {
    /// A dma-buf over `frames`, each of which this object now holds one
    /// reference to - the caller cloned them for exactly this purpose (see
    /// the DRM card's PRIME export).
    pub fn new(width: u32, height: u32, pitch: u32, frames: Vec<usize>) -> DmaBuf {
        DmaBuf {
            ino: NEXT_INO.fetch_add(1, Ordering::Relaxed),
            width,
            height,
            pitch,
            size: pitch as usize * height as usize,
            frames: Mutex::new(RefCell::new(frames)),
        }
    }

    /// The number this buffer reports as `st_ino` - the DRM card's identity
    /// key for "the same object again".
    pub fn ino(&self) -> u64 {
        self.ino
    }

    pub fn width(&self) -> u32 {
        self.width
    }

    pub fn height(&self) -> u32 {
        self.height
    }

    /// The buffer's pitch - the byte stride the importing side must honor.
    pub fn pitch(&self) -> u32 {
        self.pitch
    }

    /// The pages with one fresh reference taken on each, for the DRM import
    /// that builds a GEM handle out of this descriptor. The references are
    /// the importer's own: its `GEM_CLOSE`/`DESTROY_DUMB` drops them.
    pub fn clone_frames(&self) -> Vec<usize> {
        critical_section::with(|cs| {
            let frames = self.frames.borrow(cs).borrow().clone();
            for &frame in &frames {
                if frame != 0 {
                    clone_frame_ref(frame);
                }
            }
            frames
        })
    }
}

impl Inode for DmaBuf {
    fn metadata(&self) -> Metadata {
        // A regular file the size of the buffer: `fstat` on the descriptor is
        // how an importer learns how many bytes there are to map.
        Metadata {
            ino: self.ino,
            kind: NodeKind::File,
            size: self.size as u64,
        }
    }

    // No `read_at`/`write_at`: a dma-buf is memory to map, not a file to
    // stream. Linux has no byte-stream face on one either, and the inherited
    // default ("unsupported") is the honest refusal here.

    fn poll_ready(&self) -> Ready {
        // A buffer is coherent and always reachable: a `poll` on the
        // descriptor (Linux's implicit-sync readiness question) never waits.
        Ready::ALL
    }

    /// Page `index` of the buffer, as a physical frame `mmap` maps directly -
    /// the mechanism by which the importer's renderer and the exporter's
    /// scanout alias the same pixels. The mapping takes its own reference,
    /// per the module's discipline.
    fn mmap_frame(&self, index: u64) -> Option<usize> {
        let frame = critical_section::with(|cs| {
            self.frames.borrow(cs).borrow().get(index as usize).copied()
        });
        match frame {
            Some(frame) if frame != 0 => {
                clone_frame_ref(frame);
                Some(frame)
            }
            _ => None,
        }
    }

    fn as_dmabuf(&self) -> Option<&DmaBuf> {
        Some(self)
    }
}

impl Drop for DmaBuf {
    fn drop(&mut self) {
        // The last descriptor closed: give back the references the export
        // cloned for this object. Frames still mapped, or still owned by the
        // exporting handle, outlive this drop - each of those holders has a
        // count of its own.
        let frames: Vec<usize> =
            critical_section::with(|cs| core::mem::take(&mut *self.frames.borrow(cs).borrow_mut()));
        for frame in frames {
            if frame != 0 {
                free_frames(frame, 0);
            }
        }
    }
}
