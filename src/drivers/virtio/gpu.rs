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
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use critical_section::Mutex;

use crate::mm::{self, PAGE_SIZE};
use crate::vfs::{Inode, Metadata, NodeKind, Ready, VfsResult};

use virtio_drivers::transport::Transport;
use virtio_drivers::transport::pci::PciTransport;

use super::gpu_ctrl::Gpu;
use super::hal::transport;

/// The modern device ID; virtio-gpu never had a legacy form. It names the PCI
/// vendor's ID range, so it is the first machine's: the second finds the same
/// device by the `virtio,mmio` window its device tree names, whose `DeviceID` is
/// `16` (`VIRTIO_ID_GPU`).
const DEVICE_GPU: &[u16] = &[0x1050];

/// The resolution a headless machine gets: the one every framebuffer
/// program understands, and the one a display would show if it were there.
const FALLBACK: (u32, u32) = (1024, 768);

/// The scanout's geometry, read and written as one value.
///
/// One value rather than four fields, because callers do not use them one at a
/// time: a compositor's flip takes the width and the buffer address together and
/// copies rows into the block the address names using the pitch the width
/// implies. Read separately across a resize, the two can be a pair that never
/// existed - the width of one screen with the address of another - and a
/// *shrinking* screen makes that an out-of-bounds write, not a wrong pixel. The
/// lock is the same `critical_section::Mutex<RefCell<_>>` every other piece of
/// shared state here uses, so reading it is one interrupt-off section.
#[derive(Clone, Copy)]
pub struct Scanout {
    pub width: u32,
    pub height: u32,
    /// Where the pixel buffer lives, as the device addresses it. The direct map
    /// turns it back into a pointer the kernel can copy through.
    pub fb_phys: usize,
    /// The buffer's length in bytes.
    pub fb_len: usize,
}

/// Pack a width and height into the single `usize` the desired size is stored
/// in, so the two halves are never observed as a mismatched pair.
fn pack_dims(width: u32, height: u32) -> usize {
    ((width as usize) << 32) | height as usize
}

/// The inverse of [`pack_dims`].
fn unpack_dims(packed: usize) -> (u32, u32) {
    ((packed >> 32) as u32, packed as u32)
}

pub struct VirtioGpuDev {
    /// The scanout, as the device was last set up with it. Not a constant: the
    /// host can ask for a different size at any time - a QEMU window resize or a
    /// fullscreen toggle is exactly that - and the driver follows by rebuilding
    /// the resource ([`VirtioGpuDev::resized`], and the resize step in
    /// `drivers::video::render`).
    scanout: Mutex<RefCell<Scanout>>,
    /// Whether userland has ever mapped the pixels. A resize would like to
    /// hand the old block back to the allocator, and must not while a mapping
    /// of it may still exist: a program writing through a mapping that stopped
    /// being the screen would be writing into whatever the allocator gave
    /// those frames to next.
    mapped: AtomicBool,
    /// The size the host currently wants, packed `width << 32 | height` so the
    /// two are read as one value. This is what the connector *advertises* as its
    /// available mode - the mode a compositor re-probes for and may commit - and
    /// it is deliberately separate from [`scanout`][Self::scanout], the size the
    /// scanout is *actually driven at*. On a host event the driver learns a new
    /// `desired` and fires a hotplug uevent without touching the scanout; the
    /// scanout only follows when a KMS master commits a mode of that size, which
    /// is how Linux's virtio-gpu driver behaves. They are equal at boot and
    /// after any committed resize; they differ only in the window between the
    /// host asking and a compositor agreeing. A text console with no compositor
    /// to commit leaves them unequal forever - the screen stays the size it was
    /// driven at, exactly as Linux's fbcon does.
    desired: AtomicUsize,
    /// The size of the resource the scanout is *presenting*, when that resource
    /// is a client's rather than this driver's own framebuffer - `None` when
    /// this driver's framebuffer is on the screen. A compositor draws into its
    /// own resource and the kernel points the device at it
    /// (`drivers::drm::client_scanout`, `Gpu::scan_out_client`), so the size the
    /// screen *is* at that moment is this one, not
    /// [`scanout`][Self::scanout]'s - which is the geometry of the driver's
    /// private buffer, a buffer nothing is showing any more. [`driven_dims`]
    /// is what a reader wants, and keeping the two apart is what stops the card
    /// reporting a size it is not driving: a compositor that reads a stale size
    /// back believes its mode is already set, never commits one, and has every
    /// frame it then sends refused for not fitting the size the card is
    /// actually driving.
    presented: Mutex<RefCell<Option<(u32, u32)>>>,
}

