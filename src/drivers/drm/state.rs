/// The device resource a buffer backs, when it is a `DRM_VIRTGPU_*` one.
///
/// A dumb buffer that `CREATE_DUMB` made has none: its pixels reach the screen
/// through this card's own scanout copy (`copy_to_scanout`), which reads the
/// frames directly, so no device resource is involved. One that
/// `DRM_VIRTGPU_RESOURCE_CREATE` made does, because that is what the request
/// *is* - and the family's other three requests name it.
pub(crate) struct Resource {
    /// The id the device knows the resource by. This card chooses it (the device
    /// does not allocate resource ids - Linux's driver takes them from an ida,
    /// and so does this), and for a buffer a client asked for it *is* the GEM
    /// handle: one number, which is what the guest's command stream spells a
    /// resource with. A driver-owned resource - the scanout's framebuffer, the
    /// host cursor - has no handle to borrow and takes its constant from the
    /// driver's own high range (`virtgpu_abi::RESOURCE_FB`/`_CURSOR`); see
    /// [`virtgpu_resource_create`] for why the two must not overlap.
    pub(crate) id: u32,
    /// The size the *caller* asked for, or a page when it asked for zero - and
    /// the buffer's own `pitch * height` when the resource was made for a dumb
    /// buffer, which is the size Linux's `virtio_gpu_mode_dumb_create` gives the
    /// object it creates.
    ///
    /// Not `width * height * 4`, and that is Linux's shape rather than a
    /// simplification: `virtio_gpu_resource_create_ioctl` takes `params.size`
    /// from the request and substitutes `PAGE_SIZE` when it is zero, which is
    /// why the probe instrument `user/staging/virtgpu-probe/` (now deleted; its
    /// table is in `docs/NOTES.md` §7.4) measured a 64x64
    /// resource answering `size = 4096`. It is the guest backing's size, and for
    /// a 3D resource it is a placeholder while the device owns the real storage.
    pub(crate) size: u64,
    /// Whether this is a 3D (virgl) resource rather than a 2D one.
    ///
    /// It is what decides which transfer a `TRANSFER_TO_HOST` becomes: Linux
    /// routes a virgl resource through the 3D command and a 2D one through
    /// `TRANSFER_TO_HOST_2D`, because a 3D resource belongs to a context and the
    /// 2D command has none to address it in.
    pub(crate) virgl: bool,
    /// Whether the **device** holds this resource's pixels rather than the guest
    /// pages behind it.
    ///
    /// True for a resource a client made through this card's 3D-ioctl family
    /// (`VIRTGPU_RESOURCE_CREATE`) on a card that negotiated 3D, and that is the
    /// whole of the predicate: the driver behind that call is Mesa's virgl winsys,
    /// which draws into the device's own copy of the resource (its texture) and
    /// whose export path hands the guest a handle and no readback
    /// (`virgl_drm_winsys_resource_get_handle`). `.workbuddy/probe-fb-pixels`
    /// measured what that means from the guest's side: the framebuffer niri
    /// committed, 1280x800, was 4096000 bytes of **zero** while niri was drawing
    /// into it - so the scanout copy carries black, and a frame like that has to
    /// reach the display by the device scanning the resource out instead
    /// ([`client_scanout`], `virtio::gpu_ctrl`'s `scan_out_client`).
    ///
    /// False for everything else, deliberately: a dumb buffer's pixels are written
    /// by the guest (a software rasteriser maps it), an imported dma-buf's are
    /// whoever wrote them on the other side, and both reach the display through
    /// that same copy. Which is why this is a fact about the resource's *origin*
    /// and not about its target, its format or its size.
    pub(crate) host_pixels: bool,
}

