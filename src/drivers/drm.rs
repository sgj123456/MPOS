//! DRM/KMS: a `/dev/dri/card0` a compositor can drive.
//!
//! weston's modern backend - and any DRM/KMS compositor - opens
//! `/dev/dri/card0`, enumerates a single CRTC/encoder/connector/plane, makes
//! CPU-writable *dumb buffers* (GEM), attaches them with `ADDFB`, and shows
//! them with `SETCRTC`/`PAGE_FLIP`, waiting on vblank events read back off
//! the card fd. There is no 3D here; GBM and its GL clients reach these same
//! dumb buffers from the other side, as PRIME dma-bufs (`drm.rs` exports a
//! handle to a descriptor, Mesa maps the descriptor, renders into it with
//! llvmpipe, and the compositor imports the fb back to scan out).
//!
//! The whole card is virtual and fixed: exactly one of each KMS object, the
//! dumb buffers are page-backed like a memfd (so `MAP_DUMB` + `mmap` aliases
//! the real frames, with the same per-mapping reference discipline so a
//! unmapping process cannot free a buffer another still owns). A page flip
//! is applied by *copying the framebuffer into the GPU's scanout buffer* -
//! row by row, because the dumb buffer's pitch is 64-byte aligned while the
//! scanout's is `width * 4` - and then asking the render task to flush, so
//! what the compositor drew actually reaches the host window. The ioctl
//! numbers and struct layouts follow the Linux UAPI exactly (`drm.h`,
//! `drm_mode.h`), which is what libdrm compiles against.
//!
//! `/dev/dri/renderD128` is the same card state under the render node's
//! name: GBM opens the render node to allocate (a client is not the master,
//! so libdrm's convention is to keep allocation off `card0`), and a teaching
//! kernel has no permission boundary to draw between the two. Both nodes
//! ioctl one shared `DrmState`.

use alloc::collections::VecDeque;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::cell::RefCell;

use critical_section::Mutex;

use crate::mm::{PAGE_SIZE, clone_frame_ref, free_frames, phys_to_virt};
use crate::vfs::{Inode, Metadata, NodeKind, Ready, VfsResult};

/// Fixed KMS object ids - one of each, never changing. libdrm only cares that
/// `GETRESOURCES` and the `GET*` ioctls agree on them.
const CRTC_ID: u32 = 34;
const ENCODER_ID: u32 = 33;
const CONNECTOR_ID: u32 = 35;
const PLANE_ID: u32 = 36;
/// Fixed property ids, allocated the same way: `OBJ_GETPROPERTIES` and
/// `GETPROPERTY` have to agree on them, and nothing else cares what they are.
/// `CRTC_ID` and `DPMS` live on the connector, `ACTIVE` on the CRTC, and
/// `type` on the plane - the set weston's DRM backend looks every one of
/// them up by *name* for.
const PROP_CRTC_ID: u32 = 70;
const PROP_DPMS: u32 = 71;
const PROP_ACTIVE: u32 = 72;
const PROP_TYPE: u32 = 73;
/// The properties an *atomic* commit is made of - the ones a compositor sets
/// to describe a frame in one request rather than through `SETCRTC` and
/// `PAGE_FLIP`. They are what weston 16's DRM backend speaks and nothing
/// else: its legacy path is gone, so a card that cannot take an atomic commit
/// is a card it refuses to initialize.
///
/// The CRTC carries `MODE_ID` (a blob holding a `drmModeModeInfo`) beside
/// `ACTIVE`; the plane carries the buffer and the two rectangles
/// (`FB_ID`/`CRTC_ID`, `SRC_*` in 16.16 fixed point, `CRTC_*` in pixels) and
/// the blend mode its pixels are composited with; the connector's `CRTC_ID`
/// is the binding that says which CRTC drives it.
const PROP_MODE_ID: u32 = 74;
const PROP_PLANE_FB_ID: u32 = 75;
const PROP_PLANE_CRTC_ID: u32 = 76;
const PROP_SRC_X: u32 = 77;
const PROP_SRC_Y: u32 = 78;
const PROP_SRC_W: u32 = 79;
const PROP_SRC_H: u32 = 80;
const PROP_CRTC_X: u32 = 81;
const PROP_CRTC_Y: u32 = 82;
const PROP_CRTC_W: u32 = 83;
const PROP_CRTC_H: u32 = 84;
const PROP_BLEND: u32 = 85;
/// The fence a client's buffer arrives behind. Nothing on this machine has an
/// asynchronous engine to wait for - the renderer is llvmpipe writing through
/// the CPU, and the buffers are its own memory - so the value is accepted and
/// recorded nowhere. Accepting it matters all the same: weston adds this
/// property to a commit whenever a client handed it a buffer with an acquire
/// fence, and a card without the property fails the whole commit rather than
/// the one entry.
const PROP_IN_FENCE_FD: u32 = 86;
/// `DRM_MODE_CONNECTOR_VIRTUAL` / `DRM_MODE_ENCODER_VIRTUAL`.
const CONNECTOR_TYPE: u32 = 15;
const ENCODER_TYPE: u32 = 5;
/// `DRM_MODE_CONNECTED`.
const CONNECTED: u32 = 1;
/// `DRM_EVENT_FLIP_COMPLETE`: the type a page-flip completion carries.
///
/// The number is not this card's to choose. libdrm reads the event off the
/// card's fd and dispatches on this field - `drmHandleEvent` switches on it
/// and calls the flip handler only for the value the UAPI names, which is
/// `0x02`. An event of any other type is an event nothing recognises: weston
/// waited for a flip that had already happened, never repainted its output,
/// and left the screen as it was.
///
/// The high bit some DRM numbers carry is `DRM_IOCTL_*`'s *direction*, which
/// says nothing about an event's type.
const EVENT_FLIP_COMPLETE: u32 = 0x02;

/// The ioctl `nr` byte (bits 8..15) for the KMS + core requests we serve.
const NR_GET_CAP: u32 = 0x0c;
const NR_SET_CLIENT_CAP: u32 = 0x0d;
const NR_GETRESOURCES: u32 = 0xa0;
const NR_GETCRTC: u32 = 0xa1;
const NR_SETCRTC: u32 = 0xa2;
const NR_GETENCODER: u32 = 0xa6;
const NR_GETCONNECTOR: u32 = 0xa7;
const NR_GETPROPERTY: u32 = 0xaa;
const NR_SETPROPERTY: u32 = 0xab;
const NR_GETPROPBLOB: u32 = 0xac;
const NR_GETFB: u32 = 0xad;
const NR_ADDFB: u32 = 0xae;
const NR_RMFB: u32 = 0xaf;
const NR_PAGE_FLIP: u32 = 0xb0;
const NR_CREATE_DUMB: u32 = 0xb2;
const NR_MAP_DUMB: u32 = 0xb3;
const NR_DESTROY_DUMB: u32 = 0xb4;
const NR_GETPLANERESOURCES: u32 = 0xb5;
const NR_GETPLANE: u32 = 0xb6;
const NR_MODE_ATOMIC: u32 = 0xbc;
const NR_CREATE_BLOB: u32 = 0xbd;
const NR_DESTROY_BLOB: u32 = 0xbe;
/// `DRM_IOCTL_MODE_ADDFB2` / `..._OBJ_GETPROPERTIES` / `..._OBJ_SETPROPERTY`:
/// the fourcc form of ADDFB and the object-typed property interface.
const NR_ADDFB2: u32 = 0xb8;
const NR_OBJ_GETPROPERTIES: u32 = 0xb9;
const NR_OBJ_SETPROPERTY: u32 = 0xba;
/// The core (non-KMS) requests: version, auth, and the PRIME pair that hands
/// a GEM handle to a dma-buf descriptor and back. `SET_VERSION` and the
/// magic/auth pair are what libdrm's setup probes; PRIME is what GBM builds
/// its buffers through.
const NR_VERSION: u32 = 0x00;
const NR_GET_MAGIC: u32 = 0x02;
const NR_SET_VERSION: u32 = 0x07;
const NR_GEM_CLOSE: u32 = 0x09;
const NR_AUTH_MAGIC: u32 = 0x11;
const NR_PRIME_HANDLE_TO_FD: u32 = 0x2d;
const NR_PRIME_FD_TO_HANDLE: u32 = 0x2e;
/// Session-ownership requests - `DRM_IOCTL_AUTH` / `_SET_MASTER` /
/// `_DROP_MASTER`. They are `_IO` (no argument), so `arg` is null for them
/// and they must be answered before the read-buffer check in `drm_ioctl`. A
/// seat manager reaches them through libdrm when it hands the card to a
/// compositor. mpos has one card and whoever opened it is its master, so
/// all three succeed by doing nothing - refusing `SET_MASTER` is what stops
/// weston from ever getting a master fd it can set a mode on.
const NR_AUTH: u32 = 0x1c;
const NR_SET_MASTER: u32 = 0x1e;
const NR_DROP_MASTER: u32 = 0x1f;

/// `DRM_CAP_DUMB_BUFFER`; `GET_CAP` answers 1 for it and 0 elsewhere.
const CAP_DUMB_BUFFER: u64 = 0x1;
/// `DRM_CAP_PRIME`: both directions of PRIME work here (the dma-buf of
/// `prime_handle_to_fd`), which is what tells Mesa's GBM that the render
/// node can hand buffers to the card.
const CAP_PRIME: u64 = 0x5;
/// `DRM_PRIME_CAP_IMPORT | DRM_PRIME_CAP_EXPORT`.
const PRIME_CAPS: u64 = 0x3;
/// `DRM_CAP_TIMESTAMP_MONOTONIC`: the timestamps on the events this card
/// hands back are in `CLOCK_MONOTONIC`, not `CLOCK_REALTIME`.
///
/// It is not decoration: weston's DRM backend refuses to initialize without
/// it ("kernel DRM KMS does not support DRM_CAP_TIMESTAMP_MONOTONIC"), because
/// every frame it schedules is scheduled against the timestamp the flip
/// completion carried. A card that answered 1 while stamping events with the
/// wall clock would have weston compute frame deadlines from an epoch-based
/// number against its own monotonic presentation clock - which is why the
/// event's timestamp is built from the same `since_boot_nanos` that
/// `clock_gettime(CLOCK_MONOTONIC)` reads, and not from the RTC anchor.
const CAP_TIMESTAMP_MONOTONIC: u64 = 0x6;
/// `DRM_CAP_CURSOR_WIDTH` / `_HEIGHT`: the largest cursor the hardware takes.
///
/// This card has no cursor plane, so nothing here consumes the size - but a
/// cap query that answers *success* with value 0 reads to weston as "the
/// cursor is zero pixels wide" (`device->cursor_width = cap`), and 64 is the
/// size the kernel itself falls back to. The honest answer is the one a
/// cursor plane would have, not a zero that would size a buffer to nothing.
const CAP_CURSOR_WIDTH: u64 = 0x8;
const CAP_CURSOR_HEIGHT: u64 = 0x9;
/// The cursor a compositor would be handed if it asked for one.
const CURSOR_SIZE: u64 = 64;
/// `DRM_CAP_CRTC_IN_VBLANK_EVENT`: the flip and vblank events carry the CRTC
/// they happened on.
///
/// True here - `drm_event_vblank` has the field and this card fills it - and
/// load-bearing for the same reason `ATOMIC` is: weston's `init_kms_caps`
/// decides whether the kernel can do atomic mode setting from
/// `drmSetClientCap(ATOMIC)` *and* the last `drmGetCap` it made, which is
/// this one. Answering 0 turns an atomic-capable card into "does not support
/// atomic modesetting", and the compositor exits without ever sending a
/// commit.
const CAP_CRTC_IN_VBLANK_EVENT: u64 = 0x12;
/// `DRM_CLIENT_CAP_UNIVERSAL_PLANES`: the client wants the plane objects the
/// card has, which is how a compositor finds a primary plane to scan out on.
const CLIENT_CAP_UNIVERSAL_PLANES: u64 = 2;
/// `DRM_CLIENT_CAP_ATOMIC`: the client will describe a frame as one atomic
/// commit of property writes (`MODE_ATOMIC`), rather than as a sequence of
/// `SETCRTC`/`PAGE_FLIP` calls that can be observed half-applied.
///
/// The number is 3, and it is worth saying so: 1 is `STEREO_3D` and 2 is
/// universal planes, so a card that answered 1 here would accept a capability
/// nobody asked for and refuse the one weston needs - which is exactly how
/// the first version of this code read, and why weston still reported
/// "does not support atomic modesetting" with the request sitting in the
/// trace as `SET_CLIENT_CAP = -1 EOPNOTSUPP`.
const CLIENT_CAP_ATOMIC: u64 = 3;
/// The rest of `include/uapi/drm/drm.h`'s client capabilities, named so the
/// handler can tell "the card does not do this" from "no such capability" -
/// Linux answers the first with `EOPNOTSUPP` and the second with `EINVAL`, and
/// a client that gets the second wrong can never tell a typo from a driver
/// that lacks a feature.
const CLIENT_CAP_STEREO_3D: u64 = 1;
const CLIENT_CAP_ASPECT_RATIO: u64 = 4;
const CLIENT_CAP_WRITEBACK_CONNECTORS: u64 = 5;
const CLIENT_CAP_CURSOR_PLANE_HOTSPOT: u64 = 6;
const CLIENT_CAP_PLANE_COLOR_PIPELINE: u64 = 7;
/// `DRM_CLOEXEC == O_CLOEXEC` on Linux: mark the handed-out descriptor the
/// way `open(O_CLOEXEC)` would.
const DRM_CLOEXEC: u32 = 0x80000;
/// The magic `GET_MAGIC` hands out. Any value is self-consistent on a card
/// whose master is whoever opened it; what matters is that the pair of calls
/// exists for libdrm's setup to make.
const MAGIC: u32 = u32::from_le_bytes(*b"MPOS");