impl VirtioGpuDev {
    /// Read pixels out of the buffer. The copies go through raw pointers
    /// because the buffer is one memory several writers share - a program
    /// writing through its `mmap`, the kernel through `write(2)` - and a
    /// torn pixel is a pixel, not a broken invariant. Bounds are clamped to
    /// the buffer, so a reader past the end sees end of input rather than
    /// whatever the device's DMA block happens to neighbor.
    fn read(&self, offset: u64, buf: &mut [u8]) -> usize {
        let Scanout {
            fb_phys, fb_len, ..
        } = self.scanout();
        let at = (offset as usize).min(fb_len);
        let len = buf.len().min(fb_len - at);
        unsafe {
            core::ptr::copy_nonoverlapping(
                (mm::phys_to_virt(fb_phys) + at) as *const u8,
                buf.as_mut_ptr(),
                len,
            );
        }
        len
    }

    /// Write pixels into the buffer and put them on the screen. The flush is
    /// the point: `write(2)` returning is the promise that the pixels left, the
    /// same contract a write-through disk keeps. The device itself is owned by
    /// the render task, so this asks that task to flush and parks until it has.
    fn write(&self, offset: u64, buf: &[u8]) -> usize {
        let Scanout {
            fb_phys, fb_len, ..
        } = self.scanout();
        let at = (offset as usize).min(fb_len);
        let len = buf.len().min(fb_len - at);
        critical_section::with(|_| unsafe {
            core::ptr::copy_nonoverlapping(
                buf.as_ptr(),
                (mm::phys_to_virt(fb_phys) + at) as *mut u8,
                len,
            );
        });
        // The single flusher is the render task; block until it has moved the
        // frame we just edited, so the write-through promise still holds.
        #[cfg(target_os = "none")]
        crate::drivers::video::render::request_flush_blocking();
        len
    }

    /// The scanout as it is now: geometry and block together.
    ///
    /// A caller that needs two of these numbers must take them from *one*
    /// snapshot - see [`Scanout`] for the write that reading them separately
    /// would allow.
    pub fn scanout(&self) -> Scanout {
        critical_section::with(|cs| *self.scanout.borrow(cs).borrow())
    }

    /// The resolution the framebuffer is driving - what `FBIOGET_VSCREENLINFO`
    /// reports and what a program drawing rows needs.
    ///
    /// It is the *current* one, which a resize can change under a caller: the
    /// value is read here rather than assumed to hold, so a program that asks
    /// again gets the screen that is actually being driven.
    pub fn dims(&self) -> (u32, u32) {
        let scanout = self.scanout();
        (scanout.width, scanout.height)
    }

    /// The size the connector advertises as its available mode - what the host
    /// last asked for, whether or not the scanout has been driven to it. A
    /// compositor reads this through `GETCONNECTOR`/`GETCRTC` when it re-probes
    /// after a hotplug uevent; a pixel copy never does, and uses [`dims`][Self::dims].
    pub fn advertised_dims(&self) -> (u32, u32) {
        unpack_dims(self.desired.load(Ordering::Relaxed))
    }

    /// Record the size the host wants. Returns whether it actually changed, so
    /// the caller only fires a hotplug uevent on a real new mode rather than on
    /// every window move that re-reports the same size.
    pub fn set_advertised(&self, width: u32, height: u32) -> bool {
        let want = pack_dims(width, height);
        self.desired.swap(want, Ordering::Relaxed) != want
    }