/// One dumb buffer: its geometry and the physical pages behind it.
pub(crate) struct Dumb {
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) pitch: u32,
    pub(crate) bpp: u32,
    /// One physical frame per page of the buffer, grown on the first write or
    /// map that touches it (the inode holds a reference on each; every process
    /// that maps it takes its own via `clone_frame_ref`). A `DRM_VIRTGPU_*`
    /// resource has all of its pages made up front instead, because the device
    /// was handed their addresses as its backing and cannot be handed a page
    /// that does not exist yet.
    pub(crate) frames: Vec<usize>,
    // The card holds one reference per frame until DESTROY_DUMB; frames are
    // released then, and only actually recycled once no live mapping still
    // references them (each mapping took its own via clone_frame_ref).
    /// Set when this buffer is a `DRM_VIRTGPU_*` resource rather than a plain
    /// dumb buffer. It is what `RESOURCE_INFO` reports (and what
    /// `TRANSFER_TO_HOST` needs), and it is also what the handle's release has
    /// to tell the device about - see [`destroy_handle`].
    pub(crate) resource: Option<Resource>,
    /// How many framebuffers name this buffer - Linux's `drm_framebuffer_init`
    /// taking a reference on the GEM object, which is what lets a client close
    /// the handle it made and go on flipping the framebuffer it made from it.
    ///
    /// That idiom is not exotic: it is how Mesa's GBM allocator and Aquamarine
    /// hand a scanout buffer to the card, and without the count the buffer left
    /// the table at `GEM_CLOSE` while a framebuffer still named it - so every
    /// commit of that framebuffer was refused (the fit test asks whether the
    /// buffer is there) and the compositor came up with no picture. Measured as
    /// one `handle 11 is released while a framebuffer still names it` followed by
    /// four refusals whose *only* failing condition was `handle_live=false`, on a
    /// framebuffer of exactly the mode's size (`file` mode_blob=Some(1)
    /// test_only=true fb=(11, 1280, 800, 5120, 32, XR24)`).
    pub(crate) fb_refs: u32,
    /// One entry per open file description that still holds a *name* for this
    /// buffer - the fact a single `handle_closed` flag got wrong. Linux's handle
    /// table is **per `drm_file`**: two files that imported the same dma-buf, or
    /// a second open of the same card, each own their name, and one releasing its
    /// own must not take the number out from under the other. Measured as
    /// `name 39 released (ofd=...3912848)` followed by every `EXECBUFFER ... handle
    /// 39` from a *different* file (`ofd=...3914384`) being refused `ENOENT` - which
    /// Mesa turns into `got error from kernel - expect bad rendering 2` and a dead
    /// winsys. The buffer is freed when the last name and the last framebuffer
    /// both go ([`drop_fb_hold`], [`destroy_handle`]).
    pub(crate) holders: Vec<u64>,
}

use super::abi::{BLEND_PREMULT, CRTC_ID, PAGES_PER_BUFFER};
use super::sync::SyncObjRec;

use crate::mm::{PAGE_SIZE, free_frames, phys_to_virt};
use alloc::collections::VecDeque;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::cell::RefCell;
use critical_section::Mutex;

impl Dumb {
    pub(crate) fn new(width: u32, height: u32, bpp: u32) -> Dumb {
        let pitch = (width * bpp / 8).next_multiple_of(64);
        Dumb {
            width,
            height,
            pitch,
            bpp,
            frames: Vec::new(),
            resource: None,
            fb_refs: 0,
            holders: Vec::new(),
        }
    }

    /// A buffer that is also a device resource, with every page already made.
    ///
    /// The frames are passed in rather than grown here because they were
    /// allocated *before* the device was told about them: the command that
    /// attaches a backing names their addresses, so a frame allocated after it
    /// would be a page the device was never told to read.
    ///
    /// `virgl` says which of the family's two create paths made it, and it is
    /// carried here rather than re-derived at each use because the request that
    /// would answer it is gone by then: a transfer has a handle, not a target.
    pub(crate) fn resource(
        width: u32,
        height: u32,
        frames: Vec<usize>,
        resource: Resource,
    ) -> Dumb {
        let mut buf = Dumb::new(width, height, 32);
        buf.frames = frames;
        buf.resource = Some(resource);
        buf
    }

    /// Add one file's name to this buffer. The create or import that hands a
    /// handle out is what calls it, so the number a client was given is a number
    /// *that client* holds.
    pub(crate) fn named_by(mut self, ofd: u64) -> Dumb {
        self.holders.push(ofd);
        self
    }

    pub(crate) fn size(&self) -> u64 {
        self.pitch as u64 * self.height as u64
    }

