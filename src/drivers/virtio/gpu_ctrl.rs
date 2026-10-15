//! The virtio-gpu control queue, driven by this kernel rather than by the crate.
//!
//! The kernel used to take the whole device from
//! `virtio_drivers::device::gpu::VirtIOGpu`. It does not any more, and the
//! reason is the 3D family: `DRM_VIRTGPU_*` is carried by this queue, the
//! crate's command set stops at `GET_EDID`, and the queue it owns has no
//! accessor - so a driver that must send `CTX_CREATE`, `RESOURCE_CREATE_3D` or
//! `SUBMIT_3D` has to own the queue, and therefore the encoding with it.
//! `NOTES.md` §7.4 records the measurement this started from and why the crate
//! was not vendored instead.
//!
//! What is left is small because of what the kernel actually asked the crate
//! for: `new`, `resolution()`, `change_resolution()` and `flush()`. Nothing here
//! reads EDID (the DRM layer synthesizes its own block). The cursor queue *is*
//! configured when the device has one - `UPDATE_CURSOR`/`MOVE_CURSOR` go out on it
//! (`submit_cursor`, [`Gpu::update_cursor`]) so a virtualized host draws the
//! pointer itself - and where a device has no second queue the DRM layer falls back
//! to blending the cursor into the scanout (`drm.rs`'s `paint_cursor`). Which path
//! runs is decided once at bring-up by whether `VirtQueue::new` accepted the cursor
//! queue, published upward as `Card::cursor_q`; see `docs/notes/` §7.4 for the
//! measurement that made the host cursor the primary path.
//!
//! This file lands as a *refactor*: the same commands in the same order, so a
//! boot behaves as it did and the pins that already cover the display path
//! (`t-fb`, `t-drm`, the boot's own probe) are the evidence. The commands the
//! 3D family needs are added on top of it, not in it - which is the whole
//! reason the split is here.

use alloc::vec::Vec;
use core::ptr::NonNull;

use virtio_drivers::queue::VirtQueue;
use virtio_drivers::transport::Transport;
use virtio_drivers::{BufferDirection, Error, Hal};

use crate::drivers::virtio::hal::HalImpl;
use crate::gpu::abi;
use crate::mm::PAGE_SIZE;

/// The control queue: the one every command in `virtgpu_abi` travels on.
const QUEUE_CONTROL: u16 = 0;
/// The cursor queue: the one `UPDATE_CURSOR` and `MOVE_CURSOR` travel on. Every
/// other command in this driver is a control-queue command; these two are the
/// exception the spec carves out (`virtio_gpu_update_cursor`/`move_cursor` are
/// "data passed in the cursor vq"), so they need their own ring and their own
/// scratch rather than the control queue's.
const QUEUE_CURSOR: u16 = 1;
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

/// The command surface the render task drives, over whichever transport the
/// device arrived on.
///
/// This is where the device's type is erased, and it exists for one reason: the
/// crate's `Transport` is not object-safe, so `Gpu<T>` cannot be boxed as a
/// *device* and `video::render` - which owns the one device in a `static` and is
/// the only context that may call `flush` - could not hold both machines' cards
/// without this. The trait names exactly the commands that task sends (and
/// nothing else: a method no caller has is a promise nothing keeps) and
/// [`Gpu<T>`] implements it once, so the transport stops here and the render
/// task, the DRM layer above it, the framebuffer below it and `drivers::video`
/// are one copy on both machines (`NOTES.md` §13.26).
///
/// `Send` because the device is moved into the render task.
pub trait GpuDevice: Send {
    /// How many cap sets the device's configuration says it has.
    fn num_capsets(&self) -> u32;

    /// The features the transport actually agreed to - what every 3D answer
    /// above this is gated on.
    fn features(&self) -> abi::Features;

    /// What resolution the host's display has, or a device with no display.
    fn resolution(&mut self) -> Result<(u32, u32), Error>;

    /// Build the scanout's resource at `width` x `height` and point the scanout
    /// at it, handing back the pixels.
    fn change_resolution(&mut self, width: u32, height: u32) -> Result<&mut [u8], Error>;

    /// Put the current scanout's rectangle on the host's display.
    fn flush(&mut self) -> Result<(), Error>;

    /// Put a **client's** frame on the display: point the scanout at `resource`
    /// and flush that resource, with no transfer - the device holds its pixels.
    ///
    /// The other present path, and the one a compositor needs on a card with 3D:
    /// [`GpuDevice::flush`] carries *this driver's* framebuffer out of guest
    /// memory, and a client's frame was never there. Which of the two a frame
    /// wants is `drivers/drm/`'s `client_scanout` to say; this call is the
    /// render task performing it.
    fn scan_out_client(&mut self, resource: u32, width: u32, height: u32) -> Result<(), Error>;

    /// Attach a resource's backing store: `(address, length)` pairs of guest
    /// memory the device may read for that resource.
    fn attach_backing(&mut self, id: u32, entries: &[(u64, u32)]) -> Result<(), Error>;

    /// Release a resource.
    fn unref(&mut self, id: u32) -> Result<(), Error>;

    /// Make a 2D resource `width` x `height` - the scanout's own kind.
    fn create_2d(&mut self, id: u32, width: u32, height: u32) -> Result<(), Error>;

    /// Make a 3D resource from the caller's request - what a render server
    /// allocates for a client.
    fn create_3d(&mut self, request: &abi::ResourceCreate3d) -> Result<(), Error>;

    /// Copy a rectangle of a resource from guest memory to the device.
    fn transfer_to_host_2d(&mut self, id: u32, width: u32, height: u32) -> Result<(), Error>;