    /// The size of the pixel buffer in bytes - `smem_len` in
    /// `fb_fix_screeninfo` terms.
    pub fn fb_len(&self) -> usize {
        self.scanout().fb_len
    }

    /// The physical address of the buffer - what `fb_fix_screeninfo` calls
    /// `smem_start`, and what the pin below keeps out of the allocator's
    /// hands once userland holds a mapping of it.
    pub fn fb_phys(&self) -> usize {
        self.scanout().fb_phys
    }

    /// Whether userland ever mapped the pixels - asked before a resize returns
    /// the old block to the allocator (see the `mapped` field).
    pub fn mapped(&self) -> bool {
        self.mapped.load(Ordering::Relaxed)
    }

    /// A new scanout: the geometry and the block it lives in, as
    /// `drivers::video::render` set them up after a resize.
    pub fn resized(&self, width: u32, height: u32, fb_phys: usize, fb_len: usize) {
        critical_section::with(|cs| {
            *self.scanout.borrow(cs).borrow_mut() = Scanout {
                width,
                height,
                fb_phys,
                fb_len,
            };
        });
    }

    /// The scanout now shows a *client's* resource of this size.
    ///
    /// Recorded rather than inferred: [`driven_dims`] is the only honest answer
    /// to "what size is the screen", and once a compositor's own buffer is the
    /// scanout, the driver's private framebuffer's geometry is a fact about a
    /// buffer nobody is showing.
    pub fn presenting_client(&self, width: u32, height: u32) {
        critical_section::with(|cs| {
            *self.presented.borrow(cs).borrow_mut() = Some((width, height));
        });
    }

    /// The scanout is back on this driver's own framebuffer, so its geometry is
    /// the answer again.
    pub fn presenting_own_framebuffer(&self) {
        critical_section::with(|cs| {
            *self.presented.borrow(cs).borrow_mut() = None;
        });
    }

    /// The size the screen is *actually* driving: the presented resource's when
    /// a client's resource is on the screen, this driver's framebuffer's
    /// otherwise.
    ///
    /// What `GETCRTC` answers and what a committed framebuffer is judged
    /// against - so it must be the size a viewer of the screen would measure,
    /// which is the presented resource's, not the private buffer's.
    pub fn driven_dims(&self) -> (u32, u32) {
        match critical_section::with(|cs| *self.presented.borrow(cs).borrow()) {
            Some(dims) => dims,
            None => self.dims(),
        }
    }

    /// Build the buffer view over a device the machine has already opened: ask
    /// the host what its display is, fall back to the headless shape, and build
    /// the resource at that size.
    ///
    /// The one door in, and it takes the *device* rather than a bus - the first
    /// machine opens the card on its PCI bus ([`init_global`]), the second on
    /// its MMIO window (`super::mmio::gpu_device`), and everything here - the
    /// resolution, the resource, the pinning, the view `/dev/fb0` shares - is
    /// the same on both. `config` is the window the host reports its own display
    /// events in, which only a machine can name; `None` is a machine that cannot
    /// name one, and then the display size is whatever it was at boot, for good.
    ///
    /// Returns the buffer view (which `/dev/fb0` and the page pins share) *and*
    /// the device itself, moved out - the render task is its sole owner, and
    /// the only thing that may call `flush` on it.
    pub fn install<T: Transport>(
        mut device: Gpu<T>,
        config: Option<usize>,
    ) -> Option<(Arc<Self>, Gpu<T>)> {
        // Recorded before the transport is consumed into the device above.
        if let Some(config) = config {
            CONFIG.store(config, Ordering::Relaxed);
        }

        let dims = match device.resolution() {
            Ok((width, height)) if width > 0 && height > 0 => (width, height),
            _ => FALLBACK,
        };
        let slice = device.change_resolution(dims.0, dims.1).ok()?;

        let fb_len = slice.len();
        let fb_phys = mm::virt_to_phys(slice.as_ptr() as usize);
        let pages = fb_len.div_ceil(PAGE_SIZE);

        let dev = Arc::new(VirtioGpuDev {
            scanout: Mutex::new(RefCell::new(Scanout {
                width: dims.0,
                height: dims.1,
                fb_phys,
                fb_len,
            })),
            mapped: AtomicBool::new(false),
            desired: AtomicUsize::new(pack_dims(dims.0, dims.1)),
            presented: Mutex::new(RefCell::new(None)),
        });

        // The pages now have more than one kind of owner: the device DMAes
        // through them, and from the first `mmap` a user process will hold
        // a mapping too. Pin them against every path that hands a mapped
        // page back - `munmap`, a fork's copy-on-write, a process exit -
        // any one of which would otherwise recycle a page the device is
        // still drawing the screen out of.
        mm::pin_frames(fb_phys, pages * PAGE_SIZE);

        // The view the devfs builder publishes as `/dev/fb0`, recorded here
        // because there is one card per machine and this is the call that built
        // it - whichever bus it arrived on.
        critical_section::with(|cs| {
            *GLOBAL.borrow(cs).borrow_mut() = Some(dev.clone());
        });

        crate::drivers::devices::publish("virtio-gpu", publish_nodes);

        Some((dev, device))
    }
}