    /// Grow to cover page `idx`, returning its frame (0 if the cap or memory
    /// is hit). Allocates outside any borrow of the global card state? no -
    /// callers hold it; allocation itself takes no card lock.
    pub(crate) fn ensure_page(&mut self, idx: usize) -> usize {
        if idx >= PAGES_PER_BUFFER {
            return 0;
        }
        while self.frames.len() <= idx {
            match crate::mm::alloc_frames(0) {
                Some(p) => {
                    unsafe {
                        core::ptr::write_bytes(phys_to_virt(p) as *mut u8, 0, PAGE_SIZE);
                    }
                    self.frames.push(p);
                }
                None => return 0,
            }
        }
        self.frames[idx]
    }
    pub(crate) fn release(&mut self) {
        for p in self.frames.drain(..) {
            if p != 0 {
                free_frames(p, 0);
            }
        }
    }
    /// Copy `len` bytes of the buffer at byte `src_off` into `dst`. The pages
    /// are read through the direct map; a page the buffer never grew reads as
    /// zeros, which is what an untouched dumb page shows on Linux.
    pub(crate) fn copy_span(&self, src_off: usize, dst: *mut u8, len: usize) {
        let mut done = 0;
        while done < len {
            let pos = src_off + done;
            let off = pos % PAGE_SIZE;
            let chunk = (PAGE_SIZE - off).min(len - done);
            match self.frames.get(pos / PAGE_SIZE) {
                Some(&frame) if frame != 0 => unsafe {
                    core::ptr::copy_nonoverlapping(
                        (phys_to_virt(frame) + off) as *const u8,
                        dst.add(done),
                        chunk,
                    );
                },
                _ => unsafe {
                    core::ptr::write_bytes(dst.add(done), 0, chunk);
                },
            }
            done += chunk;
        }
    }
}

/// A framebuffer: `ADDFB`/`ADDFB2` bound a dumb buffer to an id a CRTC can
/// display. The pitch is the caller's (what `GETFB` reports back); the
/// presentation copy reads the dumb buffer's own geometry.
pub(crate) struct Framebuffer {
    pub(crate) id: u32,
    pub(crate) handle: u32,
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) pitch: u32,
    pub(crate) bpp: u32,
    pub(crate) depth: u32,
    /// The fourcc this framebuffer was made from: what `ADDFB2` was given, or
    /// what Linux's `drm_mode_legacy_fb_format` synthesizes from `bpp`/`depth`
    /// for the legacy `ADDFB`, whose struct has no format field at all. `0` when
    /// neither names one, which `GETFB2` reports as such rather than inventing a
    /// layout the caller did not ask for. [`legacy_fb_format`] is that mapping.
    pub(crate) format: u32,
    /// The format modifier, `0` unless `ADDFB2` carried
    /// `DRM_MODE_FB_MODIFIERS`. Kept rather than discarded so that `GETFB2` can
    /// report how the framebuffer was actually made; the plane is linear either
    /// way, which is the only modifier `ADDFB2` accepts.
    pub(crate) modifier: u64,
    /// The open file description this framebuffer was made through, which is
    /// what Linux's `struct drm_file` is: a framebuffer belongs to the
    /// descriptor that created it and is released when that description's last
    /// descriptor closes (`drm_file_free` → `drm_fb_release`). Without the
    /// owner this card's framebuffers outlive every session that made them,
    /// and `GETRESOURCES` then lists ids no client can account for.
    pub(crate) owner: u64,
}

/// The identity record of one dma-buf this card has been involved with: the
/// buffer's ino, the GEM handle the relationship runs through, and - for a
/// buffer this card *exported* - the object itself, so a second export of
/// the same handle answers the same dma-buf rather than a second alias. A
/// record dies with its handle (`GEM_CLOSE`/`DESTROY_DUMB` prune it), which
/// is what bounds the table by the handle table; the import half keeps only
/// the (ino, handle) pair, because the descriptor's own lifetime owns the
/// buffer there.
pub(crate) struct PrimeRecord {
    pub(crate) ino: u64,
    pub(crate) handle: u32,
    pub(crate) buf: Option<Arc<crate::vfs::dmabuf::DmaBuf>>,
}