    /// Make a 3D context - the render server's per-client door.
    fn ctx_create(&mut self, id: u32, context_init: u32, name: &[u8]) -> Result<(), Error>;

    /// Bind a resource to a context, so the context's commands may name it.
    fn ctx_attach_resource(&mut self, ctx: u32, resource: u32) -> Result<(), Error>;

    /// The other direction: the context stops being allowed to name `resource`.
    ///
    /// Sent when the handle that named it is released, and before that release's
    /// own unref - which is Linux's order (`gem_object_close` detaches, the object
    /// outlives the handle) and this card's too, because a resource id here is a
    /// handle slot the next create re-uses.
    fn ctx_detach_resource(&mut self, ctx: u32, resource: u32) -> Result<(), Error>;

    /// Move a resource between guest memory and the device, in either
    /// direction.
    fn transfer_host_3d(
        &mut self,
        to_host: bool,
        ctx: u32,
        request: &abi::TransferHost3d,
    ) -> Result<(), Error>;

    /// Submit one command stream to a context, under a fence the caller names.
    fn submit_3d(&mut self, ctx: u32, ring_idx: u8, fence: u64, stream: &[u8])
    -> Result<(), Error>;

    /// What the device's cap set at `index` is - the walk that builds the DRM
    /// layer's table.
    fn get_capset_info(&mut self, index: u32) -> Result<abi::RespCapsetInfo, Error>;

    /// One cap set's bytes, by id and version.
    fn get_capset(&mut self, id: u32, version: u32, out: &mut [u8]) -> Result<usize, Error>;

    /// Whether this device has a cursor queue - the fact the DRM layer branches
    /// on between a host-rendered pointer and a software-blended one.
    fn has_cursor_queue(&self) -> bool;

    /// Load `resource_id` as the pointer image at (`x`, `y`) with hotspot
    /// (`hot_x`, `hot_y`), on the cursor queue. See the inherent method for the
    /// resource-must-already-be-transferred ordering this depends on.
    fn update_cursor(
        &mut self,
        resource_id: u32,
        x: u32,
        y: u32,
        hot_x: u32,
        hot_y: u32,
    ) -> Result<(), Error>;

    /// Move the loaded pointer image to (`x`, `y`) on the cursor queue, touching
    /// no resource.
    fn move_cursor(&mut self, x: u32, y: u32) -> Result<(), Error>;
}

/// The device: one transport, one control queue, and the scanout's memory.
///
/// Generic over the transport, because the crate's `Transport` is not
/// object-safe (`begin_init<F: Flags>` is a generic method, and `VirtQueue::new`
/// takes `&mut T where T: Transport`), so a `Box<dyn Transport>` is not a device
/// this driver could be built over. Which transport it is remains the machine's
/// fact; everything above it reaches the device through [`GpuDevice`], which is
/// where the type is erased (`NOTES.md` §13.26).
pub struct Gpu<T: Transport> {
    transport: T,
    control: VirtQueue<HalImpl, QUEUE_SIZE>,
    /// The cursor queue, when this device has one. `None` on a device that did
    /// not configure a second queue - which is the whole of "no hardware cursor
    /// here", and why the DRM layer falls back to blending the pointer into the
    /// scanout rather than failing. It is built with `.ok()` in [`Gpu::new`] and
    /// never `?`-ed: a device that refuses its cursor queue is still a device
    /// with a display, and a `?` here would take the screen down with it.
    cursor: Option<VirtQueue<HalImpl, QUEUE_SIZE>>,
    /// The feature word the transport actually agreed to, kept rather than
    /// discarded once the queue is built.
    ///
    /// It is the only honest answer to "can this card run a 3D context": the
    /// device names what it offers, the driver asks for what it has commands
    /// behind, and the *intersection* is the truth about both. Reading
    /// `WANTED_FEATURES` back would be reading the driver's own request, which
    /// on a `virtio-gpu-pci` machine says `VIRGL` and is wrong - the device
    /// agreed to neither 3D bit. `crate::gpu::virtgpu::Card` is built from this.
    agreed: abi::Features,
    /// The request being sent, and the response being read. One pair reused for
    /// every command, because `add_notify_wait_pop` does not return until the
    /// device has consumed the first and written the second - so no two commands
    /// can be in flight over them.
    ///
    /// Both are `Frames` - contiguous DMA blocks - rather than heap `Vec`s, and
    /// that is not tidiness. A virtio descriptor names one *physically
    /// contiguous* extent, and `HalImpl::share` turns a kernel pointer into an
    /// address with `virt_to_phys`, which is only the start of the range: for a
    /// buffer of more than one page that is not contiguous, the address the
    /// device is given is the first page's and the length runs off the end of it.
    /// One page of heap happens to be one frame, which is why this was invisible
    /// while every request fitted in a page - and the backing list below is the
    /// first request that does not.
    send: Frames,
    recv: Frames,
    /// How many pages `send` currently has, so `reserve` does not reallocate a
    /// block that is already big enough.
    send_pages: usize,
    /// How many pages `recv` has, for the same reason - and the reason it is
    /// needed at all is `GET_CAPSET`, whose reply is `24 + capset_max_size`
    /// bytes rather than one fixed struct. The measured GL card's VIRGL2 set is
    /// 1408 bytes, so this block grows where the 2D path never made it.
    recv_pages: usize,
    /// How much of `send` the command in flight occupies. `pixels` hands back the
    /// whole block, and only this much of it is the request.
    flight: usize,
    /// The cursor queue's own request/response scratch, mirroring `send`/`recv`
    /// above. They are *separate* blocks rather than shared with the control
    /// queue because the single-flight invariant that lets one pair be reused is
    /// per-queue: a control command and a cursor command are two rings the device
    /// drains independently, and overwriting one buffer while the other ring still
    /// names it would hand the device torn bytes. Cursor requests are always one
    /// struct (`virtio_gpu_update_cursor`, 48 bytes), so these never grow past a
    /// page and there is no `cursor_reserve` - the same bound `reserve` states,
    /// reached without the growth path.
    cursor_send: Frames,
    cursor_recv: Frames,
    cursor_flight: usize,
    /// The framebuffer's resource memory, while a scanout points at it.
    fb: Option<Frames>,
    /// The rectangle the scanout was last set to, which `flush` needs and
    /// `change_resolution` is the only thing that sets.
    rect: Option<abi::Rect>,
    /// The resource the scanout currently *names*: `RESOURCE_FB` while this
    /// driver's own framebuffer is what the host scans out, and a client's
    /// resource id while a compositor's frame is.
    ///
    /// Kept because pointing the scanout is a command and the two present paths
    /// have to agree on which one is in force: [`Gpu::flush`] puts this driver's
    /// frame back when a client's was the last thing shown, and
    /// [`Gpu::scan_out_client`] re-points only when the name actually changes.
    scanout_resource: u32,
    /// How many cap sets the device's configuration says it has.
    ///
    /// Read once at bring-up, and it is what bounds the walk that builds the
    /// cap set table: the DRM layer asks `GET_CAPSET_INFO` for indices `0..this`
    /// and no further, so a device that answered one index more than it has is
    /// never asked. Zero on a device with no 3D, which is what makes the walk
    /// empty rather than a reason to skip it.
    num_capsets: u32,
}