/// Where the device's own configuration lives, as an MMIO address: the window
/// holding `events_read` and `events_clear`. Zero when the machine has no GPU,
/// which is the same as "nothing to poll".
static CONFIG: AtomicUsize = AtomicUsize::new(0);

/// `VIRTIO_GPU_EVENT_DISPLAY`: bit 0 of `events_read`, set by the host when the
/// display it wants this machine to drive changed shape.
const EVENT_DISPLAY: u32 = 1;

/// Whether the host has asked for a different display since the last look,
/// taking the event out of `events_read` as it answers.
///
/// This is where a window resize enters the kernel, and it is a plain read of
/// the device configuration rather than a control-queue round trip: the host
/// sets the bit and raises a config-change interrupt, and a kernel that polls -
/// this one does - reads the bit instead of taking the interrupt. What the size
/// actually *is* is a `GET_DISPLAY_INFO` command, and that is only worth
/// spending once this says there is a reason to.
pub fn take_display_event() -> bool {
    let at = CONFIG.load(Ordering::Relaxed);
    if at == 0 {
        return false;
    }
    let events = unsafe { (at as *const u32).read_volatile() };
    if events & EVENT_DISPLAY == 0 {
        return false;
    }
    // `events_clear` is the word after `events_read`, and writing the bit back
    // is the acknowledgement: a bit left set would ask the same question on
    // every tick.
    unsafe { (at as *mut u32).add(1).write_volatile(EVENT_DISPLAY) };
    true
}

/// The device the boot found, if it found one - the buffer view only. The
/// render task holds the live device; this is what `/dev/fb0` reaches.
static GLOBAL: Mutex<RefCell<Option<Arc<VirtioGpuDev>>>> = Mutex::new(RefCell::new(None));

/// Probe the PCI bus for the card and bring it up - the first machine's door
/// into [`VirtioGpuDev::install`].
///
/// The card's home is this function, and that is *declared* here rather than
/// inferred by `sysfs` from the class code, which would also match the
/// firmware's own display adapter on a q35 machine and hang the DRM nodes off
/// the wrong function (`NOTES.md` §7.4). A machine with no PCI bus has no
/// function to declare, so it has no `/sys/class/drm` card - a named gap, and
/// not a claim about the card: the device is the same one over its own
/// transport (`NOTES.md` §13.26).
pub fn init_global() -> Option<(Arc<VirtioGpuDev>, Gpu<PciTransport>)> {
    // The device's own configuration window, where the display's *events* arrive
    // (`VIRTIO_GPU_EVENT_DISPLAY`, a resize). It is the window the MSI-X table
    // shares, so a machine with no MSI controller has nothing to poll there and
    // the card comes up without one - the honest `None`, and the same value the
    // event poll treats as "no configuration space" (`mpos_msi`).
    #[cfg(mpos_msi)]
    let config = super::hal::device_config(DEVICE_GPU);
    #[cfg(not(mpos_msi))]
    let config = None;
    let device = Gpu::new(transport(DEVICE_GPU)?).ok()?;
    super::hal::claim_drm_card(DEVICE_GPU);
    VirtioGpuDev::install(device, config)
}