/// The card's mutable state, all under one lock. There is a single device, so
/// a process opening `card0` shares it with every other opener - which is
/// fine, because exactly one compositor owns the seat.
pub(crate) struct DrmState {
    pub(crate) dumb: Vec<Option<Dumb>>,
    pub(crate) fbs: Vec<Option<Framebuffer>>,
    /// The framebuffer currently driven to the scanout by the CRTC.
    pub(crate) current_fb: u32,
    /// The connector's `CRTC_ID` property value and its `DPMS` mode, and the
    /// CRTC's `ACTIVE` - recorded by `SETPROPERTY`, reported by
    /// `GETCONNECTOR`/`OBJ_GETPROPERTIES`. Presentation itself follows
    /// `current_fb`, not these words: they are the state a compositor reads
    /// back, not a second display path.
    pub(crate) conn_crtc: u32,
    pub(crate) dpms: u32,
    pub(crate) crtc_active: bool,
    /// The mode an atomic commit last bound to the CRTC, as the blob id
    /// `MODE_ID` carries. Reported back by `OBJ_GETPROPERTIES`, which is
    /// where a compositor reads the state it inherits.
    pub(crate) crtc_mode_id: u32,
    /// The plane's `CRTC_ID`, and the last committed destination rectangle
    /// (`CRTC_X/Y/W/H`) and source rectangle (`SRC_*`, 16.16). The card has
    /// one plane showing one whole buffer, so the rectangles do not steer
    /// presentation - they are the state the caller reads back, exactly as
    /// the connector's `CRTC_ID` and the CRTC's `ACTIVE` are.
    pub(crate) plane_crtc: u32,
    pub(crate) plane_dst: (u32, u32, u32, u32),
    pub(crate) plane_src: (u32, u32, u32, u32),
    /// The plane's `pixel blend mode`, as written. It steers nothing - the
    /// scanout is the compositor's finished frame, so the pixels are already
    /// pre-multiplied - but it is state a caller may write and read back, which
    /// is what every other field here is and what this one was not.
    pub(crate) plane_blend: u64,
    /// The cursor plane: the framebuffer bound to it, the CRTC it serves, where
    /// its hotspot sits on the scanout, and how far into the image that hotspot
    /// is (what `MODE_CURSOR2`'s `hot_x`/`hot_y` carry, and 0,0 for the older
    /// struct, whose arrow-tip convention it defaults to).
    ///
    /// These are *not* the primary plane's kind of bookkeeping - they are what
    /// the composition reads on every flush, which is how a pointer moves
    /// without the compositor redrawing anything.
    pub(crate) cursor_fb: u32,
    pub(crate) cursor_crtc: u32,
    pub(crate) cursor_pos: (i32, i32),
    pub(crate) cursor_hotspot: (i32, i32),
    /// The device resource the host cursor is currently drawn from, and the
    /// framebuffer it was made from - `Some((res_id, fb_id))` once an image has
    /// been transferred to the host and shown there. It exists so a pointer move
    /// sends only `MOVE_CURSOR` rather than re-transferring the image every frame,
    /// and so a *rebind* (a different `fb_id`) is recognized as needing the full
    /// create+transfer+update again. `None` means no host cursor is showing; the
    /// the id is this driver's own constant in the high range beside the
    /// scanout's - fixed rather than counted because the discipline below is
    /// destroy-before-create, so the device holds at most one cursor resource
    /// and nothing else can be aliased. See `show_cursor`.
    pub(crate) cursor_resource: Option<(u32, u32)>,
    /// Property blobs, by id (1-based, as every id space here is). A mode
    /// arrives as one, and so does a damage-rectangle list.
    pub(crate) blobs: Vec<Option<Vec<u8>>>,
    /// The two blobs the *driver* publishes: the connector's `EDID` bytes and
    /// the plane's `IN_FORMATS`.
    ///
    /// They are fields rather than entries in `blobs` because their ids are
    /// handed to clients that must not be able to take them away: destroying a
    /// blob id that a property still holds is a no-op on Linux (the property
    /// owns a reference), and two id spaces is how that is true here without a
    /// refcount. A driver blob's id is [`DRIVER_BLOB_EDID`] or
    /// [`DRIVER_BLOB_IN_FORMATS`], both far above anything the client table
    /// hands out.
    pub(crate) edid_bytes: Option<Vec<u8>>,
    /// The size the EDID was built for, so a host resize rebuilds it: its
    /// detailed timing *is* the mode being advertised, and the two move
    /// together.
    pub(crate) edid_dims: (u32, u32),
    pub(crate) in_formats_bytes: Option<Vec<u8>>,
    /// The cursor plane's own `IN_FORMATS`, from [`CURSOR_FORMATS`] and this
    /// card's one linear modifier. Separate from the primary plane's for the
    /// reason a real card keeps them separate: a format list is a fact about a
    /// *plane*, and the two planes do not scan the same set.
    pub(crate) cursor_in_formats_bytes: Option<Vec<u8>>,
    /// The PRIME identity table (see [`PrimeRecord`]).
    pub(crate) prime: Vec<PrimeRecord>,
    /// Sequence number for flip/vblank events, echoed back to the reader.
    pub(crate) seq: u32,
    /// Queued `drmEvent` objects awaiting `read` on the card fd: (type,
    /// payload-u64-words). Only flip-completion is produced.
    pub(crate) events: VecDeque<drm_event::Vblank>,
    /// The sync objects clients have made, each with the open file description
    /// that made it - which is what makes a handle mean something to exactly one
    /// client. A record dies with its descriptor (`DESTROY`, or the last
    /// reference to the description closing), and the object itself lives on
    /// while a `SYNCOBJ_HANDLE_TO_FD` descriptor names it, which is the sharing
    /// the descriptor exists for.
    pub(crate) syncobjs: Vec<Option<SyncObjRec>>,
    /// The open file descriptions that have set `DRM_CLIENT_CAP_ATOMIC`.
    ///
    /// The cap is the *file's*, not the card's: Linux records it on `struct
    /// drm_file`, and the measurement behind [`ATOMIC_PROPS`] settled that this
    /// is the scope a client observes rather than an assumption - on two
    /// descriptions of one card, one setting the cap changed what only *that*
    /// one was shown, on the connector and on the CRTC alike. The key is
    /// therefore the same `ofd` the framebuffers already carry as `owner`, which
    /// is this kernel's `struct drm_file`.
    ///
    /// It is the only cap kept. The others this card accepts - `STEREO_3D`,
    /// `UNIVERSAL_PLANES`, `ASPECT_RATIO`, `WRITEBACK_CONNECTORS` - are promises
    /// about which fields of a reply the *client* will read, and this card
    /// answers every one of them the same either way; storing them would be
    /// state with no reader. This one has a reader: the list doors.
    pub(crate) atomic_ofds: Vec<u64>,
    /// The open file descriptions that have set `DRM_CLIENT_CAP_CURSOR_PLANE_HOTSPOT`.
    /// The cursor plane's [`HOTSPOT_PROPS`] are enumerated only for these, exactly
    /// as [`Self::atomic_ofds`] gates the atomic properties - the cap is per client,
    /// and a description that never asked must not be shown a property it will not
    /// set. Cleared on close and on losing master, so a recycled `ofd` cannot inherit
    /// another client's view (§7.4's leak lesson).
    pub(crate) hotspot_ofds: Vec<u64>,
}