/// `DRM_MODE_OBJECT_CRTC` / `_CONNECTOR` / `_PLANE`: the object-type tags of
/// `OBJ_GETPROPERTIES`/`OBJ_SETPROPERTY`.
const OBJECT_CRTC: u32 = 0xcccc_cccc;
const OBJECT_CONNECTOR: u32 = 0xc0c0_c0c0;
const OBJECT_PLANE: u32 = 0xeeee_eeee;
/// `DRM_MODE_PROP_*`, on the bit numbers `drm_mode.h` gives them: the legacy
/// types are `RANGE` (bit 1), `IMMUTABLE` (bit 2), `ENUM` (bit 3), `BLOB`
/// (bit 4) and `BITMASK` (bit 5 - no property here is one, so it is named
/// rather than defined), and the extended types are
/// `DRM_MODE_PROP_TYPE(n) = n << 6`, which is where `OBJECT` (n = 1) and
/// `SIGNED_RANGE` (n = 2) come from.
///
/// These bits do not describe the property to this file: they describe it to
/// every caller that reads it back through `drmModeGetProperty`, which sorts
/// the reply by them before building the property userspace sees. `BLOB` sends
/// libdrm down its blob path, and that path sizes the value array from
/// `count_enum_blobs` rather than `count_values` - so marking a *range* with
/// the blob bit hands the caller a property that says it has values and has
/// none. That null `values` is what weston's `drm_property_info_populate`
/// dereferences in its range loop, and it is what `IN_FENCE_FD` was:
/// `RANGE | 1<<4`, which every caller reads as `RANGE | BLOB`.
const PROP_RANGE: u32 = 1 << 1;
const PROP_IMMUTABLE: u32 = 1 << 2;
const PROP_ENUM: u32 = 1 << 3;
const PROP_BLOB: u32 = 1 << 4;
const PROP_OBJECT: u32 = 1 << 6;
const PROP_SIGNED_RANGE: u32 = 1 << 7;
/// `DRM_PLANE_TYPE_PRIMARY`: the one plane is the CRTC's primary surface.
const PLANE_TYPE_PRIMARY: u64 = 1;
/// `DRM_MODE_PROP_BLEND`'s two enum values, as the kernel spells them:
/// pre-multiplied alpha, and straight "coverage" alpha.
const BLEND_PREMULT: u64 = 0;
const BLEND_COVERAGE: u64 = 1;

/// `DRM_MODE_ATOMIC_*`: the commit is a rehearsal (`TEST_ONLY`), a commit that
/// must be allowed to change the mode (`ALLOW_MODESET`), or one that may
/// return before it completes (`NONBLOCK` - this card completes it before it
/// returns either way, which is what Linux permits).
const ATOMIC_TEST_ONLY: u32 = 0x0100;
const ATOMIC_ALLOW_MODESET: u32 = 0x0400;
const ATOMIC_NONBLOCK: u32 = 0x0200;
/// `DRM_MODE_PAGE_FLIP_EVENT`: the commit's completion arrives as an event on
/// the card fd, which is how a compositor paces its frames.
const PAGE_FLIP_EVENT: u32 = 0x1;
/// The largest blob this card will store - a mode is 68 bytes and a damage
/// rectangle list a few hundred; a megabyte is a ceiling no caller reaches
/// and no attacker can use to fill the kernel's heap through one ioctl.
const MAX_BLOB: usize = 1 << 20;

/// The 32-bit fourccs `ADDFB2` accepts - every BGRA-order spelling of a
/// 32-bit pixel this scanout can show. `fourcc_code('X','R','2','4')` and
/// friends, little-endian.
const DRM_FORMAT_XRGB8888: u32 = 0x3432_5258;
const DRM_FORMAT_ARGB8888: u32 = 0x3432_4152;
const DRM_FORMAT_XBGR8888: u32 = 0x3432_4258;
const DRM_FORMAT_ABGR8888: u32 = 0x3432_4142;

/// A dumb buffer's pages are found in the mmap address space at
/// `handle * PAGES_PER_BUFFER * PAGE`, so `mmap_frame` can decode the buffer
/// back out of an absolute page index. 65536 pages (256 MiB) per slot is
/// comfortably above any single frame.
const PAGES_PER_BUFFER: usize = 1 << 16;
/// Cap on buffers and framebuffers the card will track.
const MAX_HANDLES: usize = 64;

/// One dumb buffer: its geometry and the physical pages behind it.
struct Dumb {
    width: u32,
    height: u32,
    pitch: u32,
    bpp: u32,
    /// One physical frame per page of the buffer, grown on the first write or
    /// map that touches it (the inode holds a reference on each; every process
    /// that maps it takes its own via `clone_frame_ref`).
    frames: Vec<usize>,
    // The card holds one reference per frame until DESTROY_DUMB; frames are
    // released then, and only actually recycled once no live mapping still
    // references them (each mapping took its own via clone_frame_ref).
}

