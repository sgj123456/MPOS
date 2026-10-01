//! The virtio-gpu control queue, driven by this kernel rather than by the crate.
//!
//! The kernel used to take the whole device from
//! `virtio_drivers::device::gpu::VirtIOGpu`. It does not any more, and the
//! reason is the 3D family: `DRM_VIRTGPU_*` is carried by this queue, the
//! crate's command set stops at `GET_EDID`, and the queue it owns has no
//! accessor - so a driver that must send `CTX_CREATE`, `RESOURCE_CREATE_3D` or
//! `SUBMIT_3D` has to own the queue, and therefore the encoding with it.
//! `PLAN.md` §7.4 records the measurement this started from and why the crate
//! was not vendored instead.
//!
//! What is left is small because of what the kernel actually asked the crate
//! for: `new`, `resolution()`, `change_resolution()` and `flush()`. Nothing here
//! reads EDID (the DRM layer synthesizes its own block), nothing drives the
//! device's cursor queue (the DRM cursor is a real second plane,
//! `drm_cursor.rs`), and the cursor queue is not even configured - a device
//! whose second queue nobody uses is a queue with no reason to exist.
//!
//! This file lands as a *refactor*: the same commands in the same order, so a
//! boot behaves as it did and the pins that already cover the display path
//! (`t-fb`, `t-drm`, the boot's own probe) are the evidence. The commands the
//! 3D family needs are added on top of it, not in it - which is the whole
//! reason the split is here.

use alloc::vec;
use alloc::vec::Vec;
use core::ptr::NonNull;

use virtio_drivers::queue::VirtQueue;
use virtio_drivers::transport::Transport;
use virtio_drivers::transport::pci::PciTransport;
use virtio_drivers::{BufferDirection, Error, Hal};

use crate::drivers::virtio::hal::HalImpl;
use crate::mm::PAGE_SIZE;
use crate::virtgpu_abi as abi;

/// The control queue: the one every command in `virtgpu_abi` travels on.
const QUEUE_CONTROL: u16 = 0;
/// Two descriptors deep, which is what the crate's device was built with and is
/// all this driver can use: every command below is sent and waited for one at a
/// time, so a third descriptor would be a slot nothing can occupy.
const QUEUE_SIZE: usize = 2;

/// A block of frames the device may read, and the address it reads them at.
///
/// Handed out by the HAL rather than by the allocator directly, because the
/// device needs a *physical* address and the HAL is the only thing that knows
/// how this kernel turns one into the other. The frames are freed when this is
/// dropped - except that a range the DRM layer has pinned is refused, which is
/// the case a resize walks through on purpose (`mm::free_frames`).
struct Frames {
    paddr: usize,
    vaddr: NonNull<u8>,
    pages: usize,
}

impl Frames {
    /// `pages` frames, zeroed. The device may read any of it before the driver
    /// has written a byte, so it starts zeroed rather than as whatever the
    /// allocator had - which is also what makes a partially-drawn first screen
    /// black instead of noise.
    fn new(pages: usize) -> Self {
        let (paddr, vaddr) = <HalImpl as Hal>::dma_alloc(pages, BufferDirection::DriverToDevice);
        Frames {
            paddr: paddr as usize,
            vaddr,
            pages,
        }
    }

    /// The whole block, as the caller's pixels.
    ///
    /// The length is the *allocation's*, `pages * PAGE_SIZE`, and not the
    /// bytes a width and height multiply to - which is what the framebuffer's
    /// own length has always been, and what `fb_fix_screeninfo.smem_len` reports
    /// and the DRM layer pins to. Rounding up here rather than at each caller is
    /// what keeps a `mmap` of the last page a mapping of memory the device owns.
    fn pixels(&mut self) -> &mut [u8] {
        // SAFETY: `dma_alloc` returned `pages` contiguous frames and the pointer
        // to their start; nothing else has a reference to them, and the borrow
        // of `self` is what keeps that true for the life of the slice.
        unsafe { core::slice::from_raw_parts_mut(self.vaddr.as_ptr(), self.pages * PAGE_SIZE) }
    }
}

impl Drop for Frames {
    fn drop(&mut self) {
        // SAFETY: these are the same `paddr`, `vaddr` and page count that
        // `dma_alloc` returned, and nothing else has freed them - `self` is the
        // only owner and this runs once.
        unsafe {
            <HalImpl as Hal>::dma_dealloc(self.paddr as u64, self.vaddr, self.pages);
        }
    }
}

// SAFETY: the frames are uniquely owned by this value. Nothing else in the
// kernel holds their address - the device reads them but never writes them - and
// the kernel is uniprocessor, so the property `Send` has to carry here is that
// two cores cannot reach the same pages at once. That holds by construction: the
// only way to them is through `&mut self`, and the value lives behind the render
// task's `critical_section::Mutex`. The crate makes the same claim of its own
// DMA block; this is that block, allocated the same way.
unsafe impl Send for Frames {}