/// The card, if the machine has one.
/// Publish `/dev/fb0` for a device set built after this driver came up -
/// registered by [`VirtioGpuDev::install`], not named by
/// `devices::standard_devfs`.
///
/// The node has more than a stream face (`mmap` of the pixels, the `FBIOGET_*`
/// ioctls), so it is registered as the inode it is, through the door the tty
/// uses for the same reason.
fn publish_nodes(dev: &crate::vfs::devfs::DevFs) {
    if let Some(gpu) = global() {
        dev.register_node("fb0", Arc::new(FbDev::new(gpu)));
    }
}

pub fn global() -> Option<Arc<VirtioGpuDev>> {
    critical_section::with(|cs| GLOBAL.borrow(cs).borrow().clone())
}

/// `/dev/fb0`: the framebuffer as the fd table sees it. A device node
/// rather than a `CharDevice`, because its face is bigger than a stream:
/// reads and writes at offsets, `mmap` of the pixels themselves, and the
/// two `FBIOGET_*` ioctls the fbdev ABI is entered through.
pub struct FbDev(Arc<VirtioGpuDev>, crate::vfs::devfs::NodePerms);

impl FbDev {
    pub fn new(dev: Arc<VirtioGpuDev>) -> Self {
        // devtmpfs births a framebuffer node `0600 root:root` - measured with
        // udev's queue frozen: `/dev/fb1` is strict at creation, then the
        // `graphics` rule hands it to group `video`.
        FbDev(
            dev,
            crate::vfs::devfs::NodePerms::new(crate::vfs::devfs::BIRTH_RESTRICTED, 0, 0),
        )
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

    /// The scanout as it is now, for a caller that needs more than one of its
    /// numbers to agree - `FBIOGET_FSCREENINFO` reports the address *and* the
    /// length of the buffer in one struct.
    pub fn scanout(&self) -> Scanout {
        self.0.scanout()
    }
}

impl Inode for FbDev {
    fn keeps_modes(&self) -> bool {
        self.1.keeps()
    }

    fn mode(&self) -> u32 {
        self.1.mode()
    }

    fn set_mode(&self, mode: u32) {
        self.1.set_mode(mode);
    }

    fn owner(&self) -> (u32, u32) {
        self.1.owner()
    }

    fn set_owner(&self, uid: u32, gid: u32) {
        self.1.set_owner(uid, gid);
    }

    fn atime(&self) -> u64 {
        self.1.atime()
    }

    fn mtime(&self) -> u64 {
        self.1.mtime()
    }

    fn set_times(&self, atime: u64, mtime: u64) -> VfsResult<()> {
        self.1.set_times(atime, mtime);
        Ok(())
    }

    fn metadata(&self) -> Metadata {
        Metadata {
            ino: 0x9001,
            kind: NodeKind::CharDev,
            size: self.0.fb_len() as u64,
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
    ///
    /// The mapping is recorded as taken: a resize may not return the block it
    /// was made from to the allocator while it exists (see
    /// [`VirtioGpuDev::resized`] and the `mapped` field).
    fn mmap_frame(&self, index: u64) -> Option<usize> {
        let index = index as usize;
        let Scanout {
            fb_phys, fb_len, ..
        } = self.0.scanout();
        let pages = fb_len.div_ceil(PAGE_SIZE);
        (index < pages).then(|| {
            self.0.mapped.store(true, Ordering::Relaxed);
            fb_phys + index * PAGE_SIZE
        })
    }

    /// The framebuffer is the page itself: a mapping of it is the scanout, so
    /// it must alias the pinned frames rather than copy them.
    fn frame_backed(&self) -> bool {
        true
    }
}