impl<T: Transport> Gpu<T> {
    /// Bring up the device: negotiate features, configure the control queue,
    /// and set the driver-status to OK.
    ///
    /// The feature word is `abi::WANTED_FEATURES` and not "whatever the device
    /// offers" - a feature accepted and unused is a promise to a device that
    /// will act on it. What comes back is what was *agreed*, which is what the
    /// queue layout depends on: indirect descriptors and the event index are
    /// queue features, and asking `VirtQueue::new` for a layout the device did
    /// not agree to would build a ring it cannot read.
    pub fn new(mut transport: T) -> Result<Self, Error> {
        let agreed = transport.begin_init(abi::WANTED_FEATURES);
        let control = VirtQueue::new(
            &mut transport,
            QUEUE_CONTROL,
            agreed.contains(abi::Features::RING_INDIRECT_DESC),
            agreed.contains(abi::Features::RING_EVENT_IDX),
        )?;
        // The cursor queue, built in the same pre-`finish_init` window because
        // that is the last moment this holds `&mut transport`, and `.ok()` rather
        // than `?` because a device with no second queue is a device with no
        // hardware cursor, not a device with no display. `VirtQueue::new`'s own
        // error is the probe: `InvalidParam` when the device reports no queue at
        // this index (there is no `num_queues`/`queue_is_valid` to ask first), so
        // its `Ok`/`Err` *is* "does this machine have a cursor queue".
        let cursor = VirtQueue::new(
            &mut transport,
            QUEUE_CURSOR,
            agreed.contains(abi::Features::RING_INDIRECT_DESC),
            agreed.contains(abi::Features::RING_EVENT_IDX),
        )
        .ok();
        // Read before `finish_init` for the same reason the queue is built
        // before it: the config space is the transport's, and this is the last
        // moment this function holds it. A device that has no cap sets answers
        // zero here, which is the honest bound for the walk that follows.
        let num_capsets =
            virtio_drivers::config::read_config!(transport, abi::Config, num_capsets)?;
        transport.finish_init();

        Ok(Gpu {
            transport,
            control,
            cursor,
            agreed,
            send: Frames::new(1),
            recv: Frames::new(1),
            send_pages: 1,
            recv_pages: 1,
            flight: 0,
            cursor_send: Frames::new(1),
            cursor_recv: Frames::new(1),
            cursor_flight: 0,
            fb: None,
            rect: None,
            scanout_resource: abi::RESOURCE_FB,
            num_capsets,
        })
    }

    /// How many cap sets the device says it has - see the field's own note.
    pub fn num_capsets(&self) -> u32 {
        self.num_capsets
    }

    /// What the transport agreed to, which is what every 3D answer is gated on.
    ///
    /// The DRM layer asks the render task for this once, at bring-up, and builds
    /// `drm_virtgpu::Card` out of it: `VIRGL` here is `GETPARAM(1)`'s `1`, and
    /// `CONTEXT_INIT` is `GETPARAM(6)`'s. Nothing else in the kernel is allowed
    /// to decide a 3D capability from anything but this word - a driver that
    /// answered from its own request would advertise a context it cannot make.
    pub fn features(&self) -> abi::Features {
        self.agreed
    }

    /// Make sure the send block can hold `bytes`, growing it if it cannot.
    ///
    /// The size is a caller's to steer - it is a resource's page count times a
    /// `MemEntry` - so it carries a bound, which is `Frames`' own: the allocator
    /// refuses rather than overcommitting, and a resource big enough to matter
    /// has already failed to allocate its own frames first.
    fn reserve(&mut self, bytes: usize) {
        let pages = bytes.div_ceil(PAGE_SIZE).max(1);
        if pages > self.send_pages {
            self.send = Frames::new(pages);
            self.send_pages = pages;
        }
    }