impl Dumb {
    fn new(width: u32, height: u32, bpp: u32) -> Dumb {
        let pitch = (width * bpp / 8).next_multiple_of(64);
        Dumb {
            width,
            height,
            pitch,
            bpp,
            frames: Vec::new(),
        }
    }
    fn size(&self) -> u64 {
        self.pitch as u64 * self.height as u64
    }
    /// Grow to cover page `idx`, returning its frame (0 if the cap or memory
    /// is hit). Allocates outside any borrow of the global card state? no -
    /// callers hold it; allocation itself takes no card lock.
    fn ensure_page(&mut self, idx: usize) -> usize {
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
    fn release(&mut self) {
        for p in self.frames.drain(..) {
            if p != 0 {
                free_frames(p, 0);
            }
        }
    }
    /// Copy `len` bytes of the buffer at byte `src_off` into `dst`. The pages
    /// are read through the direct map; a page the buffer never grew reads as
    /// zeros, which is what an untouched dumb page shows on Linux.
    fn copy_span(&self, src_off: usize, dst: *mut u8, len: usize) {
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
struct Framebuffer {
    id: u32,
    handle: u32,
    width: u32,
    height: u32,
    pitch: u32,
    bpp: u32,
    depth: u32,
}

/// The identity record of one dma-buf this card has been involved with: the
/// buffer's ino, the GEM handle the relationship runs through, and - for a
/// buffer this card *exported* - the object itself, so a second export of
/// the same handle answers the same dma-buf rather than a second alias. A
/// record dies with its handle (`GEM_CLOSE`/`DESTROY_DUMB` prune it), which
/// is what bounds the table by the handle table; the import half keeps only
/// the (ino, handle) pair, because the descriptor's own lifetime owns the
/// buffer there.
struct PrimeRecord {
    ino: u64,
    handle: u32,
    buf: Option<Arc<crate::vfs::dmabuf::DmaBuf>>,
}

/// The card's mutable state, all under one lock. There is a single device, so
/// a process opening `card0` shares it with every other opener - which is
/// fine, because exactly one compositor owns the seat.
struct DrmState {
    dumb: Vec<Option<Dumb>>,
    fbs: Vec<Option<Framebuffer>>,
    /// The framebuffer currently driven to the scanout by the CRTC.
    current_fb: u32,
    /// The connector's `CRTC_ID` property value and its `DPMS` mode, and the
    /// CRTC's `ACTIVE` - recorded by `SETPROPERTY`, reported by
    /// `GETCONNECTOR`/`OBJ_GETPROPERTIES`. Presentation itself follows
    /// `current_fb`, not these words: they are the state a compositor reads
    /// back, not a second display path.
    conn_crtc: u32,
    dpms: u32,
    crtc_active: bool,
    /// The mode an atomic commit last bound to the CRTC, as the blob id
    /// `MODE_ID` carries. Reported back by `OBJ_GETPROPERTIES`, which is
    /// where a compositor reads the state it inherits.
    crtc_mode_id: u32,
    /// The plane's `CRTC_ID`, and the last committed destination rectangle
    /// (`CRTC_X/Y/W/H`) and source rectangle (`SRC_*`, 16.16). The card has
    /// one plane showing one whole buffer, so the rectangles do not steer
    /// presentation - they are the state the caller reads back, exactly as
    /// the connector's `CRTC_ID` and the CRTC's `ACTIVE` are.
    plane_crtc: u32,
    plane_dst: (u32, u32, u32, u32),
    plane_src: (u32, u32, u32, u32),
    /// Property blobs, by id (1-based, as every id space here is). A mode
    /// arrives as one, and so does a damage-rectangle list.
    blobs: Vec<Option<Vec<u8>>>,
    /// The PRIME identity table (see [`PrimeRecord`]).
    prime: Vec<PrimeRecord>,
    /// Sequence number for flip/vblank events, echoed back to the reader.
    seq: u32,
    /// Queued `drmEvent` objects awaiting `read` on the card fd: (type,
    /// payload-u64-words). Only flip-completion is produced.
    events: VecDeque<drm_event::Vblank>,
}

mod drm_event {
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

static STATE: Mutex<RefCell<DrmState>> = Mutex::new(RefCell::new(DrmState {
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
    blobs: Vec::new(),
    prime: Vec::new(),
    seq: 0,
    events: VecDeque::new(),
}));

/// The `drm_version` answer: this is not a real driver, so report the KMS ABI
/// we speak (4.0) and the name "mpos".
///
/// The strings move in the two-call shape libdrm's `drmGetVersion` drives, and
/// one rule of that shape is load-bearing rather than tidy: the answer to the
/// *second* call may never be larger than the length the caller brought with
/// it. The first call arrives with a null pointer and a zero length and asks
/// how much room is needed; the caller then allocates exactly `len + 1` and
/// hands the buffer back with `len` in the length field, where it means the
/// *size of that buffer*; and after the second call it terminates the string
/// with `name[name_len] = '\0'` - one byte past the length it was told, but
/// the last byte of what it allocated. A kernel that answers the second call
/// with anything bigger than what it answered the first writes that NUL
/// outside the block, which is how this card came to kill a compositor in
/// libdrm. So the length field is clamped here on the way out, and the bytes
/// are only copied when the caller's buffer actually holds the name.
///
/// Both calls *succeed*; answering the sizing call `EINVAL` made
/// `drmGetVersion` fail outright, and Mesa's loader then could not name the
/// driver at all ("failed to get driver name for fd 15", "MESA-LOADER: failed
/// to retrieve device information"), which is how llvmpipe came up not knowing
/// which device it was on.
fn fill_version(buf: usize) -> isize {
    // struct drm_version: three ints (then four bytes of padding), and a
    // length/pointer pair each for the name, the date and the description.
    const FIELDS: [(&[u8], usize, usize); 3] = [(b"mpos", 16, 24), (b"", 32, 40), (b"", 48, 56)];

    unsafe {
        let w = |off: usize, v: i32| ((buf + off) as *mut i32).write_unaligned(v);
        w(0, 4); // major
        w(4, 0); // minor
        w(8, 0); // patchlevel

        // The making of the answer, kept for the trace below: what the caller
        // brought in (name, date, and desc) and what it is being told back.
        let mut offered = [0usize; 3];
        let mut given = [0usize; 3];
        let mut ptrs = [0usize; 3];
        for (field, &(text, len_off, ptr_off)) in FIELDS.iter().enumerate() {
            let len = (buf + len_off) as *mut usize;
            let room = len.read_unaligned();
            // The *value* of the pointer field, not the address of the field:
            // the bytes of the name belong in the buffer the caller allocated
            // and handed in, and writing them at `buf + ptr_off` instead
            // overwrites the pointer itself with the name.
            let dst = ((buf + ptr_off) as *const usize).read_unaligned() as *mut u8;
            offered[field] = room;
            ptrs[field] = dst as usize;
            given[field] = if dst.is_null() {
                // The sizing call: it wants the length and brings no room.
                text.len()
            } else if !user_array(dst as usize, text.len(), 1, true) {
                // Room was offered, but not at that address: refuse the copy
                // and report the length, which is what Linux's failed
                // `copy_to_user` leaves the caller with.
                return -14; // EFAULT
            } else {
                if room >= text.len() {
                    core::ptr::copy_nonoverlapping(text.as_ptr(), dst, text.len());
                }
                room.min(text.len())
            };
            len.write_unaligned(given[field]);
        }
    }
    0
}

fn libc_EINVAL() -> isize {
    -22
}

/// Serve a DRM ioctl on the card a compositor holds.
pub fn drm_ioctl(fd: i32, nr: u32, arg: usize) -> isize {
    drm_request(fd, nr, arg)
}

/// Serve a DRM ioctl on the card. `nr` selects the request - the syscall
/// layer has already decoded it out of the request word, whichever half of
/// it carried the command number; `arg` is the user pointer to the request's
/// struct. Every struct is copied in whole, filled per the fixed topology,
/// and (for the in-out KMS structs) copied back - matching how libdrm passes
/// them.
fn drm_request(fd: i32, nr: u32, arg: usize) -> isize {
    // Most requests need a readable file that is the card.
    let file = match crate::sched::current_fds().clone_at(fd) {
        Ok(f) => f,
        Err(_) => return -9, // EBADF
    };
    if file.inode().as_drm().is_none() {
        return -25; // ENOTTY
    }
    // Master/auth requests carry no argument, so answer them before the
    // `arg == 0` fault check below would reject a legitimate no-buffer call.
    if matches!(nr, NR_AUTH | NR_SET_MASTER | NR_DROP_MASTER) {
        return 0;
    }
    // The caller's argument struct, validated before a single field of it is
    // read or written.
    //
    // Every one of these handlers reaches into `arg` with raw pointer
    // arithmetic, from kernel mode, on the caller's page tables: an address
    // that is not the caller's mapped memory is not a failed system call but a
    // paging fault *in the kernel* - which is how a compositor died mid-session
    // with a `#PF` at a poison value the allocator had filled a freed struct
    // with, and why the machine reset instead of reporting. Linux cannot be
    // faulted like that because it copies with `copy_*_user` and hears
    // `EFAULT`; this is the same guarantee, asked once per request, with the
    // size being `sizeof` the struct the request names - the allocation the
    // caller actually made. A request this card does not implement is not
    // checked here at all: it is refused below, as `ENOTTY`, without having
    // touched anything.
    if let Some((len, writable)) = arg_shape(nr) {
        if !crate::syscall::buffer_ok(arg, len, writable, true) {
            return -14; // EFAULT
        }
    }
    match nr {
        NR_VERSION => fill_version(arg),
        NR_GET_CAP => get_cap(arg),
        NR_SET_CLIENT_CAP => set_client_cap(arg),
        NR_GET_MAGIC => {
            // struct drm_auth { drm_magic_t magic; } - the token `AUTH_MAGIC`
            // accepts below. One master, so the token names nothing.
            unsafe { (arg as *mut u32).write_unaligned(MAGIC) };
            0
        }
        NR_AUTH_MAGIC => 0, // the only master authenticates its own magic
        NR_SET_VERSION => set_version(arg),
        NR_GEM_CLOSE | NR_DESTROY_DUMB => {
            // struct drm_gem_close / drm_mode_destroy_dumb: handle@0. The
            // two spell one release - a GEM handle and a dumb buffer are the
            // same object here, and both names may close it.
            let handle = unsafe { (arg as *const u32).read_unaligned() } as usize;
            destroy_handle(handle)
        }
        NR_PRIME_HANDLE_TO_FD => prime_handle_to_fd(arg),
        NR_PRIME_FD_TO_HANDLE => prime_fd_to_handle(arg),
        NR_GETRESOURCES => get_resources(arg),
        NR_GETCRTC => get_crtc(arg),
        NR_SETCRTC => set_crtc(arg),
        NR_GETENCODER => get_encoder(arg),
        NR_GETCONNECTOR => get_connector(arg),
        NR_GETPLANERESOURCES => {
            // struct drm_mode_get_plane_res { u64 plane_id_ptr@0;
            // u32 count_planes@8; } - the pointer *first*, like every other
            // KMS list request, and the count after it.
            //
            // The order is load-bearing rather than stylistic: libdrm sizes
            // its array from the count and hands the pointer back in the
            // second call, so a card that reads the count from offset 0 reads
            // the low half of that pointer instead - a number large enough to
            // pass any "is there room?" test - and then writes the plane id
            // through a pointer to a zero-length allocation. weston died of
            // exactly that, with musl's allocator finding its heap corrupted
            // and running `hlt` to say so.
            unsafe {
                let ids = (arg as *const u64).read_unaligned() as *mut u32;
                let want = ((arg + 8) as *const u32).read_unaligned();
                if want >= 1 && !ids.is_null() && user_array(ids as usize, 1, 4, true) {
                    ids.write_unaligned(PLANE_ID);
                }
                ((arg + 8) as *mut u32).write_unaligned(1);
            }
            0
        }
        NR_GETPLANE => get_plane(arg),
        NR_GETPROPERTY => get_property(arg),
        NR_SETPROPERTY => set_property(arg),
        NR_OBJ_GETPROPERTIES => obj_get_properties(arg),
        NR_OBJ_SETPROPERTY => obj_set_property(arg),
        NR_MODE_ATOMIC => mode_atomic(arg),
        NR_CREATE_BLOB => create_blob(arg),
        // The blob store: a mode arrives as a blob and a damage-rectangle
        // list does too, so the caller that made one can read it back.
        NR_GETPROPBLOB => get_propblob(arg),
        NR_DESTROY_BLOB => destroy_blob(arg),
        NR_GETFB => get_fb(arg),
        NR_ADDFB => add_fb(arg),
        NR_ADDFB2 => add_fb2(arg),
        NR_RMFB => {
            let id = unsafe { (arg as *const u32).read_unaligned() };
            critical_section::with(|cs| {
                for fb in STATE.borrow(cs).borrow_mut().fbs.iter_mut() {
                    if fb.as_ref().is_some_and(|f| f.id == id) {
                        *fb = None;
                    }
                }
            });
            0
        }
        NR_PAGE_FLIP => page_flip(arg),
        NR_CREATE_DUMB => create_dumb(arg),
        NR_MAP_DUMB => map_dumb(arg),
        _ => -25, // ENOTTY
    }
}

/// The caller's argument for request `nr`: how many bytes the kernel touches,
/// and whether it writes into them. `None` means the request carries no
/// argument at all (or is unknown), so there is nothing to validate.
///
/// The lengths are `sizeof` of the struct `<drm.h>` and `<drm_mode.h>` name for
/// the request - not the count of fields this driver happens to read - because
/// the caller allocated the former, and because a check that only covered the
/// fields in use would let a request whose later fields are read by a future
/// version of this file go unvalidated again.
fn arg_shape(nr: u32) -> Option<(usize, bool)> {
    Some(match nr {
        NR_VERSION => (64, true),  // struct drm_version
        NR_GET_MAGIC => (4, true), // struct drm_auth
        NR_AUTH_MAGIC => (4, false),
        NR_SET_VERSION => (16, false),       // struct drm_set_version
        NR_GET_CAP => (16, true),            // struct drm_get_cap
        NR_SET_CLIENT_CAP => (16, false),    // struct drm_set_client_cap
        NR_GEM_CLOSE => (8, false),          // struct drm_gem_close
        NR_PRIME_HANDLE_TO_FD => (12, true), // struct drm_prime_handle
        NR_PRIME_FD_TO_HANDLE => (12, true),
        NR_GETRESOURCES => (64, true), // struct drm_mode_card_res
        NR_GETCRTC => (104, true),     // struct drm_mode_crtc
        NR_SETCRTC => (104, false),
        NR_GETENCODER => (20, true),   // struct drm_mode_get_encoder
        NR_GETCONNECTOR => (80, true), // struct drm_mode_get_connector
        NR_GETPLANERESOURCES => (16, true), // struct drm_mode_get_plane_res
        NR_GETPLANE => (32, true),     // struct drm_mode_get_plane
        NR_GETPROPERTY => (64, true),  // struct drm_mode_get_property
        NR_SETPROPERTY => (20, false), // struct drm_mode_setproperty
        NR_OBJ_GETPROPERTIES => (32, true), // struct drm_mode_obj_get_properties
        NR_OBJ_SETPROPERTY => (24, false), // struct drm_mode_obj_set_property
        NR_MODE_ATOMIC => (56, false), // struct drm_mode_atomic
        NR_CREATE_BLOB => (16, true),  // struct drm_mode_create_blob
        NR_GETPROPBLOB => (16, true),  // struct drm_mode_get_blob
        NR_DESTROY_BLOB => (4, false), // struct drm_mode_destroy_blob
        NR_GETFB => (28, true),        // struct drm_mode_fb_cmd
        NR_ADDFB => (28, false),
        NR_ADDFB2 => (104, false),     // struct drm_mode_fb_cmd2
        NR_RMFB => (4, false),         // struct drm_mode_rmfb
        NR_PAGE_FLIP => (24, false),   // struct drm_mode_crtc_page_flip
        NR_CREATE_DUMB => (32, true),  // struct drm_mode_create_dumb
        NR_MAP_DUMB => (16, true),     // struct drm_mode_map_dumb
        NR_DESTROY_DUMB => (4, false), // struct drm_mode_destroy_dumb
        _ => return None,
    })
}

/// The caller's array, checked the same way the struct above is: `Some(ptr)`
/// when the `count` elements this request is about to touch really are the
/// caller's memory, `None` when they are not - which the caller hears as
/// `EFAULT`, exactly as it would from Linux's `copy_*_user`.
///
/// The count is what *this* request writes or reads, not what the caller's
/// count field claims: a sizing call says it wants a thousand ids and hands a
/// null pointer, and the answer to that is to report the number and touch
/// nothing.
fn user_array(ptr: usize, count: usize, elem: usize, write: bool) -> bool {
    if count == 0 || ptr == 0 {
        return false;
    }
    match count.checked_mul(elem) {
        Some(bytes) => crate::syscall::buffer_ok(ptr, bytes, write, true),
        None => false,
    }
}

/// `GET_CAP`: the caps this card has, and 0 elsewhere.
fn get_cap(arg: usize) -> isize {
    // struct drm_get_cap { u64 capability; u64 value; }
    let cap = unsafe { (arg as *const u64).read_unaligned() };
    let value = match cap {
        CAP_DUMB_BUFFER => 1,
        CAP_PRIME => PRIME_CAPS,
        CAP_TIMESTAMP_MONOTONIC => 1,
        CAP_CRTC_IN_VBLANK_EVENT => 1,
        CAP_CURSOR_WIDTH | CAP_CURSOR_HEIGHT => CURSOR_SIZE,
        _ => 0,
    };
    unsafe { ((arg + 8) as *mut u64).write_unaligned(value) };
    0
}

/// `SET_CLIENT_CAP`: the caps this card's objects actually back are accepted,
/// the caps whose objects it does not have are refused `EOPNOTSUPP`, and a
/// capability number it has never heard of is refused `EINVAL` - the three
/// answers Linux's `drm_setclient_cap` distinguishes.
///
/// `ATOMIC` is accepted because it is true: `MODE_ATOMIC` below is a real
/// implementation of the request, applying every property the caller names
/// and completing the flip. It is also not optional for a compositor -
/// weston 16's DRM backend has no legacy path left, and answers a kernel
/// without this cap with "Kernel DRM KMS does not support
/// DRM_CLIENT_CAP_ATOMIC" and exits. The plane-color cap stays refused,
/// because no plane here has a color pipeline, and so does the cursor-plane
/// hotspot, because there is no cursor plane to carry one.
fn set_client_cap(arg: usize) -> isize {
    // struct drm_set_client_cap { u64 capability; u64 value; }
    let capability = unsafe { (arg as *const u64).read_unaligned() };
    let value = unsafe { ((arg + 8) as *const u64).read_unaligned() };
    // Linux's `drm_setclient_cap` opens by rejecting any value that is not 0
    // or 1 - and a 0 is an *un*set, which is a legal thing to ask for, not a
    // refusal. The old shape here accepted only `1`, so a client clearing a
    // cap it had turned on was told the cap was unavailable.
    if value > 1 {
        return libc_EINVAL();
    }
    match capability {
        // The caps this card's own objects back, plus the ones that only say
        // which fields of a reply the client will read: the plane list it does
        // have, atomic commits it does implement, and the aspect-ratio and
        // writeback-connector halves of `GETRESOURCES`/`GETCONNECTOR`.
        //
        // `WRITEBACK_CONNECTORS` is accepted on a card with no writeback
        // connector, which is Linux's rule rather than an oversight: it is a
        // promise about what the *client* will read, and the reply to that
        // read is an empty list. Linux's `drm_setclient_cap` checks only the
        // client's own atomicity and the value, so a card that refuses it is
        // telling weston 16 - which asks unconditionally, at the top of its
        // `init_kms_caps` - that something failed that did not.
        CLIENT_CAP_STEREO_3D
        | CLIENT_CAP_UNIVERSAL_PLANES
        | CLIENT_CAP_ATOMIC
        | CLIENT_CAP_ASPECT_RATIO
        | CLIENT_CAP_WRITEBACK_CONNECTORS => 0,
        // Refused because the object behind them does not exist here: no
        // plane with a color pipeline, no cursor plane with a hotspot to
        // carry. `EOPNOTSUPP` is Linux's answer for a cap the driver declares
        // it does not do - the check is the driver's own feature, and this
        // card answers it honestly.
        CLIENT_CAP_CURSOR_PLANE_HOTSPOT | CLIENT_CAP_PLANE_COLOR_PIPELINE => -95,
        // A capability number the driver has never heard of is not "not
        // supported", it is not a request at all, and Linux says so.
        _ => libc_EINVAL(),
    }
}

/// `SET_VERSION`: the caller names the DRM *ioctl interface* it wants; the
/// reply reports the interface in force and this driver's own version,
/// 1.1.0 - the same version `fill_version` answers by name.
fn set_version(arg: usize) -> isize {
    // struct drm_set_version { int drm_di_major@0; int drm_di_minor@4;
    // int drm_dd_major@8; int drm_dd_minor@12; }
    unsafe {
        let di_major = (arg as *const i32).read_unaligned();
        let di_minor = ((arg + 4) as *const i32).read_unaligned();
        // A 0/0 (or negative) request asks for the canonical version, and
        // the KMS interface is where this card stops; a request above 1.4 is
        // refused the way newer userland is on an older kernel.
        let (major, minor) = if di_major >= 0 && (di_major > 0 || di_minor > 0) {
            if di_major > 1 || di_minor > 4 {
                return libc_EINVAL();
            }
            (di_major, di_minor)
        } else {
            (1, 4)
        };
        (arg as *mut i32).write_unaligned(major); // drm_di_major
        ((arg + 4) as *mut i32).write_unaligned(minor); // drm_di_minor
        ((arg + 8) as *mut i32).write_unaligned(1); // drm_dd_major
        ((arg + 12) as *mut i32).write_unaligned(1); // drm_dd_minor
    }
    0
}

fn get_resources(arg: usize) -> isize {
    // struct drm_mode_card_res: fb@0,crtc@8,conn@16,enc@24 (u64 ptrs);
    // count_fbs@32, crtcs@36, conns@40, encoders@44, widths/heights@48..
    //
    // The four id arrays are the caller's memory, and two answers are possible
    // when one is named: a null pointer is the *sizing* call, which the count
    // alone answers, while a non-null address that the kernel cannot write is
    // the request failing with `EFAULT` - what Linux's `copy_to_user` returns,
    // and the only answer a caller can act on. Writing through the second one
    // from kernel mode is a page fault instead.
    let fault = critical_section::with(|cs| {
        let st = STATE.borrow(cs).borrow();
        let mut fault = false;
        unsafe {
            let put_ids = |ptr_off: usize, count_off: usize, ids: &[u32], fault: &mut bool| {
                let ptr = ((arg + ptr_off) as *const u64).read_unaligned() as *mut u32;
                let want = ((arg + count_off) as *const u32).read_unaligned();
                if want >= ids.len() as u32 && !ptr.is_null() {
                    if user_array(ptr as usize, ids.len(), 4, true) {
                        for (i, id) in ids.iter().enumerate() {
                            ptr.add(i).write_unaligned(*id);
                        }
                    } else {
                        *fault = true;
                    }
                }
                ((arg + count_off) as *mut u32).write_unaligned(ids.len() as u32);
            };
            // count_fbs is set by the caller to its buffer size; the kernel
            // reports how many there are and fills fb ids when space allows.
            let fbs: Vec<u32> = st
                .fbs
                .iter()
                .filter_map(|f| f.as_ref().map(|f| f.id))
                .collect();
            let c = ((arg + 32) as *const u32).read_unaligned();
            if c >= fbs.len() as u32 {
                let p = ((arg) as *const u64).read_unaligned() as *mut u32;
                if !p.is_null() {
                    if user_array(p as usize, fbs.len(), 4, true) {
                        for (i, id) in fbs.iter().enumerate() {
                            p.add(i).write_unaligned(*id);
                        }
                    } else {
                        fault = true;
                    }
                }
            }
            ((arg + 32) as *mut u32).write_unaligned(fbs.len() as u32);
            put_ids(8, 36, &[CRTC_ID], &mut fault);
            put_ids(16, 40, &[CONNECTOR_ID], &mut fault);
            put_ids(24, 44, &[ENCODER_ID], &mut fault);
            // min/max size: the modes we offer.
            ((arg + 48) as *mut u32).write_unaligned(0); // min_width
            ((arg + 52) as *mut u32).write_unaligned(4096);
            ((arg + 56) as *mut u32).write_unaligned(0);
            ((arg + 60) as *mut u32).write_unaligned(4096);
        }
        fault
    });
    if fault {
        return -14; // EFAULT
    }
    0
}

/// Write a `drm_mode_modeinfo` describing the current scanout at `mode_off`
/// within the crtc struct (mode is at offset 36 in `drm_mode_crtc`).
fn write_mode(buf: *mut u8, clock_khz: u32, w: u32, h: u32) {
    unsafe {
        let w32 = |off: usize, v: u32| ((buf.add(off)) as *mut u32).write_unaligned(v);
        let w16 = |off: usize, v: u16| ((buf.add(off)) as *mut u16).write_unaligned(v);
        w32(0, clock_khz); // clock
        w16(4, w as u16); // hdisplay
        w16(6, w as u16); // hsync_start
        w16(8, w as u16); // hsync_end
        w16(10, w as u16); // htotal
        w16(12, 0); // hskew
        w16(14, h as u16); // vdisplay
        w16(16, h as u16); // vsync_start
        w16(18, h as u16); // vsync_end
        w16(20, h as u16); // vtotal
        w16(22, 0); // vscan
        w32(24, 60); // vrefresh
        w32(28, 0); // flags
        w32(32, 0x08 | 0x40); // type: PREFERRED|DRIVER
        // The mode's name: 32 NUL-padded bytes at 36, which Linux fills with
        // the geometry ("1280x800").
        //
        // Leaving it alone is not "no name". The caller's struct is its own
        // uninitialised memory - libdrm allocates the mode array with
        // `malloc` - so what a compositor reads back is garbage that may not
        // contain a NUL anywhere in the field, and weston's `strdup` of it
        // then walks off the end of the array. That overrun is what killed
        // weston on this card: a string copied over an adjacent pointer, and
        // the next terminator written through the pointer it had just
        // overwritten (the faulting instruction was `movb $0, (%rdx,%rax)`
        // with `rdx` holding the ASCII of the string that landed there).
        let name = alloc::format!("{}x{}", w, h);
        let field = buf.add(36);
        core::ptr::write_bytes(field, 0, 32);
        core::ptr::copy_nonoverlapping(name.as_ptr(), field, name.len().min(32));
    }
}

fn crtc_mode_dims() -> (u32, u32) {
    // The scanout geometry, if the GPU is present; else a fallback.
    match crate::drivers::virtio::gpu::global() {
        Some(gpu) => gpu.dims(),
        None => (1024, 768),
    }
}

/// Copy `fb_id`'s pixels into the GPU's scanout buffer - the step that makes
/// a `SETCRTC`/`PAGE_FLIP` a presentation rather than bookkeeping. The rows
/// go one by one because the two pitches differ: the dumb buffer's is
/// 64-byte aligned, the scanout's is `width * 4`, and a whole-buffer copy
/// would smear every row after the first.
///
/// Returns 0, or -EINVAL when the framebuffer is unknown, not 32bpp, or
/// larger than the scanout's mode - Linux refuses a framebuffer that does
/// not fit the CRTC with the same errno.
/// Whether `fb_id` names a framebuffer this card can put on the scanout: one
/// it made, 32bpp, no larger than the mode. A `TEST_ONLY` atomic commit asks
/// exactly this question, which is why it is a separate answer from the copy
/// that follows it.
fn fb_fits(st: &DrmState, fb_id: u32) -> bool {
    let Some(fb) = st
        .fbs
        .iter()
        .filter_map(|f| f.as_ref())
        .find(|f| f.id == fb_id)
    else {
        return false;
    };
    if !handle_is_dumb(st, fb.handle) {
        return false;
    }
    let (screen_w, screen_h) = crtc_mode_dims();
    fb.bpp == 32 && fb.width <= screen_w && fb.height <= screen_h
}
/// Whether `handle` names a buffer this card can scan out - which is what
/// `fb_fits` and `copy_to_scanout` both turn on, and the one question a
/// GBM/PRIME-imported buffer can fail.
fn handle_is_dumb(st: &DrmState, handle: u32) -> bool {
    st.dumb
        .get(handle as usize - 1)
        .is_some_and(|s| s.is_some())
}

/// Put the pixels of framebuffer `fb_id` on the scanout. A framebuffer the
/// card did not make, or one larger than the scanout's mode, is refused with
/// the same errno Linux uses.
fn copy_to_scanout(st: &DrmState, fb_id: u32) -> isize {
    if !fb_fits(st, fb_id) {
        return libc_EINVAL();
    }
    let fb = st
        .fbs
        .iter()
        .filter_map(|f| f.as_ref())
        .find(|f| f.id == fb_id)
        .expect("fb_fits checked the framebuffer is there");
    let dumb = st
        .dumb
        .get(fb.handle as usize - 1)
        .and_then(|s| s.as_ref())
        .expect("fb_fits checked the buffer is there");
    let (screen_w, screen_h) = crtc_mode_dims();
    // A headless machine has no scanout to draw to; the bookkeeping is still
    // right, and the flush after is the no-op it already is there.
    let Some(gpu) = crate::drivers::virtio::gpu::global() else {
        return 0;
    };
    let scanout = phys_to_virt(gpu.fb_phys()) as *mut u8;
    let scan_pitch = screen_w as usize * 4;
    let row = (fb.width as usize * 4).min(scan_pitch);
    let rows = (fb.height as usize).min(screen_h as usize);
    for y in 0..rows {
        unsafe {
            dumb.copy_span(y * dumb.pitch as usize, scanout.add(y * scan_pitch), row);
        }
    }
    0
}

fn get_crtc(arg: usize) -> isize {
    // struct drm_mode_crtc: set_connectors_ptr@0,count_connectors@8,
    // crtc_id@12, fb_id@16, x@20, y@24, gamma_size@28, mode_valid@32, mode@36.
    critical_section::with(|cs| {
        let st = STATE.borrow(cs).borrow();
        let (w, h) = crtc_mode_dims();
        unsafe {
            let p = arg as *mut u8;
            (p.add(12) as *mut u32).write_unaligned(CRTC_ID);
            (p.add(16) as *mut u32).write_unaligned(st.current_fb);
            (p.add(32) as *mut u32).write_unaligned(1); // mode_valid
            write_mode(p.add(36), (w * h * 60) / 1000, w, h);
        }
    });
    0
}

fn set_crtc(arg: usize) -> isize {
    // struct drm_mode_crtc: crtc_id@12, fb_id@16, x@20,y@24, ...
    let crtc_id = unsafe { ((arg + 12) as *const u32).read_unaligned() };
    let fb_id = unsafe { ((arg + 16) as *const u32).read_unaligned() };
    if crtc_id != CRTC_ID {
        return libc_EINVAL();
    }
    // fb_id 0 is "disable the CRTC": accepted, and nothing is drawn. Any
    // other id must be a framebuffer the card made, whose pixels are put
    // on the scanout before the call returns.
    let rc = if fb_id == 0 {
        critical_section::with(|cs| STATE.borrow(cs).borrow_mut().current_fb = 0);
        0
    } else {
        critical_section::with(|cs| {
            let mut st = STATE.borrow(cs).borrow_mut();
            match copy_to_scanout(&st, fb_id) {
                0 => {
                    st.current_fb = fb_id;
                    0
                }
                e => e,
            }
        })
    };
    // Present: flush the scanout if a render engine is running.
    if rc == 0 {
        crate::drivers::video::render::request_flush_blocking();
    }
    rc
}

fn get_encoder(arg: usize) -> isize {
    // struct drm_mode_get_encoder: encoder_id@0, encoder_type@4, crtc_id@8,
    // possible_crtcs@12, possible_clones@16.
    unsafe {
        let p = arg as *mut u32;
        let want = p.read_unaligned();
        if want != 0 && want != ENCODER_ID {
            return libc_EINVAL();
        }
        p.write_unaligned(ENCODER_ID);
        p.add(1).write_unaligned(ENCODER_TYPE);
        p.add(2).write_unaligned(CRTC_ID);
        p.add(3).write_unaligned(1); // possible_crtcs
        p.add(4).write_unaligned(0);
    }
    0
}

fn get_connector(arg: usize) -> isize {
    // struct drm_mode_get_connector:
    // encoders_ptr@0, modes_ptr@8, props_ptr@16, prop_values_ptr@24 (u64);
    // count_modes@32, count_props@36, count_encoders@40, encoder_id@44,
    // connector_id@48, connector_type@52, connector_type_id@56, connection@60,
    // mm_width@64, mm_height@68, subpixel@72, pad@76.
    let want = unsafe { ((arg + 48) as *const u32).read_unaligned() };
    if want != 0 && want != CONNECTOR_ID {
        return libc_EINVAL();
    }
    let (w, h) = crtc_mode_dims();
    // The connector's two properties and their current values - what
    // `OBJ_GETPROPERTIES` also answers, on the path libdrm fills in through
    // `drmModeGetConnector`.
    let (crtc_value, dpms_value) = critical_section::with(|cs| {
        let st = STATE.borrow(cs).borrow();
        (st.conn_crtc as u64, st.dpms as u64)
    });
    unsafe {
        let p = arg as *mut u8;
        // Fill a single mode into modes_ptr if the caller asked for it.
        let count_modes = (p.add(32) as *const u32).read_unaligned();
        let modes_ptr = ((p.add(8) as *const u64).read_unaligned()) as *mut u8;
        if count_modes >= 1 && !modes_ptr.is_null() && user_array(modes_ptr as usize, 1, 68, true) {
            write_mode(modes_ptr, (w * h * 60) / 1000, w, h);
        }
        (p.add(32) as *mut u32).write_unaligned(1); // count_modes
        // The property ids and values, when the caller has room for them.
        let count_props = (p.add(36) as *const u32).read_unaligned();
        (p.add(36) as *mut u32).write_unaligned(2); // count_props
        let props_ptr = ((p.add(16) as *const u64).read_unaligned()) as *mut u32;
        if count_props >= 2 && !props_ptr.is_null() && user_array(props_ptr as usize, 2, 4, true) {
            props_ptr.write_unaligned(PROP_CRTC_ID);
            props_ptr.add(1).write_unaligned(PROP_DPMS);
        }
        let values_ptr = ((p.add(24) as *const u64).read_unaligned()) as *mut u64;
        if count_props >= 2 && !values_ptr.is_null() && user_array(values_ptr as usize, 2, 8, true)
        {
            values_ptr.write_unaligned(crtc_value);
            values_ptr.add(1).write_unaligned(dpms_value);
        }
        (p.add(40) as *mut u32).write_unaligned(1); // count_encoders
        let enc_ptr = ((p as *const u64).read_unaligned()) as *mut u32;
        if !enc_ptr.is_null() && user_array(enc_ptr as usize, 1, 4, true) {
            enc_ptr.write_unaligned(ENCODER_ID);
        }
        (p.add(44) as *mut u32).write_unaligned(ENCODER_ID); // encoder_id
        (p.add(48) as *mut u32).write_unaligned(CONNECTOR_ID);
        (p.add(52) as *mut u32).write_unaligned(CONNECTOR_TYPE);
        (p.add(56) as *mut u32).write_unaligned(1); // connector_type_id
        (p.add(60) as *mut u32).write_unaligned(CONNECTED); // connection
        (p.add(64) as *mut u32).write_unaligned(0); // mm_width
        (p.add(68) as *mut u32).write_unaligned(0);
    }
    0
}

/// The one plane: the CRTC's primary surface, reported the way a compositor
/// matches an output format against it.
fn get_plane(arg: usize) -> isize {
    // struct drm_mode_get_plane: plane_id@0, crtc_id@4, fb_id@8,
    // possible_crtcs@12, gamma_size@16, count_format_types@20,
    // format_type_ptr@24. The format list is the two-call shape the KMS
    // list ioctls all use: count first, the array once there is room.
    let fb_id = critical_section::with(|cs| STATE.borrow(cs).borrow().current_fb);
    unsafe {
        let p = arg as *mut u32;
        p.write_unaligned(PLANE_ID); // plane_id
        p.add(1).write_unaligned(CRTC_ID); // crtc_id: the CRTC it serves
        p.add(2).write_unaligned(fb_id); // fb_id
        p.add(3).write_unaligned(1); // possible_crtcs: bit 0
        p.add(4).write_unaligned(0); // gamma_size
        // The format list, when the caller has room for it.
        let want = p.add(5).read_unaligned();
        let formats = ((arg + 24) as *const u64).read_unaligned() as *mut u32;
        if want >= 1 && !formats.is_null() && user_array(formats as usize, 1, 4, true) {
            formats.write_unaligned(DRM_FORMAT_XRGB8888);
        }
        p.add(5).write_unaligned(1); // count_format_types
    }
    0
}

/// `GETPROPERTY`: one of the three standard properties by id - the shape
/// every KMS property query takes (counts first, arrays once the caller has
/// sized them).
fn get_property(arg: usize) -> isize {
    // struct drm_mode_get_property: values_ptr@0, enum_blob_ptr@8,
    // prop_id@16, flags@20, name[32]@24, count_values@56, count_enum_blobs@60.
    let prop_id = unsafe { ((arg + 16) as *const u32).read_unaligned() };
    // (name, flags, value list, enum entries as (name, value))
    let (name, flags, values, enums): (&str, u32, &[u64], &[(&str, u64)]) = match prop_id {
        // An object-typed property: the value is the id of a CRTC (0 for
        // none). The *current* binding comes from the connector's property
        // list, not from here - this reports the property's shape.
        PROP_CRTC_ID => ("CRTC_ID", PROP_OBJECT, &[0], &[]),
        PROP_DPMS => (
            "DPMS",
            PROP_ENUM,
            &[0, 1, 2, 3],
            &[("On", 0), ("Standby", 1), ("Suspend", 2), ("Off", 3)],
        ),
        PROP_ACTIVE => ("ACTIVE", PROP_RANGE, &[0, 1], &[]),
        // The plane's class, and the one value it carries here: this is the
        // primary surface, the plane a compositor looks for by name.
        PROP_TYPE => (
            "type",
            PROP_ENUM | PROP_IMMUTABLE,
            &[0, 1, 2],
            &[("Overlay", 0), ("Primary", 1), ("Cursor", 2)],
        ),
        // The mode an atomic commit binds: a blob, which is a
        // `drmModeModeInfo` as `CREATE_BLOB` stored it.
        PROP_MODE_ID => ("MODE_ID", PROP_BLOB, &[], &[]),
        // The plane's buffer and the CRTC it feeds - object-typed, so the
        // value is the framebuffer's or the CRTC's id (0 for "none").
        PROP_PLANE_FB_ID => ("FB_ID", PROP_OBJECT, &[], &[]),
        PROP_PLANE_CRTC_ID => ("CRTC_ID", PROP_OBJECT, &[], &[]),
        // The source rectangle in 16.16 fixed point and the destination in
        // pixels: the pair that says *where* on the scanout the buffer goes.
        PROP_SRC_X => ("SRC_X", PROP_RANGE, &[0, u32::MAX as u64], &[]),
        PROP_SRC_Y => ("SRC_Y", PROP_RANGE, &[0, u32::MAX as u64], &[]),
        PROP_SRC_W => ("SRC_W", PROP_RANGE, &[0, u32::MAX as u64], &[]),
        PROP_SRC_H => ("SRC_H", PROP_RANGE, &[0, u32::MAX as u64], &[]),
        PROP_CRTC_X => ("CRTC_X", PROP_RANGE, &[0, u32::MAX as u64], &[]),
        PROP_CRTC_Y => ("CRTC_Y", PROP_RANGE, &[0, u32::MAX as u64], &[]),
        PROP_CRTC_W => ("CRTC_W", PROP_RANGE, &[0, u32::MAX as u64], &[]),
        PROP_CRTC_H => ("CRTC_H", PROP_RANGE, &[0, u32::MAX as u64], &[]),
        // How the plane's pixels are composited onto what is below them.
        // The scanout carries the compositor's finished frame, so
        // pre-multiplied alpha is what it is set to; the property exists
        // because weston looks for it and takes its absence for a plane
        // that supports neither mode.
        PROP_BLEND => (
            "pixel blend mode",
            PROP_ENUM,
            &[BLEND_PREMULT, BLEND_COVERAGE],
            &[
                ("Pre-multiplied", BLEND_PREMULT),
                ("Coverage", BLEND_COVERAGE),
            ],
        ),
        // A signed descriptor number: -1 means "no fence", which is what
        // every commit here carries. The range is the one the kernel gives
        // it, so a caller reading the shape back sees what it expects.
        PROP_IN_FENCE_FD => (
            "IN_FENCE_FD",
            PROP_RANGE | PROP_SIGNED_RANGE,
            &[-1i64 as u64, i32::MAX as u64],
            &[],
        ),
        _ => return libc_EINVAL(),
    };
    unsafe {
        // The name, NUL-padded into its 32-byte field.
        let field = (arg + 24) as *mut u8;
        core::ptr::write_bytes(field, 0, 32);
        core::ptr::copy_nonoverlapping(name.as_ptr(), field, name.len());
        ((arg + 20) as *mut u32).write_unaligned(flags);
        // The value list, when the caller has room for it.
        let want_values = ((arg + 56) as *const u32).read_unaligned();
        let values_ptr = (arg as *const u64).read_unaligned() as *mut u64;
        if want_values >= values.len() as u32
            && !values_ptr.is_null()
            && user_array(values_ptr as usize, values.len(), 8, true)
        {
            for (i, v) in values.iter().enumerate() {
                values_ptr.add(i).write_unaligned(*v);
            }
        }
        ((arg + 56) as *mut u32).write_unaligned(values.len() as u32);
        // The enum entries (struct drm_mode_property_enum: u64 value,
        // name[32]), same shape.
        let want_enums = ((arg + 60) as *const u32).read_unaligned();
        let enums_ptr = ((arg + 8) as *const u64).read_unaligned() as *mut u8;
        if want_enums >= enums.len() as u32
            && !enums_ptr.is_null()
            && user_array(enums_ptr as usize, enums.len(), 40, true)
        {
            for (i, (ename, evalue)) in enums.iter().enumerate() {
                let entry = enums_ptr.add(i * 40);
                (entry as *mut u64).write_unaligned(*evalue);
                core::ptr::write_bytes(entry.add(8), 0, 32);
                core::ptr::copy_nonoverlapping(ename.as_ptr(), entry.add(8), ename.len());
            }
        }
        ((arg + 60) as *mut u32).write_unaligned(enums.len() as u32);
    }
    0
}

/// Record a property write against one KMS object, by id: the shared tail of
/// `SETPROPERTY`, `OBJ_SETPROPERTY` and the "recorded, not acted on"
/// properties of an atomic commit.
///
/// Every property here is state a compositor reads back. The one write that
/// changes what is on the screen is the plane's `FB_ID`, and even that only
/// takes effect when a commit applies it - the pixel copy is the caller's
/// step, not this one's.
fn set_object_prop(st: &mut DrmState, obj: u32, prop_id: u32, value: u64) -> isize {
    match (obj, prop_id) {
        (CONNECTOR_ID, PROP_CRTC_ID) => st.conn_crtc = value as u32,
        (CONNECTOR_ID, PROP_DPMS) => st.dpms = value as u32,
        (CRTC_ID, PROP_ACTIVE) => st.crtc_active = value != 0,
        (CRTC_ID, PROP_MODE_ID) => st.crtc_mode_id = value as u32,
        // The plane's buffer is the one write this function does not make:
        // its caller has already put the pixels on the scanout (a commit
        // applies it, a lone property write shows it), and what records the
        // value is `current_fb`. Accepting it here keeps the two spellings of
        // a property write - the single ioctl and the commit - from
        // disagreeing about which properties the plane has.
        (PLANE_ID, PROP_PLANE_FB_ID) => {}
        (PLANE_ID, PROP_PLANE_CRTC_ID) => st.plane_crtc = value as u32,
        // The rectangles are recorded, not acted on: this card has one plane
        // showing one whole buffer, so the source and destination a caller
        // names are the state it reads back rather than a window onto the
        // scanout. Scaling and positioning are what a second plane would
        // need, and there is no second plane.
        (PLANE_ID, PROP_SRC_X) => st.plane_src.0 = value as u32,
        (PLANE_ID, PROP_SRC_Y) => st.plane_src.1 = value as u32,
        (PLANE_ID, PROP_SRC_W) => st.plane_src.2 = value as u32,
        (PLANE_ID, PROP_SRC_H) => st.plane_src.3 = value as u32,
        (PLANE_ID, PROP_CRTC_X) => st.plane_dst.0 = value as u32,
        (PLANE_ID, PROP_CRTC_Y) => st.plane_dst.1 = value as u32,
        (PLANE_ID, PROP_CRTC_W) => st.plane_dst.2 = value as u32,
        (PLANE_ID, PROP_CRTC_H) => st.plane_dst.3 = value as u32,
        // Pre-multiplied or coverage alpha: the scanout carries the
        // compositor's finished frame, so the answer is recorded nowhere -
        // there is nothing below it to blend against. The acquire fence is
        // the same kind of write: there is no asynchronous engine here to
        // wait for, so the value is accepted and forgotten.
        (PLANE_ID, PROP_BLEND) | (PLANE_ID, PROP_IN_FENCE_FD) => {}
        _ => return libc_EINVAL(),
    }
    0
}

/// Record a property write against the connector. Only the two properties it
/// carries are settable; anything else is the refusal Linux makes of a
/// property the object does not have.
fn set_connector_prop(prop_id: u32, value: u64) -> isize {
    critical_section::with(|cs| {
        let mut st = STATE.borrow(cs).borrow_mut();
        match prop_id {
            PROP_CRTC_ID => st.conn_crtc = value as u32,
            PROP_DPMS => st.dpms = value as u32,
            _ => return libc_EINVAL(),
        };
        0
    })
}

/// `SETPROPERTY`: the connector's property, by the legacy connector-typed
/// spelling.
fn set_property(arg: usize) -> isize {
    // struct drm_mode_connector_set_property: value@0(u64), prop_id@8,
    // connector_id@12.
    let value = unsafe { (arg as *const u64).read_unaligned() };
    let prop_id = unsafe { ((arg + 8) as *const u32).read_unaligned() };
    let connector_id = unsafe { ((arg + 12) as *const u32).read_unaligned() };
    if connector_id != CONNECTOR_ID {
        return libc_EINVAL();
    }
    set_connector_prop(prop_id, value)
}

/// `OBJ_SETPROPERTY`: the object-typed spelling of the same write.
///
/// The object *type* is what the caller says it is talking to, so it is
/// checked against the object's real type - a plane's id under
/// `OBJECT_CONNECTOR` is the mismatch Linux refuses. The one write that
/// moves the scanout is the plane's `FB_ID`, and it is followed by the
/// pixel copy here, because a caller that sets it outside a commit is asking
/// for the buffer to be shown now.
fn obj_set_property(arg: usize) -> isize {
    // struct drm_mode_obj_set_property: value@0(u64), prop_id@8, obj_id@12,
    // obj_type@16.
    let value = unsafe { (arg as *const u64).read_unaligned() };
    let prop_id = unsafe { ((arg + 8) as *const u32).read_unaligned() };
    let obj_id = unsafe { ((arg + 12) as *const u32).read_unaligned() };
    let obj_type = unsafe { ((arg + 16) as *const u32).read_unaligned() };
    let matches_type = matches!(
        (obj_type, obj_id),
        (OBJECT_CONNECTOR, CONNECTOR_ID) | (OBJECT_CRTC, CRTC_ID) | (OBJECT_PLANE, PLANE_ID)
    );
    if !matches_type {
        return libc_EINVAL();
    }
    let shown = prop_id == PROP_PLANE_FB_ID;
    let rc = critical_section::with(|cs| {
        let mut st = STATE.borrow(cs).borrow_mut();
        if shown {
            // The plane's buffer is the one property whose write changes what
            // is on the screen, so it is validated as a buffer before the
            // state that names it moves.
            let fb = value as u32;
            if fb != 0 && !fb_fits(&st, fb) {
                return libc_EINVAL();
            }
            match copy_to_scanout(&st, fb) {
                0 => st.current_fb = fb,
                e => return e,
            }
        }
        set_object_prop(&mut st, obj_id, prop_id, value)
    });
    if rc == 0 && shown {
        crate::drivers::video::render::request_flush_blocking();
    }
    rc
}

/// `MODE_ATOMIC`: one frame described as a list of property writes, applied
/// together.
///
/// This is the request weston 16's DRM backend builds every frame out of, and
/// the reason its legacy path no longer exists in that compositor: instead of
/// "set the CRTC, then flip" - two calls whose middle is a visible state
/// nobody asked for - the caller names every object, property and value it
/// wants, and the card either takes all of them or none.
///
/// The shape of the request is three parallel arrays walked in step: one
/// entry per object in `objs`, the number of properties that object carries
/// in `counts`, and then one `props`/`values` pair per property, in order.
/// `TEST_ONLY` asks the card to rehearse the commit - "would this be
/// accepted?" - which is how a compositor proposes a state before it commits
/// one; it changes nothing, not even the flip sequence. A commit that asks
/// for `PAGE_FLIP_EVENT` completes with an event on the card fd, which is
/// what paces the compositor's next frame.
fn mode_atomic(arg: usize) -> isize {
    // struct drm_mode_atomic: flags@0, count_objs@4, objs_ptr@8,
    // count_props_ptr@16, props_ptr@24, prop_values_ptr@32, reserved@40,
    // user_data@48.
    let flags = unsafe { (arg as *const u32).read_unaligned() };
    let count_objs = unsafe { ((arg + 4) as *const u32).read_unaligned() } as usize;
    let objs = unsafe { ((arg + 8) as *const u64).read_unaligned() } as *const u32;
    let counts = unsafe { ((arg + 16) as *const u64).read_unaligned() } as *const u32;
    let props = unsafe { ((arg + 24) as *const u64).read_unaligned() } as *const u32;
    let values = unsafe { ((arg + 32) as *const u64).read_unaligned() } as *const u64;
    const KNOWN_FLAGS: u32 =
        PAGE_FLIP_EVENT | 0x2 | ATOMIC_TEST_ONLY | ATOMIC_NONBLOCK | ATOMIC_ALLOW_MODESET;
    // An unknown flag is a request this card cannot honour; a rehearsal that
    // also asks for a flip event asks for two different things at once.
    if flags & !KNOWN_FLAGS != 0
        || flags & (ATOMIC_TEST_ONLY | PAGE_FLIP_EVENT) == (ATOMIC_TEST_ONLY | PAGE_FLIP_EVENT)
    {
        return libc_EINVAL();
    }
    if count_objs == 0 || objs.is_null() || counts.is_null() || props.is_null() || values.is_null()
    {
        return libc_EINVAL();
    }
    // The two per-object arrays are read `count_objs` times each, so they are
    // checked for that many elements before the first read: this request is
    // the one a compositor makes sixty times a second with a plan built out of
    // the caller's own memory, and a stale `objs_ptr` from a freed commit read
    // from kernel mode is a page fault with nothing left to report.
    if !user_array(objs as usize, count_objs, 4, false)
        || !user_array(counts as usize, count_objs, 4, false)
    {
        return -14; // EFAULT
    }
    // The property lists are sized by the sum of the counts, which is only
    // known after reading the array that was just validated. A total past what
    // any real commit carries is refused outright rather than walked: the loop
    // below would read that many entries from the two property arrays, and a
    // count field that big is a corrupted struct rather than a request.
    let total = {
        let mut sum = 0usize;
        for i in 0..count_objs {
            sum += unsafe { counts.add(i).read_unaligned() } as usize;
            if sum > MAX_ATOMIC_PROPS {
                return libc_EINVAL();
            }
        }
        sum
    };
    if !user_array(props as usize, total, 4, false) || !user_array(values as usize, total, 8, false)
    {
        return -14; // EFAULT
    }

    // Walk the request into a plan first: a commit that is refused halfway
    // must not have applied half of itself, which is the property the whole
    // request exists to provide.
    let mut plan = Commit::default();
    let mut index = 0usize;
    for i in 0..count_objs {
        let obj = unsafe { objs.add(i).read_unaligned() };
        let n = unsafe { counts.add(i).read_unaligned() } as usize;
        for _ in 0..n {
            let prop = unsafe { props.add(index).read_unaligned() };
            let value = unsafe { values.add(index).read_unaligned() };
            index += 1;
            if let Err(e) = plan.record(obj, prop, value) {
                return e;
            }
        }
    }

    let rc = critical_section::with(|cs| {
        let mut st = STATE.borrow(cs).borrow_mut();
        // The refusals, before anything moves: a mode this card cannot show,
        // and a buffer it cannot scan out.
        if let Some(blob) = plan.mode_blob
            && blob != 0
            && !mode_blob_fits(&st, blob)
        {
            return libc_EINVAL();
        }
        if let Some(fb) = plan.plane_fb
            && fb != 0
            && !fb_fits(&st, fb)
        {
            return libc_EINVAL();
        }
        if flags & ATOMIC_TEST_ONLY != 0 {
            // A rehearsal: the answer is the one the checks above gave.
            return 0;
        }

        // Apply. The plane's buffer is the one entry that moves pixels; the
        // rest is the state a compositor reads back, recorded exactly as the
        // single-property ioctls record it.
        for (obj, prop, value) in plan.entries() {
            if obj == PLANE_ID && prop == PROP_PLANE_FB_ID {
                let fb = value as u32;
                if fb != 0
                    && let e @ 1.. = copy_to_scanout(&st, fb)
                {
                    return e;
                }
                st.current_fb = fb;
                continue;
            }
            let rc = set_object_prop(&mut st, obj, prop, value);
            if rc != 0 {
                return rc;
            }
        }
        // A commit that binds a mode also makes the CRTC active, the way the
        // kernel's own modeset path does: a mode bound to an inactive CRTC is
        // a state no compositor means to ask for.
        if plan.mode_blob.is_some_and(|b| b != 0) {
            st.crtc_active = true;
        }
        if flags & PAGE_FLIP_EVENT != 0 {
            st.seq = st.seq.wrapping_add(1);
            let seq = st.seq;
            // `CLOCK_MONOTONIC`, as `DRM_CAP_TIMESTAMP_MONOTONIC` promises and
            // as `clock_gettime` answers it: the compositor schedules its next
            // frame against this number.
            let now = crate::arch::timer::since_boot_nanos();
            // The commit's own `user_data` rides back in the event, which is
            // how a compositor that commits for several devices tells them
            // apart - libdrm hands it to the flip handler as its `data`.
            let user_data = unsafe { ((arg + 48) as *const u64).read_unaligned() };
            st.events.push_back(drm_event::Vblank {
                type_: EVENT_FLIP_COMPLETE,
                length: 32,
                user_data,
                time_sec: (now / 1_000_000_000) as u32,
                time_usec: ((now % 1_000_000_000) / 1000) as u32,
                sequence: seq,
                crtc: CRTC_ID,
            });
        }
        0
    });
    // Present: the scanout is flushed once the commit has landed.
    if rc == 0 {
        crate::drivers::video::render::request_flush_blocking();
    }
    rc
}

/// One `MODE_ATOMIC` request, parsed. The fields are the properties a commit
/// can carry, each `None` when the caller did not name it - which is what
/// makes "leave this alone" distinguishable from "set it to zero".
#[derive(Default)]
struct Commit {
    /// The entries in the order the caller wrote them, so applying them is
    /// the same walk the request described.
    order: Vec<(u32, u32, u64)>,
    mode_blob: Option<u32>,
    plane_fb: Option<u32>,
}

impl Commit {
    /// Record one property write, refusing an object or property this card
    /// does not have.
    fn record(&mut self, obj: u32, prop: u32, value: u64) -> Result<(), isize> {
        // Every object the card has, by the id the commit names it with: the
        // request carries ids and not types, and these four are distinct.
        let known = matches!(
            (obj, prop),
            (CONNECTOR_ID, PROP_CRTC_ID)
                | (CONNECTOR_ID, PROP_DPMS)
                | (CRTC_ID, PROP_ACTIVE)
                | (CRTC_ID, PROP_MODE_ID)
                | (PLANE_ID, PROP_PLANE_FB_ID)
                | (PLANE_ID, PROP_PLANE_CRTC_ID)
                | (PLANE_ID, PROP_BLEND)
                | (PLANE_ID, PROP_IN_FENCE_FD)
        ) || (obj == PLANE_ID && (PROP_SRC_X..=PROP_CRTC_H).contains(&prop));
        if !known {
            return Err(libc_EINVAL());
        }
        if obj == CRTC_ID && prop == PROP_MODE_ID {
            self.mode_blob = Some(value as u32);
        }
        if obj == PLANE_ID && prop == PROP_PLANE_FB_ID {
            self.plane_fb = Some(value as u32);
        }
        self.order.push((obj, prop, value));
        Ok(())
    }

    /// The writes to apply, in the caller's order.
    fn entries(&self) -> impl Iterator<Item = (u32, u32, u64)> + '_ {
        self.order.iter().copied()
    }
}

/// How many property writes one atomic commit may carry. Weston's whole
/// screen - two planes, a connector and a CRTC, each with a handful of
/// properties - is a few dozen; the cap exists to turn an absurd count into a
/// refusal instead of a walk over the caller's memory.
const MAX_ATOMIC_PROPS: usize = 512;

/// Whether the blob `id` holds a `drmModeModeInfo` this card can show. The
/// mode comes from the connector's own list, so a commit that binds one of
/// those is the only commit that fits; a mode for another panel would need a
/// different scanout, and there is one.
fn mode_blob_fits(st: &DrmState, id: u32) -> bool {
    let Some(bytes) = st.blobs.get(id as usize - 1).and_then(|b| b.as_ref()) else {
        return false;
    };
    // struct drm_mode_modeinfo: clock@0, hdisplay@4, hsync_start@6,
    // hsync_end@8, htotal@10, hskew@12, vdisplay@14, ...
    if bytes.len() < 68 {
        return false;
    }
    let hdisplay = u16::from_le_bytes([bytes[4], bytes[5]]);
    let vdisplay = u16::from_le_bytes([bytes[14], bytes[15]]);
    let (w, h) = crtc_mode_dims();
    hdisplay as u32 == w && vdisplay as u32 == h
}

/// `CREATE_BLOB`: store a property's bytes and hand back an id. A mode is one
/// of these, and so is a damage-rectangle list; the card keeps them until
/// `DESTROY_BLOB` or the last opener goes.
fn create_blob(arg: usize) -> isize {
    // struct drm_mode_create_blob { u64 data@0; u32 length@8; u32 blob_id@12; }
    let data = unsafe { (arg as *const u64).read_unaligned() } as *const u8;
    let length = unsafe { ((arg + 8) as *const u32).read_unaligned() } as usize;
    if length == 0 || length > MAX_BLOB || data.is_null() {
        return libc_EINVAL();
    }
    // The bytes are the caller's, and this is a read of them: a length the
    // card will copy and an address that is not mapped are the two ways this
    // request can take the machine down from kernel mode, and only the second
    // one is fatal - so it is asked here, before the copy.
    if !user_array(data as usize, length, 1, false) {
        return -14; // EFAULT
    }
    let bytes = unsafe { core::slice::from_raw_parts(data, length) }.to_vec();
    let id = critical_section::with(|cs| {
        let mut st = STATE.borrow(cs).borrow_mut();
        // A freed slot is reused before the table grows, the same
        // lowest-first rule every id space here follows.
        match st.blobs.iter().position(|b| b.is_none()) {
            Some(slot) => {
                st.blobs[slot] = Some(bytes);
                slot + 1
            }
            None => {
                st.blobs.push(Some(bytes));
                st.blobs.len()
            }
        }
    });
    unsafe { ((arg + 12) as *mut u32).write_unaligned(id as u32) };
    0
}

/// `GETPROPBLOB`: the bytes a blob holds, in the two-call shape every KMS
/// list uses - the length is always written, the data only when the caller
/// has room for it.
fn get_propblob(arg: usize) -> isize {
    // struct drm_mode_get_blob { u32 blob_id@0; u32 length@4; u64 data@8; }
    let id = unsafe { (arg as *const u32).read_unaligned() };
    let want = unsafe { ((arg + 4) as *const u32).read_unaligned() };
    let data = unsafe { ((arg + 8) as *const u64).read_unaligned() } as *mut u8;
    let Some(bytes) = critical_section::with(|cs| {
        let st = STATE.borrow(cs).borrow();
        st.blobs
            .get(id.wrapping_sub(1) as usize)
            .and_then(|b| b.as_ref())
            .cloned()
    }) else {
        return libc_EINVAL();
    };
    unsafe {
        if want as usize >= bytes.len()
            && !data.is_null()
            && user_array(data as usize, bytes.len(), 1, true)
        {
            core::ptr::copy_nonoverlapping(bytes.as_ptr(), data, bytes.len());
        }
        ((arg + 4) as *mut u32).write_unaligned(bytes.len() as u32);
    }
    0
}

/// `DESTROY_BLOB`: drop a blob's bytes. A blob id that names nothing is the
/// `EINVAL` Linux answers.
fn destroy_blob(arg: usize) -> isize {
    // struct drm_mode_destroy_blob { u32 blob_id@0; }
    let id = unsafe { (arg as *const u32).read_unaligned() };
    critical_section::with(|cs| {
        let mut st = STATE.borrow(cs).borrow_mut();
        match st.blobs.get_mut(id.wrapping_sub(1) as usize) {
            Some(slot @ Some(_)) => {
                *slot = None;
                0
            }
            _ => libc_EINVAL(),
        }
    })
}

/// `OBJ_GETPROPERTIES`: the property ids and values one KMS object carries -
/// the two-call list shape again, over the same facts `GETCONNECTOR`
/// reports for the connector.
fn obj_get_properties(arg: usize) -> isize {
    // struct drm_mode_obj_get_properties: props_ptr@0, prop_values_ptr@8,
    // count_props@16, obj_id@20, obj_type@24.
    let obj_id = unsafe { ((arg + 20) as *const u32).read_unaligned() };
    let obj_type = unsafe { ((arg + 24) as *const u32).read_unaligned() };
    // (ids, values, count) - the arrays are fixed-size so the lock only
    // guards a few word reads. Sixteen slots is the longest list any object
    // here carries, and the count is what says how much of it is real.
    let Some((ids, values, count)) = critical_section::with(|cs| {
        let st = STATE.borrow(cs).borrow();
        match (obj_type, obj_id) {
            (OBJECT_CONNECTOR, CONNECTOR_ID) => Some((
                [
                    PROP_CRTC_ID,
                    PROP_DPMS,
                    0,
                    0,
                    0,
                    0,
                    0,
                    0,
                    0,
                    0,
                    0,
                    0,
                    0,
                    0,
                    0,
                    0,
                ],
                [
                    st.conn_crtc as u64,
                    st.dpms as u64,
                    0,
                    0,
                    0,
                    0,
                    0,
                    0,
                    0,
                    0,
                    0,
                    0,
                    0,
                    0,
                    0,
                    0,
                ],
                2usize,
            )),
            (OBJECT_CRTC, CRTC_ID) => Some((
                [
                    PROP_ACTIVE,
                    PROP_MODE_ID,
                    0,
                    0,
                    0,
                    0,
                    0,
                    0,
                    0,
                    0,
                    0,
                    0,
                    0,
                    0,
                    0,
                    0,
                ],
                [
                    st.crtc_active as u64,
                    st.crtc_mode_id as u64,
                    0,
                    0,
                    0,
                    0,
                    0,
                    0,
                    0,
                    0,
                    0,
                    0,
                    0,
                    0,
                    0,
                    0,
                ],
                2usize,
            )),
            // The plane's type is what weston picks its primary plane by; the
            // rest is the frame it is showing, which is the state a
            // compositor inherits at startup.
            (OBJECT_PLANE, PLANE_ID) => {
                let (dx, dy, dw, dh) = st.plane_dst;
                let (sx, sy, sw, sh) = st.plane_src;
                Some((
                    [
                        PROP_TYPE,
                        PROP_PLANE_FB_ID,
                        PROP_PLANE_CRTC_ID,
                        PROP_SRC_X,
                        PROP_SRC_Y,
                        PROP_SRC_W,
                        PROP_SRC_H,
                        PROP_CRTC_X,
                        PROP_CRTC_Y,
                        PROP_CRTC_W,
                        PROP_CRTC_H,
                        PROP_BLEND,
                        PROP_IN_FENCE_FD,
                        0,
                        0,
                        0,
                    ],
                    [
                        PLANE_TYPE_PRIMARY,
                        st.current_fb as u64,
                        st.plane_crtc as u64,
                        sx as u64,
                        sy as u64,
                        sw as u64,
                        sh as u64,
                        dx as u64,
                        dy as u64,
                        dw as u64,
                        dh as u64,
                        BLEND_PREMULT,
                        -1i64 as u64,
                        0,
                        0,
                        0,
                    ],
                    13usize,
                ))
            }
            _ => None,
        }
    }) else {
        return libc_EINVAL();
    };
    unsafe {
        let want = ((arg + 16) as *const u32).read_unaligned();
        if want >= count as u32 {
            let ids_ptr = (arg as *const u64).read_unaligned() as *mut u32;
            let values_ptr = ((arg + 8) as *const u64).read_unaligned() as *mut u64;
            if !ids_ptr.is_null() && user_array(ids_ptr as usize, count, 4, true) {
                for (i, id) in ids.iter().enumerate().take(count) {
                    ids_ptr.add(i).write_unaligned(*id);
                }
            }
            if !values_ptr.is_null() && user_array(values_ptr as usize, count, 8, true) {
                for (i, value) in values.iter().enumerate().take(count) {
                    values_ptr.add(i).write_unaligned(*value);
                }
            }
        }
        ((arg + 16) as *mut u32).write_unaligned(count as u32);
    }
    0
}

/// Bind a dumb buffer to a fresh framebuffer id. The handle must be one the
/// card made or imported, and the geometry must be a shape the scanout can
/// show: 32bpp, nonzero, with a pitch wide enough for a row. `ADDFB` and
/// `ADDFB2` differ only in how they spell those facts.
fn insert_fb(
    st: &mut DrmState,
    handle: u32,
    width: u32,
    height: u32,
    pitch: u32,
    bpp: u32,
    depth: u32,
) -> Option<u32> {
    let slot = handle as usize;
    // The handle's buffer must exist - and the geometry it comes with has to
    // be a shape the scanout can show: the handle's own bpp (the scanout is
    // a 32-bit BGRA surface, whatever the ADDFB claims), nonzero, with a
    // pitch wide enough for a row.
    let dumb = st.dumb.get(slot.checked_sub(1)?).and_then(|d| d.as_ref())?;
    if dumb.bpp != 32 || bpp != 32 || width == 0 || height == 0 || pitch < width * 4 {
        return None;
    }
    let id = st.fbs.len() as u32 + 100;
    st.fbs.push(Some(Framebuffer {
        id,
        handle,
        width,
        height,
        pitch,
        bpp,
        depth,
    }));
    Some(id)
}

fn add_fb(arg: usize) -> isize {
    // struct drm_mode_fb_cmd: fb_id@0,width@4,height@8,pitch@12,bpp@16,
    // depth@20,handle@24.
    let width = unsafe { ((arg + 4) as *const u32).read_unaligned() };
    let height = unsafe { ((arg + 8) as *const u32).read_unaligned() };
    let pitch = unsafe { ((arg + 12) as *const u32).read_unaligned() };
    let bpp = unsafe { ((arg + 16) as *const u32).read_unaligned() };
    let depth = unsafe { ((arg + 20) as *const u32).read_unaligned() };
    let handle = unsafe { ((arg + 24) as *const u32).read_unaligned() };
    let id = critical_section::with(|cs| {
        insert_fb(
            &mut STATE.borrow(cs).borrow_mut(),
            handle,
            width,
            height,
            pitch,
            bpp,
            depth,
        )
    });
    match id {
        Some(id) => {
            unsafe { (arg as *mut u32).write_unaligned(id) };
            0
        }
        None => libc_EINVAL(),
    }
}

fn add_fb2(arg: usize) -> isize {
    // struct drm_mode_fb_cmd2: fb_id@0,width@4,height@8,pixel_format@12,
    // flags@16, handles[4]@20, pitches[4]@36, offsets[4]@52, modifier[4]@68.
    // The fourcc names the layout; every 32-bit spelling this scanout can
    // show lands on the ADDFB path, and the rest is refused. A nonzero
    // plane offset or format modifier is too: there is exactly one linear
    // plane, and pretending otherwise would lie about what gets scanned.
    let width = unsafe { ((arg + 4) as *const u32).read_unaligned() };
    let height = unsafe { ((arg + 8) as *const u32).read_unaligned() };
    let format = unsafe { ((arg + 12) as *const u32).read_unaligned() };
    let handle = unsafe { ((arg + 20) as *const u32).read_unaligned() };
    let pitch = unsafe { ((arg + 36) as *const u32).read_unaligned() };
    let offset = unsafe { ((arg + 52) as *const u32).read_unaligned() };
    let modifier = unsafe { ((arg + 68) as *const u64).read_unaligned() };
    if offset != 0 || modifier != 0 {
        return libc_EINVAL();
    }
    let (bpp, depth) = match format {
        DRM_FORMAT_XRGB8888 | DRM_FORMAT_XBGR8888 => (32, 24),
        DRM_FORMAT_ARGB8888 | DRM_FORMAT_ABGR8888 => (32, 32),
        _ => return libc_EINVAL(),
    };
    let id = critical_section::with(|cs| {
        insert_fb(
            &mut STATE.borrow(cs).borrow_mut(),
            handle,
            width,
            height,
            pitch,
            bpp,
            depth,
        )
    });
    match id {
        Some(id) => {
            unsafe { (arg as *mut u32).write_unaligned(id) };
            0
        }
        None => libc_EINVAL(),
    }
}

/// `GETFB`, in the direction ADDFB came from: the framebuffer's geometry
/// and the handle behind it.
fn get_fb(arg: usize) -> isize {
    // struct drm_mode_fb_cmd again: fb_id@0 selects, the rest comes back.
    let id = unsafe { (arg as *const u32).read_unaligned() };
    critical_section::with(|cs| {
        let st = STATE.borrow(cs).borrow();
        let Some(fb) = st
            .fbs
            .iter()
            .filter_map(|f| f.as_ref())
            .find(|f| f.id == id)
        else {
            return libc_EINVAL();
        };
        unsafe {
            ((arg + 4) as *mut u32).write_unaligned(fb.width);
            ((arg + 8) as *mut u32).write_unaligned(fb.height);
            ((arg + 12) as *mut u32).write_unaligned(fb.pitch);
            ((arg + 16) as *mut u32).write_unaligned(fb.bpp);
            ((arg + 20) as *mut u32).write_unaligned(fb.depth);
            ((arg + 24) as *mut u32).write_unaligned(fb.handle);
        }
        0
    })
}

/// Release a GEM handle - the one path `GEM_CLOSE` and `DESTROY_DUMB` both
/// spell. The dumb buffer gives its frames back, and the PRIME records that
/// name the handle go too: the *dma-bufs* the handle exported keep their own
/// references and their own lifetime, but a later import must not be
/// answered with a handle that no longer exists. A handle already released
/// is the refusal Linux makes too.
fn destroy_handle(handle: usize) -> isize {
    if handle == 0 {
        return libc_EINVAL();
    }
    critical_section::with(|cs| {
        let mut st = STATE.borrow(cs).borrow_mut();
        st.prime.retain(|record| record.handle as usize != handle);
        match st.dumb.get_mut(handle - 1) {
            Some(slot) if slot.is_some() => {
                if let Some(d) = slot.as_mut() {
                    d.release();
                }
                *slot = None;
                0
            }
            _ => libc_EINVAL(),
        }
    })
}

/// `PRIME_HANDLE_TO_FD`: export a handle as a dma-buf descriptor - the door
/// GBM and the GL stack walk buffers through. Every page of the buffer
/// becomes real first, so a mapping of the descriptor past what the handle
/// had grown still aliases the handle's own pages; then the descriptor is
/// the dma-buf over those pages with one reference of its own per page, and
/// the identity record lets a second export of the same handle answer the
/// same object.
fn prime_handle_to_fd(arg: usize) -> isize {
    // struct drm_prime_handle: handle@0, flags@4, fd@8.
    let handle = unsafe { (arg as *const u32).read_unaligned() } as usize;
    let flags = unsafe { ((arg + 4) as *const u32).read_unaligned() };
    let buf = critical_section::with(|cs| {
        let mut st = STATE.borrow(cs).borrow_mut();
        let st = &mut *st;
        // A handle out of range, or one the card never made, answers nothing.
        if handle == 0 || handle > MAX_HANDLES {
            return None;
        }
        let dumb = st.dumb.get_mut(handle - 1).and_then(|slot| slot.as_mut())?;
        // A handle already exported: the same object again, so both
        // descriptors map the same pages.
        if let Some(buf) = st
            .prime
            .iter()
            .find(|record| record.handle as usize == handle)
            .and_then(|record| record.buf.clone())
        {
            return Some(buf);
        }
        // Grow the whole buffer, then clone one reference per page for the
        // dma-buf; its `Drop` gives these back when the last descriptor
        // closes, so the buffer dies exactly then and never while a
        // mapping or the exporting handle still needs it.
        let pages = (dumb.size() as usize).div_ceil(PAGE_SIZE);
        for idx in 0..pages {
            if dumb.ensure_page(idx) == 0 {
                return None; // the cap or the frame allocator said no
            }
        }
        let frames = dumb.frames.clone();
        for &frame in &frames {
            clone_frame_ref(frame);
        }
        let buf = Arc::new(crate::vfs::dmabuf::DmaBuf::new(
            dumb.width,
            dumb.height,
            dumb.pitch,
            frames,
        ));
        st.prime.push(PrimeRecord {
            ino: buf.ino(),
            handle: handle as u32,
            buf: Some(buf.clone()),
        });
        Some(buf)
    });
    let Some(buf) = buf else {
        return libc_EINVAL();
    };
    // The descriptor, in the caller's own table, under the caller's own
    // descriptor ceiling.
    let file = Arc::new(crate::vfs::File::new(buf));
    let fd = match crate::sched::current_fds().install(file) {
        Ok(fd) => fd,
        Err(crate::vfs::VfsError::TooManyDescriptors) => return -24, // EMFILE
        Err(_) => return libc_EINVAL(),
    };
    if flags & DRM_CLOEXEC != 0 {
        let _ = crate::sched::current_fds().fd_flags_set(fd, crate::vfs::file::FD_CLOEXEC);
    }
    unsafe { ((arg + 8) as *mut i32).write_unaligned(fd) };
    0
}

/// `PRIME_FD_TO_HANDLE`: import a dma-buf descriptor as a handle - the
/// compositor's half of the GBM path. A descriptor this card exported
/// answers the handle it came from; any other dma-buf gets a fresh dumb
/// buffer over its pages, each page with one reference the handle owns (the
/// descriptor keeps its own), so closing either side leaves the other
/// standing.
fn prime_fd_to_handle(arg: usize) -> isize {
    // struct drm_prime_handle, the other way: fd@8 names, handle@0 receives.
    let fd = unsafe { ((arg + 8) as *const i32).read_unaligned() };
    let file = match crate::sched::current_fds().clone_at(fd) {
        Ok(f) => f,
        Err(_) => return -9, // EBADF
    };
    // The inode is bound first: `as_dmabuf` borrows from it, and the borrow
    // must outlive the statement that asks for it.
    let inode = file.inode();
    let Some(buf) = inode.as_dmabuf() else {
        return libc_EINVAL(); // not a dma-buf descriptor
    };
    let handle = critical_section::with(|cs| {
        let mut st = STATE.borrow(cs).borrow_mut();
        let st = &mut *st;
        // The same object the card already knows: its handle, so repeated
        // imports never stack up aliases of one buffer.
        if let Some(record) = st.prime.iter().find(|record| record.ino == buf.ino()) {
            return record.handle;
        }
        if st.dumb.len() >= MAX_HANDLES {
            return 0;
        }
        let frames = buf.clone_frames();
        st.dumb.push(Some(Dumb {
            width: buf.width(),
            height: buf.height(),
            pitch: buf.pitch(),
            bpp: 32,
            frames,
        }));
        let handle = st.dumb.len() as u32;
        st.prime.push(PrimeRecord {
            ino: buf.ino(),
            handle,
            buf: None,
        });
        handle
    });
    if handle == 0 {
        return libc_EINVAL(); // the handle table is full
    }
    unsafe { (arg as *mut u32).write_unaligned(handle) };
    0
}

fn page_flip(arg: usize) -> isize {
    // struct drm_mode_crtc_page_flip: crtc_id@0, fb_id@4, flags@8, reserved@12,
    // user_data@16.
    let crtc_id = unsafe { (arg as *const u32).read_unaligned() };
    let fb_id = unsafe { ((arg + 4) as *const u32).read_unaligned() };
    if crtc_id != CRTC_ID {
        return libc_EINVAL();
    }
    let rc = critical_section::with(|cs| {
        let mut st = STATE.borrow(cs).borrow_mut();
        let rc = copy_to_scanout(&st, fb_id);
        if rc == 0 {
            st.current_fb = fb_id;
            st.seq = st.seq.wrapping_add(1);
            let seq = st.seq;
            // DRM_MODE_PAGE_FLIP_EVENT (0x1) asks for the completion event.
            let flags = unsafe { ((arg + 8) as *const u32).read_unaligned() };
            let user_data = unsafe { ((arg + 16) as *const u64).read_unaligned() };
            if flags & 0x1 != 0 {
                // The timestamp is `CLOCK_MONOTONIC` - the clock
                // `DRM_CAP_TIMESTAMP_MONOTONIC` promises, and the one the
                // compositor schedules its next frame against. Reading the
                // RTC anchor here instead would put an epoch-based number
                // where weston expects seconds since boot.
                let now = crate::arch::timer::since_boot_nanos();
                st.events.push_back(drm_event::Vblank {
                    type_: EVENT_FLIP_COMPLETE,
                    length: 32,
                    user_data,
                    time_sec: (now / 1_000_000_000) as u32,
                    time_usec: ((now % 1_000_000_000) / 1000) as u32,
                    sequence: seq,
                    crtc: CRTC_ID,
                });
            }
        }
        rc
    });
    if rc == 0 {
        crate::drivers::video::render::request_flush_blocking();
    }
    rc
}

fn create_dumb(arg: usize) -> isize {
    // struct drm_mode_create_dumb: height@0,width@4,bpp@8,flags@12,
    // handle@16,pitch@20,size@24(u64).
    let height = unsafe { (arg as *const u32).read_unaligned() };
    let width = unsafe { ((arg + 4) as *const u32).read_unaligned() };
    let bpp = unsafe { ((arg + 8) as *const u32).read_unaligned() };
    if bpp == 0 || width == 0 || height == 0 {
        return libc_EINVAL();
    }
    let handle = critical_section::with(|cs| {
        let mut st = STATE.borrow(cs).borrow_mut();
        let idx = st.dumb.len();
        if idx >= MAX_HANDLES {
            return 0usize;
        }
        st.dumb.push(Some(Dumb::new(width, height, bpp)));
        idx + 1
    });
    if handle == 0 {
        return -28; // ENOSPC
    }
    critical_section::with(|cs| {
        let st = STATE.borrow(cs).borrow();
        let d = st.dumb[handle - 1].as_ref().unwrap();
        unsafe {
            ((arg + 16) as *mut u32).write_unaligned(handle as u32);
            ((arg + 20) as *mut u32).write_unaligned(d.pitch);
            ((arg + 24) as *mut u64).write_unaligned(d.size());
        }
    });
    0
}

fn map_dumb(arg: usize) -> isize {
    // struct drm_mode_map_dumb: handle@0, pad@4, offset@8(u64).
    let handle = unsafe { (arg as *const u32).read_unaligned() } as usize;
    if handle == 0 {
        return libc_EINVAL();
    }
    // Encode the buffer's slot into the mmap offset so `mmap_frame` can find
    // it from the absolute page index the kernel's mmap hands it.
    let offset = (handle as u64) * (PAGES_PER_BUFFER as u64) * (PAGE_SIZE as u64);
    unsafe { ((arg + 8) as *mut u64).write_unaligned(offset) };
    0
}

/// The `/dev/dri/card0` node. Bytes read out of it are DRM events (flip
/// completions); the buffer memory itself is reached by `mmap`, not `read`.
pub struct DrmCard {
    ino: u64,
    /// The DRM minor this name is: 0 for `card0`, 128 for `renderD128`. DRM's
    /// major is 226 by convention (fixed since XFree86), and the render node's
    /// minor is the card index plus 128.
    minor: u32,
}

impl DrmCard {
    pub fn new(ino: u64, minor: u32) -> Arc<DrmCard> {
        Arc::new(DrmCard { ino, minor })
    }

    /// Whether a `read` would return an event now.
    fn has_event() -> bool {
        critical_section::with(|cs| !STATE.borrow(cs).borrow().events.is_empty())
    }
}

impl Inode for DrmCard {
    fn metadata(&self) -> Metadata {
        Metadata {
            ino: self.ino,
            kind: NodeKind::CharDev,
            size: 0,
        }
    }

    /// The device number `stat` reports: DRM's major 226 and this name's
    /// minor. Mesa's loader identifies a DRM device through exactly this
    /// pair - it looks the number up under `/sys/dev/char/226:0` to find
    /// which driver owns the fd - so a card that reports `0,0` is a card
    /// whose driver cannot be named ("MESA-LOADER: failed to retrieve device
    /// information", which is what the log said before this existed).
    fn rdev(&self) -> Option<(u32, u32)> {
        Some((226, self.minor))
    }

    /// Serve the next queued DRM event, as `drmGetEventsReady`/a reader of the
    /// card fd expects: a `drm_event` header then the vblank payload. A
    /// blocking read is left to the caller's poll loop (this node's readiness
    /// is polled at tick granularity like other event nodes).
    fn read_at(&self, _offset: u64, buf: &mut [u8]) -> VfsResult<usize> {
        let ev = critical_section::with(|cs| STATE.borrow(cs).borrow_mut().events.pop_front());
        match ev {
            Some(ev) => {
                let bytes: &[u8] = unsafe {
                    core::slice::from_raw_parts(
                        &ev as *const _ as *const u8,
                        core::mem::size_of::<drm_event::Vblank>(),
                    )
                };
                let n = bytes.len().min(buf.len());
                buf[..n].copy_from_slice(&bytes[..n]);
                Ok(n)
            }
            None => Ok(0),
        }
    }

    fn write_at(&self, _offset: u64, _buf: &[u8]) -> VfsResult<usize> {
        // Writes to the card are ioctls' job; the file face is read-only.
        Ok(0)
    }

    fn poll_ready(&self) -> Ready {
        Ready {
            read: Self::has_event(),
            write: true,
        }
    }

    /// Map a dumb buffer's page. The buffer is chosen from the absolute page
    /// index (which carries the `MAP_DUMB` offset), and the caller is given
    /// its own reference on the frame so its later teardown cannot free a
    /// buffer another process still maps - the same discipline as the memfd.
    fn mmap_frame(&self, index: u64) -> Option<usize> {
        let index = index as usize;
        let handle = index / PAGES_PER_BUFFER;
        let local = index % PAGES_PER_BUFFER;
        if handle == 0 {
            return None;
        }
        let frame = critical_section::with(|cs| {
            let mut st = STATE.borrow(cs).borrow_mut();
            match st.dumb.get_mut(handle - 1).and_then(|slot| slot.as_mut()) {
                Some(d) => d.ensure_page(local),
                None => 0,
            }
        });
        if frame == 0 {
            return None;
        }
        clone_frame_ref(frame);
        Some(frame)
    }

    fn as_drm(&self) -> Option<&DrmCard> {
        Some(self)
    }
}