/// The device: one transport, one control queue, and the scanout's memory.
pub struct Gpu {
    transport: PciTransport,
    control: VirtQueue<HalImpl, QUEUE_SIZE>,
    /// The request being sent, and the response being read. One pair reused for
    /// every command, because `add_notify_wait_pop` does not return until the
    /// device has consumed the first and written the second - so no two commands
    /// can be in flight over them.
    send: Vec<u8>,
    recv: Vec<u8>,
    /// The framebuffer's resource memory, while a scanout points at it.
    fb: Option<Frames>,
    /// The rectangle the scanout was last set to, which `flush` needs and
    /// `change_resolution` is the only thing that sets.
    rect: Option<abi::Rect>,
}

impl Gpu {
    /// Bring up the device: negotiate features, configure the control queue,
    /// and set the driver-status to OK.
    ///
    /// The feature word is `abi::WANTED_FEATURES` and not "whatever the device
    /// offers" - a feature accepted and unused is a promise to a device that
    /// will act on it. What comes back is what was *agreed*, which is what the
    /// queue layout depends on: indirect descriptors and the event index are
    /// queue features, and asking `VirtQueue::new` for a layout the device did
    /// not agree to would build a ring it cannot read.
    pub fn new(mut transport: PciTransport) -> Result<Self, Error> {
        let agreed = transport.begin_init(abi::WANTED_FEATURES);
        let control = VirtQueue::new(
            &mut transport,
            QUEUE_CONTROL,
            agreed.contains(abi::Features::RING_INDIRECT_DESC),
            agreed.contains(abi::Features::RING_EVENT_IDX),
        )?;
        transport.finish_init();

        Ok(Gpu {
            transport,
            control,
            send: vec![0u8; PAGE_SIZE],
            recv: vec![0u8; PAGE_SIZE],
            fb: None,
            rect: None,
        })
    }

    /// Send one command and hand back the first `rsp_len` bytes of its answer.
    ///
    /// `parts` are concatenated into the send buffer and go out as one
    /// descriptor chain entry, which is how the devices below expect a command:
    /// the header and its fields are one contiguous struct, and so is a struct
    /// with a variable tail (`RESOURCE_ATTACH_BACKING` followed by its
    /// `MemEntry` list).
    fn submit(&mut self, parts: &[&[u8]], rsp_len: usize) -> Result<&[u8], Error> {
        let mut at = 0;
        for part in parts {
            self.send[at..at + part.len()].copy_from_slice(part);
            at += part.len();
        }
        // A reply longer than a header is one the driver reads fields out of, so
        // the buffer is cleared first: `add_notify_wait_pop` reports how many
        // bytes the device wrote but does not shorten the slice, and a device
        // that answered with less than it owed would otherwise have the rest
        // read back as the *previous* command's reply. A header-only reply needs
        // no clear - the header is written in full or not at all - and is not
        // cleared, because `flush` is on the per-frame path.
        if rsp_len > size_of::<abi::CtrlHeader>() {
            self.recv.fill(0);
        }
        self.control.add_notify_wait_pop(
            &[&self.send[..at]],
            &mut [&mut self.recv],
            &mut self.transport,
        )?;
        Ok(&self.recv[..rsp_len])
    }

    /// A command whose whole answer is its `CtrlHeader`.
    fn header<Req: Copy>(&mut self, request: &Req) -> Result<abi::CtrlHeader, Error> {
        let bytes = abi::as_bytes(request);
        let answer = self.submit(&[bytes], size_of::<abi::CtrlHeader>())?;
        abi::read_struct::<abi::CtrlHeader>(answer).ok_or(Error::IoError)
    }

    /// A command that succeeds by answering `RESP_OK_NODATA`, and nothing else.
    ///
    /// Every `RESP_ERR_*` carries a reason the device thought worth naming, and
    /// this is where it stops being one: `Error::IoError` is what the caller can
    /// act on (the command did not happen), and the *code* is lost. That is
    /// deliberate for now rather than an oversight - nothing in the 2D path
    /// branches on which error came back, and a caller that did would want the
    /// code routed to it, which is a shape to add when there is one.
    fn ok<Req: Copy>(&mut self, request: &Req) -> Result<(), Error> {
        let answer = self.header(request)?;
        if answer.is(abi::RESP_OK_NODATA) {
            Ok(())
        } else {
            Err(Error::IoError)
        }
    }

    /// The shape the host wants this machine to drive, from `GET_DISPLAY_INFO`.
    ///
    /// Scanout 0 and not the device's first *enabled* one: this kernel drives
    /// exactly one scanout and the DRM layer names one connector, so "the first
    /// mode" and "ours" are the same entry. An answer of zeroes is passed
    /// through rather than refused here - the callers decide, and both of them
    /// already treat a zero dimension as "no monitor" (`init` falls back to
    /// 1024x768, `probe_display` does nothing), which is the behaviour this
    /// refactor is keeping.
    pub fn resolution(&mut self) -> Result<(u32, u32), Error> {
        let request = abi::CtrlHeader::new(abi::CMD_GET_DISPLAY_INFO);
        let answer: abi::RespDisplayInfo = {
            let bytes = abi::as_bytes(&request);
            let reply = self.submit(&[bytes], size_of::<abi::RespDisplayInfo>())?;
            abi::read_struct(reply).ok_or(Error::IoError)?
        };
        if !answer.header.is(abi::RESP_OK_DISPLAY_INFO) {
            return Err(Error::IoError);
        }
        let mode = answer.pmodes[abi::SCANOUT_ID as usize];
        Ok((mode.rect.width, mode.rect.height))
    }