    /// Make sure the receive block can hold `bytes`, growing it if it cannot.
    ///
    /// The same physically-contiguous constraint `reserve` has, for the same
    /// reason, and called from the same place: a virtio descriptor names one
    /// extent, and `GET_CAPSET`'s answer is the one reply whose length the
    /// device chooses. The driver sizes it from the `capset_max_size` the device
    /// itself reported, which is a device-supplied number - so the bound on it
    /// is `Frames::new`'s, the allocator refusing rather than overcommitting.
    fn reserve_recv(&mut self, bytes: usize) {
        let pages = bytes.div_ceil(PAGE_SIZE).max(1);
        if pages > self.recv_pages {
            self.recv = Frames::new(pages);
            self.recv_pages = pages;
        }
    }

    /// Send one command on the **control queue** and hand back the first
    /// `rsp_len` bytes of its answer.
    ///
    /// `parts` are concatenated into the send buffer and go out as one
    /// descriptor chain entry, which is how the devices below expect a command:
    /// the header and its fields are one contiguous struct, and so is a struct
    /// with a variable tail (`RESOURCE_ATTACH_BACKING` followed by its
    /// `MemEntry` list, `SUBMIT_3D` followed by its stream).
    fn submit(&mut self, parts: &[&[u8]], rsp_len: usize) -> Result<&[u8], Error> {
        Ok(self.submit_written(parts, rsp_len)?.0)
    }

    /// The same on the control queue, and how many bytes the device says it wrote.
    ///
    /// The length is only interesting for the one reply whose size the *device*
    /// picks - `GET_CAPSET`'s cap set, where "the buffer could have held it" and
    /// "the device sent it" are different claims and the driver clears the
    /// buffer between commands, so the difference is zeroes that read as a
    /// capability word. Everywhere else the answer is a fixed struct the device
    /// writes in full or not at all, and `rsp_len` is already exact.
    fn submit_written(&mut self, parts: &[&[u8]], rsp_len: usize) -> Result<(&[u8], usize), Error> {
        let need: usize = parts.iter().map(|part| part.len()).sum();
        self.reserve(need);
        self.reserve_recv(rsp_len);
        {
            let buf = self.send.pixels();
            let mut at = 0;
            for part in parts {
                buf[at..at + part.len()].copy_from_slice(part);
                at += part.len();
            }
            self.flight = at;
        }
        // A reply longer than a header is one the driver reads fields out of, so
        // the buffer is cleared first: the device may write less than it owes,
        // and a device that did would otherwise have the rest read back as the
        // *previous* command's reply. A header-only reply needs no clear - the
        // header is written in full or not at all - and is not cleared, because
        // `flush` is on the per-frame path.
        if rsp_len > size_of::<abi::CtrlHeader>() {
            self.recv.pixels().fill(0);
        }
        let written = self.control.add_notify_wait_pop(
            &[&self.send.pixels()[..self.flight]],
            &mut [self.recv.pixels()],
            &mut self.transport,
        )? as usize;
        Ok((&self.recv.pixels()[..rsp_len], written))
    }