pub(crate) mod drm_event {
    /// `struct drm_event_vblank`: the 8-byte `drm_event` header (type and
    /// length), the 64-bit `user_data` the commit passed, the two 32-bit
    /// halves of the timestamp, the flip sequence, and the CRTC the flip
    /// happened on - the field `DRM_CAP_CRTC_IN_VBLANK_EVENT` promises.
    ///
    /// The field *names* matter as much as the sizes. libdrm hands
    /// `user_data` to the flip handler as its `data` argument, and weston's
    /// atomic handler takes its `struct drm_device *` from there: a card that
    /// put the sequence number in that slot would hand the compositor a
    /// pointer built out of a frame counter.
    #[repr(C)]
    #[derive(Clone, Copy)]
    pub struct Vblank {
        pub type_: u32,
        pub length: u32,
        pub user_data: u64,
        pub time_sec: u32,
        pub time_usec: u32,
        pub sequence: u32,
        pub crtc: u32,
    }
}

pub(crate) static STATE: Mutex<RefCell<DrmState>> = Mutex::new(RefCell::new(DrmState {
    dumb: Vec::new(),
    fbs: Vec::new(),
    current_fb: 0,
    conn_crtc: CRTC_ID,
    dpms: 0,
    crtc_active: true,
    crtc_mode_id: 0,
    plane_crtc: CRTC_ID,
    plane_dst: (0, 0, 0, 0),
    plane_src: (0, 0, 0, 0),
    plane_blend: BLEND_PREMULT,
    cursor_fb: 0,
    cursor_crtc: CRTC_ID,
    cursor_pos: (0, 0),
    cursor_hotspot: (0, 0),
    cursor_resource: None,
    blobs: Vec::new(),
    edid_bytes: None,
    edid_dims: (0, 0),
    in_formats_bytes: None,
    cursor_in_formats_bytes: None,
    prime: Vec::new(),
    seq: 0,
    events: VecDeque::new(),
    syncobjs: Vec::new(),
    atomic_ofds: Vec::new(),
    hotspot_ofds: Vec::new(),
}));