    /// Build a scanout of `width` x `height` and hand back its pixels.
    ///
    /// The resource is torn down first if one exists, in the order the device
    /// requires - point the scanout away, detach the backing, release the
    /// resource - and the old block's frames are freed as it goes. A frame the
    /// DRM layer has pinned is *refused* by `free_frames`, which is why a resize
    /// can leave one leaked rather than handed to the allocator while a user
    /// mapping of it may still exist (`video/render.rs` has the long version).
    ///
    /// The returned slice is the whole allocation, `pages * PAGE_SIZE` - see
    /// [`Frames::pixels`] for why the length is not `width * height * 4`.
    pub fn change_resolution(&mut self, width: u32, height: u32) -> Result<&mut [u8], Error> {
        let rect = abi::Rect::whole(width, height);

        if self.fb.is_some() {
            self.set_scanout(abi::Rect::default(), abi::SCANOUT_ID, 0)?;
            self.ok(&abi::ResourceDetachBacking {
                header: abi::CtrlHeader::new(abi::CMD_RESOURCE_DETACH_BACKING),
                resource_id: abi::RESOURCE_FB,
                padding: 0,
            })?;
            self.ok(&abi::ResourceUnref {
                header: abi::CtrlHeader::new(abi::CMD_RESOURCE_UNREF),
                resource_id: abi::RESOURCE_FB,
                padding: 0,
            })?;
            self.fb = None;
        }

        self.rect = Some(rect);
        self.ok(&abi::ResourceCreate2d {
            header: abi::CtrlHeader::new(abi::CMD_RESOURCE_CREATE_2D),
            resource_id: abi::RESOURCE_FB,
            format: abi::FORMAT_B8G8R8A8_UNORM,
            width,
            height,
        })?;

        let size = width as usize * height as usize * 4;
        let frames = Frames::new(size.div_ceil(PAGE_SIZE));
        // The request and its one `MemEntry` are two structs the device reads as
        // one stream, so they go out concatenated in that order.
        let attach = abi::ResourceAttachBacking {
            header: abi::CtrlHeader::new(abi::CMD_RESOURCE_ATTACH_BACKING),
            resource_id: abi::RESOURCE_FB,
            nr_entries: 1,
        };
        let entry = abi::MemEntry {
            addr: frames.paddr as u64,
            length: size as u32,
            padding: 0,
        };
        let answer = self.submit(
            &[abi::as_bytes(&attach), abi::as_bytes(&entry)],
            size_of::<abi::CtrlHeader>(),
        )?;
        let answer = abi::read_struct::<abi::CtrlHeader>(answer).ok_or(Error::IoError)?;
        if !answer.is(abi::RESP_OK_NODATA) {
            return Err(Error::IoError);
        }

        self.set_scanout(rect, abi::SCANOUT_ID, abi::RESOURCE_FB)?;
        self.fb = Some(frames);
        Ok(self.fb.as_mut().expect("just stored").pixels())
    }

    /// Put the framebuffer on the screen: copy it into the device's resource,
    /// then tell the device to show the rectangle.
    ///
    /// Two commands, and both are needed. The transfer is what moves the pixels
    /// out of guest memory; the flush is what makes the scanout show them. A
    /// driver that only transferred would leave the previous frame on screen
    /// while its own memory held the new one - which is the bug the transfer
    /// exists to prevent, not a thing the flush implies.
    pub fn flush(&mut self) -> Result<(), Error> {
        let rect = self.rect.ok_or(Error::NotReady)?;
        self.ok(&abi::TransferToHost2d {
            header: abi::CtrlHeader::new(abi::CMD_TRANSFER_TO_HOST_2D),
            rect,
            offset: 0,
            resource_id: abi::RESOURCE_FB,
            padding: 0,
        })?;
        self.ok(&abi::ResourceFlush {
            header: abi::CtrlHeader::new(abi::CMD_RESOURCE_FLUSH),
            rect,
            resource_id: abi::RESOURCE_FB,
            padding: 0,
        })
    }

    /// Point `scanout_id` at `resource_id`, or at nothing when `resource_id` is
    /// zero - which is how the device is told to stop scanning a resource out
    /// that is about to be released.
    fn set_scanout(
        &mut self,
        rect: abi::Rect,
        scanout_id: u32,
        resource_id: u32,
    ) -> Result<(), Error> {
        self.ok(&abi::SetScanout {
            header: abi::CtrlHeader::new(abi::CMD_SET_SCANOUT),
            rect,
            scanout_id,
            resource_id,
        })
    }
}