    /// Send one command on the **cursor queue**, which has no reply beyond its
    /// header and never grows its scratch.
    ///
    /// Both cursor commands answer `RESP_OK_NODATA`, so unlike the control queue
    /// there is no variable-length reply to size a receive buffer for and nothing
    /// past the header to read: the whole request is one fixed struct assembled
    /// from `parts`, and the whole answer is whether the device said OK. It runs
    /// on the cursor queue's own ring and its own `cursor_send` block for the
    /// reason [`Gpu::cursor_send`] states - a control command and a cursor
    /// command are drained independently, so they cannot share one buffer.
    ///
    /// Returns `Error::NotReady` when this device has no cursor queue; every
    /// caller gates on [`Gpu::has_cursor_queue`] first, so that arm is a
    /// belt-and-braces refusal rather than an expected path.
    fn submit_cursor(&mut self, parts: &[&[u8]]) -> Result<(), Error> {
        let queue = self.cursor.as_mut().ok_or(Error::NotReady)?;
        let need: usize = parts.iter().map(|part| part.len()).sum();
        // One page is the whole bound: a cursor command is a single fixed struct
        // (56 bytes for UPDATE, 40 for MOVE), so `need` never reaches a second
        // page and the block never has to grow. Stated rather than assumed, the
        // way `reserve` states its growth - here the absence of growth is the
        // property.
        if need > PAGE_SIZE {
            return Err(Error::IoError);
        }
        {
            let buf = self.cursor_send.pixels();
            let mut at = 0;
            for part in parts {
                buf[at..at + part.len()].copy_from_slice(part);
                at += part.len();
            }
            self.cursor_flight = at;
        }
        // A cursor command is consumed by the device, not answered: measured on
        // QEMU's virtio-gpu, the cursor queue completes the element with a used-ring
        // length of zero and writes no reply header back, unlike the control queue.
        // So *completion* - `add_notify_wait_pop` returning without an error, which
        // means the device took the descriptor off the available ring and returned it
        // used - is the whole of "the host got the pointer". Reading a status word
        // would be reading a buffer the device never filled: the previous attempt did
        // exactly that, saw a zeroed header, and reported a successful host-cursor
        // update as `EIO` on every motion event, so the pointer was drawn by the guest
        // blend path that had been switched off. The writable buffer stays because
        // virtio-drivers' synchronous helper requires one; its contents are ignored.
        let mut recv = [self.cursor_recv.pixels()];
        queue.add_notify_wait_pop(
            &[&self.cursor_send.pixels()[..self.cursor_flight]],
            &mut recv[..],
            &mut self.transport,
        )?;
        Ok(())
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
        // The screen may be showing a *client's* resource ([`Self::scan_out_client`]).
        // This is this driver's own frame, so the scanout goes back to it first:
        // without this a compositor's exit leaves its last frame on the display
        // and the console never comes back, because `TRANSFER_TO_HOST_2D` +
        // `RESOURCE_FLUSH` below only ever touch `RESOURCE_FB`.
        if self.scanout_resource != abi::RESOURCE_FB {
            self.set_scanout(rect, abi::SCANOUT_ID, abi::RESOURCE_FB)?;
        }
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

    /// Put a **client's** frame on the display: point the scanout at the resource
    /// the client drew into, and flush *that*.
    ///
    /// No `TRANSFER_TO_HOST_2D`, and that is the whole difference from
    /// [`Self::flush`]: the transfer is what carries this driver's own framebuffer
    /// out of guest memory, and a client's frame on a card with 3D was never in it.
    /// Mesa draws into the device's own copy of the resource - its texture, host
    /// side - and its export path hands the guest a handle and no readback
    /// (`virgl_drm_winsys_resource_get_handle`), which `probe-fb-pixels` measured
    /// from the other end: a compositor's committed framebuffer at 1280x800 was
    /// 4096000 bytes of **zero** while niri was drawing into it, so copying those
    /// pages onto the scanout is a black screen - which is exactly what every
    /// compositor on this card used to get. Naming the resource to the host instead
    /// is Linux's own shape (`virtio_gpu_cmd_set_scanout` of the framebuffer's
    /// `hw_res_handle`, then `virtio_gpu_cmd_resource_flush`), and `drivers/drm/`'s
    /// `client_scanout` is who decides that this is the frame to show.
    pub fn scan_out_client(&mut self, resource: u32, width: u32, height: u32) -> Result<(), Error> {
        let rect = abi::Rect::whole(width, height);
        if self.scanout_resource != resource {
            self.set_scanout(rect, abi::SCANOUT_ID, resource)?;
        }
        self.ok(&abi::ResourceFlush {
            header: abi::CtrlHeader::new(abi::CMD_RESOURCE_FLUSH),
            rect,
            resource_id: resource,
            padding: 0,
        })
    }

    /// Create the 2D resource `id` at `width` x `height`.
    ///
    /// Separate from [`change_resolution`][Self::change_resolution], which does
    /// the same command for the scanout's own resource: that one is the driver's
    /// private framebuffer, this one is a resource the DRM layer owns and hands
    /// out as a handle. Same fields, different owner, and an `id` the caller
    /// chooses - the device does not allocate resource ids, the driver does, and
    /// it is the DRM layer that has to record which id belongs to which handle.
    pub fn create_2d(&mut self, id: u32, width: u32, height: u32) -> Result<(), Error> {
        self.ok(&abi::ResourceCreate2d {
            header: abi::CtrlHeader::new(abi::CMD_RESOURCE_CREATE_2D),
            resource_id: id,
            format: abi::FORMAT_B8G8R8A8_UNORM,
            width,
            height,
        })
    }

    /// Attach `entries` as `id`'s backing: the guest-physical extents the device
    /// reads and writes the resource through.
    ///
    /// A *list* rather than one extent, because the DRM layer's buffers are
    /// allocated one page at a time (`drivers/drm/`'s `Dumb`) and a resource
    /// built over them is as many extents as it has pages. That is why the
    /// request is not a fixed-size struct: `RESOURCE_ATTACH_BACKING` is followed
    /// on the wire by its `MemEntry` array, and the two go out as one buffer
    /// because a virtio descriptor names one contiguous region - which is what
    /// `send` being a `Frames` is for.
    ///
    /// The sizes are the caller's, and so is the danger in them: the device will
    /// read whatever this names, so a caller that names an extent it does not
    /// own has handed the device someone else's memory. Every caller here passes
    /// frames it allocated.
    pub fn attach_backing(&mut self, id: u32, entries: &[(u64, u32)]) -> Result<(), Error> {
        let head = abi::ResourceAttachBacking {
            header: abi::CtrlHeader::new(abi::CMD_RESOURCE_ATTACH_BACKING),
            resource_id: id,
            nr_entries: entries.len() as u32,
        };
        let entry_bytes = size_of::<abi::MemEntry>();
        let mut request = Vec::with_capacity(
            size_of::<abi::ResourceAttachBacking>() + entries.len() * entry_bytes,
        );
        request.extend_from_slice(abi::as_bytes(&head));
        for &(addr, length) in entries {
            let entry = abi::MemEntry {
                addr,
                length,
                padding: 0,
            };
            request.extend_from_slice(abi::as_bytes(&entry));
        }
        let answer = self.submit(&[&request], size_of::<abi::CtrlHeader>())?;
        let answer = abi::read_struct::<abi::CtrlHeader>(answer).ok_or(Error::IoError)?;
        if answer.is(abi::RESP_OK_NODATA) {
            Ok(())
        } else {
            Err(Error::IoError)
        }
    }

    /// Release the resource `id`. The device forgets it; the caller's frames are
    /// the caller's to free.
    pub fn unref(&mut self, id: u32) -> Result<(), Error> {
        self.ok(&abi::ResourceUnref {
            header: abi::CtrlHeader::new(abi::CMD_RESOURCE_UNREF),
            resource_id: id,
            padding: 0,
        })
    }

    /// Copy `id`'s whole `width` x `height` rectangle from guest memory into the
    /// device.
    ///
    /// The 2D transfer, which is the only direction a card without 3D has (see
    /// [`crate::gpu::virtgpu::TRANSFER_FROM_HOST`]). The offset is zero and the
    /// rectangle is the whole resource because that is what the DRM family's own
    /// transfer names: `struct drm_virtgpu_3d_transfer_to_host` carries a box and
    /// an offset, and the box the DRM layer sends is the resource's full extent.
    pub fn transfer_to_host_2d(&mut self, id: u32, width: u32, height: u32) -> Result<(), Error> {
        self.ok(&abi::TransferToHost2d {
            header: abi::CtrlHeader::new(abi::CMD_TRANSFER_TO_HOST_2D),
            rect: abi::Rect::whole(width, height),
            offset: 0,
            resource_id: id,
            padding: 0,
        })
    }

    /// Create the context `id`, naming the cap set it runs.
    ///
    /// `context_init` folds the DRM layer's `ctx_set_params` into the word the
    /// device reads: the cap set id under [`abi::CONTEXT_INIT_CAPSET_ID_MASK`]
    /// and the ring count above it. This kernel passes the cap set id and leaves
    /// the rings zero, which is the single-ring path Mesa's virgl uses.
    ///
    /// A device that did not offer `CONTEXT_INIT` still takes this command:
    /// Linux sends it either way, with the word zero, and the context the first
    /// command would have made implicitly is the same one. The feature decides
    /// only whether a *caller* could name the cap set and the ring count, which
    /// is why there is one method here rather than two.
    ///
    /// The name is truncated the way Linux's `strscpy` truncates it, to one byte
    /// less than the field so the last byte stays a terminator, and `nlen` is
    /// what was actually copied rather than what was offered. It is bytes rather
    /// than a `&str` because that is what the field is and what the DRM layer
    /// has: the name arrives from a user pointer, and refusing a name that is not
    /// UTF-8 would refuse one Linux accepts.
    pub fn ctx_create(&mut self, id: u32, context_init: u32, name: &[u8]) -> Result<(), Error> {
        let mut command = abi::CtxCreate {
            header: abi::CtrlHeader {
                kind: abi::CMD_CTX_CREATE,
                ctx_id: id,
                ..Default::default()
            },
            nlen: 0,
            context_init,
            debug_name: [0; 64],
        };
        let room = command.debug_name.len() - 1;
        let n = name.len().min(room);
        command.debug_name[..n].copy_from_slice(&name[..n]);
        command.nlen = n as u32;
        self.ok(&command)
    }

    /// Release the context `id`.
    ///
    /// The device forgets the context; what it held is the device's to unmake,
    /// and no resource is released by this - a resource outlives the context
    /// that could name it.
    pub fn ctx_destroy(&mut self, id: u32) -> Result<(), Error> {
        self.ok(&abi::CtxDestroy {
            header: abi::CtrlHeader {
                kind: abi::CMD_CTX_DESTROY,
                ctx_id: id,
                ..Default::default()
            },
        })
    }

    /// Make `resource` reachable to the context `ctx`.
    ///
    /// The step between allocating a 3D resource and submitting a stream that
    /// names it: the stream carries the guest's own resource ids, so the context
    /// has to have been told which of them it may touch.
    pub fn ctx_attach_resource(&mut self, ctx: u32, resource: u32) -> Result<(), Error> {
        self.ctx_resource(abi::CMD_CTX_ATTACH_RESOURCE, ctx, resource)
    }

    /// The other direction: the context stops being allowed to name `resource`,
    /// which is what a caller does before releasing it.
    pub fn ctx_detach_resource(&mut self, ctx: u32, resource: u32) -> Result<(), Error> {
        self.ctx_resource(abi::CMD_CTX_DETACH_RESOURCE, ctx, resource)
    }

    /// Both directions of `CTX_{ATTACH,DETACH}_RESOURCE`, which differ in the
    /// command word and in nothing else.
    fn ctx_resource(&mut self, kind: u32, ctx: u32, resource: u32) -> Result<(), Error> {
        self.ok(&abi::CtxResource {
            header: abi::CtrlHeader {
                kind,
                ctx_id: ctx,
                ..Default::default()
            },
            resource_id: resource,
            padding: 0,
        })
    }

    /// Create a 3D resource, from the caller's own fields.
    ///
    /// The whole struct rather than eleven arguments, because every field is the
    /// caller's: the DRM layer has a `drm_virtgpu_resource_create` in its hand
    /// and the mapping between the two is its business, not this driver's.
    ///
    /// **No context**, and that is Linux's shape rather than an omission:
    /// `virtio_gpu_cmd_resource_create_3d` fills in the resource fields and
    /// leaves the header's `ctx_id` zero, so a 3D resource is born unattached
    /// and the context that may name it is told afterwards by
    /// [`ctx_attach_resource`][Self::ctx_attach_resource]. The header's command
    /// word is the one thing set here, because it is the one thing that decides
    /// whether the device reads these fields as a 3D resource at all.
    pub fn create_3d(&mut self, request: &abi::ResourceCreate3d) -> Result<(), Error> {
        let mut command = *request;
        command.header.kind = abi::CMD_RESOURCE_CREATE_3D;
        command.header.ctx_id = 0;
        self.ok(&command)
    }

    /// Move `request.box3d` of a resource between guest memory and the device,
    /// in context `ctx`.
    ///
    /// One method for both directions because the request is one struct: only
    /// the command word says which way the pixels go, and every field is
    /// identical. `to_host` is a `bool` rather than two methods with similar
    /// names, because the direction is the one thing a caller must not choose
    /// wrongly.
    pub fn transfer_host_3d(
        &mut self,
        to_host: bool,
        ctx: u32,
        request: &abi::TransferHost3d,
    ) -> Result<(), Error> {
        let mut command = *request;
        command.header.kind = if to_host {
            abi::CMD_TRANSFER_TO_HOST_3D
        } else {
            abi::CMD_TRANSFER_FROM_HOST_3D
        };
        command.header.ctx_id = ctx;
        self.ok(&command)
    }

    /// Submit `stream` to context `ctx`, under the fence id `fence`.
    ///
    /// The request is the 32-byte [`abi::CmdSubmit`] followed by the stream and
    /// nothing else - see that struct's own note for why there is no resource
    /// list, and for why the bytes are opaque. Nothing in this function walks a
    /// length that came out of the stream; `size` is the number of bytes copied.
    ///
    /// `ring_idx` selects one of the context's command rings and is zero for the
    /// single-ring path, so a caller cannot get it out of step with the word the
    /// device reads.
    ///
    /// The fence id is the *caller's* to allocate: it is a number a userland
    /// ioctl gets back (`drm_virtgpu_execbuffer.fence`), so the layer that
    /// answers that ioctl is the layer that hands it out, and the mailbox
    /// between here and there stays an errno. `FLAG_FENCE` is what makes the
    /// answer mean something - the device holds the response until the stream's
    /// work is done, so returning from here is the work being done rather than
    /// the bytes being taken. The response *is* the fence.
    pub fn submit_3d(
        &mut self,
        ctx: u32,
        ring_idx: u8,
        fence: u64,
        stream: &[u8],
    ) -> Result<(), Error> {
        let size = u32::try_from(stream.len()).map_err(|_| Error::IoError)?;
        let command = abi::CmdSubmit {
            header: abi::CtrlHeader {
                kind: abi::CMD_SUBMIT_3D,
                flags: abi::FLAG_FENCE,
                fence_id: fence,
                ctx_id: ctx,
                ring_idx,
                ..Default::default()
            },
            size,
            padding: 0,
        };
        let answer = self.submit(
            &[abi::as_bytes(&command), stream],
            size_of::<abi::CtrlHeader>(),
        )?;
        let answer = abi::read_struct::<abi::CtrlHeader>(answer).ok_or(Error::IoError)?;
        if answer.is(abi::RESP_OK_NODATA) {
            Ok(())
        } else {
            Err(Error::IoError)
        }
    }

    /// Ask the device what its cap set at `index` is.
    ///
    /// By index, and the reply is what says which set that is. The DRM layer
    /// walks the indices once at bring-up to build its table; the ioctl path
    /// never calls this, it answers out of that table.
    pub fn get_capset_info(&mut self, index: u32) -> Result<abi::RespCapsetInfo, Error> {
        let request = abi::GetCapsetInfo {
            header: abi::CtrlHeader::new(abi::CMD_GET_CAPSET_INFO),
            capset_index: index,
            padding: 0,
        };
        let bytes = abi::as_bytes(&request);
        let reply = self.submit(&[bytes], size_of::<abi::RespCapsetInfo>())?;
        let answer: abi::RespCapsetInfo = abi::read_struct(reply).ok_or(Error::IoError)?;
        if !answer.header.is(abi::RESP_OK_CAPSET_INFO) {
            return Err(Error::IoError);
        }
        Ok(answer)
    }

    /// Fetch the cap set `id` at `version` into `out`.
    ///
    /// `out` is the caller's buffer and its length is the whole of the bound:
    /// the device sends `24 + capset_max_size` bytes, and the caller sizes `out`
    /// from that same number, which the device reported a round trip earlier.
    /// A device that sent less is refused rather than read, because the driver
    /// clears the receive buffer between commands - the bytes past what it wrote
    /// are zeroes, and a capability word of zeroes is one a caller would act on.
    ///
    /// Stripping the header is [`abi::resp_capset_payload`]'s job, and this is
    /// the one request in the protocol where the device decides a length.
    pub fn get_capset(&mut self, id: u32, version: u32, out: &mut [u8]) -> Result<usize, Error> {
        let request = abi::GetCapset {
            header: abi::CtrlHeader::new(abi::CMD_GET_CAPSET),
            capset_id: id,
            capset_version: version,
        };
        let want = size_of::<abi::CtrlHeader>() + out.len();
        let bytes = abi::as_bytes(&request);
        let (reply, written) = self.submit_written(&[bytes], want)?;
        if written < want {
            return Err(Error::IoError);
        }
        let payload = abi::resp_capset_payload(reply).ok_or(Error::IoError)?;
        if payload.len() < out.len() {
            return Err(Error::IoError);
        }
        out.copy_from_slice(&payload[..out.len()]);
        Ok(out.len())
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
        })?;
        // Recorded here rather than at the call sites, because this is the only
        // command that changes the answer and every way of changing it goes
        // through here - including `change_resolution`'s own pair and the zero
        // that stops a scanout before its resource is released.
        self.scanout_resource = resource_id;
        Ok(())
    }

    /// Whether this device has a cursor queue at all - the single fact the DRM
    /// layer branches on to decide between a host-rendered pointer and blending
    /// one into the scanout. It is exactly whether [`Gpu::cursor`] was built,
    /// which is decided once at bring-up from the device's own answer.
    pub fn has_cursor_queue(&self) -> bool {
        self.cursor.is_some()
    }

    /// Load `resource_id` as the pointer image and place it at (`x`, `y`) with
    /// its hotspot at (`hot_x`, `hot_y`) - the cursor queue's `UPDATE_CURSOR`.
    ///
    /// The caller must have already created, attached backing to and transferred
    /// that resource on the **control** queue and seen those land; this command
    /// reads the resource, it does not fill it. That ordering holds for free when
    /// both halves run through the render task in push order (the control commands
    /// are synchronous round trips that complete before the next job is drained),
    /// which is how `drm.rs` posts them - see `Job::CreateCursor2d`/`TransferCursor`
    /// there.
    ///
    /// A `resource_id` of zero hides the pointer, the same spelling `SET_SCANOUT`
    /// uses for a screen.
    pub fn update_cursor(
        &mut self,
        resource_id: u32,
        x: u32,
        y: u32,
        hot_x: u32,
        hot_y: u32,
    ) -> Result<(), Error> {
        let request = abi::UpdateCursor {
            header: abi::CtrlHeader::new(abi::CMD_UPDATE_CURSOR),
            pos: abi::CursorPos {
                scanout_id: abi::SCANOUT_ID,
                x,
                y,
                padding: 0,
            },
            resource_id,
            hot_x,
            hot_y,
        };
        self.submit_cursor(&[abi::as_bytes(&request)])?;
        Ok(())
    }

    /// Move the already-loaded pointer image to (`x`, `y`) - the cursor queue's
    /// `MOVE_CURSOR`, the cheap half that touches no resource.
    ///
    /// This is what a pointer-motion event costs once an image is loaded, and why
    /// the DRM layer keeps "is an image bound" separate from "where is it": a move
    /// sends neither a resource id nor a hotspot, because the image and its hotspot
    /// are unchanged.
    ///
    /// **No ring index**, named as a limitation rather than an omission: on a
    /// device that negotiated `VIRTIO_GPU_F_CURSOR_RING_IDX`, `MOVE_CURSOR` carries
    /// `FLAG_INFO_RING_IDX` and a `ring_idx` selecting which cursor ring. This
    /// kernel does not ask for that feature (`abi::WANTED_FEATURES`), so the field
    /// stays zero - correct for QEMU's single cursor ring today, and the thing to
    /// revisit if multi-ring cursors are ever negotiated.
    pub fn move_cursor(&mut self, x: u32, y: u32) -> Result<(), Error> {
        let request = abi::MoveCursor {
            header: abi::CtrlHeader::new(abi::CMD_MOVE_CURSOR),
            pos: abi::CursorPos {
                scanout_id: abi::SCANOUT_ID,
                x,
                y,
                padding: 0,
            },
        };
        self.submit_cursor(&[abi::as_bytes(&request)])?;
        Ok(())
    }
}

