//! virtio-gpu, through the `virtio-drivers` crate.
//!
//! The device is a 2D scanout: the crate creates a host-visible resource,
//! attaches a block of DMA memory to it, points a scanout at it, and hands
//! back that block as a pixel buffer - BGRA, row-major, one `flush` away
//! from the screen. What is left here is which resolution to set up, the
//! write-through face of the buffer, and `/dev/fb0`, the node a framebuffer
//! program opens.
//!
//! The resolution is asked for, not assumed: `GET_DISPLAY_INFO` reports what
//! the host attached, and a headless machine reports nothing - a QEMU with
//! `-display none` has no monitor to have a shape - so the driver falls back
//! to the classic 1024x768 rather than build a zero-pixel framebuffer that
//! could never be shown. The scanout is set to the resource either way: the
//! host renders whatever arrives, whenever a display is attached later.
//!
//! The buffer is exposed three ways, all honestly: `read(2)` and `write(2)`
//! through the node - `write` copies in and flushes, the same write-through
//! rule the disk filesystem lives by, so what a writer sent is on screen
//! before its syscall returns; `mmap(2)`, which maps the DMA pages
//! themselves into the caller rather than a copy of them, so a program
//! drawing directly into the framebuffer sees its pixels on the next flush;
//! and the `FBIOGET_*` ioctls, which answer the `struct fb_var_screeninfo`
//! and `fb_fix_screeninfo` every fbdev program starts by reading.

use alloc::sync::Arc;
use core::cell::RefCell;

use critical_section::Mutex;
use virtio_drivers::device::gpu::VirtIOGpu;
use virtio_drivers::transport::pci::PciTransport;

use crate::mm::{self, PAGE_SIZE};
use crate::vfs::{Inode, Metadata, NodeKind, Ready, VfsResult};

use super::hal::{HalImpl, transport};

/// The modern device ID; virtio-gpu never had a legacy form.
const DEVICE_GPU: &[u16] = &[0x1050];

/// The resolution a headless machine gets: the one every framebuffer
/// program understands, and the one a display would show if it were there.
const FALLBACK: (u32, u32) = (1024, 768);

pub struct VirtioGpuDev {
    /// The device, behind a lock: every operation the crate offers takes
    /// `&mut`, and the framebuffer is one device however many windows are
    /// drawn through it.
    device: Mutex<RefCell<VirtIOGpu<HalImpl, PciTransport>>>,
    width: u32,
    height: u32,
    /// Where the pixel buffer lives, as the device addresses it. The direct
    /// map turns it back into a pointer the kernel can copy through.
    fb_phys: usize,
    fb_len: usize,
}

impl VirtioGpuDev {
    /// Read pixels out of the buffer. The copies go through raw pointers
    /// because the buffer is one memory several writers share - a program
    /// writing through its `mmap`, the kernel through `write(2)` - and a
    /// torn pixel is a pixel, not a broken invariant. Bounds are clamped to
    /// the buffer, so a reader past the end sees end of input rather than
    /// whatever the device's DMA block happens to neighbor.
    fn read(&self, offset: u64, buf: &mut [u8]) -> usize {
        let at = (offset as usize).min(self.fb_len);
        let len = buf.len().min(self.fb_len - at);
        unsafe {
            core::ptr::copy_nonoverlapping(
                (mm::phys_to_virt(self.fb_phys) + at) as *const u8,
                buf.as_mut_ptr(),
                len,
            );
        }
        len
    }

    /// Write pixels into the buffer and put them on the screen. The flush
    /// is the point: `write(2)` returning is the promise that the pixels
    /// left, the same contract a write-through disk keeps.
    fn write(&self, offset: u64, buf: &[u8]) -> usize {
        let at = (offset as usize).min(self.fb_len);
        let len = buf.len().min(self.fb_len - at);
        critical_section::with(|cs| {
            unsafe {
                core::ptr::copy_nonoverlapping(
                    buf.as_ptr(),
                    (mm::phys_to_virt(self.fb_phys) + at) as *mut u8,
                    len,
                );
            }
            // One flush per write, under the lock the device's queue needs
            // anyway: a writer that interleaved with another's flush could
            // put half of each on the screen, and the cheap answer - the
            // whole write, then the flush - is the one the write-through
            // contract promises.
            let _ = self.device.borrow(cs).borrow_mut().flush();
        });
        len
    }

