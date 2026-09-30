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
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use critical_section::Mutex;

use crate::mm::{PAGE_SIZE, clone_frame_ref, free_frames, phys_to_virt};
use crate::vfs::{Inode, Metadata, NodeKind, Ready, VfsResult};

/// Fixed KMS object ids - one of each, never changing. libdrm only cares that
/// `GETRESOURCES` and the `GET*` ioctls agree on them.
const CRTC_ID: u32 = 34;
const ENCODER_ID: u32 = 33;
const CONNECTOR_ID: u32 = 35;
const PLANE_ID: u32 = 36;
/// DRM's major number - `DRM_MAJOR` in the kernel's own headers, and the number
/// every `/sys/class/drm/cardN/dev` prints and every uevent about a card
/// carries. Fixed since XFree86 4.x.
const DRM_MAJOR: u32 = 226;
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
/// The properties §7.4 lists as "what a compositor finds by name", of which
/// this card has the four its hardware can back and no more.
///
/// The set is a *measurement* before it is a list. `modetest -c -p` against the
/// real card on the development host reports which of these families a driver
/// creates, and the answer is not "all of them": `subconnector` is on the
/// connectors that have one, `panel orientation` and `FB_DAMAGE_CLIPS` and
/// `OUT_FENCE_PTR` are absent, and `GAMMA_LUT`/`VRR_ENABLED` are there because
/// amdgpu has a gamma LUT and a VRR engine to honour them with. This card has
/// neither, and a property whose write changes nothing is the §4.2 lie - so the
/// four below are what is left, each of them true:
///
///   * `EDID` - the bytes of a display this kernel synthesizes.
///   * `non-desktop` - an *immutable range* with the values 0 and 1, not the
///     bool the sources suggest; measured on a real connector.
///   * `max bpc` - a range, and here a range of exactly one value, because the
///     scanout is eight bits per channel and no property can raise it.
///   * `vrr_capable` - an immutable range reporting 0: this display cannot
///     vary its refresh rate, and saying so is what stops a compositor asking.
const PROP_EDID: u32 = 87;
const PROP_NON_DESKTOP: u32 = 88;
const PROP_MAX_BPC: u32 = 89;
const PROP_VRR_CAPABLE: u32 = 90;
const PROP_IN_FORMATS: u32 = 94;
/// The connector's property list, in one place: `GETCONNECTOR` and
/// `OBJ_GETPROPERTIES` both report it, and a client that read it from one and
/// not the other would see two different connectors.
const CONNECTOR_PROPS: [u32; 6] = [
    PROP_CRTC_ID,
    PROP_DPMS,
    PROP_EDID,
    PROP_NON_DESKTOP,
    PROP_MAX_BPC,
    PROP_VRR_CAPABLE,
];
/// What `max bpc` reports: the scanout is BGRA32 and nothing here can change
/// it, so the range has one value and a caller asking for 10 or 12 or 16 is
/// told `EINVAL` rather than being handed a number that changes nothing.
const BPC_HERE: u64 = 8;
/// The formats the one plane scans out, in the order `GETPLANE` reports them -
/// which is the order the `IN_FORMATS` bitmask indexes, so the plane's list and
/// the blob are built from this one array and cannot drift apart. `ADDFB2`
/// accepts exactly these four, which is the third place the same set has to
/// agree.
const PLANE_FORMATS: [u32; 4] = [
    DRM_FORMAT_XRGB8888,
    DRM_FORMAT_ARGB8888,
    DRM_FORMAT_XBGR8888,
    DRM_FORMAT_ABGR8888,
];
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
/// The lease family, `<drm_mode.h>`'s numbers: `MODE_CREATE_LEASE` 0xd3,
/// `MODE_LIST_LESSES` 0xd4, `MODE_REVOKE_LEASE` 0xd5, `MODE_GET_LEASE` 0xd6.
/// A lease is not a second driver: it is a card descriptor whose set of legal
/// object ids is a subset, and the only new object in the whole family is the
/// file descriptor `CREATE_LEASE` hands back.
const NR_MODE_CREATE_LEASE: u32 = 0xd3;
const NR_MODE_LIST_LESSES: u32 = 0xd4;
const NR_MODE_REVOKE_LEASE: u32 = 0xd5;
const NR_MODE_GET_LEASE: u32 = 0xd6;
/// `DRM_MODE_LEASE_IMMUTABLE`: the lessor promises not to revoke. Recorded and
/// reported, *not* enforced below - see [`LeaseSet`].
const LEASE_IMMUTABLE: u32 = 1;
/// `DRM_MODE_SEAL_ALL`: seal every sealing option there is. Same honesty: the
/// flag is stored so a caller reads back what it asked for, and nothing more.
const LEASE_SEAL_ALL: u32 = 4;
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
/// compositor, and the answer has to mean something rather than merely
/// succeed: refusing `SET_MASTER` outright is what stops weston ever getting a
/// master fd it can set a mode on (its `drmIsMaster` probe is an `AUTH`), and
/// accepting it without *recording* which console the master sits on is what
/// lets a compositor keep painting the screen after its VT has been switched
/// away. See [`master_request`] and [`master_ok`].
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
/// `DRM_CAP_ADDFB2_MODIFIERS`: whether `ADDFB2` honours `DRM_MODE_FB_MODIFIERS`.
/// Answered 1, and `add_fb2` below is where that is made true - it takes the
/// flag with the linear modifier and refuses any other.
const CAP_ADDFB2_MODIFIERS: u64 = 0x10;
/// `DRM_CAP_ASYNC_PAGE_FLIP`: whether a flip can be requested that tears.
///
/// 0, and named rather than left to the catch-all, because the difference
/// matters to a reader: this is not a capability that is missing yet, it is one
/// this card does not have. There is no engine presenting outside the frame
/// boundary, so the `PAGE_FLIP_ASYNC` flag it gates has nothing to mean.
const CAP_ASYNC_PAGE_FLIP: u64 = 0x07;
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
    edid_bytes: Option<Vec<u8>>,
    /// The size the EDID was built for, so a host resize rebuilds it: its
    /// detailed timing *is* the mode being advertised, and the two move
    /// together.
    edid_dims: (u32, u32),
    in_formats_bytes: Option<Vec<u8>>,
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
    edid_bytes: None,
    edid_dims: (0, 0),
    in_formats_bytes: None,
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