/// The erase point: every method is the inherent one above, so the render task
/// names the *contract* and never a transport. `T: Send` is what the task needs
/// to hold the device across a spawn, and it is the whole of what the type
/// parameter adds here.
impl<T: Transport + Send> GpuDevice for Gpu<T> {
    fn num_capsets(&self) -> u32 {
        self.num_capsets()
    }

    fn features(&self) -> abi::Features {
        self.features()
    }

    fn resolution(&mut self) -> Result<(u32, u32), Error> {
        self.resolution()
    }

    fn change_resolution(&mut self, width: u32, height: u32) -> Result<&mut [u8], Error> {
        self.change_resolution(width, height)
    }

    fn flush(&mut self) -> Result<(), Error> {
        self.flush()
    }

    fn scan_out_client(&mut self, resource: u32, width: u32, height: u32) -> Result<(), Error> {
        self.scan_out_client(resource, width, height)
    }

    fn attach_backing(&mut self, id: u32, entries: &[(u64, u32)]) -> Result<(), Error> {
        self.attach_backing(id, entries)
    }

    fn unref(&mut self, id: u32) -> Result<(), Error> {
        self.unref(id)
    }

    fn create_2d(&mut self, id: u32, width: u32, height: u32) -> Result<(), Error> {
        self.create_2d(id, width, height)
    }