    /// The resolution the framebuffer was built at - what
    /// `FBIOGET_VSCREENLINFO` reports and what a program drawing rows needs.
    pub fn dims(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    /// The size of the pixel buffer in bytes - `smem_len` in
    /// `fb_fix_screeninfo` terms.
    pub fn fb_len(&self) -> usize {
        self.fb_len
    }

    /// The physical address of the buffer - what `fb_fix_screeninfo` calls
    /// `smem_start`, and what the pin below keeps out of the allocator's
    /// hands once userland holds a mapping of it.
    pub fn fb_phys(&self) -> usize {
        self.fb_phys
    }

    /// Probe the PCI bus and bring up the first virtio-gpu device: ask the
    /// host what its display is, fall back to the headless shape, and build
    /// the resource at that size.
    pub fn init() -> Option<Arc<Self>> {
        let mut device = VirtIOGpu::<HalImpl, _>::new(transport(DEVICE_GPU)?).ok()?;

        let dims = match device.resolution() {
            Ok((width, height)) if width > 0 && height > 0 => (width, height),
            _ => FALLBACK,
        };
        let slice = device.change_resolution(dims.0, dims.1).ok()?;

        let fb_len = slice.len();
        let fb_phys = mm::virt_to_phys(slice.as_ptr() as usize);
        let pages = fb_len.div_ceil(PAGE_SIZE);

        let dev = Arc::new(VirtioGpuDev {
            device: Mutex::new(RefCell::new(device)),
            width: dims.0,
            height: dims.1,
            fb_phys,
            fb_len,
        });

        // The pages now have more than one kind of owner: the device DMAes
        // through them, and from the first `mmap` a user process will hold
        // a mapping too. Pin them against every path that hands a mapped
        // page back - `munmap`, a fork's copy-on-write, a process exit -
        // any one of which would otherwise recycle a page the device is
        // still drawing the screen out of.
        mm::pin_frames(fb_phys, pages * PAGE_SIZE);

        Some(dev)
    }
}

/// The device the boot found, if it found one.
static GLOBAL: Mutex<RefCell<Option<Arc<VirtioGpuDev>>>> = Mutex::new(RefCell::new(None));

/// Probe for the card and record it under the name userland will open.
pub fn init_global() -> Option<Arc<VirtioGpuDev>> {
    let dev = VirtioGpuDev::init()?;
    critical_section::with(|cs| {
        let mut slot = GLOBAL.borrow(cs).borrow_mut();
        *slot = Some(dev.clone());
    });
    Some(dev)
}

/// The card, if the machine has one.
pub fn global() -> Option<Arc<VirtioGpuDev>> {
    critical_section::with(|cs| GLOBAL.borrow(cs).borrow().clone())
}

/// `/dev/fb0`: the framebuffer as the fd table sees it. A device node
/// rather than a `CharDevice`, because its face is bigger than a stream:
/// reads and writes at offsets, `mmap` of the pixels themselves, and the
/// two `FBIOGET_*` ioctls the fbdev ABI is entered through.
pub struct FbDev(Arc<VirtioGpuDev>);

impl FbDev {
    pub fn new(dev: Arc<VirtioGpuDev>) -> Self {
        FbDev(dev)
    }

    /// The resolution the framebuffer was built at.
    pub fn dims(&self) -> (u32, u32) {
        self.0.dims()
    }

    /// The size of the pixel buffer in bytes.
    pub fn fb_len(&self) -> usize {
        self.0.fb_len()
    }

    /// The physical address of the pixel buffer.
    pub fn fb_phys(&self) -> usize {
        self.0.fb_phys()
    }
}

impl Inode for FbDev {
    fn metadata(&self) -> Metadata {
        Metadata {
            ino: 0x9001,
            kind: NodeKind::CharDev,
            size: self.0.fb_len as u64,
        }
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> VfsResult<usize> {
        Ok(self.0.read(offset, buf))
    }

    fn write_at(&self, offset: u64, buf: &[u8]) -> VfsResult<usize> {
        Ok(self.0.write(offset, buf))
    }

    fn poll_ready(&self) -> Ready {
        // A framebuffer never blocks: there is always a screenful to read
        // and always room to write one.
        Ready::ALL
    }

    /// The `index`-th page of the framebuffer, as a physical address
    /// `mmap` can map. Frames reach userland through here, which is why
    /// the device pinned them at boot.
    fn mmap_frame(&self, index: u64) -> Option<usize> {
        let index = index as usize;
        let pages = self.0.fb_len.div_ceil(PAGE_SIZE);
        (index < pages).then(|| self.0.fb_phys + index * PAGE_SIZE)
    }

    /// The framebuffer this node is - how the fbdev ioctls reach the
    /// resolution and the buffer without the syscall layer knowing where
    /// framebuffers live.
    #[cfg(target_os = "none")]
    fn as_fb(&self) -> Option<&FbDev> {
        Some(self)
    }
}