fn libc_einval() -> isize {
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
    let inode = file.inode();
    let Some(card) = inode.as_drm() else {
        return -25; // ENOTTY
    };
    // Which of the card's two names this is: `card0`'s minor 0 is the one that
    // owns the screen, `renderD128`'s 128 is an allocation handle and is never
    // asked to prove it owns anything.
    let minor = card.minor();
    let ofd = file.ofd();
    // Master/auth requests carry no argument, so answer them before the
    // `arg == 0` fault check below would reject a legitimate no-buffer call.
    if matches!(nr, NR_AUTH | NR_SET_MASTER | NR_DROP_MASTER) {
        return master_request(nr, minor, ofd);
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
    if let Some((len, writable)) = arg_shape(nr)
        && !crate::uaccess::buffer_ok(arg, len, writable, true)
    {
        return -14; // EFAULT
    }
    // The presentation requests, refused before a byte of the request is read
    // or a row of it is copied into the scanout: a compositor whose console has
    // been switched away from must lose the screen at the same moment it loses
    // the ioctl, not paint one more frame and be told no afterwards.
    if !master_ok(nr, minor, ofd) {
        return -13; // EACCES
    }
    match nr {
        NR_VERSION => fill_version(arg),
        NR_GET_CAP => get_cap(arg),
        NR_SET_CLIENT_CAP => set_client_cap(arg),
        NR_GET_MAGIC => take_magic(minor, ofd, arg),
        // `struct drm_auth { drm_magic_t magic; }` again, read this way round:
        // the token the *other* client was given, spent by the master.
        NR_AUTH_MAGIC => auth_magic(ofd, unsafe { (arg as *const u32).read_unaligned() }),
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
        NR_SETCRTC => set_crtc(arg, card.lease()),
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
        NR_PAGE_FLIP => page_flip(arg, card.lease()),
        NR_MODE_CREATE_LEASE => create_lease(card, minor, ofd, arg),
        NR_MODE_LIST_LESSES => list_leases(card, arg),
        NR_MODE_REVOKE_LEASE => revoke_lease(card, ofd, arg),
        NR_MODE_GET_LEASE => get_lease(arg),
        NR_CREATE_DUMB => create_dumb(arg),
        NR_MAP_DUMB => map_dumb(arg),
        _ => -25, // ENOTTY
    }
}

/// Which open file description holds the master, 0 for nobody.
///
/// The master belongs to a *description*, not a process, because that is the
/// only identity that survives the way a seat manager actually hands a
/// compositor its card: `SCM_RIGHTS` passes the descriptor, and both ends then
/// refer to one `File` with one address (`crate::vfs::File::ofd`) - the same key
/// `flock` and `EVIOCGRAB` arbitrate by. A master recorded against a pid would
/// be lost the moment weston, rather than seatd, called the next ioctl.
static MASTER_OFD: AtomicU64 = AtomicU64::new(0);
/// The thread group that holds it, for the `KDSETMODE` check further down. Linux
/// does not need this because its tty carries a pointer to the DRM `master`;
/// this kernel has no object that spans the two, so the process is the closest
/// thing to one.
static MASTER_PID: AtomicU64 = AtomicU64::new(0);
/// The console master was taken on lives in `crate::vfs::vcmux`, not here: it
/// is the console layer's fact - the same one `KDSETMODE` and `VT_ACTIVATE`
/// write - and reading it from two places would only let the two disagree.
/// Whether the master has been revoked by a console switch since it was held:
/// the one edge in this card's life a compositor can hear without polling,
/// because it turns the card fd's readability into `POLLHUP`.
static REVOKED: AtomicBool = AtomicBool::new(false);

/// A magic the master has been handed and not yet spent: the token, and the
/// description it names.
type PendingMagic = (u32, u64);

/// The tokens on the card, the descriptions they name, and the descriptions the
/// master has authenticated.
///
/// Linux keeps these as a `drm_magic_list` per `drm_master` plus an `auth` list
/// per file private, and because its masters are real objects the lists are
/// shared between the fds of one session. mpos has exactly one master at a
/// time, which collapses both lists to one table each - and the collapse is the
/// reason a token here is *global* rather than per-master: it names a
/// description, and only the master's right to spend it is checked at
/// `AUTH_MAGIC`.
static MAGICS: critical_section::Mutex<RefCell<Vec<PendingMagic>>> =
    critical_section::Mutex::new(RefCell::new(Vec::new()));
static AUTH_OFD: critical_section::Mutex<RefCell<Vec<u64>>> =
    critical_section::Mutex::new(RefCell::new(Vec::new()));
/// The next token to hand out. Non-zero, and never reused within a boot, so a
/// token is unambiguous even after its description has closed.
static NEXT_MAGIC: AtomicU32 = AtomicU32::new(1);

/// `DRM_IOCTL_SET_MASTER` / `_DROP_MASTER` / `_AUTH`, which carry no argument
/// and so are answered before the request's buffer is checked.
///
/// mpos has one card and one screen, and the whole point of these three is not
/// arbitration between competing masters (there is nobody to arbitrate
/// between) but the *handover*: a compositor that cannot get past `SET_MASTER`
/// never gets an fd it may set a mode on, and one that is never told it lost
/// the screen paints over the text console forever. So taking the master
/// records which console it was taken on, and losing that console revokes it.
///
/// The render node is exempt throughout: Mesa's GBM allocates buffers on
/// `renderD128` precisely because a GL client is not the display master, and a
/// card that asked it to prove otherwise would take the renderer down with it.
fn master_request(nr: u32, minor: u32, ofd: u64) -> isize {
    if minor != 0 {
        // Not a request a render node answers with anything but success; it has
        // no screen to own, and libdrm asks anyway on its way to an allocation.
        return 0;
    }
    match nr {
        NR_SET_MASTER => {
            // Linux's `SET_MASTER` is *void*: it never fails, and it takes the
            // card from whoever held it. That is not sloppiness - it is the
            // recovery path. A compositor that regains its console calls
            // `SET_MASTER` again and must succeed unconditionally, because the
            // alternative is a session that lost a race with a `^A` and can
            // never come back. Taking the master therefore ends the previous
            // holder's claim, drops the authentications it granted, and leaves
            // any unspent tokens in place: a token names a *description*, not a
            // master, and Linux's `DROP_MASTER` keeps those lists too.
            let previous = MASTER_OFD.swap(ofd, Ordering::Relaxed);
            if previous != 0 && previous != ofd {
                critical_section::with(|cs| AUTH_OFD.borrow(cs).borrow_mut().clear());
            }
            let vc = current_console().unwrap_or_else(crate::vfs::vcmux::active);
            MASTER_PID.store(crate::sched::current_tgid().0 as u64, Ordering::Relaxed);
            // The master is always bound to a console. A holder with no
            // controlling terminal - seatd taking the card before it hands the
            // fd to weston, or a launch chain that never ran `TIOCSCTTY` - is
            // bound to the console on the screen at the moment master is taken,
            // which is the console the compositor is driving. An *unbound*
            // master could never be revoked from a `^A` switch: `master_ok`
            // passes it, `painted()` says the text console may draw, and the two
            // then race on one scanout - the mouse that repaints a text VT, and
            // the flicker of a shell and a compositor overwriting each other.
            // Binding here is what makes "losing the console revokes it" true.
            crate::vfs::vcmux::set_master_vc(vc);
            // A fresh master on a console that still holds the screen clears
            // the hang-up it was revoked under; a master on a console that is
            // not on the screen stays revoked, which is the honest state.
            if crate::vfs::vcmux::master_holds_screen() {
                REVOKED.store(false, Ordering::Relaxed);
            }
            0
        }
        NR_DROP_MASTER => {
            // Also void, and also unconditional: releasing the master is always
            // allowed, including from a description that is not the master (the
            // call is a no-op for it rather than an error).
            if MASTER_OFD.load(Ordering::Relaxed) == ofd {
                MASTER_OFD.store(0, Ordering::Relaxed);
                MASTER_PID.store(0, Ordering::Relaxed);
                crate::vfs::vcmux::set_master_vc(0);
                REVOKED.store(false, Ordering::Relaxed);
            }
            0
        }
        // `DRM_IOCTL_AUTH` - the one request whose *failure* libdrm reads as
        // "you are the master". `drmIsMaster` opens with it, and weston's DRM
        // backend refuses to build a display device on a descriptor it cannot
        // prove owns the card, so a card that answered this the other way round
        // is a card no compositor will drive.
        _ => {
            if MASTER_OFD.load(Ordering::Relaxed) == ofd {
                -22 // EINVAL: the master cannot authenticate itself to itself
            } else {
                -13 // EACCES
            }
        }
    }
}

/// `DRM_IOCTL_GET_MAGIC`: mint a token naming the calling description, for the
/// master to spend on it.
///
/// One outstanding token per description, which is Linux's rule and is what the
/// list below enforces by removing an old entry before adding the new one: a
/// client that asks twice must not be authenticatable twice, and a master that
/// holds a stale token should hear nothing rather than something surprising.
fn take_magic(minor: u32, ofd: u64, arg: usize) -> isize {
    if arg == 0 {
        return -14; // EFAULT: `struct drm_auth` is the whole request
    }
    if minor != 0 {
        // The render node has no master to authenticate to; libdrm does not ask,
        // and an answer of "no token for you" would be more honest than a token
        // nothing can spend.
        return -13; // EACCES
    }
    let token = NEXT_MAGIC.fetch_add(1, Ordering::Relaxed);
    critical_section::with(|cs| {
        let mut m = MAGICS.borrow(cs).borrow_mut();
        m.retain(|&(_, owner)| owner != ofd);
        m.push((token, ofd));
    });
    // `struct drm_auth { __u32 magic; }`
    unsafe { (arg as *mut u32).write_unaligned(token) };
    0
}

/// `DRM_IOCTL_AUTH_MAGIC`: the master spends a token to let *another*
/// description drive the card. The caller must be the master (Linux's
/// `EACCES`), the token must still exist, and it is consumed either way.
fn auth_magic(caller: u64, token: u32) -> isize {
    if !is_master(caller) {
        return -13; // EACCES: only the master may let someone else in
    }
    let named = critical_section::with(|cs| {
        let mut m = MAGICS.borrow(cs).borrow_mut();
        m.iter()
            .position(|&(t, _)| t == token)
            .map(|at| m.remove(at).1)
    });
    match named {
        Some(owner) => {
            critical_section::with(|cs| {
                let mut a = AUTH_OFD.borrow(cs).borrow_mut();
                if !a.contains(&owner) {
                    a.push(owner);
                }
            });
            0
        }
        // An unknown or already-spent token. Linux answers EACCES here as well:
        // the caller proved nothing about the token it named.
        None => -13,
    }
}

/// The console the calling task is on, which is the console its session was
/// given when the seat manager opened it. `(4, 0)` and `(4, 64)` mean "the one
/// on the screen now", and `Tty::vc_number` already resolves both.
fn current_console() -> Option<u32> {
    let (major, minor) = crate::sched::controlling_tty_of_current()?;
    crate::vfs::tty::by_dev_id((major, minor))?.vc_number()
}

/// Whether the calling task's console is the one a DRM master holds. Linux lets
/// this answer stand in for a *terminal*'s foreground at `KDSETMODE`, because the
/// driver that owns the screen is allowed to say what is drawn on it; a seat
/// manager that has taken the card but not yet re-opened its own console tty is
/// the ordinary case it exists for.
///
/// The identity checked here is the task's, not the descriptor's, because
/// `KDSETMODE` arrives on a tty fd and cannot name the card fd it is talking
/// about - which is also why the answer is "somebody in this process holds the
/// card", not "this fd is authenticated".
pub fn card_grabbed_by_current() -> bool {
    if !master_held() {
        return false;
    }
    let tgid = crate::sched::current_tgid();
    MASTER_PID.load(Ordering::Relaxed) == tgid.0 as u64
}

/// Whether anybody holds the card at all. The console the holder named is a
/// separate question (`crate::vfs::vcmux::master_vc`), because a holder that
/// named none - a session with no controlling terminal, which is what a test
/// program has - has no console to lose and so must not be treated as one.
pub fn master_held() -> bool {
    MASTER_OFD.load(Ordering::Relaxed) != 0
}

/// Whether this description is the one that holds the master. Input uses it to
/// tell "the grab belongs to the session on the screen" from "somebody's grab
/// outlived their claim on the screen".
pub fn is_master(ofd: u64) -> bool {
    ofd != 0 && MASTER_OFD.load(Ordering::Relaxed) == ofd
}

/// Whether this description may drive the screen: the master itself, or one the
/// master has authenticated with a token. This is Linux's `drm_is_current_master`
/// answer to `DRM_AUTH`-gated requests, and the whole reason a client of a
/// compositor (a `weston-simple-dmabuf-egl`, an XWayland) can page-flip without
/// ever being a master of its own.
fn is_authorized(ofd: u64) -> bool {
    is_master(ofd) || critical_section::with(|cs| AUTH_OFD.borrow(cs).borrow().contains(&ofd))
}

/// Refuse a presentation request from a description that is neither the master
/// nor authenticated by it, or from a master whose console is no longer the one
/// on the screen.
///
/// The gate is asked before the request's struct is even read, and *not*
/// applied to the render node, so GBM keeps allocating and `mmap`ing while the
/// compositor that owns the screen is the only thing that can be locked out.
///
/// The second half of the condition is what the console work is about: a page
/// flip from a session that has lost its console is the mouse-move that paints a
/// compositor over a text VT, and the frame it commits is already in the scanout
/// by the time the answer comes back.
fn master_ok(nr: u32, minor: u32, ofd: u64) -> bool {
    if minor != 0 || !PRESENTATION.contains(&nr) {
        return true;
    }
    let Some(vc) = crate::vfs::vcmux::master_vc() else {
        // Nobody has claimed the card. (A *held* master is always bound now:
        // `SET_MASTER` falls back to the console on the screen, so `None` here
        // means "no master", not "a master that named no console".) A
        // compositor's first `SETCRTC` can arrive before its
        // seat manager's `SET_MASTER`, and a machine whose kernel refused it
        // there would be a machine that cannot show a mode at all - so an
        // unbound card serves its holder, and only a card bound to a console
        // can be revoked from one.
        return true;
    };
    if !crate::vfs::vcmux::holds_screen(vc) {
        revoke();
        return false;
    }
    is_authorized(ofd)
}

/// Queue a flip-completion event, *after* the frame it reports has been
/// flushed to the host.
///
/// The order is the point. A completion queued at commit time reports a frame
/// that has not left the machine yet, and a compositor that schedules against it
/// (which is precisely what `DRM_CAP_TIMESTAMP_MONOTONIC` promises it may do) is
/// then pacing itself against an event that arrives early, every frame, by
/// however long the present takes. `request_flush_blocking()` has returned by
/// now, so `since_boot_nanos` is the landing instant and the render task has
/// already counted the boundary (which is what `FBIO_WAITFORVSYNC` waits on).
///
/// This is the closest thing the machine has to a vertical blank, and the name
/// says so rather than claiming one: there is no display controller here to be
/// in blank, only a frame that has gone and one that has not.
fn queue_flip_event(sequence: u32, user_data: u64) {
    let now = crate::arch::timer::since_boot_nanos();
    critical_section::with(|cs| {
        STATE
            .borrow(cs)
            .borrow_mut()
            .events
            .push_back(drm_event::Vblank {
                type_: EVENT_FLIP_COMPLETE,
                length: 32,
                user_data,
                time_sec: (now / 1_000_000_000) as u32,
                time_usec: ((now % 1_000_000_000) / 1000) as u32,
                sequence,
                crtc: CRTC_ID,
            });
    });
    // A reader parked in `poll`/`select` on the card fd was waiting for this
    // byte; say so, rather than leaving it to the next tick's re-check.
    wake_card_readers();
}

/// The tasks parked on the card fd's readability.
static CARD_WAITERS: critical_section::Mutex<RefCell<Vec<crate::sched::policy::TaskId>>> =
    critical_section::Mutex::new(RefCell::new(Vec::new()));

/// Wake whoever is polling the card. Called on every frame boundary as well as
/// on a queued event, because the two questions a card reader asks - "is there
/// an event?" and "has my flip landed?" - are answered at the same moment.
fn wake_card_readers() {
    let waiters = critical_section::with(|cs| CARD_WAITERS.borrow(cs).borrow().clone());
    for id in waiters {
        crate::sched::wake(id);
    }
}

/// The render task has presented a frame: tell anyone waiting on the card that
/// the boundary moved.
pub fn frame_landed() {
    wake_card_readers();
}

/// Whether a lease permits the object `id`. An unleased descriptor is the whole
/// card, so it permits whatever the card has.
fn leased(lease: Option<&LeaseSet>, id: u32) -> bool {
    lease.is_none_or(|l| l.holds(id))
}

/// `DRM_IOCTL_MODE_CREATE_LEASE`: the master hands a subset of the card's
/// objects to somebody else, as a *file descriptor*.
///
/// `struct drm_mode_create_lease { __u64 objects_ptr, __u32 object_count,
/// __u32 flags, __u32 fd, __u32 __pad }`. The objects are read the way every
/// other id list on this card is read - `user_array` first, then element by
/// element - and the fd is installed the way `PRIME_HANDLE_TO_FD` installs its
/// dma-buf: a new `File`, the lowest free slot, `O_CLOEXEC` if the caller asked.
///
/// Only the master may lease. That is not a courtesy: a lease *is* a claim on
/// the screen, and letting an unauthorised descriptor mint one would be a way to
/// take the screen without ever calling `SET_MASTER`.
fn create_lease(card: &DrmCard, minor: u32, caller: u64, arg: usize) -> isize {
    if minor != 0 || !is_master(caller) {
        return -13; // EACCES
    }
    let objects_ptr = unsafe { (arg as *const u64).read_unaligned() } as usize;
    let count = unsafe { ((arg + 8) as *const u32).read_unaligned() } as usize;
    let flags = unsafe { ((arg + 12) as *const u32).read_unaligned() };
    if count == 0 || objects_ptr == 0 {
        return -22; // EINVAL
    }
    // The two flags this card knows. `IMMUTABLE` and `SEAL_ALL` are recorded on
    // the lease and read back; neither is enforced, which is spelled out on
    // [`LeaseSet`] rather than hidden here.
    if flags & !(LEASE_IMMUTABLE | LEASE_SEAL_ALL | DRM_CLOEXEC) != 0 {
        return -22; // EINVAL
    }
    // Linux bounds a lease at 16 objects; this card has four KMS objects in
    // total, so the bound is met by construction and asked only to catch a
    // caller whose count was garbage.
    if count > 16 {
        return -7; // E2BIG
    }
    let ids = unsafe { core::slice::from_raw_parts(objects_ptr as *const u32, count) };
    for id in ids {
        // Every id must be one this card actually has - and for a lease out of
        // a lease, one its own lease covers. A lease cannot widen what it was
        // given, which is the whole safety property of the family.
        if !known_object(*id) || !leased(card.lease(), *id) {
            return -22; // EINVAL
        }
    }
    let lease = card.lease_of(ids.to_vec(), flags);
    let fd = match crate::vfs::open_inode(lease) {
        Ok(fd) => fd,
        Err(_) => return -24, // EMFILE
    };
    if flags & DRM_CLOEXEC != 0 {
        // A failed `FD_SETFD` on a descriptor we just made is not this request's
        // failure to report; the lease is real and the flag is the caller's own
        // `exec` hygiene.
        let _ = crate::sched::current_fds().fd_flags_set(fd, crate::vfs::file::FD_CLOEXEC);
    }
    unsafe { ((arg + 16) as *mut u32).write_unaligned(fd as u32) };
    0
}

/// `DRM_IOCTL_MODE_GET_LEASE`: read a lease fd back - which objects it covers,
/// and what it was sealed with. The caller names the descriptor in `fd`, and on
/// a kernel with one card and one lease holder the descriptor answering the
/// request *is* the one being asked about: an fd belonging to another process is
/// refused the same `EINVAL` Linux gives a lease that does not exist.
fn get_lease(arg: usize) -> isize {
    let want_fd = unsafe { ((arg + 16) as *const u32).read_unaligned() } as i32;
    let Ok(current) = crate::sched::current_fds().clone_at(want_fd) else {
        return -22; // EINVAL: there is no such descriptor to ask about
    };
    let inode = current.inode();
    // The descriptor is looked up in the *caller's* table, so the only fd that
    // can answer is one this process holds - which is the same restriction
    // Linux imposes by walking the caller's own file-private lease list.
    let Some(card) = inode.as_drm() else {
        return -22;
    };
    let Some(lease) = card.lease() else {
        return -22;
    };
    let ids = lease.objects();
    unsafe { (arg as *mut u32).write_unaligned(ids.len() as u32) };
    let ptr = unsafe { ((arg + 8) as *const u64).read_unaligned() } as usize;
    if !ids.is_empty() && ptr != 0 && user_array(ptr, ids.len(), 4, true) {
        let out = ptr as *mut u32;
        for (i, id) in ids.iter().enumerate() {
            unsafe { out.add(i).write_unaligned(*id) };
        }
    }
    0
}

/// The card's four fixed KMS object ids. Property ids are *not* leaseable:
/// Linux leases connectors, crtcs and planes, and nothing else.
fn known_object(id: u32) -> bool {
    matches!(id, CRTC_ID | ENCODER_ID | CONNECTOR_ID | PLANE_ID)
}

/// `DRM_IOCTL_MODE_LIST_LESSES`: how many leases this descriptor handed out,
/// and - given room - which of the card's objects are under one.
///
/// `struct drm_mode_get_lease { __u32 count_objects, __u32 pad, __u64
/// objects_ptr }`. The count is always rewritten; the array is filled only when
/// the caller brought room for it, which is the same two-call shape
/// `GETRESOURCES` uses.
fn list_leases(card: &DrmCard, arg: usize) -> isize {
    let Some(lease) = card.lease() else {
        // The card itself has no lease list to enumerate: on Linux this is the
        // lessor's file private, and a plain card descriptor has never leased
        // anything from itself.
        unsafe { (arg as *mut u32).write_unaligned(0) };
        return 0;
    };
    let ids = lease.objects();
    unsafe { (arg as *mut u32).write_unaligned(ids.len() as u32) };
    // The caller's `count_objects` is both the answer slot and, on the sizing
    // call, zero: read it back *after* writing, which is what Linux's
    // `list_len` comparison does with the value the caller brought.
    let want = unsafe { (arg as *const u32).read_unaligned() } as usize;
    let ptr = unsafe { ((arg + 8) as *const u64).read_unaligned() } as usize;
    if want >= ids.len() && ptr != 0 && user_array(ptr, ids.len(), 4, true) {
        let out = ptr as *mut u32;
        for (i, id) in ids.iter().enumerate() {
            unsafe { out.add(i).write_unaligned(*id) };
        }
    }
    0
}

/// `DRM_IOCTL_MODE_REVOKE_LEASE`: the lessor takes a lease back by the
/// descriptor number it handed out.
///
/// The card cannot look the fd up in the *lessor's* table from here (the ioctl
/// arrives with the lessor's descriptor, and the lease fd may live in another
/// process entirely), so Linux marks the lease's file private dead. Here the
/// lease's own object list is emptied, which refuses every later request that
/// names an object - the same practical effect for the one card this kernel has.
fn revoke_lease(_card: &DrmCard, caller: u64, arg: usize) -> isize {
    // The number names a descriptor in the *caller's* table, and the caller is
    // the one entitled to take a lease back: a lease may only be revoked by
    // whoever holds the master that made it.
    if !is_master(caller) {
        return -13; // EACCES
    }
    let target = unsafe { (arg as *const u32).read_unaligned() } as i32;
    let Ok(file) = crate::sched::current_fds().clone_at(target) else {
        return -22; // EINVAL: no such descriptor to take back
    };
    let inode = file.inode();
    let Some(leased) = inode.as_drm().and_then(|c| c.lease()) else {
        return -22; // EINVAL: the descriptor is not a lease
    };
    leased.revoke();
    0
}

/// The requests that put pixels on the screen. Everything else the card serves
/// without asking who owns it: enumerating the topology, making and mapping
/// buffers, reading back properties. `RMFB` is here because a framebuffer the
/// CRTC is driving is a framebuffer that cannot be removed, and that is the
/// master's business rather than an allocator's.
const PRESENTATION: &[u32] = &[
    NR_SETCRTC,
    NR_PAGE_FLIP,
    NR_MODE_ATOMIC,
    NR_OBJ_SETPROPERTY,
    NR_RMFB,
];

/// The master has lost its console: stop it driving the scanout, and make the
/// card fd say so.
fn revoke() {
    if REVOKED.swap(true, Ordering::Relaxed) {
        return;
    }
    // Drain what is queued rather than holding it back. A compositor blocked in
    // `poll` on the card fd, waiting for the flip-completion event its own
    // commit promised, is not resumed by a `POLLHUP` it cannot see until the
    // events it already waited for are read: it would sit there with its frames
    // never landing and its session never ending. Empty the queue first and let
    // the hang-up close it.
    critical_section::with(|cs| STATE.borrow(cs).borrow_mut().events.clear());
}

/// The screen moved onto a console that is not the master's: take the
/// presentation away *now*, not on the master's next try.
///
/// `master_ok` refuses a lost master lazily, on the next `SETCRTC`/`PAGE_FLIP` -
/// enough to stop its pixels landing, not enough to tell the compositor it has
/// lost the screen. A compositor that is never told parks quietly in `poll`, and
/// when its console comes back it still believes it holds the card, so it re-draws
/// only on the next input event: the switch to the desktop shows a stale screen
/// until the mouse moves. Linux is not so quiet - the VT move takes the master,
/// the compositor reads the hang-up, releases and re-takes its session, and
/// repaints the moment the console returns. This is that call: revoke on the
/// switch, then wake the parked reader so the `POLLHUP` is seen. The master's
/// recovery - `SET_MASTER` after its `acqsig` - clears the hang-up again (see
/// [`revoked`]), which is what makes the switch *back* repaint without a mouse.
pub fn revoke_now() {
    if !master_held() {
        return;
    }
    // Only a master that has actually lost the screen. A master whose console is
    // still the one on the screen keeps presenting.
    if crate::vfs::vcmux::master_holds_screen() {
        return;
    }
    revoke();
    // The compositor is parked in `poll` on the card fd waiting for a frame that
    // will now never land; wake it so the hang-up closes its wait and it can
    // pause and, on the `acqsig`, come back.
    wake_card_readers();
}

/// The screen has moved back onto the DRM master's console: give it the card
/// again and put its picture back on the scanout, the way Linux's VT switch
/// re-grants the master and the compositor's re-commit re-shows it.
///
/// `revoke_now` stopped the master presenting while its console was away, and
/// the text render filled the screen in its place - so the desktop's pixels are
/// gone from the scanout even though the master's committed framebuffer is still
/// the card's `current_fb` (weston keeps rendering into its dumb buffer whether or
/// not it can present, and only a *mouse* event makes it flip a fresh one). Relying
/// on the compositor to re-commit is what left a stale screen until the mouse moved.
/// So the kernel does the re-present itself, exactly as Linux hands the master back
/// and forces the output to be re-applied on `VT_ACTIVATE`: clear the hang-up, then
/// ask the render task - the only flusher - to copy `current_fb` onto the scanout
/// and flush. The desktop is back on the very frame that switched to it.
pub fn grant_and_resent() {
    if !master_held() {
        return;
    }
    // Only when the master's console is genuinely back on the screen, in graphics
    // mode - otherwise there is nothing to hand back and the text console keeps it.
    if !crate::vfs::vcmux::master_holds_screen() {
        return;
    }
    // The hang-up was because it lost the console; the console is back.
    REVOKED.store(false, Ordering::Relaxed);
    if critical_section::with(|cs| STATE.borrow(cs).borrow().current_fb) != 0 {
        crate::drivers::video::render::request_re_present();
    }
    // A reader parked in `poll` on the card fd now sees the fd live again, not
    // hang-up, and its next flip will land.
    wake_card_readers();
}

/// Copy the master's current framebuffer onto the scanout. The render task's
/// re-present path (`drivers::video::render::request_re_present`) calls this as
/// the one flusher, then flushes. Unlike a compositor's own flip this is the
/// kernel's act, so it is not gated by `master_ok` - it is what hands the screen
/// back. Returns 0 on success, the `copy_to_scanout` error if there is no frame
/// to show.
pub fn re_present_now() -> isize {
    critical_section::with(|cs| {
        let st = STATE.borrow(cs).borrow();
        let fb = st.current_fb;
        if fb == 0 { 0 } else { copy_to_scanout(&st, fb) }
    })
}

/// A description closed its last reference: it holds nothing any more, and no
/// token or authentication may name it.
///
/// The removal is unconditional rather than "only if it was the master" because
/// a closed description's address is *reusable*: a token left behind for an
/// `Arc<File>` that has since been freed would one day name a different file,
/// and that is a use-after-free dressed up as an authorisation.
pub fn master_closed(ofd: u64) {
    if ofd == 0 {
        return;
    }
    critical_section::with(|cs| {
        MAGICS
            .borrow(cs)
            .borrow_mut()
            .retain(|&(_, owner)| owner != ofd);
        AUTH_OFD
            .borrow(cs)
            .borrow_mut()
            .retain(|&owner| owner != ofd);
    });
    if MASTER_OFD.load(Ordering::Relaxed) == ofd {
        MASTER_OFD.store(0, Ordering::Relaxed);
        MASTER_PID.store(0, Ordering::Relaxed);
        REVOKED.store(false, Ordering::Relaxed);
        crate::vfs::vcmux::set_master_vc(0);
    }
}

/// Whether the card fd should report hang-up: true from the moment a master
/// loses its console until it either re-takes it (`SET_MASTER` after the
/// `acqsig`, which is weston's own recovery path) or lets go of the descriptor.
pub fn revoked() -> bool {
    REVOKED.load(Ordering::Relaxed)
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
        NR_ADDFB2 => (104, false),   // struct drm_mode_fb_cmd2
        NR_RMFB => (4, false),       // struct drm_mode_rmfb
        NR_PAGE_FLIP => (24, false), // struct drm_mode_crtc_page_flip
        // `struct drm_mode_create_lease`: objects pointer, count, flags, the fd
        // that comes back, padding - 24 bytes, and both read and written.
        NR_MODE_CREATE_LEASE => (24, true), // struct drm_mode_create_lease
        NR_MODE_LIST_LESSES => (16, true),  // struct drm_mode_get_lease
        NR_MODE_REVOKE_LEASE => (8, false), // struct drm_mode_revoke_lease
        // `struct drm_mode_get_lease` is the *create* struct minus the flags:
        // objects pointer, count, the fd being asked about, padding - 24 bytes.
        NR_MODE_GET_LEASE => (24, true), // struct drm_mode_get_lease is 16; the
        // 24-byte form is CREATE's answer
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
        Some(bytes) => crate::uaccess::buffer_ok(ptr, bytes, write, true),
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
        // True, and only because `ADDFB2` honours the flag this advertises: a
        // commit may carry `DRM_MODE_FB_MODIFIERS` with the linear modifier,
        // which is the only one a dumb buffer is ever in. A cap answered 1 over
        // an `ADDFB2` that refused every modifier would be the caller building
        // a buffer it cannot then bind.
        CAP_ADDFB2_MODIFIERS => 1,
        // Named rather than left to the catch-all: this is *not* "unsupported
        // for now". A tearing flip is a mode of presentation this card does not
        // have - there is no engine to present outside the frame boundary - so
        // a compositor looking for it is told no, and one that ignores the cap
        // and asks anyway is refused the `PAGE_FLIP_ASYNC` flag it would need.
        CAP_ASYNC_PAGE_FLIP => 0,
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
        return libc_einval();
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
        _ => libc_einval(),
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
                return libc_einval();
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

/// The size the connector advertises as its available mode: what the host last
/// asked for, whether or not the scanout has been driven to it. `GETCONNECTOR`
/// and `GETCRTC` report this, because it is the mode a compositor re-probes for
/// after a hotplug uevent and may then commit.
fn advertised_mode_dims() -> (u32, u32) {
    match crate::drivers::virtio::gpu::global() {
        Some(gpu) => gpu.advertised_dims(),
        None => (1024, 768),
    }
}

/// The size the scanout is actually driven at: the block a pixel copy writes
/// into, and the ceiling a committed framebuffer has to fit once any resize this
/// very commit asked for has landed. It equals [`advertised_mode_dims`] except
/// in the window between the host asking for a new size and a compositor
/// committing it.
fn programmed_mode_dims() -> (u32, u32) {
    match crate::drivers::virtio::gpu::global() {
        Some(gpu) => gpu.dims(),
        None => (1024, 768),
    }
}

/// The CRTC's mode changed under userland: announce it the way Linux does.
///
/// This is what makes a compositor follow a display that changed size, and it is
/// a *uevent* rather than an event on the card's fd: the DRM uapi has no
/// hotplug event type (`libdrm`'s `drmHandleEvent` refuses a type it does not
/// know), so Linux announces a mode change as a `change` uevent on the card
/// device carrying `HOTPLUG=1`. A compositor's udev monitor is watching exactly
/// that - weston's DRM backend logs "[udev] HOTPLUG event" and re-probes its
/// connectors - and a re-probe is where the new mode is found, because
/// `GETCONNECTOR` and `GETCRTC` report the size the connector advertises, which
/// is what [`advertised_mode_dims`] answers.
///
/// Called by the render task when a host event teaches it a new size, *before*
/// anything is rebuilt: the announcement is the question, not the answer. The
/// scanout only follows once a compositor commits the advertised mode through a
/// modeset. A machine with no compositor to commit - a plain text console - takes
/// the uevent and drives nothing, which is exactly how Linux's fbcon behaves for
/// a connector nobody re-modes.
pub fn display_changed() {
    crate::uevent::publish(&crate::uevent::Event {
        action: "change",
        devpath: alloc::string::String::from("/devices/virtual/drm/card0"),
        subsystem: "drm",
        devname: alloc::string::String::from("dri/card0"),
        major: DRM_MAJOR,
        minor: 0,
        extra: &[("HOTPLUG", "1")],
    });
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
    let (screen_w, screen_h) = programmed_mode_dims();
    fb_fits_at(st, fb_id, screen_w, screen_h)
}

/// Whether `fb_id` fits a screen of `screen_w`x`screen_h`. `fb_fits` asks it of
/// the size now programmed; an atomic commit that carries a new mode asks it of
/// the size that commit is about to drive, because the framebuffer a compositor
/// builds for a fresh mode is the size of that mode, not of the screen that has
/// not been resized yet.
fn fb_fits_at(st: &DrmState, fb_id: u32, screen_w: u32, screen_h: u32) -> bool {
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
        return libc_einval();
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
    // The mode and the block it names are read as *one* value, not two: a
    // resize can land between them, and the width of one screen with the address
    // of another is a copy that runs off the end of a screen that just got
    // smaller (`virtio::gpu::Scanout`).
    let Some(gpu) = crate::drivers::virtio::gpu::global() else {
        // A headless machine has no scanout to draw to; the bookkeeping is still
        // right, and the flush after is the no-op it already is there.
        return 0;
    };
    let scanout_info = gpu.scanout();
    let (screen_w, screen_h) = (scanout_info.width, scanout_info.height);
    let scanout = phys_to_virt(scanout_info.fb_phys) as *mut u8;
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
        let (w, h) = advertised_mode_dims();
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

fn set_crtc(arg: usize, lease: Option<&LeaseSet>) -> isize {
    // struct drm_mode_crtc: crtc_id@12, fb_id@16, x@20,y@24, ...
    let crtc_id = unsafe { ((arg + 12) as *const u32).read_unaligned() };
    let fb_id = unsafe { ((arg + 16) as *const u32).read_unaligned() };
    if crtc_id != CRTC_ID || !leased(lease, crtc_id) {
        return libc_einval();
    }
    // A legacy modeset carries its mode inline (`drm_mode_crtc.mode` at +36,
    // whose hdisplay is at +4 and vdisplay at +14). When the caller names the
    // size the connector advertises but the scanout does not yet drive, that is
    // a resize asked of the kernel - taken here, before the copy, because the
    // copy writes into the resized block. Any other mode (the size already
    // driven, a zeroed mode, or one the host never offered) leaves the geometry
    // alone, and a framebuffer that does not fit is refused by the copy as
    // before. Weston drives the atomic path; this keeps the legacy door honest.
    if fb_id != 0 {
        let hdisplay = unsafe { ((arg + 40) as *const u16).read_unaligned() } as u32;
        let vdisplay = unsafe { ((arg + 50) as *const u16).read_unaligned() } as u32;
        let (adv_w, adv_h) = advertised_mode_dims();
        if hdisplay > 0
            && vdisplay > 0
            && (hdisplay, vdisplay) == (adv_w, adv_h)
            && (hdisplay, vdisplay) != programmed_mode_dims()
            && !crate::drivers::video::render::resize_blocking(hdisplay, vdisplay)
        {
            return libc_einval();
        }
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
            return libc_einval();
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
        return libc_einval();
    }
    let (w, h) = advertised_mode_dims();
    // The connector's properties and their current values - what
    // `OBJ_GETPROPERTIES` also answers, on the path libdrm fills in through
    // `drmModeGetConnector`. The EDID's *bytes* are built here too, so that a
    // client which reads the property and then fetches the blob finds it; the
    // immutable ones report the one value they have.
    let (crtc_value, dpms_value) = critical_section::with(|cs| {
        let mut st = STATE.borrow(cs).borrow_mut();
        let _ = edid_blob_bytes(&mut st);
        (st.conn_crtc as u64, st.dpms as u64)
    });
    const CONNECTOR_PROPS: [u32; 6] = [
        PROP_CRTC_ID,
        PROP_DPMS,
        PROP_EDID,
        PROP_NON_DESKTOP,
        PROP_MAX_BPC,
        PROP_VRR_CAPABLE,
    ];
    let connector_values: [u64; 6] = [
        crtc_value,
        dpms_value,
        DRIVER_BLOB_EDID as u64,
        0, // non-desktop: this is a real display, not a head-mounted one
        BPC_HERE,
        0, // vrr_capable: the refresh rate cannot vary
    ];
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
        (p.add(36) as *mut u32).write_unaligned(CONNECTOR_PROPS.len() as u32);
        let props_ptr = ((p.add(16) as *const u64).read_unaligned()) as *mut u32;
        if count_props >= CONNECTOR_PROPS.len() as u32
            && !props_ptr.is_null()
            && user_array(props_ptr as usize, CONNECTOR_PROPS.len(), 4, true)
        {
            for (index, prop) in CONNECTOR_PROPS.iter().enumerate() {
                props_ptr.add(index).write_unaligned(*prop);
            }
        }
        let values_ptr = ((p.add(24) as *const u64).read_unaligned()) as *mut u64;
        if count_props >= CONNECTOR_PROPS.len() as u32
            && !values_ptr.is_null()
            && user_array(values_ptr as usize, CONNECTOR_PROPS.len(), 8, true)
        {
            for (index, value) in connector_values.iter().enumerate() {
                values_ptr.add(index).write_unaligned(*value);
            }
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
        // The physical size, taken from the EDID's own centimetre fields and
        // reported as the millimetres a connector reports them in - ten times
        // the field, which is the relation the measurement found (30 cm and
        // 19 cm on a real panel gave exactly 300x190 here).
        let (mm_w, mm_h) = crate::drm_blob::size_mm(w, h);
        (p.add(64) as *mut u32).write_unaligned(mm_w);
        (p.add(68) as *mut u32).write_unaligned(mm_h);
        // Sub-pixel order: 0, the unknown value. Measured on a real connected
        // eDP connector, which also reported 0 - so this is the honest default
        // rather than an invented arrangement of a virtual display's pixels.
        (p.add(72) as *mut u32).write_unaligned(crate::drm_blob::SUBPIXEL_UNKNOWN);
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
        // The format list, when the caller has room for it. All four the plane
        // scans out and `ADDFB2` accepts, in the order the `IN_FORMATS` bitmask
        // indexes - one array so that the plane's list, the blob's bitmask and
        // the accept-list cannot disagree.
        let want = p.add(5).read_unaligned();
        let formats = ((arg + 24) as *const u64).read_unaligned() as *mut u32;
        if want >= PLANE_FORMATS.len() as u32
            && !formats.is_null()
            && user_array(formats as usize, PLANE_FORMATS.len(), 4, true)
        {
            for (index, format) in PLANE_FORMATS.iter().enumerate() {
                formats.add(index).write_unaligned(*format);
            }
        }
        p.add(5).write_unaligned(PLANE_FORMATS.len() as u32); // count_format_types
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
        // The connector's `EDID`: a blob, and `IMMUTABLE` because a client that
        // could write it would be claiming to be the monitor. Its *value* is
        // the blob id the card publishes, which is what `OBJ_GETPROPERTIES`
        // reports; this is the shape.
        PROP_EDID => ("EDID", PROP_BLOB | PROP_IMMUTABLE, &[], &[]),
        // Measured on a real connector, and not what the sources suggest: this
        // is a *range* with the two values 0 and 1, not a bool.
        PROP_NON_DESKTOP => ("non-desktop", PROP_RANGE | PROP_IMMUTABLE, &[0, 1], &[]),
        // The scanout is eight bits per channel and no property can change
        // that, so the range has one value - a caller asking for 16 is refused
        // rather than handed a number that would change nothing.
        PROP_MAX_BPC => ("max bpc", PROP_RANGE, &[BPC_HERE, BPC_HERE], &[]),
        // Also a range, also measured: every DP output on the real card carries
        // this, at 0 where the sink cannot vary its rate - which is this
        // display, so it reports 0 and a compositor stops asking.
        PROP_VRR_CAPABLE => ("vrr_capable", PROP_RANGE | PROP_IMMUTABLE, &[0, 1], &[]),
        // The plane's format-and-modifier list, as the blob `drm_blob::in_formats`
        // builds: one linear modifier over every format the plane reports.
        PROP_IN_FORMATS => ("IN_FORMATS", PROP_BLOB | PROP_IMMUTABLE, &[], &[]),
        _ => return libc_einval(),
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
        // The one connector property that is settable and has a range. A value
        // the range does not hold is refused rather than accepted, which is
        // what makes the one-value range mean something: `max bpc` 16 here is
        // `EINVAL`, not a success that changes no pixels.
        (CONNECTOR_ID, PROP_MAX_BPC) => {
            if value != BPC_HERE {
                return libc_einval();
            }
        }
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
        _ => return libc_einval(),
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
            _ => return libc_einval(),
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
        return libc_einval();
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
        return libc_einval();
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
                return libc_einval();
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
        return libc_einval();
    }
    if count_objs == 0 || objs.is_null() || counts.is_null() || props.is_null() || values.is_null()
    {
        return libc_einval();
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
                return libc_einval();
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

    // The commit's own `user_data` rides back in the completion event, which is
    // how a compositor that commits for several devices tells them apart -
    // libdrm hands it to the flip handler as its `data` argument.
    let want_event = flags & PAGE_FLIP_EVENT != 0;
    let user_data = unsafe { ((arg + 48) as *const u64).read_unaligned() };
    // Validate the plan's geometry under the lock, but apply nothing yet. A
    // commit that carries a new mode has to resize the scanout, and that parks
    // on the render task, so it cannot happen inside a critical section. This
    // first section answers "can this commit be honoured" and, when it can and
    // it moves the geometry, names the size to drive the scanout to.
    let (rc, resize_to) = critical_section::with(|cs| {
        let st = STATE.borrow(cs).borrow();
        let mut resize_to: Option<(u32, u32)> = None;
        // A mode this card cannot show is refused before anything moves.
        if let Some(blob) = plan.mode_blob.filter(|b| *b != 0) {
            match (mode_fits(&st, blob), mode_size(&st, blob)) {
                (true, Some(dims)) => resize_to = Some(dims),
                _ => return (libc_einval(), None),
            }
        }
        // The framebuffer is judged against the size this commit will drive -
        // for a resize that is the committed mode, not the scanout that has not
        // been rebuilt yet, because the buffer a compositor builds for a new
        // mode is that mode's size.
        let (cur_w, cur_h) = programmed_mode_dims();
        let (fit_w, fit_h) = resize_to.unwrap_or((cur_w, cur_h));
        if let Some(fb) = plan.plane_fb.filter(|f| *f != 0)
            && !fb_fits_at(&st, fb, fit_w, fit_h)
        {
            return (libc_einval(), None);
        }
        // A rehearsal is answered by the checks above and drives no resize.
        if flags & ATOMIC_TEST_ONLY != 0 {
            return (0, None);
        }
        (0, resize_to)
    });
    if rc != 0 {
        return rc;
    }

    // The scanout rebuild a new mode asks for, done on the render task's hands
    // because it owns the device - and outside the section, because it parks. A
    // size the host will not do comes back as the same `EINVAL` Linux gives a
    // modeset the panel refuses.
    if let Some((w, h)) = resize_to
        && (w, h) != programmed_mode_dims()
        && !crate::drivers::video::render::resize_blocking(w, h)
    {
        return libc_einval();
    }

    // Apply, now that any resize has moved the geometry under it.
    let mut seq = 0u32;
    let rc = critical_section::with(|cs| {
        let mut st = STATE.borrow(cs).borrow_mut();
        // The plane's buffer is the one entry that moves pixels; the rest is
        // the state a compositor reads back, recorded exactly as the
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
            seq = st.seq;
        }
        0
    });
    if rc == 0 {
        // Present, *then* report: the commit's completion event is queued once
        // the frame has actually gone to the host, so its timestamp is the
        // landing rather than the request. See [`queue_flip_event`].
        crate::drivers::video::render::request_flush_blocking();
        if want_event {
            queue_flip_event(seq, user_data);
        }
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
            return Err(libc_einval());
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

/// The `hdisplay`/`vdisplay` of the `drmModeModeInfo` stored in blob `id`, if it
/// is there and long enough to be one.
fn mode_size(st: &DrmState, id: u32) -> Option<(u32, u32)> {
    let bytes = st.blobs.get(id as usize - 1).and_then(|b| b.as_ref())?;
    // struct drm_mode_modeinfo: clock@0, hdisplay@4, hsync_start@6,
    // hsync_end@8, htotal@10, hskew@12, vdisplay@14, ...
    if bytes.len() < 68 {
        return None;
    }
    let hdisplay = u16::from_le_bytes([bytes[4], bytes[5]]);
    let vdisplay = u16::from_le_bytes([bytes[14], bytes[15]]);
    Some((hdisplay as u32, vdisplay as u32))
}

/// Whether the blob `id` holds a `drmModeModeInfo` this card can show. The mode
/// comes from the connector's own list, so a commit that binds one of those
/// fits: either the size the connector advertises now - the one the host asked
/// for and which this very commit will drive the scanout to - or the size
/// already programmed, for a commit that keeps the geometry and only rebinds a
/// buffer. A mode for a panel the host never offered fits neither.
fn mode_fits(st: &DrmState, id: u32) -> bool {
    let Some((w, h)) = mode_size(st, id) else {
        return false;
    };
    (w, h) == advertised_mode_dims() || (w, h) == programmed_mode_dims()
}

/// `CREATE_BLOB`: store a property's bytes and hand back an id. A mode is one
/// of these, and so is a damage-rectangle list; the card keeps them until
/// `DESTROY_BLOB` or the last opener goes.
fn create_blob(arg: usize) -> isize {
    // struct drm_mode_create_blob { u64 data@0; u32 length@8; u32 blob_id@12; }
    let data = unsafe { (arg as *const u64).read_unaligned() } as *const u8;
    let length = unsafe { ((arg + 8) as *const u32).read_unaligned() } as usize;
    if length == 0 || length > MAX_BLOB || data.is_null() {
        return libc_einval();
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

/// The ids the card's own blobs answer to.
///
/// A client's blob ids are small and allocated lowest-first; these are far
/// above anything that table will reach, so `get_propblob` can tell the two
/// apart and `DESTROY_BLOB` cannot free what a property is holding.
const DRIVER_BLOB_EDID: u32 = 0x4000_0001;
const DRIVER_BLOB_IN_FORMATS: u32 = 0x4000_0002;

/// What the synthesized display calls itself. A compositor shows this in its
/// settings panel and in `wl_output`, which is the whole reason an EDID carries
/// a name descriptor.
const MONITOR_NAME: &str = "MPOS Virtual";

/// The connector's `EDID`, built on first use and rebuilt whenever the host has
/// resized the window - the blob's detailed timing is the mode being advertised,
/// so a stale one would describe a display this card no longer is.
fn edid_blob_bytes(st: &mut DrmState) -> &[u8] {
    let dims = advertised_mode_dims();
    if st.edid_bytes.is_none() || st.edid_dims != dims {
        st.edid_dims = dims;
        st.edid_bytes =
            Some(crate::drm_blob::edid_base_block(dims.0, dims.1, MONITOR_NAME).to_vec());
    }
    st.edid_bytes.as_deref().unwrap_or(&[])
}

/// The plane's `IN_FORMATS`: one modifier entry covering every format in
/// [`PLANE_FORMATS`], because a dumb buffer here is always linear. Built once -
/// neither the format list nor the modifier set depends on anything that can
/// change.
fn in_formats_blob_bytes(st: &mut DrmState) -> &[u8] {
    if st.in_formats_bytes.is_none() {
        st.in_formats_bytes = Some(crate::drm_blob::in_formats(
            &PLANE_FORMATS,
            crate::drm_blob::FORMAT_MOD_LINEAR,
        ));
    }
    st.in_formats_bytes.as_deref().unwrap_or(&[])
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
        let mut st = STATE.borrow(cs).borrow_mut();
        match id {
            // The card's own blobs, built on demand: a client that asks for the
            // EDID before anything has asked for the property still gets it.
            DRIVER_BLOB_EDID => Some(edid_blob_bytes(&mut st).to_vec()),
            DRIVER_BLOB_IN_FORMATS => Some(in_formats_blob_bytes(&mut st).to_vec()),
            _ => st
                .blobs
                .get(id.wrapping_sub(1) as usize)
                .and_then(|b| b.as_ref())
                .cloned(),
        }
    }) else {
        return libc_einval();
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
///
/// A *driver's* blob id is the one exception, and it succeeds while dropping
/// nothing: the property still holds the blob, which is exactly what happens on
/// Linux when a client destroys a blob id it read off a property - the id goes
/// and the blob stays. Freeing the EDID here would leave the connector's
/// property naming nothing.
fn destroy_blob(arg: usize) -> isize {
    // struct drm_mode_destroy_blob { u32 blob_id@0; }
    let id = unsafe { (arg as *const u32).read_unaligned() };
    if id == DRIVER_BLOB_EDID || id == DRIVER_BLOB_IN_FORMATS {
        return 0;
    }
    critical_section::with(|cs| {
        let mut st = STATE.borrow(cs).borrow_mut();
        match st.blobs.get_mut(id.wrapping_sub(1) as usize) {
            Some(slot @ Some(_)) => {
                *slot = None;
                0
            }
            _ => libc_einval(),
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
                    PROP_EDID,
                    PROP_NON_DESKTOP,
                    PROP_MAX_BPC,
                    PROP_VRR_CAPABLE,
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
                    // The blob *id*, which is what a client hands to
                    // `GETPROPBLOB`; the bytes are not this reply's business.
                    DRIVER_BLOB_EDID as u64,
                    0, // non-desktop
                    BPC_HERE,
                    0, // vrr_capable
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
                CONNECTOR_PROPS.len(),
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
                        PROP_IN_FORMATS,
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
                        // The plane's format-and-modifier list, as a blob id.
                        DRIVER_BLOB_IN_FORMATS as u64,
                        0,
                        0,
                    ],
                    14usize,
                ))
            }
            _ => None,
        }
    }) else {
        return libc_einval();
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
        None => libc_einval(),
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
    let flags = unsafe { ((arg + 16) as *const u32).read_unaligned() };
    let modifier = unsafe { ((arg + 68) as *const u64).read_unaligned() };
    // A plane offset other than the start of the buffer is still refused: the
    // scanout copies the buffer from its beginning and has nowhere to start
    // part-way in.
    if offset != 0 {
        return libc_einval();
    }
    // The modifier, which `DRM_CAP_ADDFB2_MODIFIERS` promises this card takes:
    // the flag has to be set to carry one, and the only one it can take is
    // `LINEAR` - a dumb buffer's layout. Anything else, including a modifier
    // named without the flag, is the `EINVAL` Linux answers for a format and
    // modifier pair this plane cannot scan out; the pair *is* listed in
    // `IN_FORMATS`, which is the same truth in the other direction.
    const FB_MODIFIERS: u32 = 1 << 1;
    let modifier = if flags & FB_MODIFIERS != 0 {
        if modifier != crate::drm_blob::FORMAT_MOD_LINEAR {
            return libc_einval();
        }
        modifier
    } else {
        0
    };
    let _ = modifier;
    let (bpp, depth) = match format {
        DRM_FORMAT_XRGB8888 | DRM_FORMAT_XBGR8888 => (32, 24),
        DRM_FORMAT_ARGB8888 | DRM_FORMAT_ABGR8888 => (32, 32),
        _ => return libc_einval(),
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
        None => libc_einval(),
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
            return libc_einval();
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
        return libc_einval();
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
            _ => libc_einval(),
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
        return libc_einval();
    };
    // The descriptor, in the caller's own table, under the caller's own
    // descriptor ceiling.
    let file = Arc::new(crate::vfs::File::new(buf));
    let fd = match crate::sched::current_fds().install(file) {
        Ok(fd) => fd,
        Err(crate::vfs::VfsError::TooManyDescriptors) => return -24, // EMFILE
        Err(_) => return libc_einval(),
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
        return libc_einval(); // not a dma-buf descriptor
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
        return libc_einval(); // the handle table is full
    }
    unsafe { (arg as *mut u32).write_unaligned(handle) };
    0
}

fn page_flip(arg: usize, lease: Option<&LeaseSet>) -> isize {
    // struct drm_mode_crtc_page_flip: crtc_id@0, fb_id@4, flags@8, reserved@12,
    // user_data@16.
    let crtc_id = unsafe { (arg as *const u32).read_unaligned() };
    let fb_id = unsafe { ((arg + 4) as *const u32).read_unaligned() };
    if crtc_id != CRTC_ID || !leased(lease, crtc_id) {
        return libc_einval();
    }
    // DRM_MODE_PAGE_FLIP_EVENT (0x1) asks for the completion event.
    let flags = unsafe { ((arg + 8) as *const u32).read_unaligned() };
    let user_data = unsafe { ((arg + 16) as *const u64).read_unaligned() };
    let want_event = flags & PAGE_FLIP_EVENT != 0;
    let rc = critical_section::with(|cs| {
        let mut st = STATE.borrow(cs).borrow_mut();
        let rc = copy_to_scanout(&st, fb_id);
        let mut seq = 0;
        if rc == 0 {
            st.current_fb = fb_id;
            st.seq = st.seq.wrapping_add(1);
            seq = st.seq;
        }
        (rc, seq)
    });
    let (rc, seq) = rc;
    if rc == 0 {
        crate::drivers::video::render::request_flush_blocking();
        // The completion is queued *after* the frame has gone out, and carries
        // that moment's timestamp - see [`queue_flip_event`].
        if want_event {
            queue_flip_event(seq, user_data);
        }
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
        return libc_einval();
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
        return libc_einval();
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
    /// minor is the card index plus 128. A lease gets one of its own (see
    /// [`DrmCard::lease`]), because `fstat` on a lease fd must not be
    /// indistinguishable from `fstat` on the card it came from.
    minor: u32,
    /// The objects this descriptor may drive, or `None` for the whole card.
    ///
    /// A lease is *only* this: a subset of the card's object ids, plus the fact
    /// that its holder is treated as a master of them. Everything else - the
    /// dumb buffers, the framebuffers, the event queue - is the one shared card
    /// state, exactly as on Linux, where a lease is a `drm_file` pointing at the
    /// same `drm_device` with a list of allowed object ids attached.
    lease: Option<Arc<LeaseSet>>,
    /// The node's mode and owner - `chmod`/`chown`/udev's rules on `card0`
    /// stick here and are what `stat` and the permission checks read
    /// (see `crate::vfs::devfs::NodePerms`).
    perms: crate::vfs::devfs::NodePerms,
}

/// The object ids one lease descriptor may drive, and the flags it was made
/// with.
///
/// `sealed` is the flags the lessor passed, reported back by `GET_LEASE` and
/// `LIST_LESSES`-adjacent queries. It is *recorded, not enforced*: an
/// `IMMUTABLE` lease can still be revoked by its lessor here, and a sealed one
/// still lets its holder do everything the object ids allow. That is the honest
/// shape of the gap - Linux's sealing refuses *plane and mode* changes on the
/// leased object, which is a property set this virtual card does not have, and a
/// kernel that pretended to seal would be refusing requests it cannot explain.
pub struct LeaseSet {
    objects: critical_section::Mutex<RefCell<Vec<u32>>>,
    sealed: u32,
}

impl LeaseSet {
    fn new(objects: Vec<u32>, sealed: u32) -> LeaseSet {
        LeaseSet {
            objects: critical_section::Mutex::new(RefCell::new(objects)),
            sealed,
        }
    }

    /// Whether this lease permits the KMS object `id`.
    pub fn holds(&self, id: u32) -> bool {
        critical_section::with(|cs| self.objects.borrow(cs).borrow().contains(&id))
    }

    /// The object ids, as `MODE_GET_LEASE` and `MODE_LIST_LESSES` report them.
    pub fn objects(&self) -> Vec<u32> {
        critical_section::with(|cs| self.objects.borrow(cs).borrow().clone())
    }

    pub fn sealed(&self) -> u32 {
        self.sealed
    }

    /// `MODE_REVOKE_LEASE`: the lessor takes the lease back, which empties it.
    ///
    /// The descriptor is not torn down - Linux does not do that either, it marks
    /// the lease's file private dead - so a holder that keeps using it finds
    /// every object it names refused, which is the same practical effect on the
    /// one card this kernel has.
    fn revoke(&self) {
        critical_section::with(|cs| self.objects.borrow(cs).borrow_mut().clear());
    }
}

impl DrmCard {
    pub fn new(ino: u64, minor: u32) -> Arc<DrmCard> {
        Arc::new(DrmCard {
            ino,
            minor,
            lease: None,
            perms: crate::vfs::devfs::NodePerms::new(0o666, 0, 0),
        })
    }

    /// A lease of this card: a fresh inode number, the next lease minor, and the
    /// object ids it may drive.
    fn lease_of(&self, objects: Vec<u32>, sealed: u32) -> Arc<DrmCard> {
        static LEASE_INO: core::sync::atomic::AtomicU64 =
            core::sync::atomic::AtomicU64::new(0x9110);
        static LEASE_MINOR: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(64);
        Arc::new(DrmCard {
            ino: LEASE_INO.fetch_add(1, Ordering::Relaxed),
            minor: LEASE_MINOR.fetch_add(1, Ordering::Relaxed),
            lease: Some(Arc::new(LeaseSet::new(objects, sealed))),
            perms: crate::vfs::devfs::NodePerms::new(0o666, 0, 0),
        })
    }

    /// The object ids this descriptor may drive, for the requests that name an
    /// object by id. `None` is the whole card.
    pub fn lease(&self) -> Option<&LeaseSet> {
        self.lease.as_deref()
    }

    /// Which of the card's names this node is: 0 = `card0`, the master's screen;
    /// 128 = `renderD128`, an allocation handle that is never asked who owns the
    /// display.
    pub fn minor(&self) -> u32 {
        self.minor
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

    fn keeps_modes(&self) -> bool {
        self.perms.keeps()
    }

    fn mode(&self) -> u32 {
        self.perms.mode()
    }

    fn set_mode(&self, mode: u32) {
        self.perms.set_mode(mode);
    }

    fn owner(&self) -> (u32, u32) {
        self.perms.owner()
    }

    fn set_owner(&self, uid: u32, gid: u32) {
        self.perms.set_owner(uid, gid);
    }

    /// The device number `stat` reports: DRM's major 226 and this name's
    /// minor. Mesa's loader identifies a DRM device through exactly this
    /// pair - it looks the number up under `/sys/dev/char/226:0` to find
    /// which driver owns the fd - so a card that reports `0,0` is a card
    /// whose driver cannot be named ("MESA-LOADER: failed to retrieve device
    /// information", which is what the log said before this existed).
    fn rdev(&self) -> Option<(u32, u32)> {
        Some((DRM_MAJOR, self.minor))
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

    /// The card reports hang-up once its master has lost the console.
    ///
    /// This is the only notice a compositor gets that does not depend on it
    /// asking: weston parks in `poll` on the card fd inside `drmHandleEvent`,
    /// and a master that has been revoked has nothing left to wait *for* - its
    /// flips are refused, so no completion event will ever arrive to wake it.
    /// Without the bit the session is not merely frozen, it is frozen with the
    /// keyboard and the pointer still pointed at it.
    fn poll_hup(&self) -> bool {
        revoked()
    }

    /// A card reader parks on this list and is woken by the frame that carries
    /// its event, which is what takes this node off the "readiness nobody can
    /// announce" list `poll_select`'s comment describes.
    fn poll_subscribe(&self, id: crate::sched::policy::TaskId) {
        critical_section::with(|cs| {
            let mut v = CARD_WAITERS.borrow(cs).borrow_mut();
            if !v.contains(&id) {
                v.push(id);
            }
        });
    }

    fn poll_unsubscribe(&self, id: crate::sched::policy::TaskId) {
        critical_section::with(|cs| {
            CARD_WAITERS.borrow(cs).borrow_mut().retain(|&w| w != id);
        });
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

    /// A dumb buffer is real memory the card scans out; its mapping must be
    /// those pages, so the mapping is frame-backed.
    fn frame_backed(&self) -> bool {
        true
    }
}