    fn create_3d(&mut self, request: &abi::ResourceCreate3d) -> Result<(), Error> {
        self.create_3d(request)
    }

    fn transfer_to_host_2d(&mut self, id: u32, width: u32, height: u32) -> Result<(), Error> {
        self.transfer_to_host_2d(id, width, height)
    }

    fn ctx_create(&mut self, id: u32, context_init: u32, name: &[u8]) -> Result<(), Error> {
        self.ctx_create(id, context_init, name)
    }

    fn ctx_attach_resource(&mut self, ctx: u32, resource: u32) -> Result<(), Error> {
        self.ctx_attach_resource(ctx, resource)
    }

    fn ctx_detach_resource(&mut self, ctx: u32, resource: u32) -> Result<(), Error> {
        self.ctx_detach_resource(ctx, resource)
    }

    fn transfer_host_3d(
        &mut self,
        to_host: bool,
        ctx: u32,
        request: &abi::TransferHost3d,
    ) -> Result<(), Error> {
        self.transfer_host_3d(to_host, ctx, request)
    }

    fn submit_3d(
        &mut self,
        ctx: u32,
        ring_idx: u8,
        fence: u64,
        stream: &[u8],
    ) -> Result<(), Error> {
        self.submit_3d(ctx, ring_idx, fence, stream)
    }

    fn get_capset_info(&mut self, index: u32) -> Result<abi::RespCapsetInfo, Error> {
        self.get_capset_info(index)
    }

    fn get_capset(&mut self, id: u32, version: u32, out: &mut [u8]) -> Result<usize, Error> {
        self.get_capset(id, version, out)
    }

    fn has_cursor_queue(&self) -> bool {
        self.has_cursor_queue()
    }

    fn update_cursor(
        &mut self,
        resource_id: u32,
        x: u32,
        y: u32,
        hot_x: u32,
        hot_y: u32,
    ) -> Result<(), Error> {
        self.update_cursor(resource_id, x, y, hot_x, hot_y)
    }

    fn move_cursor(&mut self, x: u32, y: u32) -> Result<(), Error> {
        self.move_cursor(x, y)
    }
}
