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
/// The cursor plane, the card's second plane.
///
/// It is a *plane* rather than only the legacy `MODE_CURSOR` door because that is
/// what a compositor looks for: weston picks its cursor plane out of the plane
/// list by the `type` property and drives it atomically, and only falls back to
/// drawing the pointer into its own framebuffer when there is none. A card with
/// cursor *hardware* and no cursor plane is a real configuration - the UAPI
/// documents it - but it is not this one, and the difference matters because a
/// compositor that cannot find the plane will not ask for it.
const CURSOR_PLANE_ID: u32 = 37;
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
/// The fence a client's buffer arrives behind: a `sync_file` descriptor a commit
/// is ordered behind before a pixel of it is copied.
///
/// It was accepted and recorded nowhere for as long as the `SYNCOBJ_*` family
/// did not exist, because a value with nothing to name is a value this card
/// could not honour - and weston adds this property to a commit whenever a
/// client handed it a buffer with an acquire fence, so a card *without* the
/// property failed the whole commit rather than the one entry. Now the family is
/// there and the wait is real; what is left of the old shape is that the wait is
/// always over at once, because nothing on this machine can leave a fence
/// unsignaled. See `mode_atomic` for that reasoning in full.
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
/// The plane's `OUT_FENCE_PTR`: a *pointer* in the caller's memory where the
/// commit writes back a fence descriptor for the frame it just committed.
///
/// It is the other half of `IN_FENCE_FD`, and the half this card was missing
/// entirely - the list in `NOTES.md` §7.4 has it absent, because amdgpu does not
/// publish one either. The measured shape on a card that does is a range whose
/// two ends are 0 and the whole 64-bit span, since the value is an address and
/// not a number with a meaning of its own.
const PROP_OUT_FENCE_PTR: u32 = 95;
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
/// accepts exactly these two, which is the third place the same set has to
/// agree.
const PLANE_FORMATS: [u32; 2] = [DRM_FORMAT_XRGB8888, DRM_FORMAT_ARGB8888];
/// The cursor plane's formats, which is a shorter list for a measured reason:
/// `modetest -p` on the real amdgpu card reports `formats: AR24` for every one
/// of its cursor planes. A cursor is alpha-blended, so the format that carries
/// alpha is the one it is in; the opaque spelling is what a primary plane has.
const CURSOR_FORMATS: [u32; 1] = [DRM_FORMAT_ARGB8888];
/// `DRM_MODE_CURSOR_BO` and `DRM_MODE_CURSOR_MOVE`: the two things a cursor
/// request can ask for, and `DRM_MODE_CURSOR_FLAGS` the mask of both.
const CURSOR_BO: u32 = 0x01;
const CURSOR_MOVE: u32 = 0x02;
const CURSOR_FLAGS: u32 = 0x03;
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
/// The four requests §7.4's list names that this card answers by *refusing*
/// rather than by falling off the end of the dispatch `match`, plus `GETFB2`.
/// Naming them is the difference between "not implemented" and "deliberately
/// not this card's": a caller is owed the second, and `NOTES.md` §7.4 owes the
/// reason beside it. See `gamma_refused` and `sequence_refused`.
const NR_MODE_GETGAMMA: u32 = 0xa4;
const NR_MODE_SETGAMMA: u32 = 0xa5;
const NR_CRTC_GET_SEQUENCE: u32 = 0x3b;
const NR_CRTC_QUEUE_SEQUENCE: u32 = 0x3c;
const NR_GETFB2: u32 = 0xce;
/// The two spellings of the cursor request. `MODE_CURSOR2` is the same struct
/// with the hotspot at the end, which is why both are served by one handler.
const NR_MODE_CURSOR: u32 = 0xa3;
const NR_MODE_CURSOR2: u32 = 0xbb;
/// The lease family, `<drm.h>`'s numbers: `MODE_CREATE_LEASE` 0xc6,
/// `MODE_LIST_LESSEES` 0xc7, `MODE_GET_LEASE` 0xc8, `MODE_REVOKE_LEASE` 0xc9.
/// A lease is not a second driver: it is a card descriptor whose set of legal
/// object ids is a subset, and the only new object in the whole family is the
/// file descriptor `CREATE_LEASE` hands back.
///
/// The four were written down as 0xd3..0xd6 first, and that is not a family the
/// UAPI has: `drm.h` puts it at 0xc6..0xc9, which is what every real caller
/// sends - libdrm's `drmModeCreateLease` and its three siblings. A request
/// numbered thirteen too high misses every arm of the dispatch `match` and is
/// answered `ENOTTY` by the catch-all, so the whole family was unreachable from
/// userspace while `t-drm-lease` stayed green: that test built its requests from
/// these same four constants, and an expectation taken from the thing it checks
/// agrees with any value at all. It now pins them against `<drm.h>`'s literals
/// instead, which is the check that would have caught this.
///
/// The mistake is worth naming because it is the general shape: a number a
/// caller and a driver must *agree* on cannot be pinned by a test that only ever
/// exercises the two against each other. Something outside the pair has to hold
/// the value, and here that is the header.
const NR_MODE_CREATE_LEASE: u32 = 0xc6;
const NR_MODE_LIST_LESSES: u32 = 0xc7;
const NR_MODE_REVOKE_LEASE: u32 = 0xc9;
const NR_MODE_GET_LEASE: u32 = 0xc8;
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

/// The sync object family, `<drm.h>`'s numbers, in the two runs the header puts
/// them in. `CREATE`..`SIGNAL` sit between the lease family and `GETFB2`, and the
/// later four were added years after them - which is why a request number is
/// never inferred from the one next to it.
///
/// The whole family is decided by `crate::gpu::sync`, where the rules are written
/// down with a host test each; what is here is the argument marshalling and the
/// table of objects.
const NR_SYNCOBJ_CREATE: u32 = 0xbf;
const NR_SYNCOBJ_DESTROY: u32 = 0xc0;
const NR_SYNCOBJ_HANDLE_TO_FD: u32 = 0xc1;
const NR_SYNCOBJ_FD_TO_HANDLE: u32 = 0xc2;
const NR_SYNCOBJ_WAIT: u32 = 0xc3;
const NR_SYNCOBJ_RESET: u32 = 0xc4;
const NR_SYNCOBJ_SIGNAL: u32 = 0xc5;
const NR_SYNCOBJ_TIMELINE_WAIT: u32 = 0xca;
const NR_SYNCOBJ_QUERY: u32 = 0xcb;
const NR_SYNCOBJ_TRANSFER: u32 = 0xcc;
const NR_SYNCOBJ_TIMELINE_SIGNAL: u32 = 0xcd;
const NR_SYNCOBJ_EVENTFD: u32 = 0xcf;

/// The `DRM_VIRTGPU_*` block: `drm.h` puts the family at `DRM_COMMAND_BASE + 1`
/// and up (`0x41`), which is its own numbering and not this file's - the same
/// trap the lease family's comment above records, so these are written from the
/// header's `#define` list rather than chosen.
///
/// Only the requests this card can *answer* are dispatched below. The four that
/// name a resource handle - `RESOURCE_INFO` (`0x45`), `MAP` (`0x41`),
/// `WAIT` (`0x48`), `TRANSFER_TO_HOST` (`0x47`) - have no arm and keep the
/// tail's `ENOTTY`; `src/drm_virtgpu.rs`'s header says why that is the honest
/// thing until a resource can exist, rather than answering `ENOENT` for handles
/// a Linux card would accept. They still get an `arg_shape` row, because Linux's
/// ioctl layer copies every DRM struct in *before* the handler can refuse it, so
/// a caller whose pointer is not its own memory must hear `EFAULT` rather than
/// any refusal at all.
const NR_VIRTGPU_MAP: u32 = 0x41;
const NR_VIRTGPU_EXECBUFFER: u32 = 0x42;
const NR_VIRTGPU_GETPARAM: u32 = 0x43;
const NR_VIRTGPU_RESOURCE_CREATE: u32 = 0x44;
const NR_VIRTGPU_RESOURCE_INFO: u32 = 0x45;
const NR_VIRTGPU_TRANSFER_FROM_HOST: u32 = 0x46;
const NR_VIRTGPU_TRANSFER_TO_HOST: u32 = 0x47;
const NR_VIRTGPU_WAIT: u32 = 0x48;
const NR_VIRTGPU_GET_CAPS: u32 = 0x49;
const NR_VIRTGPU_RESOURCE_CREATE_BLOB: u32 = 0x4a;
const NR_VIRTGPU_CONTEXT_INIT: u32 = 0x4b;

/// `DRM_SYNCOBJ_CREATE_SIGNALED`: the object starts with a fence behind it.
const SYNCOBJ_CREATE_SIGNALED: u32 = 1 << 0;
/// `DRM_SYNCOBJ_HANDLE_TO_FD_FLAGS_EXPORT_SYNC_FILE` and its import twin: the
/// descriptor is a `sync_file` over a fence rather than the object itself.
const SYNCOBJ_EXPORT_SYNC_FILE: u32 = 1 << 0;
const SYNCOBJ_IMPORT_SYNC_FILE: u32 = 1 << 0;
/// `DRM_SYNCOBJ_*_FLAGS_TIMELINE` on both directions. This host's header has it
/// and the older upstream one does not.
///
/// The note here used to read "the measured card accepts it and does not
/// require it, so it is accepted and changes nothing", from an instrument that
/// sent the flag with a `point` of zero. Measured again with every combination
/// of flags and points (§7.4), that is half right and misses the half the flag
/// is *for*: it is what makes the request's `point` a **timeline** point rather
/// than a number the operation may ignore, and the two directions read it
/// differently. Both are honoured below - the export side in full, the import
/// side as far as this card's fence can carry it, with the rest named at the
/// one place it stops.
const SYNCOBJ_FLAGS_TIMELINE: u32 = 1 << 1;
/// The wait flags. `ALL` and `FOR_SUBMIT` are the two with behaviour;
/// `AVAILABLE` and `DEADLINE` are accepted and mean nothing to a card with no
/// engine to schedule against.
const SYNCOBJ_WAIT_ALL: u32 = 1 << 0;
const SYNCOBJ_WAIT_FOR_SUBMIT: u32 = 1 << 1;
const SYNCOBJ_WAIT_AVAILABLE: u32 = 1 << 2;
const SYNCOBJ_WAIT_DEADLINE: u32 = 1 << 3;
/// `DRM_SYNCOBJ_QUERY_FLAGS_LAST_SUBMITTED`. Nothing here is submitted and
/// unsignaled, so the last submitted point and the last signaled one are the
/// same number and the flag changes no answer - which is why it is accepted
/// rather than refused.
const SYNCOBJ_QUERY_LAST_SUBMITTED: u32 = 1 << 0;
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
/// `DRM_CAP_SYNCOBJ`: whether the card has the sync object family at all. The
/// real card answers 1, so answering 1 here is a promise the `SYNCOBJ_*` arms
/// below have to keep rather than a capability this card can decline.
const CAP_SYNCOBJ: u64 = 0x13;
/// `DRM_CAP_SYNCOBJ_TIMELINE`: whether those objects can carry a point.
///
/// It is the one Smithay asks before it will expose `wp_linux_drm_syncobj_v1`,
/// and the protocol is *only* about timelines - a client hands the compositor a
/// point on its own timeline rather than a fence per frame. So this is the cap
/// that decides whether explicit sync exists on this machine at all.
const CAP_SYNCOBJ_TIMELINE: u64 = 0x14;
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

/// `DRM_MODE_OBJECT_CRTC` / `_CONNECTOR` / `_PLANE` / `_FB`: the object-type
/// tags of `OBJ_GETPROPERTIES`/`OBJ_SETPROPERTY`, and the single *value* every
/// `OBJECT`-typed property carries. The four numbers are `<drm_mode.h>`'s, and
/// they are also what a caller compares against: drm-rs maps them to the
/// `ValueType` a property reports (`0xfbfbfbfb` is `Framebuffer`, `0xcccccccc`
/// is `CRTC`), so a tag answered wrong is a property the caller has no type for.
const OBJECT_CRTC: u32 = 0xcccc_cccc;
const OBJECT_CONNECTOR: u32 = 0xc0c0_c0c0;
const OBJECT_PLANE: u32 = 0xeeee_eeee;
const OBJECT_FB: u32 = 0xfbfb_fbfb;
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
/// `DRM_MODE_PROP_ATOMIC`: not a *type* like the bits above, but a statement
/// about who may see the property. Linux sets it on the properties an atomic
/// commit is made of and leaves those out of the lists it answers a client that
/// has not set `DRM_CLIENT_CAP_ATOMIC` - so the flag `GETPROPERTY` reports and
/// the skip the two list doors make are one fact, and [`ATOMIC_PROPS`] is where
/// this file keeps it.
const PROP_ATOMIC: u32 = 1 << 31;
/// The properties this card flags `ATOMIC`, in one place, because two readers
/// need the same answer: `GETPROPERTY` (which reports the flag and must answer
/// for a property the client cannot *see*) and `GETCONNECTOR`/
/// `OBJ_GETPROPERTIES` (which leave the properties out of their lists).
///
/// The list is a *measurement*, not a reading of `<drm_mode.h>`, and it was
/// taken on this host's card across two open file descriptions of it (§7.4):
/// every property of every connector, CRTC and plane with its flags, on a
/// descriptor that had set the cap. What it says is that `ATOMIC` marks exactly
/// the object-typed properties (`CRTC_ID` on the connector, `FB_ID`/`CRTC_ID` on
/// the planes), the plane's two rectangles and its two fences, and the CRTC's
/// `ACTIVE`/`MODE_ID`/`OUT_FENCE_PTR` - while `type`, `IN_FORMATS`, `EDID`,
/// `DPMS`, `non-desktop`, `max bpc`, `vrr_capable`, `TILE` and the colour
/// properties are not marked and are shown to every client.
///
/// One property of this card is deliberately absent from the list: `pixel blend
/// mode` is not measurable on the host at all - its planes carry no blend
/// property, so nothing there says whether Linux would mark it - and marking a
/// property because its neighbours are marked is the guess this file refuses
/// elsewhere.
const ATOMIC_PROPS: [u32; 15] = [
    PROP_CRTC_ID,
    PROP_ACTIVE,
    PROP_MODE_ID,
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
    PROP_IN_FENCE_FD,
    PROP_OUT_FENCE_PTR,
];
/// `DRM_PLANE_TYPE_PRIMARY`: the one plane is the CRTC's primary surface.
const PLANE_TYPE_PRIMARY: u64 = 1;
/// `DRM_PLANE_TYPE_CURSOR`, the `type` value that makes the second plane the
/// cursor rather than an overlay.
const PLANE_TYPE_CURSOR: u64 = 2;
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

/// The 32-bit fourccs `ADDFB2` accepts: the two spellings whose *bytes* are in
/// the order this scanout holds - blue, green, red, then an unused or alpha byte.
///
/// `fourcc_code` packs the four named characters low byte first, so
/// `DRM_FORMAT_ARGB8888` is `0x3432_5241` ("AR24") and its bytes in memory are
/// B, G, R, A - the same order as `XRGB8888`, which differs only in what the
/// fourth byte means. `DRM_FORMAT_XBGR8888` ("XB24") is that 32-bit word read the
/// other way round in memory, bytes R, G, B, X, and `ABGR8888` with it.
///
/// Those two are therefore *refused*, which is a narrower list than this card
/// used to publish. Presentation here is a verbatim row copy into a BGRA block
/// (`drivers::virtio::gpu`) and nothing in the path can swap channels the way a
/// real plane's scaler does, so accepting them would put a client's red where
/// its blue belongs, silently and at full frame rate. `IN_FORMATS` is the same
/// statement in the other direction, and `t-drm-props` pins that the two agree.
///
/// The byte order is measured rather than assumed: `crate::term::blit`'s `put`
/// writes a pixel as blue, green, red, 0, and that is the console this machine
/// has been showing correctly since it was written.
///
/// Both constants also had to be *corrected* rather than merely narrowed. The
/// pair here used to be `0x3432_4152` and `0x3432_4142`, which decode to "RA24"
/// (`RGBA8888`) and "BA24" (`BGRA8888`) - so `ADDFB2` was accepting two formats
/// it never named and refusing the one it did, which is how the cursor plane's
/// buffer format came to be un-bindable.
const DRM_FORMAT_XRGB8888: u32 = 0x3432_5258;
const DRM_FORMAT_ARGB8888: u32 = 0x3432_5241;
/// `DRM_MODE_FB_MODIFIERS`: the bit in `struct drm_mode_fb_cmd2.flags` that says
/// the `modifier[]` array is meaningful. Named at module scope because both
/// directions read it - `ADDFB2` refuses a modifier that arrives without it, and
/// `GETFB2` sets it only for a framebuffer that was made with one, which is the
/// same statement about the same array.
const FB_MODIFIERS: u32 = 1 << 1;

/// A dumb buffer's pages are found in the mmap address space at
/// `handle * PAGES_PER_BUFFER * PAGE`, so `mmap_frame` can decode the buffer
/// back out of an absolute page index. 65536 pages (256 MiB) per slot is
/// comfortably above any single frame.
const PAGES_PER_BUFFER: usize = 1 << 16;
/// Cap on buffers and framebuffers the card will track.
const MAX_HANDLES: usize = 64;

/// The device resource a buffer backs, when it is a `DRM_VIRTGPU_*` one.
///
/// A dumb buffer that `CREATE_DUMB` made has none: its pixels reach the screen
/// through this card's own scanout copy (`copy_to_scanout`), which reads the
/// frames directly, so no device resource is involved. One that
/// `DRM_VIRTGPU_RESOURCE_CREATE` made does, because that is what the request
/// *is* - and the family's other three requests name it.
struct Resource {
    /// The id the device knows the resource by. This card chooses it (the device
    /// does not allocate resource ids - Linux's driver takes them from an ida,
    /// and so does this), and it starts above the scanout's own framebuffer,
    /// which is `virtgpu_abi::RESOURCE_FB` and is not in this table.
    id: u32,
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
    size: u64,
    /// Whether this is a 3D (virgl) resource rather than a 2D one.
    ///
    /// It is what decides which transfer a `TRANSFER_TO_HOST` becomes: Linux
    /// routes a virgl resource through the 3D command and a 2D one through
    /// `TRANSFER_TO_HOST_2D`, because a 3D resource belongs to a context and the
    /// 2D command has none to address it in.
    virgl: bool,
}

/// One dumb buffer: its geometry and the physical pages behind it.
struct Dumb {
    width: u32,
    height: u32,
    pitch: u32,
    bpp: u32,
    /// One physical frame per page of the buffer, grown on the first write or
    /// map that touches it (the inode holds a reference on each; every process
    /// that maps it takes its own via `clone_frame_ref`). A `DRM_VIRTGPU_*`
    /// resource has all of its pages made up front instead, because the device
    /// was handed their addresses as its backing and cannot be handed a page
    /// that does not exist yet.
    frames: Vec<usize>,
    // The card holds one reference per frame until DESTROY_DUMB; frames are
    // released then, and only actually recycled once no live mapping still
    // references them (each mapping took its own via clone_frame_ref).
    /// Set when this buffer is a `DRM_VIRTGPU_*` resource rather than a plain
    /// dumb buffer. It is what `RESOURCE_INFO` reports (and what
    /// `TRANSFER_TO_HOST` needs), and it is also what the handle's release has
    /// to tell the device about - see [`destroy_handle`].
    resource: Option<Resource>,
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
            resource: None,
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
    fn resource(
        width: u32,
        height: u32,
        frames: Vec<usize>,
        id: u32,
        size: u64,
        virgl: bool,
    ) -> Dumb {
        let mut buf = Dumb::new(width, height, 32);
        buf.frames = frames;
        buf.resource = Some(Resource { id, size, virgl });
        buf
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
    /// The fourcc this framebuffer was made from: what `ADDFB2` was given, or
    /// what Linux's `drm_mode_legacy_fb_format` synthesizes from `bpp`/`depth`
    /// for the legacy `ADDFB`, whose struct has no format field at all. `0` when
    /// neither names one, which `GETFB2` reports as such rather than inventing a
    /// layout the caller did not ask for. [`legacy_fb_format`] is that mapping.
    format: u32,
    /// The format modifier, `0` unless `ADDFB2` carried
    /// `DRM_MODE_FB_MODIFIERS`. Kept rather than discarded so that `GETFB2` can
    /// report how the framebuffer was actually made; the plane is linear either
    /// way, which is the only modifier `ADDFB2` accepts.
    modifier: u64,
    /// The open file description this framebuffer was made through, which is
    /// what Linux's `struct drm_file` is: a framebuffer belongs to the
    /// descriptor that created it and is released when that description's last
    /// descriptor closes (`drm_file_free` → `drm_fb_release`). Without the
    /// owner this card's framebuffers outlive every session that made them,
    /// and `GETRESOURCES` then lists ids no client can account for.
    owner: u64,
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
    /// The next device resource id a `DRM_VIRTGPU_RESOURCE_CREATE` will take.
    ///
    /// This card's to hand out, not the device's: a resource id is a field the
    /// *request* carries, so the layer that has to remember which id belongs to
    /// which handle is the layer that chooses it. It starts above the scanout's
    /// own framebuffer (`virtgpu_abi::RESOURCE_FB`), which was this driver's
    /// before the family existed and is not in the handle table.
    next_res_id: u32,
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
    /// The cursor plane: the framebuffer bound to it, the CRTC it serves, where
    /// its hotspot sits on the scanout, and how far into the image that hotspot
    /// is (what `MODE_CURSOR2`'s `hot_x`/`hot_y` carry, and 0,0 for the older
    /// struct, whose arrow-tip convention it defaults to).
    ///
    /// These are *not* the primary plane's kind of bookkeeping - they are what
    /// the composition reads on every flush, which is how a pointer moves
    /// without the compositor redrawing anything.
    cursor_fb: u32,
    cursor_crtc: u32,
    cursor_pos: (i32, i32),
    cursor_hotspot: (i32, i32),
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
    /// The cursor plane's own `IN_FORMATS`, from [`CURSOR_FORMATS`] and this
    /// card's one linear modifier. Separate from the primary plane's for the
    /// reason a real card keeps them separate: a format list is a fact about a
    /// *plane*, and the two planes do not scan the same set.
    cursor_in_formats_bytes: Option<Vec<u8>>,
    /// The PRIME identity table (see [`PrimeRecord`]).
    prime: Vec<PrimeRecord>,
    /// Sequence number for flip/vblank events, echoed back to the reader.
    seq: u32,
    /// Queued `drmEvent` objects awaiting `read` on the card fd: (type,
    /// payload-u64-words). Only flip-completion is produced.
    events: VecDeque<drm_event::Vblank>,
    /// The sync objects clients have made, each with the open file description
    /// that made it - which is what makes a handle mean something to exactly one
    /// client. A record dies with its descriptor (`DESTROY`, or the last
    /// reference to the description closing), and the object itself lives on
    /// while a `SYNCOBJ_HANDLE_TO_FD` descriptor names it, which is the sharing
    /// the descriptor exists for.
    syncobjs: Vec<Option<SyncObjRec>>,
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
    atomic_ofds: Vec<u64>,
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
    next_res_id: crate::gpu::abi::RESOURCE_FB + 1,
    fbs: Vec::new(),
    current_fb: 0,
    conn_crtc: CRTC_ID,
    dpms: 0,
    crtc_active: true,
    crtc_mode_id: 0,
    plane_crtc: CRTC_ID,
    plane_dst: (0, 0, 0, 0),
    plane_src: (0, 0, 0, 0),
    cursor_fb: 0,
    cursor_crtc: CRTC_ID,
    cursor_pos: (0, 0),
    cursor_hotspot: (0, 0),
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
}));

/// The `drm_version` answer: the KMS ABI version this kernel speaks (4.0), and
/// the *driver's* name - which on Linux is the hardware driver's, and for this
/// machine's card is `virtio_gpu`.
///
/// The name is not decoration, and this answered `"mpos"` until that was
/// measured. Linux's `drm_version.name` is `drm_dev->driver->name`, and Mesa
/// maps it to a renderer: its kmsro table is keyed by the kernel driver name -
/// `libgallium` carries the string `virtio_gpu` beside `kms_swrast` - and a
/// card whose name is in no table gets the software rasteriser. Measured on the
/// GL card: with the name `"mpos"`, `eglinfo`'s every platform reported
/// `llvmpipe (LLVM 23.1.2, 128 bits)` on a card with 3D behind it, which is
/// what a compositor refusing a *software* device acts on. A private spelling
/// in a field a caller identifies the driver by is the shape `AGENTS.md` §4.3
/// forbids; `virtio_gpu` is the name Linux gives the very device this card is.
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
    const FIELDS: [(&[u8], usize, usize); 3] =
        [(b"virtio_gpu", 16, 24), (b"", 32, 40), (b"", 48, 56)];

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
    let Some(card) = inode.as_any().downcast_ref::<DrmCard>() else {
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
        NR_SET_CLIENT_CAP => set_client_cap(arg, ofd),
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
        NR_GETCONNECTOR => get_connector(arg, ofd),
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
                if want >= 1 && !ids.is_null() && user_array(ids as usize, 2, 4, true) {
                    ids.write_unaligned(PLANE_ID);
                    ids.add(1).write_unaligned(CURSOR_PLANE_ID);
                }
                ((arg + 8) as *mut u32).write_unaligned(2);
            }
            0
        }
        NR_GETPLANE => get_plane(arg),
        NR_GETPROPERTY => get_property(arg),
        NR_SETPROPERTY => set_property(arg),
        NR_OBJ_GETPROPERTIES => obj_get_properties(arg, ofd),
        NR_OBJ_SETPROPERTY => obj_set_property(arg),
        NR_MODE_ATOMIC => mode_atomic(arg),
        NR_CREATE_BLOB => create_blob(arg),
        // The blob store: a mode arrives as a blob and a damage-rectangle
        // list does too, so the caller that made one can read it back.
        NR_GETPROPBLOB => get_propblob(arg),
        NR_DESTROY_BLOB => destroy_blob(arg),
        NR_GETFB => get_fb(arg),
        // The two-call framebuffer query with the format in it, which the
        // legacy `GETFB` above cannot spell.
        NR_GETFB2 => get_fb2(arg),
        // Refused, and named so that the refusal is this card's answer rather
        // than the catch-all's. Both carry a measurement in their doc comment.
        NR_MODE_GETGAMMA | NR_MODE_SETGAMMA => gamma_refused(),
        NR_CRTC_GET_SEQUENCE | NR_CRTC_QUEUE_SEQUENCE => sequence_refused(),
        NR_ADDFB => add_fb(arg, ofd),
        NR_ADDFB2 => add_fb2(arg, ofd),
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
        // The cursor plane's legacy door. Both spellings carry the same request
        // with the hotspot appended to the second, so one handler serves them.
        NR_MODE_CURSOR | NR_MODE_CURSOR2 => mode_cursor(arg, nr == NR_MODE_CURSOR2, card.lease()),
        NR_MODE_CREATE_LEASE => create_lease(card, minor, ofd, arg),
        NR_MODE_LIST_LESSES => list_leases(card, arg),
        NR_MODE_REVOKE_LEASE => revoke_lease(card, ofd, arg),
        NR_MODE_GET_LEASE => get_lease(arg),
        // The sync object family. Every one of these is per *client* - the
        // handle a request names means something only next to the description
        // that made it - which is why they are the only arms here that take
        // `ofd`, and why two clients can each have a handle 1.
        NR_SYNCOBJ_CREATE => syncobj_create(ofd, arg),
        NR_SYNCOBJ_DESTROY => syncobj_destroy(ofd, arg),
        NR_SYNCOBJ_HANDLE_TO_FD => syncobj_handle_to_fd(ofd, arg),
        NR_SYNCOBJ_FD_TO_HANDLE => syncobj_fd_to_handle(ofd, arg),
        NR_SYNCOBJ_WAIT => syncobj_wait(ofd, arg),
        NR_SYNCOBJ_RESET => syncobj_reset(ofd, arg),
        NR_SYNCOBJ_SIGNAL => syncobj_signal(ofd, arg),
        NR_SYNCOBJ_TIMELINE_WAIT => syncobj_timeline_wait(ofd, arg),
        NR_SYNCOBJ_QUERY => syncobj_query(ofd, arg),
        NR_SYNCOBJ_TRANSFER => syncobj_transfer(ofd, arg),
        NR_SYNCOBJ_TIMELINE_SIGNAL => syncobj_timeline_signal(ofd, arg),
        NR_SYNCOBJ_EVENTFD => syncobj_eventfd(ofd, arg),
        NR_CREATE_DUMB => create_dumb(arg),
        NR_MAP_DUMB => map_dumb(arg),
        // The `DRM_VIRTGPU_*` family. What a caller hears here is not one
        // refusal but a table of per-request answers - a card with no 3D behind
        // it succeeds at some of these, refuses others with `ENOSYS` and others
        // with `EINVAL` - and `crate::gpu::virtgpu` holds that table with the
        // measurement beside every entry.
        //
        // All eleven are dispatched now. The four that name a resource handle
        // were withheld in the previous pass and are here because a resource can
        // exist: `RESOURCE_CREATE` makes one, and `INFO`, `MAP`, `WAIT` and
        // `TRANSFER_TO_HOST` answer for it. `TRANSFER_FROM_HOST` and
        // `EXECBUFFER` stay refused - those are the 3D path, and the measurement
        // says a card without virgl refuses them too.
        NR_VIRTGPU_GETPARAM => virtgpu_getparam(arg),
        NR_VIRTGPU_RESOURCE_CREATE => virtgpu_resource_create(arg, ofd),
        NR_VIRTGPU_RESOURCE_INFO => virtgpu_resource_info(arg),
        NR_VIRTGPU_MAP => virtgpu_map(arg),
        NR_VIRTGPU_WAIT => virtgpu_wait(arg),
        NR_VIRTGPU_TRANSFER_TO_HOST => virtgpu_transfer_to_host(arg, ofd),
        NR_VIRTGPU_GET_CAPS => virtgpu_get_caps(arg),
        NR_VIRTGPU_EXECBUFFER => virtgpu_execbuffer(arg, ofd),
        NR_VIRTGPU_TRANSFER_FROM_HOST => virtgpu_transfer_from_host(arg, ofd),
        NR_VIRTGPU_CONTEXT_INIT => virtgpu_context_init(arg, ofd),
        NR_VIRTGPU_RESOURCE_CREATE_BLOB => crate::gpu::virtgpu::RESOURCE_CREATE_BLOB,
        _ => -25, // ENOTTY
    }
}

/// What the GPU driver negotiated and the cap sets it fetched, published once.
///
/// The ioctl path cannot reach the device - the render task owns it - and none
/// of these values changes after negotiation, so they are handed over once
/// (`video/render.rs`, at the task's start) rather than asked for per request.
/// Before that publication this is the "no card" shape, which is what a machine
/// whose GPU never came up owes a caller: every 3D answer `0`, every 3D request
/// refused, rather than anything a caller could read as a capability.
struct Published {
    /// The card, as `drm_virtgpu::Card` describes it.
    card: crate::gpu::virtgpu::Card,
    /// How many cap sets the *device* said it had.
    ///
    /// Kept apart from `capsets.len()`, which can be smaller: a set whose fetch
    /// failed is skipped, and the walk stays honest about the others. This is
    /// what `GET_CAPS`'s `ENOSYS` is gated on, because Linux's gate is the
    /// device's own count - so a card that offered two sets and produced one
    /// answers `EINVAL` for the other rather than `ENOSYS` for the family.
    num_capsets: u32,
    /// The sets that were fetched, in the order the device named them.
    capsets: Vec<crate::gpu::virtgpu::Capset>,
}

static PUBLISHED: Mutex<RefCell<Published>> = Mutex::new(RefCell::new(Published {
    card: crate::gpu::virtgpu::Card::NONE,
    num_capsets: 0,
    capsets: Vec::new(),
}));

/// Hand the DRM layer what the GPU driver negotiated.
///
/// Called once, by the task that owns the device. It is a plain setter rather
/// than something the ioctl path could also reach, because there is exactly one
/// moment at which the answer is knowable and exactly one task that knows it.
pub fn publish_card(
    card: crate::gpu::virtgpu::Card,
    num_capsets: u32,
    capsets: &[crate::gpu::virtgpu::Capset],
) {
    critical_section::with(|cs| {
        let mut published = PUBLISHED.borrow(cs).borrow_mut();
        published.card = card;
        published.num_capsets = num_capsets;
        published.capsets.clear();
        published.capsets.extend_from_slice(capsets);
    });
}

/// The card to answer a `DRM_VIRTGPU_*` request from.
fn card() -> crate::gpu::virtgpu::Card {
    critical_section::with(|cs| PUBLISHED.borrow(cs).borrow().card)
}

/// How many cap sets the device claimed - see [`Published::num_capsets`].
fn num_capsets() -> u32 {
    critical_section::with(|cs| PUBLISHED.borrow(cs).borrow().num_capsets)
}

/// The cap set `id` offers at `version`, or `None` if it offers neither.
///
/// Linux's search, down to the comparison: it accepts `id == cap_set_id` with
/// `max_version >= cap_set_ver`, so a caller may ask for any version the set is
/// good for rather than only the newest one it could have named. A set that is
/// not there, or a version past what it has, is `EINVAL` - which is what `None`
/// becomes at the one place that asks.
fn capset(id: u32, version: u32) -> Option<crate::gpu::virtgpu::Capset> {
    critical_section::with(|cs| {
        PUBLISHED
            .borrow(cs)
            .borrow()
            .capsets
            .iter()
            .find(|c| c.id == id && c.max_version >= version)
            .cloned()
    })
}

/// One open file description's 3D context.
///
/// Per *description* rather than per process, for the reason the master is
/// (`MASTER_OFD`): the identity that survives a seat manager handing a card over
/// `SCM_RIGHTS` is the description, and Linux's context hangs off
/// `drm_file->driver_priv`, which is the same thing.
struct Context {
    /// The file description that made it - `crate::vfs::File::ofd`.
    ofd: u64,
    /// The id the device knows the context by. This card chooses it, from 1: 0 is
    /// the protocol's "no context", which is what every 2D header carries.
    id: u32,
    /// Whether the *device* has been told to make it.
    ///
    /// Linux's `context_created`, and the whole reason `CONTEXT_INIT` can answer
    /// `EEXIST`: the first 3D command a file makes creates the context
    /// implicitly, so an explicit request arrives second. Only `ensure_context`
    /// sets it.
    created: bool,
    /// The next fence id to hand out for this context.
    ///
    /// Per context because a fence id names a command within *that* context's
    /// rings, which is why Linux allocates a `dma_fence_context` per file rather
    /// than numbering globally. It starts at 1 so 0 keeps meaning "no fence".
    next_fence: u64,
}

/// The contexts on the card, one per file description that needed one.
static CONTEXTS: Mutex<RefCell<Vec<Context>>> = Mutex::new(RefCell::new(Vec::new()));

/// The next context id. Never reused within a boot, so an id names one context
/// for the life of the machine.
static NEXT_CTX_ID: AtomicU32 = AtomicU32::new(1);

/// This file's context: its id, and whether the device has been told to make it.
///
/// Allocating an id is bookkeeping and sends nothing - the command that *uses* a
/// context is what has to be told, and it is `ensure_context` that does the
/// telling.
fn context_of(ofd: u64) -> (u32, bool) {
    critical_section::with(|cs| {
        let mut contexts = CONTEXTS.borrow(cs).borrow_mut();
        if let Some(existing) = contexts.iter().find(|c| c.ofd == ofd) {
            return (existing.id, existing.created);
        }
        let id = NEXT_CTX_ID.fetch_add(1, Ordering::Relaxed);
        contexts.push(Context {
            ofd,
            id,
            created: false,
            next_fence: 1,
        });
        (id, false)
    })
}

/// Make sure the device has been told to create this file's context, and answer
/// its id.
///
/// The implicit creation Linux does from the first command that needs a context:
/// `virtio_gpu_create_context` runs from resource-create, the transfers and
/// execbuffer alike, and it is what makes an explicit `CONTEXT_INIT` afterwards
/// answer `EEXIST`. A context whose command the device refused leaves `created`
/// false, so the next request tries again rather than believing in a context that
/// is not there.
///
/// `context_init` is the wire word, which is the cap set id and nothing else -
/// Linux's `vfpriv->context_init` is only ever `|`-ed with a cap set id. The
/// implicit path passes zero, which is what a device without `CONTEXT_INIT` would
/// have got anyway.
fn ensure_context(ofd: u64, context_init: u32, name: &[u8]) -> Result<u32, isize> {
    let (id, created) = context_of(ofd);
    if created {
        return Ok(id);
    }
    let made =
        crate::drivers::video::render::job(crate::drivers::video::render::Job::ContextCreate {
            id,
            context_init,
            name: name.to_vec(),
        });
    if made != 0 {
        return Err(made);
    }
    critical_section::with(|cs| {
        if let Some(context) = CONTEXTS
            .borrow(cs)
            .borrow_mut()
            .iter_mut()
            .find(|c| c.ofd == ofd)
        {
            context.created = true;
        }
    });
    Ok(id)
}

/// The fence id for this file's next submitted command.
///
/// Zero when the file has no context, which cannot happen at a call site that has
/// just been through [`ensure_context`] - and zero is the protocol's "no fence",
/// so a mistake there is a missing fence rather than a wrong one.
fn take_fence(ofd: u64) -> u64 {
    critical_section::with(|cs| {
        let mut contexts = CONTEXTS.borrow(cs).borrow_mut();
        let Some(context) = contexts.iter_mut().find(|c| c.ofd == ofd) else {
            return 0;
        };
        let fence = context.next_fence;
        context.next_fence = context.next_fence.wrapping_add(1);
        fence
    })
}

/// `DRM_VIRTGPU_GETPARAM`: `struct drm_virtgpu_getparam { u64 param; u64 value; }`
/// where `value` is the *address* of the caller's answer rather than the answer.
///
/// Linux writes a four-byte `int` there, not the eight the field's width
/// suggests: the field is declared `__u64` and the copy is `sizeof(int)`, so a
/// caller that zeroed eight bytes finds the low half changed and the high half
/// as it left it. Writing eight would be the wrong shape in exactly the place
/// it is invisible, which is why the width is written down here.
///
/// The order is Linux's: an unknown parameter is `EINVAL` *before* the
/// destination is looked at, so a caller that asked for nothing hears what is
/// wrong with the question rather than with its buffer. Measured - `GETPARAM(0)`,
/// `(9)` and `(100)` all answer `EINVAL` on a real card.
///
/// The card the answers come from is the one the GPU driver published
/// ([`Published`]); before that publication it is `Card::NONE`, because the
/// answers are a statement about what this kernel can do and a machine whose GPU
/// has not come up can do none of it.
fn virtgpu_getparam(arg: usize) -> isize {
    let param = unsafe { (arg as *const u64).read_unaligned() };
    let Ok(value) = crate::gpu::virtgpu::param(param, &card()) else {
        return crate::err::EINVAL;
    };
    let out = unsafe { ((arg + 8) as *const u64).read_unaligned() } as *mut i32;
    if out.is_null() || !crate::uaccess::buffer_ok(out as usize, 4, true, true) {
        return crate::err::EFAULT;
    }
    unsafe { out.write_unaligned(value as i32) };
    0
}

/// `DRM_VIRTGPU_GET_CAPS`: `struct drm_virtgpu_get_caps { u32 cap_set_id;
/// u32 cap_set_ver; u64 addr; u32 size; }`, where `addr` is the caller's buffer
/// and `size` how much of it may be written.
///
/// Linux's order, which the measurement backs at every step: a card with no cap
/// sets at all answers `ENOSYS` for the whole request; a caller that asked for
/// nothing is `EINVAL`; a set that is not offered, or not at the version asked
/// for, is `EINVAL`; and only then is the buffer looked at. A copy *smaller* than
/// the set is a **success** - `min(caller, device)`, which is the half of this
/// rule a guess gets wrong, and `drm_virtgpu::caps_may_copy` is where it lives.
///
/// The bytes come out of the cache the render task built at bring-up, so this
/// never touches the device. `GET_CAPS` is a request a compositor makes on every
/// startup, and a control-queue round trip per set would be one per startup.
fn virtgpu_get_caps(arg: usize) -> isize {
    // struct drm_virtgpu_get_caps: cap_set_id@0, cap_set_ver@4, addr@8 (u64),
    // size@16.
    let id = unsafe { (arg as *const u32).read_unaligned() };
    let version = unsafe { ((arg + 4) as *const u32).read_unaligned() };
    let addr = unsafe { ((arg + 8) as *const u64).read_unaligned() } as usize;
    let size = unsafe { ((arg + 16) as *const u32).read_unaligned() };

    if num_capsets() == 0 {
        return crate::gpu::virtgpu::CAPS;
    }
    let Some(set) = capset(id, version) else {
        // Not offered, or a version past what the set has. Linux answers the
        // same `EINVAL` for both, so there is nothing to tell apart here.
        return crate::err::EINVAL;
    };
    let Ok(copy) = crate::gpu::virtgpu::caps_may_copy(size, set.max_size) else {
        return crate::err::EINVAL;
    };
    if !crate::uaccess::buffer_ok(addr, copy, true, true) {
        return crate::err::EFAULT;
    }
    // SAFETY: the destination is the caller's, and `buffer_ok` has just checked
    // that all `copy` bytes of it are mapped and writable. The source is the
    // cache's own `Vec`, which is exactly `max_size` bytes: a fetch that came
    // back short is not cached at all (`gpu_ctrl::get_capset` refuses it), so
    // `copy <= max_size` is within it.
    unsafe {
        core::ptr::copy_nonoverlapping(set.bytes.as_ptr(), addr as *mut u8, copy);
    }
    0
}

/// `DRM_VIRTGPU_CONTEXT_INIT`: `struct drm_virtgpu_context_init { u32 num_params;
/// u32 pad; u64 ctx_set_params; }`, where that pointer is `num_params` of
/// `struct drm_virtgpu_context_set_param { u64 param; u64 value; }`.
///
/// Linux's order, and two of its steps are worth following exactly:
///
/// 1. `EINVAL` unless the device has **both** `CONTEXT_INIT` and `VIRGL`, which is
///    why this is one of the refusals a card without 3D makes with `EINVAL` rather
///    than `ENOSYS`.
/// 2. `EINVAL` for more than four parameters, before the array is read at all.
/// 3. **`EEXIST` if this file already has a context** - and that comes *before*
///    the parameters are validated. The first 3D command made one implicitly, so a
///    second explicit request is not a malformed request, it is a late one, and
///    the measured card is where this shows: `CONTEXT_INIT` answers `EEXIST` on a
///    virgl machine.
/// 4. The parameters, each at most once. `CAPSET_ID` must be one the device
///    offers; `DEBUG_NAME` is the caller's string; `NUM_RINGS` and
///    `POLL_RINGS_MASK` are accepted and not acted on, because the only thing
///    either gates is the multi-ring fence protocol, and this kernel refuses that
///    protocol's flag in `EXECBUFFER` rather than half-driving it.
fn virtgpu_context_init(arg: usize, ofd: u64) -> isize {
    let num_params = unsafe { (arg as *const u32).read_unaligned() };
    let params = unsafe { ((arg + 8) as *const u64).read_unaligned() } as usize;

    let card = card();
    if !card.context_init || !card.virgl {
        return crate::gpu::virtgpu::CONTEXT_INIT;
    }
    if num_params > 4 {
        return libc_einval();
    }
    if context_of(ofd).1 {
        return -17; // EEXIST
    }
    if num_params > 0 && !crate::uaccess::buffer_ok(params, num_params as usize * 16, false, true) {
        return -14; // EFAULT
    }

    // The wire word is the cap set id and nothing else: Linux only ever `|`-es
    // that into it, and `VIRTIO_GPU_CONTEXT_INIT_CAPSET_ID_MASK` is the low byte
    // for that reason.
    let mut context_init = 0u32;
    let mut name: Option<Vec<u8>> = None;
    for index in 0..num_params as usize {
        let param = unsafe { ((params + index * 16) as *const u64).read_unaligned() };
        let value = unsafe { ((params + index * 16 + 8) as *const u64).read_unaligned() };
        match param {
            // VIRTGPU_CONTEXT_PARAM_CAPSET_ID
            1 => {
                if context_init & 0xff != 0 || value > 31 || card.capset_ids & (1u32 << value) == 0
                {
                    return libc_einval();
                }
                context_init |= value as u32;
            }
            // VIRTGPU_CONTEXT_PARAM_NUM_RINGS / _POLL_RINGS_MASK: accepted and
            // not acted on. Only the ring-count fence protocol reads either, and
            // `EXECBUFFER` refuses `VIRTGPU_EXECBUF_RING_IDX` outright, so a
            // caller that asked for rings cannot reach the path that would use
            // them. Refusing here instead would turn away a caller Linux serves.
            2 | 3 => {}
            // VIRTGPU_CONTEXT_PARAM_DEBUG_NAME
            4 => {
                if name.is_some() || value == 0 {
                    return libc_einval();
                }
                if !crate::uaccess::buffer_ok(value as usize, 64, false, true) {
                    return -14; // EFAULT
                }
                let mut bytes = [0u8; 64];
                // SAFETY: `buffer_ok` has just checked all 64 bytes are mapped
                // readable in the caller, and `bytes` is 64 bytes of local.
                unsafe {
                    core::ptr::copy_nonoverlapping(
                        value as *const u8,
                        bytes.as_mut_ptr(),
                        bytes.len(),
                    );
                }
                // A name, not a string: the field is 64 bytes and the caller's is
                // a C string, so it ends at the first NUL. `strnlen`'s job.
                let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
                name = Some(bytes[..end].to_vec());
            }
            _ => return libc_einval(),
        }
    }
    let name = name.unwrap_or_else(|| b"mpos".to_vec());
    match ensure_context(ofd, context_init, &name) {
        Ok(_) => 0,
        Err(errno) => errno,
    }
}

/// `DRM_VIRTGPU_EXECBUFFER`: `struct drm_virtgpu_execbuffer { u32 flags;
/// u32 size; u64 command; u64 bo_handles; u32 num_bo_handles; s32 fence_fd;
/// u32 ring_idx; u32 syncobj_stride; u32 num_in_syncobjs; u32 num_out_syncobjs;
/// u64 in_syncobjs; u64 out_syncobjs; }` - 64 bytes.
///
/// Four things are refused rather than ignored, and each because ignoring it would
/// be a promise:
///
/// - **`FENCE_FD_IN`/`FENCE_FD_OUT` and the syncobj fields.** They are explicit
///   synchronisation, which needs `sync_file` and `drm_syncobj` - both of which
///   this kernel refuses (`SYNCOBJ_*`), so a descriptor or a handle here names a
///   synchronisation primitive that does not exist. `EINVAL`, not a silent
///   accepted-and-dropped fence, which is how a caller's buffer would be read with
///   no ordering at all while it believed otherwise.
/// - **`VIRTGPU_EXECBUF_RING_IDX`.** One ring is driven; a submit on another would
///   be a command nothing ever completes.
/// - **A `bo_handle` this card never made.** Linux answers `ENOENT` there, and the
///   handles are checked even though they do not reach the wire (see below),
///   because that errno is what a caller branches on.
///
/// What is *not* checked is `size`: Linux does not check it either - it hands
/// `size` bytes to `vmemdup_user` and forwards whatever comes back - so a
/// zero-length stream is submitted as a zero-length stream rather than invented
/// into an error. `buffer_ok` is what bounds it, and it bounds it by the caller's
/// own mapping.
///
/// The stream is forwarded untouched. Linux's `virtio_gpu_init_submit` is
/// `vmemdup_user(exbuf->command, exbuf->size)` and nothing more, and the
/// `bo_handles` become a guest-side reservation list there and never a wire
/// element - `virtgpu_abi::CmdSubmit` has the long version.
fn virtgpu_execbuffer(arg: usize, ofd: u64) -> isize {
    let field = |offset: usize| unsafe { ((arg + offset) as *const u32).read_unaligned() };
    let (flags, size) = (field(0), field(4));
    let command = unsafe { ((arg + 8) as *const u64).read_unaligned() } as usize;
    let handles = unsafe { ((arg + 16) as *const u64).read_unaligned() } as usize;
    let num_handles = field(24);
    let ring_idx = field(32);
    let (syncobj_stride, num_in_syncobjs, num_out_syncobjs) = (field(36), field(40), field(44));

    // Every flag this kernel knows, refused by name rather than as a group: an
    // unknown flag is a caller that expects something else to happen, and it gets
    // `EINVAL` rather than a submit that quietly did less.
    //
    // The capability gate comes first, because it is the whole family's answer on
    // a card with no 3D (measured: `ENOSYS`) and a caller that hears `EINVAL`
    // instead would go looking for the malformed field it does not have.
    if !card().virgl {
        return crate::gpu::virtgpu::EXECBUFFER;
    }
    const FENCE_FD_IN: u32 = 0x01;
    const FENCE_FD_OUT: u32 = 0x02;
    const RING_IDX: u32 = 0x04;
    if flags & (FENCE_FD_IN | FENCE_FD_OUT | RING_IDX) != 0 || flags & !0x07 != 0 {
        return libc_einval();
    }
    if ring_idx != 0 {
        return libc_einval();
    }
    if syncobj_stride != 0 || num_in_syncobjs != 0 || num_out_syncobjs != 0 {
        return libc_einval();
    }
    if size > 0 && !crate::uaccess::buffer_ok(command, size as usize, false, true) {
        return -14; // EFAULT
    }
    // The handles are resolved even though they never reach the device, because
    // Linux's answer for a handle this file does not hold is `ENOENT` and a caller
    // acts on it before it ever looks at the fence.
    if num_handles > 0 {
        if !crate::uaccess::buffer_ok(handles, num_handles as usize * 4, false, true) {
            return -14; // EFAULT
        }
        let known = critical_section::with(|cs| {
            let st = STATE.borrow(cs).borrow();
            (0..num_handles as usize).all(|index| {
                let handle =
                    unsafe { ((handles + index * 4) as *const u32).read_unaligned() } as usize;
                matches!(
                    st.dumb.get(handle.wrapping_sub(1)).and_then(|slot| slot.as_ref()),
                    Some(d) if d.resource.is_some()
                )
            })
        });
        if !known {
            return -2; // ENOENT
        }
    }

    let mut stream = Vec::new();
    stream.resize(size as usize, 0);
    if size > 0 {
        // SAFETY: `buffer_ok` has just checked `size` bytes at `command` are
        // mapped readable in the caller, and `stream` is `size` bytes of local.
        unsafe {
            core::ptr::copy_nonoverlapping(
                command as *const u8,
                stream.as_mut_ptr(),
                size as usize,
            );
        }
    }

    let ctx = match ensure_context(ofd, 0, b"mpos") {
        Ok(id) => id,
        Err(errno) => return errno,
    };
    let fence = take_fence(ofd);
    crate::drivers::video::render::job(crate::drivers::video::render::Job::Submit3d {
        ctx,
        ring_idx: 0,
        fence,
        stream,
    })
}

/// `DRM_VIRTGPU_TRANSFER_FROM_HOST`: the same struct as
/// `DRM_VIRTGPU_TRANSFER_TO_HOST`, and the other direction.
///
/// The asymmetry is Linux's and was the least guessable thing in the measurement:
/// a card without 3D transfers *to* the host through the 2D command and answers
/// this one `ENOSYS`, because the only command that reads a resource back is the
/// 3D one. So this is `ENOSYS` without 3D and a real transfer with it, which is
/// the opposite of a symmetric pair - and the reason both are written out.
fn virtgpu_transfer_from_host(arg: usize, ofd: u64) -> isize {
    let field = |offset: usize| unsafe { ((arg + offset) as *const u32).read_unaligned() };
    let handle = field(0) as usize;
    let box3d = crate::gpu::abi::Box3d {
        x: field(4),
        y: field(8),
        z: field(12),
        w: field(16),
        h: field(20),
        d: field(24),
    };
    let (level, offset, stride, layer_stride) = (field(28), field(32), field(36), field(40));

    if !card().virgl {
        return crate::gpu::virtgpu::TRANSFER_FROM_HOST;
    }
    let found = critical_section::with(|cs| {
        let st = STATE.borrow(cs).borrow();
        st.dumb
            .get(handle.wrapping_sub(1))
            .and_then(|slot| slot.as_ref())
            .and_then(|d| d.resource.as_ref())
            .map(|r| r.id)
    });
    let Some(id) = found else {
        return -2; // ENOENT
    };
    let ctx = match ensure_context(ofd, 0, b"mpos") {
        Ok(id) => id,
        Err(errno) => return errno,
    };
    let request = crate::gpu::abi::TransferHost3d {
        header: crate::gpu::abi::CtrlHeader::new(crate::gpu::abi::CMD_TRANSFER_FROM_HOST_3D),
        box3d,
        offset: offset as u64,
        resource_id: id,
        level,
        stride,
        layer_stride,
    };
    crate::drivers::video::render::job(crate::drivers::video::render::Job::TransferHost3d {
        to_host: false,
        ctx,
        request,
    })
}

/// `DRM_VIRTGPU_RESOURCE_CREATE`: allocate a buffer and tell the device to make
/// a resource of it.
///
/// Two answers, and the order they are given in is the point. The five field
/// checks a card without 3D makes come first (`src/drm_virtgpu.rs`), so a caller
/// whose *arguments* are wrong hears what is wrong with them; a caller whose
/// arguments are fine gets a resource, which on Linux it would have got all
/// along. Before this the good case was `ENOSYS`, and the reason it could not be
/// served was that telling the device is a control-queue command - so the work
/// goes to the render task as a job and this parks.
///
/// The frames are made *before* the device is told about them, and that order is
/// forced: the attach command names their addresses, so a page allocated after
/// it would be a page the device was never told to read.
///
/// The returned `bo_handle` is this card's GEM handle - the same space
/// `CREATE_DUMB` allocates in, so `MAP`, `ADDFB` and `GEM_CLOSE` take it - and
/// `res_handle` is the id the device knows the resource by.
fn virtgpu_resource_create(arg: usize, ofd: u64) -> isize {
    let field = |offset: usize| unsafe { ((arg + offset) as *const u32).read_unaligned() };
    let (target, width, height) = (field(0), field(12), field(16));
    let virgl = card().virgl;
    // The validation is the *non*-virgl branch of Linux's driver, which is why a
    // card with 3D skips it: the virgl branch forwards `target`, `depth`,
    // `array_size`, `last_level`, `nr_samples` and `flags` rather than checking
    // them, and the measurement is where that shows - `target 4` is `EINVAL`
    // without 3D and a resource with it.
    if !virgl
        && let Err(errno) = crate::gpu::virtgpu::resource_create_args_ok(
            target,
            field(20),
            field(32),
            field(28),
            field(24),
        )
    {
        return errno;
    }
    if width == 0 || height == 0 {
        return libc_einval();
    }
    // `target` is the switch, in Linux's virgl branch and here alike: 2 is the 2D
    // pipe and 4 is virgl, and the branch above is what keeps the second out of a
    // card that has no 3D.
    let three_d = target == 4;
    // The object's size is the caller's, or a page when it asked for zero -
    // Linux's rule, and the reason the instrument measured 4096 for a 64x64
    // resource. See `Resource::size`.
    let asked = field(48) as u64;
    let size = if asked == 0 { PAGE_SIZE as u64 } else { asked };
    let pages = (size as usize).div_ceil(PAGE_SIZE);
    if pages == 0 || pages > PAGES_PER_BUFFER {
        return -28; // ENOSPC
    }

    let mut frames = Vec::with_capacity(pages);
    let mut entries = Vec::with_capacity(pages);
    for _ in 0..pages {
        let Some(p) = crate::mm::alloc_frames(0) else {
            for p in frames.drain(..) {
                free_frames(p, 0);
            }
            return -12; // ENOMEM
        };
        unsafe {
            core::ptr::write_bytes(phys_to_virt(p) as *mut u8, 0, PAGE_SIZE);
        }
        frames.push(p);
        entries.push((p as u64, PAGE_SIZE as u32));
    }

    let id = critical_section::with(|cs| {
        let mut st = STATE.borrow(cs).borrow_mut();
        let id = st.next_res_id;
        st.next_res_id = st.next_res_id.wrapping_add(1);
        id
    });

    // The device is told, and this parks until it has answered - with no lock
    // held and no frames borrowed, because the task that answers is the render
    // task and a caller parked while holding either would be a task waiting for
    // something it is itself preventing.
    // The context is made *before* the target is consulted, because that is
    // Linux's order: `virtio_gpu_resource_create_ioctl` calls
    // `virtio_gpu_create_context` as soon as `has_virgl_3d` is set, and only then
    // looks at `target`. It is why a `CONTEXT_INIT` after a purely **2D** create
    // answers `EEXIST` on a virgl card, and matching it is cheaper than
    // explaining a divergence a caller can see for itself.
    let ctx = if virgl {
        match ensure_context(ofd, 0, b"mpos") {
            Ok(id) => Some(id),
            Err(errno) => {
                for p in frames.drain(..) {
                    free_frames(p, 0);
                }
                return errno;
            }
        }
    } else {
        None
    };
    let made = if three_d {
        // `three_d` implies `virgl` - the branch above is what keeps a 3D target
        // out of a card that has no 3D - so the context is always there.
        let ctx = ctx.expect("a 3D target is a virgl target, and virgl made a context");
        crate::drivers::video::render::job(crate::drivers::video::render::Job::Create3d {
            ctx,
            id,
            request: crate::gpu::abi::ResourceCreate3d {
                header: crate::gpu::abi::CtrlHeader::new(crate::gpu::abi::CMD_RESOURCE_CREATE_3D),
                resource_id: id,
                target,
                format: field(4),
                bind: field(8),
                width,
                height,
                depth: field(20),
                array_size: field(24),
                last_level: field(28),
                nr_samples: field(32),
                flags: field(36),
                padding: 0,
            },
            entries,
        })
    } else {
        crate::drivers::video::render::job(crate::drivers::video::render::Job::Create2d {
            id,
            width,
            height,
            entries,
        })
    };
    if made != 0 {
        for p in frames.drain(..) {
            free_frames(p, 0);
        }
        return made;
    }

    let mut frames = frames;
    let pushed = critical_section::with(|cs| {
        let mut st = STATE.borrow(cs).borrow_mut();
        if st.dumb.len() >= MAX_HANDLES {
            return None;
        }
        let idx = st.dumb.len();
        st.dumb.push(Some(Dumb::resource(
            width,
            height,
            core::mem::take(&mut frames),
            id,
            size,
            three_d,
        )));
        Some(idx + 1)
    });
    let Some(handle) = pushed else {
        // No handle slot left. The device has already made the resource, so it
        // is told to let go of it and the frames go back; neither may leak, and
        // the order of the two does not matter.
        let _ =
            crate::drivers::video::render::job(crate::drivers::video::render::Job::Unref { id });
        for p in frames.drain(..) {
            free_frames(p, 0);
        }
        return -28; // ENOSPC
    };

    unsafe {
        ((arg + 40) as *mut u32).write_unaligned(handle as u32); // bo_handle
        ((arg + 44) as *mut u32).write_unaligned(id); // res_handle
    }
    0
}

/// Back a plain dumb buffer with the `DRM_VIRTGPU_*` device resource Linux's
/// `CREATE_DUMB` would have made for it, the first time a caller addresses the
/// handle as one.
///
/// On Linux there is no such thing as "a dumb buffer that is not a resource":
/// `virtio_gpu_mode_dumb_create` goes through `virtio_gpu_object_create`, the
/// same object creation `RESOURCE_CREATE` uses, so `RESOURCE_INFO` names it and
/// `TRANSFER_TO_HOST` completes. Here a dumb buffer has been the card's own
/// pages and nothing else - the scanout copy reads them directly and the device
/// was never told - and the two answers that produced are named in
/// `crates/gpu/src/virtgpu.rs`'s header.
///
/// **The backing is made here rather than in `CREATE_DUMB`, and that is a
/// difference in this implementation and not in the ABI.** Every dumb buffer
/// would otherwise be eagerly backed and cost a device round trip at creation -
/// and the console, weston and the fbdev node all allocate through exactly that
/// path while none of them ever asks this question. A caller that asks gets
/// Linux's answer; a caller that does not pays nothing. What a client can
/// observe is the same either way: the id is opaque, and no request reports
/// whether a resource exists without asking about it.
///
/// The pages are materialised before the device is told, because the command
/// names their addresses as the backing and cannot be handed a page that does
/// not exist yet - the same reason `Dumb::resource` takes its frames already
/// made. The list is grown through `ensure_page`, so the frames stay the ones
/// the scanout and the fbdev node have been reading: the device's copy is a
/// *second* view of the same pixels, and a transfer is what moves them.
///
/// Answers `(the resource's id, its size)`, or `ENOENT` for a handle this card
/// never made. Idempotent: a buffer that already has one answers with it, which
/// is also what a second caller finds if two race.
fn back_dumb_with_a_resource(handle: usize) -> Result<(u32, u64), isize> {
    let existing = critical_section::with(|cs| -> Result<Option<(u32, u64)>, isize> {
        let st = STATE.borrow(cs).borrow();
        let Some(d) = st
            .dumb
            .get(handle.wrapping_sub(1))
            .and_then(|slot| slot.as_ref())
        else {
            return Err(-2); // ENOENT
        };
        Ok(d.resource.as_ref().map(|r| (r.id, r.size)))
    })?;
    if let Some(found) = existing {
        return Ok(found);
    }

    // Everything under the lock is arithmetic and `Vec` growth; the device round
    // trip below happens with no lock held, because the task that answers it is
    // the render task and a caller parked holding this lock would be waiting for
    // something it is itself preventing (`RESOURCE_CREATE` says the same).
    let prepared = critical_section::with(|cs| -> Result<Option<_>, isize> {
        let mut st = STATE.borrow(cs).borrow_mut();
        let Some(d) = st
            .dumb
            .get_mut(handle.wrapping_sub(1))
            .and_then(|s| s.as_mut())
        else {
            return Err(-2); // ENOENT
        };
        if d.resource.is_some() {
            return Ok(None); // another caller got there first
        }
        let pages = (d.size() as usize).div_ceil(PAGE_SIZE);
        for page in 0..pages {
            if d.ensure_page(page) == 0 {
                // The pages made so far stay: they are this buffer's own, and
                // `DESTROY_DUMB` releases them whether a resource was made or
                // not.
                return Err(-12); // ENOMEM
            }
        }
        let entries: Vec<(u64, u32)> = d
            .frames
            .iter()
            .take(pages)
            .map(|&p| (p as u64, PAGE_SIZE as u32))
            .collect();
        Ok(Some((d.width, d.height, entries, d.size())))
    });
    let Some((width, height, entries, size)) = (match prepared {
        Ok(found) => found,
        Err(errno) => return Err(errno),
    }) else {
        // The race above: whoever won has recorded the resource, so its id is
        // there to answer with.
        return back_dumb_with_a_resource(handle);
    };

    // The id is taken only once a command is really going to be sent, so a
    // caller that failed above leaves no gap in the sequence. The device never
    // hears an id it was not told about either way.
    let id = critical_section::with(|cs| {
        let mut st = STATE.borrow(cs).borrow_mut();
        let id = st.next_res_id;
        st.next_res_id = st.next_res_id.wrapping_add(1);
        id
    });

    let made = crate::drivers::video::render::job(crate::drivers::video::render::Job::Create2d {
        id,
        width,
        height,
        entries,
    });
    if made != 0 {
        return Err(made);
    }

    critical_section::with(|cs| {
        let mut st = STATE.borrow(cs).borrow_mut();
        if let Some(d) = st
            .dumb
            .get_mut(handle.wrapping_sub(1))
            .and_then(|s| s.as_mut())
            && d.resource.is_none()
        {
            d.resource = Some(Resource {
                id,
                size,
                virgl: false,
            });
        }
    });
    Ok((id, size))
}

/// `DRM_VIRTGPU_RESOURCE_INFO`: what a handle names.
///
/// The handle has to be one this card made - anything else is `ENOENT`, which is
/// Linux's answer from `drm_gem_object_lookup` - and the size is the resource's
/// own when the handle is one.
///
/// A plain dumb buffer gets its resource made here, on this request, because
/// that is what Linux's `CREATE_DUMB` would have done at creation: see
/// [`back_dumb_with_a_resource`] for why this card makes it on the request that
/// asks rather than on the one that allocates, and for why the answer a caller
/// reads is Linux's either way.
fn virtgpu_resource_info(arg: usize) -> isize {
    let handle = unsafe { (arg as *const u32).read_unaligned() } as usize;
    let (id, size) = match back_dumb_with_a_resource(handle) {
        Ok(found) => found,
        Err(errno) => return errno,
    };
    unsafe {
        ((arg + 4) as *mut u32).write_unaligned(id);
        ((arg + 8) as *mut u32).write_unaligned(size as u32);
        ((arg + 12) as *mut u32).write_unaligned(0); // blob_mem: no blobs here
    }
    0
}

/// `DRM_VIRTGPU_MAP`: the mmap offset for a handle's pages.
///
/// The same offset `MAP_DUMB` computes, over the same table, because a resource
/// and a dumb buffer are the same kind of object here - both are the card's
/// pages. What differs is the answer to a handle this card never made: Linux's
/// `drm_gem_dumb_map_offset` looks the handle up and answers `ENOENT`, and so
/// does this rather than handing back an offset for pages that are not there.
fn virtgpu_map(arg: usize) -> isize {
    // struct drm_virtgpu_map: offset@0 (u64), handle@8, pad@12.
    let handle = unsafe { ((arg + 8) as *const u32).read_unaligned() } as usize;
    let known = critical_section::with(|cs| {
        STATE
            .borrow(cs)
            .borrow()
            .dumb
            .get(handle.wrapping_sub(1))
            .is_some_and(|slot| slot.is_some())
    });
    if !known {
        return -2; // ENOENT
    }
    let offset = (handle as u64) * (PAGES_PER_BUFFER as u64) * (PAGE_SIZE as u64);
    unsafe { (arg as *mut u64).write_unaligned(offset) };
    0
}

/// `DRM_VIRTGPU_WAIT`: wait for a resource's work to be done.
///
/// Always already done, and that is a fact rather than a shortcut: a fence here
/// means a device command still in flight, and every command this card sends is
/// waited for before the ioctl that sent it returns - so by the time a caller
/// can ask, nothing is pending. `VIRTGPU_WAIT_NOWAIT` therefore answers `0` too,
/// where a card with work in flight would answer `EBUSY`; `drm_sync.rs` records
/// the same absence for the sync-object family, and the two are the same fact
/// about this machine seen through two doors.
///
/// An unknown handle is still `ENOENT`: a caller waiting on something that does
/// not exist has a bug, and "done" would hide it.
fn virtgpu_wait(arg: usize) -> isize {
    // struct drm_virtgpu_3d_wait: handle@0, flags@4.
    let handle = unsafe { (arg as *const u32).read_unaligned() } as usize;
    let known = critical_section::with(|cs| {
        STATE
            .borrow(cs)
            .borrow()
            .dumb
            .get(handle.wrapping_sub(1))
            .is_some_and(|slot| slot.is_some())
    });
    if known { 0 } else { -2 } // ENOENT
}

/// `DRM_VIRTGPU_TRANSFER_TO_HOST`: copy a resource's pixels into the device.
///
/// The one direction a card without 3D has - the reverse is `ENOSYS` at the
/// dispatch arm, as measured. The box the caller sends is read for its width and
/// height, and the whole rectangle is transferred from offset zero, because that
/// is what this card's resources are: one buffer, no layers and no mip levels,
/// so a caller asking for anything else is asking about a shape the resource
/// does not have and is refused `EINVAL`.
///
/// A plain dumb buffer is a device resource on Linux, and it is one here now:
/// the first transfer against such a handle makes the resource and copies into
/// it, which is the answer Linux gives. Before that this was `EINVAL` - "this
/// command does not apply to that handle" - which was honest about this card and
/// was not Linux's; [`back_dumb_with_a_resource`] records what changed and why
/// the backing is made on the request that wants it rather than at
/// `CREATE_DUMB`.
///
/// **Two routes, chosen by the resource rather than by this request**, because
/// one struct carries both and cannot say which it means. A 3D resource goes
/// through `TRANSFER_TO_HOST_3D` with the caller's box, level, offset and
/// strides intact; a 2D one takes the path described above, where those are
/// restricted to the single-level, single-layer shape a 2D resource has. Linux
/// splits on the same fact - `virtio_gpu_transfer_to_host_ioctl` reads the
/// object's own kind - and the split is why the restrictions above are not a
/// property of the request.
fn virtgpu_transfer_to_host(arg: usize, ofd: u64) -> isize {
    // struct drm_virtgpu_3d_transfer_to_host: bo_handle@0, then the box
    // (x@4, y@8, z@12, w@16, h@20, d@24), level@28, offset@32, stride@36,
    // layer_stride@40.
    let field = |offset: usize| unsafe { ((arg + offset) as *const u32).read_unaligned() };
    let handle = field(0) as usize;
    let (width, height, depth, level, offset, stride, layer_stride) = (
        field(16),
        field(20),
        field(24),
        field(28),
        field(32),
        field(36),
        field(40),
    );
    let found = critical_section::with(|cs| {
        let st = STATE.borrow(cs).borrow();
        let d = st
            .dumb
            .get(handle.wrapping_sub(1))
            .and_then(|slot| slot.as_ref())?;
        Some(d.resource.as_ref().map(|r| (r.id, r.size, r.virgl)))
    });
    let Some(res) = found else {
        return -2; // ENOENT
    };
    // A plain dumb buffer is a device resource on Linux, so the first transfer
    // against one has it made here and then takes the 2D path below: the
    // resource's pages are the ones the scanout has been reading, so the copy
    // this request sends is of the pixels the caller drew.
    let (id, size, virgl) = match res {
        Some(resource) => resource,
        None => match back_dumb_with_a_resource(handle) {
            Ok((id, size)) => (id, size, false),
            Err(errno) => return errno,
        },
    };

    if virgl {
        // A 3D resource goes through the 3D transfer, and Linux routes it the
        // same way: from the resource's own *kind* rather than from the request,
        // because the request is one struct for both directions and cannot say.
        //
        // The box, the level, the offset and the two strides are all the
        // caller's here. The checks below are restrictions of the **2D** command
        // - it has one level, no pitch of its own and no layer - and applying
        // them to this one would refuse a box a virgl caller legitimately sends.
        // The over-read guard is skipped for the same reason: it exists to catch
        // a caller that allocated a single page and then pushed a whole screen
        // through the 2D command, and a 3D resource's backing is sized by its own
        // create call rather than by this one.
        let ctx = match ensure_context(ofd, 0, b"mpos") {
            Ok(id) => id,
            Err(errno) => return errno,
        };
        let request = crate::gpu::abi::TransferHost3d {
            header: crate::gpu::abi::CtrlHeader::new(crate::gpu::abi::CMD_TRANSFER_TO_HOST_3D),
            box3d: crate::gpu::abi::Box3d {
                x: field(4),
                y: field(8),
                z: field(12),
                w: width,
                h: height,
                d: depth,
            },
            offset: offset as u64,
            resource_id: id,
            level,
            stride,
            layer_stride,
        };
        return crate::drivers::video::render::job(
            crate::drivers::video::render::Job::TransferHost3d {
                to_host: true,
                ctx,
                request,
            },
        );
    }

    if width == 0
        || height == 0
        || depth > 1
        || level != 0
        || offset != 0
        || stride != 0
        || layer_stride != 0
    {
        return libc_einval();
    }
    // The transfer has to fit the backing the device was given. It reads
    // `width * height * 4` bytes from the frames attached at create, and a
    // resource whose caller asked for a smaller object - Linux substitutes a page
    // for a zero size, and Mesa passes zero - would have it read past the end of
    // them and into whatever the allocator put next. Linux makes no such check
    // and would over-read; this refuses the request instead. It is a divergence
    // that can only turn a broken caller's garbage into an error, which is the
    // direction `NOTES.md` §4's worst-case rule asks for.
    if width as u64 * height as u64 * 4 > size {
        return libc_einval();
    }
    crate::drivers::video::render::job(crate::drivers::video::render::Job::TransferToHost {
        id,
        width,
        height,
    })
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
            let vc = current_console().unwrap_or_else(crate::term::vc::active);
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
    let now = crate::hal::since_boot_nanos();
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
    let Some(card) = inode.as_any().downcast_ref::<DrmCard>() else {
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
    let Some(leased) = inode
        .as_any()
        .downcast_ref::<DrmCard>()
        .and_then(|c| c.lease())
    else {
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
    // The cursor is presentation too, and belongs here for the reason the list
    // exists: a master that has lost its console must stop putting pixels on the
    // screen, and a pointer that keeps moving on someone else's screen is pixels.
    // The server drawing the opposite way round - a cloned output whose cursor is
    // handled per-client - is a driver that would not use this list at all.
    NR_MODE_CURSOR,
    NR_MODE_CURSOR2,
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
    // The console comes back in graphics mode before anything asks whether it
    // is, and that is the whole fix for a card this one cannot drive away from
    // its desktop.
    //
    // The measurement: seatd opens the console, writes `VT_SETMODE` and closes
    // the descriptor - and it writes the mode with **`relsig` and `acqsig` both
    // zero**, which is what a seat manager that watches the console itself
    // rather than waiting to be told looks like. So the handover has no keeper
    // to signal, `release_console` takes Linux's own `reset_vc` road, and the
    // console lands back in `KD_TEXT`. That is Linux's answer too; what Linux
    // does not do is let the word matter afterwards, because there a master
    // drives the screen because it is the master. Here the *veto* the text
    // console paints under is `master_holds_screen()`, and that asks `KD_GRAPHICS`
    // as one of its three facts - so a handover nobody answered cost the
    // compositor its screen for good: the text grid repainted over the desktop
    // and every `MODE_ATOMIC` was answered `EACCES` (which is what a `^A` away
    // and back showed, with `switch to=1 master_vc=Some(1) active=1 kd=0` in
    // the log beside it).
    //
    // So the master's own console is a graphics console again the moment it is
    // the one on the screen and a live master still holds the card. Nobody
    // else's: a master that has let go (`master_held` above), one on another
    // console, and one whose console is not the one being shown all leave the
    // text console its screen, which is what `^A` away from a desktop is.
    if let Some(vc) = crate::vfs::vcmux::master_vc()
        && crate::term::vc::active() == vc
        && crate::vfs::vcmux::kd_mode(vc) != crate::term::vc::KD_GRAPHICS
    {
        crate::vfs::vcmux::set_kd_mode(vc, crate::term::vc::KD_GRAPHICS);
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
    // And what it holds by *description* rather than by any object: the sync
    // objects it made, and the `DRM_CLIENT_CAP_ATOMIC` it set. A handle is per
    // description, so a description that is gone has nobody left who could name
    // one - and the objects themselves are not dropped here: a
    // `SYNCOBJ_HANDLE_TO_FD` descriptor that outlived the client still holds the
    // one it names, which is the whole reason that descriptor exists. The cap is
    // the same shape, and forgetting it here is not a leak of pixels but a
    // descriptor number that a *later* file could be given: an `ofd` left in the
    // list would hand some other client the atomic properties it never asked
    // for, which is the use-after-free of `master_closed`'s own header, one
    // table over.
    critical_section::with(|cs| {
        let mut st = STATE.borrow(cs).borrow_mut();
        st.syncobjs
            .retain(|rec| rec.as_ref().is_none_or(|rec| rec.owner != ofd));
        st.atomic_ofds.retain(|&owner| owner != ofd);
    });
    release_framebuffers(ofd);
}

/// Take a description's framebuffers away with it: Linux's `drm_fb_release`,
/// which `drm_file_free` runs for every framebuffer the closing `struct
/// drm_file` made.
///
/// This is what stops a dead session's ids being *enumerated*. `GETRESOURCES`
/// reports the framebuffers the card has, and a client that asks for one of
/// them gets an id whose owner is long gone - smithay's `DrmDevice` reads the
/// properties of every framebuffer it is handed on the way in, and this card
/// answers `EINVAL` for an object that is not a connector, CRTC or plane
/// (Linux's `!obj->properties`, which is what a framebuffer is). The whole
/// device then fails to open: "Error reading properties on device
/// `/dev/dri/card0` (Invalid argument)", and niri comes up with no output and
/// no way to say why. A framebuffer nobody made is the lie underneath it.
///
/// A framebuffer that was being scanned out is dropped from the state that
/// names it rather than from the pixels: the scanout keeps the last frame -
/// which is what a compositor's exit looks like on a real card too, and what
/// lets the text console take the screen over from here.
fn release_framebuffers(ofd: u64) {
    critical_section::with(|cs| {
        let mut st = STATE.borrow(cs).borrow_mut();
        let gone: Vec<u32> = st
            .fbs
            .iter()
            .filter_map(|f| f.as_ref())
            .filter(|f| f.owner == ofd)
            .map(|f| f.id)
            .collect();
        if gone.is_empty() {
            return;
        }
        st.fbs.retain(|f| f.as_ref().is_none_or(|f| f.owner != ofd));
        if gone.contains(&st.current_fb) {
            st.current_fb = 0;
        }
        if gone.contains(&st.cursor_fb) {
            st.cursor_fb = 0;
        }
    });
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
        NR_MODE_CURSOR => (28, false), // struct drm_mode_cursor
        NR_MODE_CURSOR2 => (36, false), // struct drm_mode_cursor2
        NR_GETFB2 => (104, true),      // struct drm_mode_fb_cmd2, in and out
        // The legacy gamma pair, `struct drm_mode_crtc_lut`. Both are
        // `DRM_IOWR` in `<drm.h>`, so the struct is validated writable - which
        // is a statement about the struct, not about the three arrays it names:
        // a `GETGAMMA` fills the arrays the caller's pointers aim at, and this
        // card never gets that far (see `gamma_refused`).
        NR_MODE_GETGAMMA | NR_MODE_SETGAMMA => (32, true),
        // The CRTC-sequence pair, `struct drm_crtc_get_sequence` and
        // `struct drm_crtc_queue_sequence`, both `DRM_IOWR` as well.
        //
        // The row matters more than it looks: these two are *refused*, and
        // Linux's ioctl layer copies every DRM struct in before the handler can
        // refuse it, so a caller whose pointer is not its own memory hears
        // `EFAULT` rather than the refusal. Without the row this card answered
        // `ENOTTY` without ever looking at the struct.
        NR_CRTC_GET_SEQUENCE | NR_CRTC_QUEUE_SEQUENCE => (24, true),
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
        // The sync object family. The two wait structs are the ones worth
        // reading twice: this host's `drm.h` has grown a trailing
        // `deadline_nsec` on each, so they are 40 and 48 bytes and *not* the 32
        // and 40 the older upstream headers describe. A caller built against the
        // old layout sends a request word eight bytes short, and every wait in
        // the family answers `EINVAL` - which is exactly what the instrument
        // programs measured before the sizes were corrected, and why the numbers
        // here are the header's rather than a recollection of one.
        NR_SYNCOBJ_CREATE => (8, true),   // struct drm_syncobj_create
        NR_SYNCOBJ_DESTROY => (8, false), // struct drm_syncobj_destroy
        NR_SYNCOBJ_HANDLE_TO_FD => (24, true), // struct drm_syncobj_handle
        NR_SYNCOBJ_FD_TO_HANDLE => (24, true), // struct drm_syncobj_handle
        NR_SYNCOBJ_WAIT => (40, true),    // struct drm_syncobj_wait
        NR_SYNCOBJ_RESET | NR_SYNCOBJ_SIGNAL => (16, false), // drm_syncobj_array
        NR_SYNCOBJ_TIMELINE_WAIT => (48, true), // drm_syncobj_timeline_wait
        // `struct drm_syncobj_timeline_array` is read *and* written: `QUERY`
        // fills the points array, and the two signals read it.
        NR_SYNCOBJ_QUERY
        | NR_SYNCOBJ_TRANSFER
        | NR_SYNCOBJ_TIMELINE_SIGNAL
        | NR_SYNCOBJ_EVENTFD => match nr {
            NR_SYNCOBJ_QUERY | NR_SYNCOBJ_TIMELINE_SIGNAL => (24, true),
            NR_SYNCOBJ_TRANSFER => (32, true), // struct drm_syncobj_transfer
            _ => (24, true),                   // struct drm_syncobj_eventfd
        },
        // The `DRM_VIRTGPU_*` family, sized from `virtgpu_drm.h`'s structs. All
        // eleven are here, including the four with no dispatch arm: Linux's
        // ioctl layer copies every DRM struct in *before* the handler can refuse
        // it, so a caller whose pointer is not its own memory must hear `EFAULT`
        // rather than a refusal - the same reason the gamma and sequence rows
        // above are here. Two of the sizes are worth reading twice:
        // `drm_virtgpu_execbuffer` is 64 bytes (it grew the sync-object fields),
        // and the transfer pair is 44, not the 48 a `drm_virtgpu_3d_box` plus
        // four words might suggest.
        NR_VIRTGPU_MAP => (16, true),        // struct drm_virtgpu_map
        NR_VIRTGPU_EXECBUFFER => (64, true), // struct drm_virtgpu_execbuffer
        NR_VIRTGPU_GETPARAM => (16, true),   // struct drm_virtgpu_getparam
        NR_VIRTGPU_RESOURCE_CREATE => (56, true), // struct drm_virtgpu_resource_create
        NR_VIRTGPU_RESOURCE_INFO => (16, true), // struct drm_virtgpu_resource_info
        NR_VIRTGPU_TRANSFER_FROM_HOST | NR_VIRTGPU_TRANSFER_TO_HOST => (44, true),
        NR_VIRTGPU_WAIT => (8, true),      // struct drm_virtgpu_3d_wait
        NR_VIRTGPU_GET_CAPS => (24, true), // struct drm_virtgpu_get_caps
        NR_VIRTGPU_RESOURCE_CREATE_BLOB => (56, true), // struct drm_virtgpu_resource_create_blob
        NR_VIRTGPU_CONTEXT_INIT => (16, true), // struct drm_virtgpu_context_init
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
        // Both answered 1, and made true below rather than merely advertised:
        // the family is dispatched, `SYNCOBJ_EVENTFD` is the probe a compositor
        // makes before it will expose the explicit-sync protocol, and a timeline
        // is a shape these objects really have once a point is submitted.
        CAP_SYNCOBJ | CAP_SYNCOBJ_TIMELINE => 1,
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
/// hotspot, because there is no `HOTSPOT_X`/`HOTSPOT_Y` plane property for it to
/// be about - which is what a real card's cursor planes measure as well, and a
/// different question from whether the card has a cursor at all.
///
/// `ATOMIC` is also the one cap this card *records* rather than merely accepts,
/// because it is the one whose answer changes what the card reports: the
/// properties in [`ATOMIC_PROPS`] appear in `GETCONNECTOR`'s and
/// `OBJ_GETPROPERTIES`'s lists only for a description that has set it. Recording
/// it is what makes the second half of the cap true - before this, the flag was
/// absent and every client was shown the atomic properties, which is the
/// "accepted and stored nowhere" the §7.4 record named as the honest gap.
fn set_client_cap(arg: usize, ofd: u64) -> isize {
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
        | CLIENT_CAP_ASPECT_RATIO
        | CLIENT_CAP_WRITEBACK_CONNECTORS => 0,
        // The one cap with a reader: recorded per description, so that the two
        // list doors can leave the atomic properties out for a client that has
        // not asked for them. A `value` of 0 is an *un*set - the same request
        // weston makes on teardown - and takes the description out again, which
        // is why this pushes rather than only inserting.
        CLIENT_CAP_ATOMIC => {
            critical_section::with(|cs| {
                let mut st = STATE.borrow(cs).borrow_mut();
                st.atomic_ofds.retain(|&owner| owner != ofd);
                if value == 1 {
                    st.atomic_ofds.push(ofd);
                }
            });
            0
        }
        // Refused because the object behind them does not exist here: no
        // plane with a color pipeline, and no `HOTSPOT_X`/`HOTSPOT_Y` plane
        // property for the cursor cap to be about. That last one is the
        // *measurement* rather than a shortcut - `modetest -p` on the real
        // amdgpu card reports `type`, `IN_FORMATS` and `IN_FORMATS_ASYNC` as
        // the only properties on any of its four cursor planes, with no
        // hotspot property anywhere in the machine - so a real card refuses
        // this cap's premise too, and a driver opting in is what the cap is
        // for. Having a cursor *plane* is not the same thing: the hotspot a
        // compositor needs is also carried by `MODE_CURSOR2`'s struct, which
        // this card honours. `EOPNOTSUPP` is Linux's answer for a cap the
        // driver declares it does not do, and this card answers it honestly.
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
    //
    // An *empty* array is neither of those: there is nothing to copy, so the
    // pointer is never looked at, which is also what Linux does - its copy sits
    // inside the count test. That distinction is load-bearing rather than
    // pedantic, because of how the two-call enumeration is written: the second
    // call carries the counts the first one reported beside buffers reserved at
    // exactly those counts, so a count of zero arrives with the *dangling
    // non-null* pointer an empty `Vec` has (address 4 for `u32`), never a null.
    // Reading that pointer as "somewhere the caller asked me to write" answered
    // this request `EFAULT`, and that is where niri's `DrmDevice::new` stopped
    // loading the primary node's resources (`resource_handles()`, "Bad address
    // (os error 14)"), measured in the guest: the card has no framebuffers
    // before the first commit, so `count_fbs` is 0 on that card's first
    // `GETRESOURCES` and nothing else was wrong.
    let fault = critical_section::with(|cs| {
        let st = STATE.borrow(cs).borrow();
        let mut fault = false;
        unsafe {
            let put_ids = |ptr_off: usize, count_off: usize, ids: &[u32], fault: &mut bool| {
                let ptr = ((arg + ptr_off) as *const u64).read_unaligned() as *mut u32;
                let want = ((arg + count_off) as *const u32).read_unaligned();
                if !ids.is_empty() && want >= ids.len() as u32 && !ptr.is_null() {
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
            if !fbs.is_empty() && c >= fbs.len() as u32 {
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
/// Whether `fb_id` names a framebuffer the cursor plane can show: one this card
/// made, in the format a cursor carries its alpha in, and no larger than the
/// 64x64 this card answers for `DRM_CAP_CURSOR_WIDTH`/`HEIGHT`.
///
/// Both doors to the cursor plane - the legacy `MODE_CURSOR` and the atomic
/// `FB_ID` - ask this one question, so that a framebuffer one of them accepts
/// cannot be refused by the other. `MODE_CURSOR` additionally checks the
/// `width`/`height` the caller wrote against the framebuffer's own, because the
/// legacy struct makes the caller state them and believing the statement would
/// let a caller describe an image that is not the one being composited.
fn cursor_fb_ok(st: &DrmState, fb_id: u32) -> bool {
    st.fbs.iter().filter_map(|f| f.as_ref()).any(|f| {
        f.id == fb_id
            && f.format == DRM_FORMAT_ARGB8888
            && f.width > 0
            && f.height > 0
            && f.width <= CURSOR_SIZE as u32
            && f.height <= CURSOR_SIZE as u32
    })
}

/// The pixel size of the buffer on the cursor plane, `(0, 0)` when there is
/// none. This is what the plane's `SRC_*` and `CRTC_W`/`CRTC_H` are, and what a
/// commit naming them is checked against: the cursor is drawn whole, so the
/// image's own size *is* its rectangle.
fn cursor_image_dims(st: &DrmState, fb_id: u32) -> (u32, u32) {
    st.fbs
        .iter()
        .filter_map(|f| f.as_ref())
        .find(|f| f.id == fb_id)
        .map(|f| (f.width, f.height))
        .unwrap_or((0, 0))
}

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
    // The pointer goes on last, over the frame that was just copied. See
    // [`paint_cursor`] for why this is the right place for it.
    paint_cursor(st, scanout, screen_w, screen_h);
    0
}

/// Draw the cursor plane onto the scanout, over whatever the framebuffer copy has
/// just put there.
///
/// This is the step a real card does in its display controller, and this one has
/// to do in software: there is no engine to composite an overlay, so the cursor is
/// blended into the scanout block by the same pass that fills it. Doing it *here*,
/// at the end of every copy, is what makes the two orderings come out right
/// without either side knowing about the other - a compositor's flip lands *under*
/// the pointer rather than erasing it, and a `MODE_CURSOR` move re-copies the
/// framebuffer (through `request_re_present`) and re-blends, so the pointer moves
/// without the compositor redrawing anything.
///
/// A cursor this card cannot show is skipped rather than refused here: whether a
/// framebuffer may be *bound* is decided where it is bound (`MODE_CURSOR`, the
/// plane's `FB_ID`), and by the time a flush runs the only question left is
/// whether there is something to draw.
fn paint_cursor(st: &DrmState, scanout: *mut u8, screen_w: u32, screen_h: u32) {
    if st.cursor_fb == 0 {
        return;
    }
    let Some((handle, w, h, pitch)) = st
        .fbs
        .iter()
        .filter_map(|f| f.as_ref())
        .find(|f| f.id == st.cursor_fb)
        .map(|f| (f.handle, f.width, f.height, f.pitch))
    else {
        return;
    };
    // A cursor image is at most 64x64 at the alignment's pitch, which is what
    // `DRM_CAP_CURSOR_WIDTH`/`HEIGHT` answer and what a real card's cursor planes
    // are. The bound is stated *before* the allocation rather than checked after
    // it, because the global allocator still panics on failure: `ADDFB2` accepts
    // any geometry that fits inside the buffer, so without this a caller could
    // reach a 64 MiB gather through a "cursor" a million pixels wide
    // (`AGENTS.md`'s worst-case rule). A pitch above 256 is the same refusal by
    // another route - a 64-wide ARGB row needs 256 bytes.
    if w == 0 || h == 0 || w > CURSOR_SIZE as u32 || h > CURSOR_SIZE as u32 || pitch > 256 {
        return;
    }
    let Some(dumb) = handle
        .checked_sub(1)
        .and_then(|slot| st.dumb.get(slot as usize))
        .and_then(|d| d.as_ref())
    else {
        return;
    };
    // The image is a dumb buffer whose pages are separate frames, so it is
    // gathered into one flat block the composition can index by row.
    let len = pitch as usize * h as usize;
    let mut image = alloc::vec![0u8; len];
    dumb.copy_span(0, image.as_mut_ptr(), len);
    let dst = unsafe {
        core::slice::from_raw_parts_mut(scanout, screen_w as usize * screen_h as usize * 4)
    };
    let _ = crate::gpu::cursor::compose(
        dst,
        screen_w as usize * 4,
        screen_w as usize,
        screen_h as usize,
        &image,
        pitch as usize,
        w as usize,
        h as usize,
        st.cursor_hotspot,
        st.cursor_pos,
    );
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
            // `gamma_size` at 28, written rather than left alone, and the reason
            // is the same one `write_mode` gives for the mode's name: the
            // caller's struct is its own uninitialised memory - libdrm allocates
            // it with `malloc` - so a field this card does not fill is a number
            // the caller reads as garbage. This card has no gamma table at all
            // (see `gamma_refused`), which makes 0 the truthful report rather
            // than a missing one, and it is the number that tells a caller the
            // legacy gamma pair is not worth asking about.
            (p.add(28) as *mut u32).write_unaligned(0);
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

fn get_connector(arg: usize, ofd: u64) -> isize {
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
    let (crtc_value, dpms_value, atomic) = critical_section::with(|cs| {
        let mut st = STATE.borrow(cs).borrow_mut();
        let _ = edid_blob_bytes(&mut st);
        (
            st.conn_crtc as u64,
            st.dpms as u64,
            st.atomic_ofds.contains(&ofd),
        )
    });
    let connector_values: [u64; 6] = [
        crtc_value,
        dpms_value,
        DRIVER_BLOB_EDID as u64,
        0, // non-desktop: this is a real display, not a head-mounted one
        BPC_HERE,
        0, // vrr_capable: the refresh rate cannot vary
    ];
    // The connector's list, less the properties an un-atomic client is not shown
    // - the `CRTC_ID` this card is bound by, which is in [`ATOMIC_PROPS`] for the
    // same reason the planes' is. `OBJ_GETPROPERTIES` answers the same object
    // through `without_atomic` over the same table, so the two doors cannot hand
    // the same client two different connectors; the measurement that fixed the
    // set (§7.4) saw exactly this on a connector: seventeen properties to a
    // description without the cap, eighteen with it, and per *description*.
    let all = CONNECTOR_PROPS.len();
    let mut visible_props = CONNECTOR_PROPS;
    let mut visible_values = connector_values;
    let visible = if atomic {
        all
    } else {
        without_atomic(&mut visible_props, &mut visible_values, all)
    };
    unsafe {
        let p = arg as *mut u8;
        // Fill a single mode into modes_ptr if the caller asked for it.
        let count_modes = (p.add(32) as *const u32).read_unaligned();
        let modes_ptr = ((p.add(8) as *const u64).read_unaligned()) as *mut u8;
        if count_modes >= 1 && !modes_ptr.is_null() && user_array(modes_ptr as usize, 1, 68, true) {
            write_mode(modes_ptr, (w * h * 60) / 1000, w, h);
        }
        (p.add(32) as *mut u32).write_unaligned(1); // count_modes
        // The property ids and values, when the caller has room for them. The
        // count is the *visible* one, so a client is told how long the list it
        // can read is and never a longer one than it will receive.
        let count_props = (p.add(36) as *const u32).read_unaligned();
        (p.add(36) as *mut u32).write_unaligned(visible as u32);
        let props_ptr = ((p.add(16) as *const u64).read_unaligned()) as *mut u32;
        if count_props >= visible as u32
            && !props_ptr.is_null()
            && user_array(props_ptr as usize, visible, 4, true)
        {
            for (index, prop) in visible_props.iter().take(visible).enumerate() {
                props_ptr.add(index).write_unaligned(*prop);
            }
        }
        let values_ptr = ((p.add(24) as *const u64).read_unaligned()) as *mut u64;
        if count_props >= visible as u32
            && !values_ptr.is_null()
            && user_array(values_ptr as usize, visible, 8, true)
        {
            for (index, value) in visible_values.iter().take(visible).enumerate() {
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
        let (mm_w, mm_h) = crate::gpu::blob::size_mm(w, h);
        (p.add(64) as *mut u32).write_unaligned(mm_w);
        (p.add(68) as *mut u32).write_unaligned(mm_h);
        // Sub-pixel order: 0, the unknown value. Measured on a real connected
        // eDP connector, which also reported 0 - so this is the honest default
        // rather than an invented arrangement of a virtual display's pixels.
        (p.add(72) as *mut u32).write_unaligned(crate::gpu::blob::SUBPIXEL_UNKNOWN);
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
    //
    // The `plane_id` selects *which* plane is described, and this card used to
    // ignore it: every request answered with the primary plane, so a caller
    // asking about the cursor plane was described the one it had not asked for.
    // That is not a detail once there are two - a compositor finds its cursor
    // plane by asking about each id `GETPLANERESOURCES` gave it and reading the
    // `type` back - so an id this card does not have is refused, the way Linux's
    // `drm_plane_find` refuses one.
    let want = unsafe { (arg as *const u32).read_unaligned() };
    let (formats, fb_id) = match want {
        PLANE_ID => (
            &PLANE_FORMATS[..],
            critical_section::with(|cs| STATE.borrow(cs).borrow().current_fb),
        ),
        CURSOR_PLANE_ID => (
            &CURSOR_FORMATS[..],
            critical_section::with(|cs| STATE.borrow(cs).borrow().cursor_fb),
        ),
        // No plane has that id, which is `ENOENT` - the answer the rest of this
        // file gives for an object id it does not know.
        _ => return -2,
    };
    unsafe {
        let p = arg as *mut u32;
        p.write_unaligned(want); // plane_id: the one that was asked about
        p.add(1).write_unaligned(CRTC_ID); // crtc_id: the CRTC it serves
        p.add(2).write_unaligned(fb_id); // fb_id
        p.add(3).write_unaligned(1); // possible_crtcs: bit 0
        p.add(4).write_unaligned(0); // gamma_size: neither plane has a LUT
        // The format list, when the caller has room for it: this plane's own, so
        // that the cursor plane answers `ARGB8888` alone and the primary answers
        // the two opaque/alpha spellings it scans.
        let count = p.add(5).read_unaligned();
        let formats_ptr = ((arg + 24) as *const u64).read_unaligned() as *mut u32;
        if count >= formats.len() as u32
            && !formats_ptr.is_null()
            && user_array(formats_ptr as usize, formats.len(), 4, true)
        {
            for (index, format) in formats.iter().enumerate() {
                formats_ptr.add(index).write_unaligned(*format);
            }
        }
        p.add(5).write_unaligned(formats.len() as u32); // count_format_types
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
        // An object-typed property: its one value is the *kind* of object it
        // names, not an id - `drm_property_create_object` stores a single entry
        // (`values[0] = type`, `num_values = 1`) and a caller reads it to learn
        // what the property points at. Measured on this host's card through
        // libdrm: the connector's `CRTC_ID` and the planes' `CRTC_ID` report
        // `count_values = 1`, `values[0] = 0xcccccccc`, and the planes' `FB_ID`
        // `values[0] = 0xfbfbfbfb` - `DRM_MODE_OBJECT_CRTC` and `_FB` out of
        // `<drm_mode.h>`. The *current* binding comes from the connector's
        // property list, not from here - this reports the property's shape.
        //
        // An empty list here is not a smaller truth, it is a broken shape: a
        // caller that sorts a property by its flags reads `values[0]` as soon
        // as it sees `OBJECT`, so drm-rs panicked with "index out of bounds: the
        // len is 0 but the index is 0" (`drm-0.14.1/src/control/mod.rs:497`) and
        // took niri with it, which is how the planes' `FB_ID` and `CRTC_ID` were
        // found advertising no values at all.
        PROP_CRTC_ID => ("CRTC_ID", PROP_OBJECT, &[OBJECT_CRTC as u64], &[]),
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
        // The plane's buffer and the CRTC it feeds - object-typed, so each
        // carries the one value that names *what kind* of object it points at
        // (the framebuffer tag and the CRTC tag), as the connector's above does.
        PROP_PLANE_FB_ID => ("FB_ID", PROP_OBJECT, &[OBJECT_FB as u64], &[]),
        PROP_PLANE_CRTC_ID => ("CRTC_ID", PROP_OBJECT, &[OBJECT_CRTC as u64], &[]),
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
        // A signed descriptor number: -1 means "no fence". The range is the one
        // the kernel gives it, so a caller reading the shape back sees what it
        // expects. It is now honoured rather than stored: a commit that carries
        // one waits for it before a pixel moves.
        PROP_IN_FENCE_FD => (
            "IN_FENCE_FD",
            PROP_RANGE | PROP_SIGNED_RANGE,
            &[-1i64 as u64, i32::MAX as u64],
            &[],
        ),
        // The other half: a pointer in the caller's memory where the commit
        // writes back a fence descriptor for the frame it committed. The range
        // is the whole 64-bit span because the value is an address - there is no
        // smaller number that means anything, which is also why it is a range
        // and not a `PROP_UINT`.
        PROP_OUT_FENCE_PTR => ("OUT_FENCE_PTR", PROP_RANGE, &[0, u64::MAX], &[]),
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
    // `DRM_MODE_PROP_ATOMIC` is not one of the *type* bits the arms above choose
    // between, so it is not written there: which clients may see a property is a
    // separate fact from what the property is, and [`ATOMIC_PROPS`] is the one
    // place this file keeps it - the same list the two list doors filter by.
    //
    // It is reported even to a client that cannot see the property in a list,
    // and that is deliberate rather than an oversight: Linux lets a caller ask
    // by id for a property it was not shown, and answers with the flag, so a
    // caller that kept an id from a previous capped session can find out why it
    // is gone. Measured on this host's card, on the description that had *not*
    // set the cap, before this was written.
    let flags = if ATOMIC_PROPS.contains(&prop_id) {
        flags | PROP_ATOMIC
    } else {
        flags
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
        // The cursor plane, which unlike the primary plane's rectangles *does*
        // act on what it is told: `CRTC_X`/`CRTC_Y` are where the pointer is, and
        // the very next flush draws it there. `FB_ID` is checked by the same
        // question the legacy `MODE_CURSOR` asks, so a framebuffer one door
        // accepts cannot be refused by the other - and `0` is "show nothing",
        // which is always allowed.
        (CURSOR_PLANE_ID, PROP_PLANE_FB_ID) => {
            let fb = value as u32;
            if fb != 0 && !cursor_fb_ok(st, fb) {
                return libc_einval();
            }
            st.cursor_fb = fb;
        }
        (CURSOR_PLANE_ID, PROP_PLANE_CRTC_ID) => st.cursor_crtc = value as u32,
        (CURSOR_PLANE_ID, PROP_CRTC_X) => st.cursor_pos.0 = value as i32,
        (CURSOR_PLANE_ID, PROP_CRTC_Y) => st.cursor_pos.1 = value as i32,
        // The cursor's rectangle, accepted rather than recorded: the only one
        // this card honours is the image's own, so by the time a write reaches
        // here the commit has already established that this is it
        // ([`Commit::cursor_rect_ok`]) and there is no separate state to keep.
        (CURSOR_PLANE_ID, PROP_SRC_X)
        | (CURSOR_PLANE_ID, PROP_SRC_Y)
        | (CURSOR_PLANE_ID, PROP_SRC_W)
        | (CURSOR_PLANE_ID, PROP_SRC_H)
        | (CURSOR_PLANE_ID, PROP_CRTC_W)
        | (CURSOR_PLANE_ID, PROP_CRTC_H) => {}
        // Pre-multiplied or coverage alpha: the scanout carries the
        // compositor's finished frame, so the answer is recorded nowhere -
        // there is nothing below it to blend against.
        (PLANE_ID, PROP_BLEND) => {}
        // The two fence properties are not values to *record* anywhere: a
        // commit that carries one acts on it - the in-fence is waited for before
        // the copy, the out-fence pointer is written back after the flush - and
        // the single-property door cannot honour either, because a fence handed
        // to a request that presents nothing has nothing to be ordered against.
        // A real card refuses both there too, for the same reason: they are
        // atomic-only.
        (PLANE_ID, PROP_IN_FENCE_FD) | (PLANE_ID, PROP_OUT_FENCE_PTR) => return libc_einval(),
        _ => return libc_einval(),
    }
    0
}

/// Record a property write against the connector, by the legacy
/// `SETPROPERTY` spelling.
///
/// It is the object door's own rule - [`set_object_prop`] - rather than a
/// second list of "the properties this object has": the two doors answer for
/// one connector, and a property settable through one and refused by the other
/// is a client that read the property list and still cannot write what it read
/// (`max bpc` was exactly that, and a compositor setting it in its first commit
/// lost the whole frame on it). What stays here is only the door's shape: the
/// locks are its own, and a number that is not one of this object's properties
/// is still `EINVAL`.
fn set_connector_prop(prop_id: u32, value: u64) -> isize {
    critical_section::with(|cs| {
        let mut st = STATE.borrow(cs).borrow_mut();
        set_object_prop(&mut st, CONNECTOR_ID, prop_id, value)
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
    // The acquire fence, resolved before any lock is taken: a descriptor number
    // is the caller's business, and looking one up takes the fd table's own
    // lock. It is the buffer behind this fence that the copy is about to read,
    // so a commit that cannot resolve one has nothing to apply - and it is
    // refused here, before a pixel moves, rather than half-applied.
    //
    // What `IN_FENCE_FD` names is a `sync_file`, which is what the other half of
    // the family hands out; a descriptor of any other kind is `EINVAL`, the same
    // answer Linux's `sync_file_get_fence` gives.
    let in_fence = match plan.in_fence_fd {
        None | Some(-1) => None,
        Some(fd) => {
            let Ok(file) = crate::sched::current_fds().clone_at(fd) else {
                return -9; // EBADF
            };
            let inode = file.inode();
            let Some(sync_file) = inode.as_any().downcast_ref::<SyncFileFd>() else {
                return libc_einval();
            };
            Some(sync_file.fence())
        }
    };
    // The behaviour the property's name promises: the pixels are not read until
    // the fence says the buffer is the caller's to read.
    //
    // On this machine that wait is *provably* immediate, and the reason is worth
    // writing down rather than leaving as a hope: nothing on this card can make
    // an unsignaled fence reachable from a descriptor. Every fence a client can
    // obtain is either one `SYNCOBJ_HANDLE_TO_FD` exported - which refuses an
    // object with no fence, and every fence here retires before the object is
    // told about it - or an out-fence, whose descriptor is written back only
    // after the frame has gone. So `has_retired` is true of every fence this
    // loop can see, and it is a `block_until` rather than a bounded wait because
    // the day that stops being true is the day an engine exists - which is the
    // same day this comment has to be rewritten.
    if let Some(fence) = &in_fence {
        crate::sched::block_until(|| fence.has_retired());
    }
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
        // The cursor's rectangle, which has to be the identity for the image
        // the commit is binding - see `Commit::cursor_rect_ok`.
        if !plan.cursor_rect_ok(&st) {
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
    // A commit that touches the cursor plane has to rebuild the scanout even
    // though it carried no new frame: the pointer is drawn *in* that rebuild, so
    // a move that did not re-copy would leave it where it was.
    let mut cursor_touched = false;
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
            // The two fence properties are the commit's own business and are
            // not state to record: the acquire fence has already been waited
            // for, and the out-fence pointer is written back below, once the
            // frame has gone. Recording them here would put a descriptor number
            // and a caller's address into the state a compositor reads back.
            if obj == PLANE_ID && matches!(prop, PROP_IN_FENCE_FD | PROP_OUT_FENCE_PTR) {
                continue;
            }
            let rc = set_object_prop(&mut st, obj, prop, value);
            if rc != 0 {
                return rc;
            }
            if obj == CURSOR_PLANE_ID {
                cursor_touched = true;
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
        // A commit that moved the pointer carried no new frame, and the pointer is
        // drawn in the copy - so the master's framebuffer is copied again here,
        // with the new position, before the flush that makes it visible. Same
        // reasoning as `mode_cursor`'s, and the reason an atomic cursor move and a
        // legacy one are equally prompt.
        if cursor_touched {
            critical_section::with(|cs| {
                let st = STATE.borrow(cs).borrow();
                if st.current_fb != 0 {
                    let _ = copy_to_scanout(&st, st.current_fb);
                }
            });
        }
        // The out-fence's descriptor, made *before* the frame is flushed, so
        // that a caller with no descriptor left hears `EMFILE` from a commit
        // that presented nothing rather than from one that presented a frame
        // with no fence to report it.
        //
        // The fence is built armed and retired after the flush, because that is
        // the transition a fence is for: it describes "the frame has gone", and
        // the flush is the instant that becomes true. The descriptor's *number*
        // is written into the caller's memory last of all, so there is no task
        // that could be handed an armed one - the card has one task in this
        // window, and it is the one doing the flushing.
        let mut out_fence = None;
        if plan.out_fence_ptr.is_some() {
            let fence = crate::gpu::sync::Fence::armed();
            let file = Arc::new(crate::vfs::File::new(SyncFileFd::new(fence.clone())));
            let Ok(fd) = crate::sched::current_fds().install(file) else {
                return -24; // EMFILE
            };
            out_fence = Some((fd, fence));
        }
        // Present, *then* report: the commit's completion event is queued once
        // the frame has actually gone to the host, so its timestamp is the
        // landing rather than the request. See [`queue_flip_event`].
        crate::drivers::video::render::request_flush_blocking();
        if let (Some((fd, fence)), Some(ptr)) = (&out_fence, plan.out_fence_ptr) {
            fence.retire();
            unsafe { (ptr as *mut i32).write_unaligned(*fd) };
        }
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
    /// The framebuffer the commit binds to the cursor plane, and the rectangle
    /// it puts it in. `None` for the framebuffer means the commit did not name
    /// one, so the plane keeps whatever it had; the rectangles are the identity
    /// unless the commit says otherwise, which is the state a cursor plane is
    /// in when it is carrying a whole image.
    cursor_fb: Option<u32>,
    cursor_src: Option<(u32, u32, u32, u32)>,
    cursor_dst: Option<(u32, u32)>,
    /// `IN_FENCE_FD`'s value: the descriptor the commit was handed, or -1 for
    /// the property's own spelling of "no fence". Kept as a number here and
    /// resolved to a fence before any lock is taken, because a descriptor number
    /// is the caller's business and looking one up is the fd table's.
    in_fence_fd: Option<i32>,
    /// `OUT_FENCE_PTR`'s value: where the commit writes back the descriptor it
    /// makes. Validated as the caller's memory the moment it arrives, so the
    /// write at the end of a successful commit cannot fault in the kernel.
    out_fence_ptr: Option<usize>,
}

impl Commit {
    /// Record one property write, refusing an object or property this card
    /// does not have.
    fn record(&mut self, obj: u32, prop: u32, value: u64) -> Result<(), isize> {
        // Every object the card has, by the id the commit names it with: the
        // request carries ids and not types, and these four are distinct.
        //
        // What is listed is the *settable* half of what `OBJ_GETPROPERTIES`
        // publishes, and the two have to agree: a property a compositor can
        // read off an object and cannot set is a refusal it has no way to work
        // around. `max bpc` is on the list for exactly that reason - it is a
        // range, weston 16 sets it on every output it enables, and its
        // one-value range is what makes the write a no-op rather than a lie -
        // while `EDID`, `non-desktop`, `vrr_capable`, `type` and `IN_FORMATS`
        // are `PROP_IMMUTABLE` and stay off it, which is Linux's own refusal
        // for a property a client may only read.
        let known = matches!(
            (obj, prop),
            (CONNECTOR_ID, PROP_CRTC_ID)
                | (CONNECTOR_ID, PROP_DPMS)
                | (CONNECTOR_ID, PROP_MAX_BPC)
                | (CRTC_ID, PROP_ACTIVE)
                | (CRTC_ID, PROP_MODE_ID)
                | (PLANE_ID, PROP_PLANE_FB_ID)
                | (PLANE_ID, PROP_PLANE_CRTC_ID)
                | (PLANE_ID, PROP_BLEND)
                | (PLANE_ID, PROP_IN_FENCE_FD)
                | (PLANE_ID, PROP_OUT_FENCE_PTR)
                | (CURSOR_PLANE_ID, PROP_PLANE_FB_ID)
                | (CURSOR_PLANE_ID, PROP_PLANE_CRTC_ID)
                | (CURSOR_PLANE_ID, PROP_CRTC_X)
                | (CURSOR_PLANE_ID, PROP_CRTC_Y)
        ) || (obj == PLANE_ID && (PROP_SRC_X..=PROP_CRTC_H).contains(&prop))
            || (obj == CURSOR_PLANE_ID && (PROP_SRC_X..=PROP_CRTC_H).contains(&prop));
        if !known {
            return Err(libc_einval());
        }
        if obj == CRTC_ID && prop == PROP_MODE_ID {
            self.mode_blob = Some(value as u32);
        }
        if obj == PLANE_ID && prop == PROP_PLANE_FB_ID {
            self.plane_fb = Some(value as u32);
        }
        if obj == CURSOR_PLANE_ID {
            match prop {
                PROP_PLANE_FB_ID => self.cursor_fb = Some(value as u32),
                // The source rectangle in 16.16 fixed point, the destination in
                // pixels. Kept so the validation pass can refuse a crop or a
                // scale this card cannot do rather than accept a state it would
                // then draw differently - see [`Commit::cursor_rect_ok`].
                PROP_SRC_X | PROP_SRC_Y | PROP_SRC_W | PROP_SRC_H => {
                    let mut r = self.cursor_src.unwrap_or((0, 0, 0, 0));
                    match prop {
                        PROP_SRC_X => r.0 = value as u32 >> 16,
                        PROP_SRC_Y => r.1 = value as u32 >> 16,
                        PROP_SRC_W => r.2 = value as u32 >> 16,
                        _ => r.3 = value as u32 >> 16,
                    }
                    self.cursor_src = Some(r);
                }
                PROP_CRTC_W | PROP_CRTC_H => {
                    let mut d = self.cursor_dst.unwrap_or((0, 0));
                    if prop == PROP_CRTC_W {
                        d.0 = value as u32;
                    } else {
                        d.1 = value as u32;
                    }
                    self.cursor_dst = Some(d);
                }
                _ => {}
            }
        }
        if obj == PLANE_ID && prop == PROP_IN_FENCE_FD {
            // Sign-extended by the caller because the property is a signed
            // range; -1 is the spelling of "no fence".
            self.in_fence_fd = Some(value as i32);
        }
        if obj == PLANE_ID && prop == PROP_OUT_FENCE_PTR {
            // The value is an address this card will write four bytes to once
            // the frame has gone, so it is checked here - where the value
            // arrives, and before anything has been applied - rather than at the
            // write. `0` is the property saying "no fence wanted", which is not
            // an address at all.
            let ptr = value as usize;
            if ptr != 0 && !crate::uaccess::buffer_ok(ptr, 4, true, true) {
                return Err(-14); // EFAULT
            }
            self.out_fence_ptr = (ptr != 0).then_some(ptr);
        }
        self.order.push((obj, prop, value));
        Ok(())
    }

    /// Whether the rectangle this commit asks for is one this card can honour:
    /// the image drawn whole, at its own size, with no crop and no scale.
    ///
    /// This is the property being true rather than merely published. The cursor
    /// plane offers `SRC_*` and `CRTC_W`/`CRTC_H` because weston will not name a
    /// state for a plane without them; what it does *with* them is this
    /// question. An identity rectangle (or no rectangle at all, which is the
    /// same thing) is the state the composition in `paint_cursor` draws, so it
    /// is accepted; anything else would be drawn differently from what the
    /// commit said, and is `EINVAL` - the answer a real card gives a cursor
    /// plane that cannot scale or crop.
    fn cursor_rect_ok(&self, st: &DrmState) -> bool {
        let fb = self.cursor_fb.unwrap_or(st.cursor_fb);
        if fb == 0 {
            // Nothing to show: the rectangles are not read by anything, so a
            // commit that only parks the pointer is free to name them.
            return true;
        }
        let (w, h) = cursor_image_dims(st, fb);
        if w == 0 {
            return false;
        }
        // A rectangle the commit did not name is the identity - the image whole
        // at its own size - which is what `paint_cursor` draws, so a commit that
        // only says "this buffer, here" is the same state said in fewer words.
        self.cursor_src.unwrap_or((0, 0, w, h)) == (0, 0, w, h)
            && self.cursor_dst.unwrap_or((w, h)) == (w, h)
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
/// The cursor plane's `IN_FORMATS`, which is its own blob because the formats are
/// the plane's rather than the card's - one array per plane, each over the
/// modifier this card's buffers are always in.
const DRIVER_BLOB_CURSOR_IN_FORMATS: u32 = 0x4000_0003;

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
            Some(crate::gpu::blob::edid_base_block(dims.0, dims.1, MONITOR_NAME).to_vec());
    }
    st.edid_bytes.as_deref().unwrap_or(&[])
}

/// The plane's `IN_FORMATS`: one modifier entry covering every format in
/// [`PLANE_FORMATS`], because a dumb buffer here is always linear. Built once -
/// neither the format list nor the modifier set depends on anything that can
/// change.
fn in_formats_blob_bytes(st: &mut DrmState) -> &[u8] {
    if st.in_formats_bytes.is_none() {
        st.in_formats_bytes = Some(crate::gpu::blob::in_formats(
            &PLANE_FORMATS,
            crate::gpu::blob::FORMAT_MOD_LINEAR,
        ));
    }
    st.in_formats_bytes.as_deref().unwrap_or(&[])
}

/// The cursor plane's `IN_FORMATS`, built the same way from its own shorter list.
/// A format list is a fact about a *plane*, which is why a real card publishes one
/// per plane and why this is a second blob rather than a second use of the first:
/// a compositor asking a cursor plane what it can scan must not be told what the
/// primary plane scans.
fn cursor_in_formats_blob_bytes(st: &mut DrmState) -> &[u8] {
    if st.cursor_in_formats_bytes.is_none() {
        st.cursor_in_formats_bytes = Some(crate::gpu::blob::in_formats(
            &CURSOR_FORMATS,
            crate::gpu::blob::FORMAT_MOD_LINEAR,
        ));
    }
    st.cursor_in_formats_bytes.as_deref().unwrap_or(&[])
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
            DRIVER_BLOB_CURSOR_IN_FORMATS => Some(cursor_in_formats_blob_bytes(&mut st).to_vec()),
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
    if id == DRIVER_BLOB_EDID || id == DRIVER_BLOB_IN_FORMATS || id == DRIVER_BLOB_CURSOR_IN_FORMATS
    {
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

/// The properties of a list a client without `DRM_CLIENT_CAP_ATOMIC` is shown:
/// the ones in [`ATOMIC_PROPS`] are taken out, in place, and the count comes
/// back with them. `values` runs parallel to `ids` and is compacted the same
/// way, because a caller reads the two as pairs.
///
/// The filter sits over an already-built list rather than inside every arm that
/// builds one, because an object's *contents* and who may see them are two
/// facts and only the second is what the cap changes. It is also the same skip
/// `GETCONNECTOR` makes over its own list, from the same table - the two must
/// agree, since a client reading a connector from one door and the other would
/// otherwise be handed two different connectors.
fn without_atomic(ids: &mut [u32], values: &mut [u64], count: usize) -> usize {
    let mut kept = 0;
    for index in 0..count {
        if ATOMIC_PROPS.contains(&ids[index]) {
            continue;
        }
        ids[kept] = ids[index];
        values[kept] = values[index];
        kept += 1;
    }
    kept
}

/// `OBJ_GETPROPERTIES`: the property ids and values one KMS object carries -
/// the two-call list shape again, over the same facts `GETCONNECTOR`
/// reports for the connector.
///
/// What it reports depends on who asks: the properties in [`ATOMIC_PROPS`] are
/// left out for a description that has not set `DRM_CLIENT_CAP_ATOMIC`. Asking
/// for one of them *by id* is still answered, and still reports the flag - see
/// [`get_property`], which is the whole of Linux's rule and the half the §7.4
/// sentence gets wrong: the cap gates the *enumeration*, not the door.
fn obj_get_properties(arg: usize, ofd: u64) -> isize {
    // struct drm_mode_obj_get_properties: props_ptr@0, prop_values_ptr@8,
    // count_props@16, obj_id@20, obj_type@24.
    let obj_id = unsafe { ((arg + 20) as *const u32).read_unaligned() };
    let obj_type = unsafe { ((arg + 24) as *const u32).read_unaligned() };
    // (ids, values, count) - the arrays are fixed-size so the lock only
    // guards a few word reads. Sixteen slots is the longest list any object
    // here carries, and the count is what says how much of it is real.
    let Some((ids, values, count)) = critical_section::with(|cs| {
        let st = STATE.borrow(cs).borrow();
        let list = match (obj_type, obj_id) {
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
                        PROP_OUT_FENCE_PTR,
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
                        // `OUT_FENCE_PTR` has no value to read back: it is a
                        // pointer the *caller* supplies for one commit, and
                        // after that commit it is the caller's variable, not
                        // this card's state.
                        0,
                        0,
                    ],
                    15usize,
                ))
            }
            // The cursor plane's properties: `type` so a compositor can find it,
            // `FB_ID`/`CRTC_ID` so it can be bound and unbound, the rectangles,
            // `CRTC_X`/`CRTC_Y` for where the pointer is, and its own
            // `IN_FORMATS`.
            //
            // The four rectangles are here, and they are the whole reason a
            // cursor appears at all. weston's atomic state is one
            // `drm_plane_state` for *every* plane, and
            // `drm_plane_state_add_props` adds `SRC_X`/`SRC_Y`/`SRC_W`/`SRC_H`
            // and `CRTC_W`/`CRTC_H` without first asking whether the plane has
            // them: a handle of 0 reaches the card as property 0, is refused,
            // and takes the whole commit down with it. That is what "couldn't
            // set plane state" and "atomic: couldn't compile atomic state" were,
            // and why the pointer moved and clicked correctly and was never
            // seen - weston could not name a state for the plane, so the plane
            // stayed empty.
            //
            // This card's cursor has no scaler and no crop - the image is drawn
            // whole, at its own size, with its hotspot on the pointer - so the
            // rectangles are honoured by *being* the image rather than by
            // transforming it: `SRC_*` and `CRTC_W`/`CRTC_H` must be the
            // identity for the buffer the commit binds, and a commit asking for
            // anything else is refused rather than quietly ignored (see
            // [`Commit::record`]). `CRTC_X`/`CRTC_Y` is the pointer and is acted
            // on. No `pixel blend mode`, which is the measurement rather than a
            // choice: a real card's cursor planes carry it nowhere, because a
            // cursor is always premultiplied.
            (OBJECT_PLANE, CURSOR_PLANE_ID) => {
                let (iw, ih) = cursor_image_dims(&st, st.cursor_fb);
                Some((
                    [
                        PROP_TYPE,
                        PROP_PLANE_FB_ID,
                        PROP_PLANE_CRTC_ID,
                        PROP_CRTC_X,
                        PROP_CRTC_Y,
                        PROP_CRTC_W,
                        PROP_CRTC_H,
                        PROP_SRC_X,
                        PROP_SRC_Y,
                        PROP_SRC_W,
                        PROP_SRC_H,
                        PROP_IN_FORMATS,
                        0,
                        0,
                        0,
                        0,
                    ],
                    [
                        PLANE_TYPE_CURSOR,
                        st.cursor_fb as u64,
                        st.cursor_crtc as u64,
                        st.cursor_pos.0 as i64 as u64,
                        st.cursor_pos.1 as i64 as u64,
                        iw as u64,
                        ih as u64,
                        0,
                        0,
                        iw as u64,
                        ih as u64,
                        DRIVER_BLOB_CURSOR_IN_FORMATS as u64,
                        0,
                        0,
                        0,
                        0,
                    ],
                    11usize,
                ))
            }
            _ => None,
        };
        let (mut ids, mut values, count) = list?;
        // What this client is shown. A description that has not set
        // `DRM_CLIENT_CAP_ATOMIC` is not shown the atomic properties *at all* -
        // not a smaller list of values, but a shorter list of ids - which is
        // what the measurement on this host's card found on a CRTC (six
        // properties uncapped, nine capped) and on a connector (seventeen and
        // eighteen, the difference being `CRTC_ID`). Asking for one by id is a
        // different door and is not filtered here; see [`get_property`].
        let count = if st.atomic_ofds.contains(&ofd) {
            count
        } else {
            without_atomic(&mut ids, &mut values, count)
        };
        Some((ids, values, count))
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

/// What a caller says a framebuffer *is*, gathered into one value so that
/// [`insert_fb`] takes one thing rather than seven.
///
/// The two doors fill it differently, and that is the point: `ADDFB`'s struct
/// has no format field, so `format` there is synthesized from `bpp`/`depth` by
/// [`legacy_fb_format`], and its `modifier` is always 0 because the struct
/// cannot carry one. `ADDFB2`'s fourcc and modifier are the caller's own words.
/// Both then store the same record, which is what lets `GETFB2` answer in terms
/// of the framebuffer rather than in terms of the request that made it.
struct FbShape {
    width: u32,
    height: u32,
    pitch: u32,
    bpp: u32,
    depth: u32,
    format: u32,
    modifier: u64,
}

/// Bind a dumb buffer to a fresh framebuffer id. The handle must be one the
/// card made or imported, and the geometry must be a shape the scanout can
/// show: 32bpp, nonzero, with a pitch wide enough for a row. `ADDFB` and
/// `ADDFB2` differ only in how they spell those facts.
///
/// `owner` is the description the request arrived on, recorded so that closing
/// it can take the framebuffers it made with it - see [`Framebuffer::owner`].
fn insert_fb(st: &mut DrmState, handle: u32, shape: FbShape, owner: u64) -> Option<u32> {
    let slot = handle as usize;
    // The handle's buffer must exist - and the geometry it comes with has to
    // be a shape the scanout can show: the handle's own bpp (the scanout is
    // a 32-bit BGRA surface, whatever the ADDFB claims), nonzero, with a
    // pitch wide enough for a row.
    let dumb = st.dumb.get(slot.checked_sub(1)?).and_then(|d| d.as_ref())?;
    if dumb.bpp != 32
        || shape.bpp != 32
        || shape.width == 0
        || shape.height == 0
        || shape.pitch < shape.width * 4
    {
        return None;
    }
    let id = st.fbs.len() as u32 + 100;
    st.fbs.push(Some(Framebuffer {
        id,
        handle,
        width: shape.width,
        height: shape.height,
        pitch: shape.pitch,
        bpp: shape.bpp,
        depth: shape.depth,
        format: shape.format,
        modifier: shape.modifier,
        owner,
    }));
    Some(id)
}

/// The fourcc Linux's `drm_mode_legacy_fb_format` synthesizes for a legacy
/// `ADDFB`, which names its layout as bits per pixel plus depth because the
/// struct predates fourcc codes entirely.
///
/// Only the two depths this card can *scan out* are mapped, and they are the
/// same two its `ADDFB2` maps: depth 24 is `XRGB8888` and depth 32 is
/// `ARGB8888`, which is what `ADDFB2`'s own fourcc table says for the same
/// pixels. Answering anything else would be a format `GETFB2` reports and
/// `ADDFB2` refuses, so an `ADDFB` at any other depth reports `0` - "made
/// without a format" - rather than a layout nothing here would produce.
fn legacy_fb_format(bpp: u32, depth: u32) -> u32 {
    match (bpp, depth) {
        (32, 24) => DRM_FORMAT_XRGB8888,
        (32, 32) => DRM_FORMAT_ARGB8888,
        _ => 0,
    }
}

fn add_fb(arg: usize, owner: u64) -> isize {
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
            FbShape {
                width,
                height,
                pitch,
                bpp,
                depth,
                format: legacy_fb_format(bpp, depth),
                modifier: 0,
            },
            owner,
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

fn add_fb2(arg: usize, owner: u64) -> isize {
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
    // Kept for `GETFB2` to report: the plane is linear either way, but *how*
    // the framebuffer was made is a fact the caller asked for and this card
    // knows.
    let modifier = if flags & FB_MODIFIERS != 0 {
        if modifier != crate::gpu::blob::FORMAT_MOD_LINEAR {
            return libc_einval();
        }
        modifier
    } else {
        0
    };
    let (bpp, depth) = match format {
        DRM_FORMAT_XRGB8888 => (32, 24),
        DRM_FORMAT_ARGB8888 => (32, 32),
        _ => return libc_einval(),
    };
    let id = critical_section::with(|cs| {
        insert_fb(
            &mut STATE.borrow(cs).borrow_mut(),
            handle,
            FbShape {
                width,
                height,
                pitch,
                bpp,
                depth,
                format,
                modifier,
            },
            owner,
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

/// `GETFB2`: the framebuffer's identity *with its format*, which the legacy
/// `GETFB` above cannot spell.
///
/// `struct drm_mode_fb_cmd2` is input and output at once: `fb_id` goes in and
/// the geometry, the fourcc, the modifier and the plane arrays come back. That
/// is the whole reason libdrm's `drmModeGetFB2` exists next to
/// `drmModeGetFB` - a compositor handed a framebuffer it did not make (out of
/// a lease, or from another client) can learn what to map, which the legacy
/// struct's `bpp`/`depth` pair cannot say.
///
/// **Every byte of the caller's struct is written, including the three planes
/// this card does not have.** The caller allocated those 104 bytes with
/// `malloc`, and libdrm reads `handles`, `pitches`, `offsets` and `modifier` as
/// four-element arrays whose meaningful length is "how many are non-zero" - so a
/// struct only half-filled is a framebuffer the caller believes has four planes
/// over four addresses it never provided. That is the same failure the mode-name
/// comment in `write_mode` describes, one field over. The three unused planes
/// are therefore written as zeros, and `flags` reports whether `modifier[0]`
/// means anything, which is precisely the flag `ADDFB2` had to be given for the
/// modifier to have been one.
///
/// An unknown `fb_id` is `EINVAL`, matching `get_fb` rather than the `ENOENT` a
/// fresh reading of Linux might suggest: the two spellings of this question are
/// answered alike here, and `t-drm-args` already pins that answer for the older
/// one.
fn get_fb2(arg: usize) -> isize {
    // struct drm_mode_fb_cmd2: fb_id@0, width@4, height@8, pixel_format@12,
    // flags@16, handles[4]@20, pitches[4]@36, offsets[4]@52, modifier[4]@68.
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
            let p = arg as *mut u8;
            let w32 = |off: usize, v: u32| (p.add(off) as *mut u32).write_unaligned(v);
            let w64 = |off: usize, v: u64| (p.add(off) as *mut u64).write_unaligned(v);
            w32(4, fb.width);
            w32(8, fb.height);
            w32(12, fb.format);
            w32(16, if fb.modifier != 0 { FB_MODIFIERS } else { 0 });
            for plane in 0..4usize {
                let first = plane == 0;
                // One plane holds the buffer and the rest say "unused", which
                // every reader of the struct spells as a zero.
                w32(20 + plane * 4, if first { fb.handle } else { 0 });
                w32(36 + plane * 4, if first { fb.pitch } else { 0 });
                w32(52 + plane * 4, 0); // offsets: the buffer starts at its start
                w64(68 + plane * 8, if first { fb.modifier } else { 0 });
            }
        }
        0
    })
}

/// `MODE_GETGAMMA`/`MODE_SETGAMMA` - the legacy gamma pair - refused for a CRTC
/// with no gamma table.
///
/// This is the door a compositor older than the `GAMMA_LUT` blob property reads
/// a colour ramp through, and this card has no ramp to read: no
/// colour-management engine, nothing behind the property, and §7.4 records
/// `GAMMA_LUT`/`GAMMA_LUT_SIZE` as **deliberately absent** for that same reason.
/// `GETCRTC` now reports `gamma_size` 0, so a caller is told the shape before it
/// asks, and this is what it hears if it asks anyway.
///
/// `EINVAL` is Linux's answer for such a CRTC: `drm_mode_getgamma` and
/// `drm_mode_setgamma` refuse a CRTC whose `gamma_size` is zero before they
/// reach the driver's `gamma_get`/`gamma_set` callback - in the 3.7 tree
/// `setgamma` returns `-EINVAL` on a `gamma_size` the CRTC cannot hold and
/// `-ENOSYS` only for a missing callback, which is a distinction a CRTC with no
/// table never gets to. The *number* is from Linux's `drivers/gpu/drm/drm_crtc.c`
/// rather than from a measurement on this machine, because no card here has a
/// CRTC without a gamma table to measure: the amdgpu CRTC next door reports
/// `GAMMA_LUT_SIZE` 4096. It is recorded as such in `NOTES.md` §7.4.
///
/// A refusal rather than an accepted no-op, because `OK` from `MODE_SETGAMMA`
/// is a compositor believing it applied a colour transformation no pixel will
/// ever show - the silent success `AGENTS.md` §4 forbids.
fn gamma_refused() -> isize {
    libc_einval()
}

/// `CRTC_GET_SEQUENCE`/`CRTC_QUEUE_SEQUENCE` refused: this card has no vertical
/// blank to report a sequence against.
///
/// `GET_SEQUENCE` asks for the CRTC's *vblank* counter and a nanosecond timestamp
/// of the last one; `QUEUE_SEQUENCE` asks to be told by `DRM_EVENT_CRTC_SEQUENCE`
/// when that counter reaches a number the caller names. Both are questions about
/// a clock, and this card has none: the only boundary it has is a frame it
/// presented, which is `drivers::video::render::landings` and the flip sequence
/// `st.seq`, and those count *commits* rather than periodic boundaries.
/// `LANDINGS`' own comment already states the rule this refusal follows -
/// "Calling it a vblank counter would be a lie the ABI cannot tell" - and §7.1
/// recorded the same decision for `WAIT_VBLANK` and its sequence facility:
/// `ENOTTY` rather than an answer that would be a clock this kernel does not
/// have.
///
/// `ENOTTY` is also what these two requests already fell through to, since
/// neither was in the dispatch `match` at all. Naming them here is what makes the
/// answer deliberate rather than an accident of the catch-all, and it is the same
/// answer §7.1 recorded for the other vblank door - one card with two spellings
/// of "I cannot tell you about vblanks" that disagreed would be the private
/// dialect `AGENTS.md` §4.3 forbids. What actually changes for a caller is the
/// `arg_shape` row: Linux's ioctl layer copies every DRM struct in before a
/// handler can refuse it, so a bad pointer now hears `EFAULT` where before this
/// answered `ENOTTY` without ever looking at the struct.
fn sequence_refused() -> isize {
    -25 // ENOTTY
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
    // The device resource id, read while the buffer is in the table. Taken out
    // of the critical section rather than dropped with the rest because telling
    // the device is a *job*, and a job parks: parking with interrupts off is
    // sleeping while holding a lock, which this kernel forbids.
    let found = critical_section::with(|cs| {
        let mut st = STATE.borrow(cs).borrow_mut();
        st.prime.retain(|record| record.handle as usize != handle);
        match st.dumb.get_mut(handle - 1) {
            Some(slot) if slot.is_some() => {
                let res = slot
                    .as_ref()
                    .and_then(|d| d.resource.as_ref())
                    .map(|r| r.id);
                if let Some(d) = slot.as_mut() {
                    d.release();
                }
                *slot = None;
                Some(res)
            }
            _ => None,
        }
    });
    let Some(res) = found else {
        return libc_einval();
    };
    // A resource the device still holds is told to let go. The answer is
    // ignored: the caller asked for the handle to be released and it has been,
    // and a device that refuses to unref a resource whose pages are already back
    // in the allocator is not something this caller can act on either way. A
    // plain dumb buffer was never a resource and skips this entirely.
    if let Some(id) = res {
        let _ =
            crate::drivers::video::render::job(crate::drivers::video::render::Job::Unref { id });
    }
    0
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
            // An imported dma-buf is not a `DRM_VIRTGPU_*` resource: nothing told
            // the device about it, and its pixels reach the screen through this
            // card's own scanout copy like any other buffer's.
            resource: None,
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

/// One sync object as *this card* keeps it: the handle a client named it by, the
/// open file description that made it, the state machine `crate::gpu::sync`
/// decides with, and the eventfd registrations waiting on it.
///
/// The handle is per *description* rather than per card, which is Linux's shape
/// and the reason the family is usable at all: two clients each get their own
/// numbering, and passing a `SYNCOBJ_HANDLE_TO_FD` descriptor between them is
/// what makes one client's timeline waitable by another.
struct SyncObjRec {
    owner: u64,
    handle: u32,
    obj: Arc<crate::gpu::sync::SyncObj>,
    /// One entry per `SYNCOBJ_EVENTFD` call. Each fires at most once - that is
    /// what was measured, and it is what makes "register, signal, read one"
    /// composable.
    events: Vec<SyncEvent>,
}

/// One `SYNCOBJ_EVENTFD` registration: the counter to bump, the point it was
/// asked about, and whether it has already been handed its one increment.
struct SyncEvent {
    /// The eventfd node itself, held as a node rather than as a number: a
    /// registration outlives the request that made it, and the increment it owes
    /// later has to reach a counter that is still there. This is the reference
    /// Linux's `eventfd_ctx_fdget` takes for the same reason.
    counter: Arc<dyn Inode>,
    /// The point asked about. Zero is not a point: it is the request saying
    /// "whatever this object is", which is how a plain binary object is
    /// registered, and the measured card fires that one on the fence alone.
    point: u64,
    /// `DRM_SYNCOBJ_WAIT_FLAGS_WAIT_AVAILABLE`: fire when *a fence exists* rather
    /// than when the point is covered.
    available: bool,
    fired: bool,
}

/// Cap on the sync objects one card tracks. Linux numbers them from an idr with
/// no ceiling of its own; this card has to bound a table a client can grow
/// (`AGENTS.md` §4.4), and no compositor is within two orders of magnitude of
/// this one.
const MAX_SYNCOBJS: usize = 256;
/// Cap on the handles one `SYNCOBJ_WAIT` may name. The real card accepted 300;
/// this is the point past which the count is a corrupted struct rather than a
/// request.
const MAX_SYNC_WAIT: usize = 4096;

/// The sync objects' state machine, in the one place a host test can reach it.
/// What it means is documented there and measured in `NOTES.md` §7.4.
///
/// The descriptor `SYNCOBJ_HANDLE_TO_FD` with `flags == 0` answers: an anonymous
/// file that *is* the sync object, so a second description that takes it in
/// (`SCM_RIGHTS`, or simply a `dup`) can `SYNCOBJ_FD_TO_HANDLE` the same object
/// into its own handle table. That sharing is the whole point of the protocol it
/// exists for - `wp_linux_drm_syncobj_v1` passes one between the client that
/// rendered and the compositor that will wait.
pub struct SyncObjFd {
    ino: u64,
    obj: Arc<crate::gpu::sync::SyncObj>,
}

impl SyncObjFd {
    fn new(obj: Arc<crate::gpu::sync::SyncObj>) -> Arc<SyncObjFd> {
        static NEXT: AtomicU64 = AtomicU64::new(0x9200);
        Arc::new(SyncObjFd {
            ino: NEXT.fetch_add(1, Ordering::Relaxed),
            obj,
        })
    }

    /// The object behind the descriptor, for the import that installs a handle
    /// for it in another table.
    pub fn object(&self) -> Arc<crate::gpu::sync::SyncObj> {
        self.obj.clone()
    }
}

impl Inode for SyncObjFd {
    fn metadata(&self) -> Metadata {
        // Anonymous, like every other descriptor that is not a file: the number
        // only has to be one nothing else claims, and `fstat` is the only
        // reporter.
        Metadata {
            ino: self.ino,
            kind: NodeKind::CharDev,
            size: 0,
        }
    }
}

/// The descriptor `SYNCOBJ_HANDLE_TO_FD` with `EXPORT_SYNC_FILE` answers, and
/// the one a commit's out-fence leaves in the caller's table: `struct sync_file`,
/// a fence behind a file descriptor.
///
/// It is a *readable* descriptor - `poll` reports `POLLIN` once the fence has
/// retired - and that readiness is the only thing it has to say, which is why
/// there is no `read_at` here: a caller waits on it or hands it to someone who
/// will.
pub struct SyncFileFd {
    ino: u64,
    fence: Arc<crate::gpu::sync::Fence>,
}

impl SyncFileFd {
    fn new(fence: Arc<crate::gpu::sync::Fence>) -> Arc<SyncFileFd> {
        static NEXT: AtomicU64 = AtomicU64::new(0x9300);
        Arc::new(SyncFileFd {
            ino: NEXT.fetch_add(1, Ordering::Relaxed),
            fence,
        })
    }

    /// The fence behind the descriptor, for `FD_TO_HANDLE` with
    /// `IMPORT_SYNC_FILE`.
    pub fn fence(&self) -> Arc<crate::gpu::sync::Fence> {
        self.fence.clone()
    }
}

impl Inode for SyncFileFd {
    fn metadata(&self) -> Metadata {
        Metadata {
            ino: self.ino,
            kind: NodeKind::CharDev,
            size: 0,
        }
    }

    /// A `sync_file` is readable exactly when its fence has retired - Linux's
    /// answer, and the one a compositor's event loop is built on.
    fn poll_ready(&self) -> Ready {
        Ready {
            read: self.fence.has_retired(),
            write: false,
        }
    }
}

/// The object a `SyncObjFd` holds, behind one more `Arc` so that the descriptor
/// type can live here while the object's own type lives in the host-tested leaf
/// module.
///
/// The extra indirection is deliberate rather than accidental: `crate::gpu::sync`
/// knows nothing about descriptors, and a `pub fn` on it that returned an `Arc`
/// of its own type would be the same thing without the wrapper. This is the
/// wrapper.
///
/// (The alias's fields are `pub(crate)` in spirit; see `drm_sync::SyncObjectAlias`.)
fn syncobj_lookup(
    st: &DrmState,
    owner: u64,
    handle: u32,
) -> Option<Arc<crate::gpu::sync::SyncObj>> {
    st.syncobjs
        .iter()
        .flatten()
        .find(|rec| rec.owner == owner && rec.handle == handle)
        .map(|rec| rec.obj.clone())
}

/// Resolve every handle of a `WAIT`-shaped request before any condition is
/// looked at: a request that names an object the caller does not have is refused
/// as a whole rather than partly answered, which is the same property the
/// commit path keeps with its plan.
fn syncobj_resolve(
    st: &DrmState,
    owner: u64,
    handles: &[u32],
) -> Result<Vec<Arc<crate::gpu::sync::SyncObj>>, isize> {
    let mut out = Vec::with_capacity(handles.len());
    for &handle in handles {
        match syncobj_lookup(st, owner, handle) {
            Some(obj) => out.push(obj),
            None => return Err(-2), // ENOENT
        }
    }
    Ok(out)
}

/// The handle array of a `SYNCOBJ_WAIT`/`_RESET`/`_SIGNAL`-shaped request.
///
/// `count == 0` is a successful no-op on the measured card and not an error, so
/// it is answered before the pointer is looked at - and a null pointer with a
/// non-zero count is `EFAULT`, which is what the kernel's own `copy_from_user`
/// of the array would give.
fn syncobj_handles(ptr: usize, count: usize) -> Result<Vec<u32>, isize> {
    if count == 0 {
        return Ok(Vec::new());
    }
    if count > MAX_SYNC_WAIT {
        return Err(libc_einval());
    }
    if ptr == 0 || !user_array(ptr, count, 4, false) {
        return Err(-14); // EFAULT
    }
    Ok(unsafe { core::slice::from_raw_parts(ptr as *const u32, count) }.to_vec())
}

/// A DRM absolute `CLOCK_MONOTONIC` deadline in nanoseconds, as the tick
/// deadline `block_until_deadline` keeps.
///
/// Zero is "do not wait" and not "wait for one tick": the measured card answers
/// `ETIME` at once when a `WAIT_FOR_SUBMIT` cannot be satisfied, so the deadline
/// has to be *now* - which is what `block_until_deadline` reports on its first
/// pass. A negative timeout is the header's "wait for ever".
fn wait_deadline(nsec: i64) -> u64 {
    if nsec < 0 {
        return u64::MAX;
    }
    let per_tick = 1_000_000_000u64 / crate::hal::TICK_HZ as u64;
    let left = (nsec as u64).saturating_sub(crate::hal::since_boot_nanos());
    crate::hal::ticks().saturating_add(left / per_tick.max(1))
}

/// Park until `satisfied` holds, the deadline passes, or a signal arrives, and
/// answer the errno the request owes. Returns 0 when the wait ended because the
/// condition held.
///
/// The objects are the ones a waiter has to be registered on: a sync object's
/// readiness changes when *another task submits a fence to it*, and that task has
/// no way to find a sleeper it was not told about - the same lock-poll-try-unlock-
/// park shape `vfs::eventfd` uses for its counter.
///
/// The waking half of that is the kernel's rather than the state machine's: a
/// parked waiter here is a scheduler task, and `gpu::sync` deliberately cannot
/// name one, so the *how* arrives as this implementation of the crate's `Waker`.
struct SchedWaker;

impl crate::gpu::sync::Waker for SchedWaker {
    fn wake(&mut self, waiter: crate::gpu::sync::WaiterId) {
        crate::sched::wake(crate::sched::TaskId(waiter));
    }
}

fn syncobj_park(
    objs: &[Arc<crate::gpu::sync::SyncObj>],
    nsec: i64,
    satisfied: impl Fn() -> bool,
) -> isize {
    let me = crate::sched::current().0;
    for obj in objs {
        obj.subscribe(me);
    }
    let end = crate::sched::block_until_deadline(&satisfied, wait_deadline(nsec));
    for obj in objs {
        obj.unsubscribe(me);
    }
    match end {
        crate::sched::WaitEnd::Condition => 0,
        crate::sched::WaitEnd::Deadline => -62,   // ETIME
        crate::sched::WaitEnd::Interrupted => -4, // EINTR
    }
}

/// Whether one handle of a wait array is satisfied. `point == 0` is a plain
/// object's question, which is "is a fence behind it" - the measured card's
/// reading, where a point that was never asked about is not asked about.
fn syncobj_one_ready(obj: &crate::gpu::sync::SyncObj, point: u64, for_submit: bool) -> bool {
    obj.ready((point != 0).then_some(point), for_submit)
}

/// The readiness of a whole `WAIT` array: `all` is
/// `DRM_SYNCOBJ_WAIT_FLAGS_WAIT_ALL`, and otherwise *any* satisfied handle ends
/// the wait.
fn syncobj_any_ready(
    objs: &[Arc<crate::gpu::sync::SyncObj>],
    points: &[u64],
    for_submit: bool,
    all: bool,
) -> bool {
    if all {
        objs.iter()
            .zip(points.iter())
            .all(|(obj, &point)| syncobj_one_ready(obj, point, for_submit))
    } else {
        objs.iter()
            .zip(points.iter())
            .any(|(obj, &point)| syncobj_one_ready(obj, point, for_submit))
    }
}

/// Which handle of the array the answer's `first_signaled` names: the first one
/// that is satisfied. `WAIT_ALL` has no such index to give and the measured card
/// leaves the field alone for it, so this is only meaningful for the `any` shape.
fn syncobj_first_ready(
    objs: &[Arc<crate::gpu::sync::SyncObj>],
    points: &[u64],
    for_submit: bool,
) -> u32 {
    objs.iter()
        .zip(points.iter())
        .position(|(obj, &point)| syncobj_one_ready(obj, point, for_submit))
        .unwrap_or(0) as u32
}

/// Hand the eventfd registrations on a record whatever they have come due - the
/// increment each was promised, once.
fn fire_sync_events(rec: &mut SyncObjRec) {
    for event in rec.events.iter_mut() {
        if event.fired {
            continue;
        }
        let due = if event.available {
            // `WAIT_AVAILABLE`: a fence existing is the whole question.
            rec.obj.has_fence()
        } else {
            // Point 0 is not a point: it is the request saying "whatever this
            // object is", which is how a plain binary syncobj is registered.
            let point = (event.point != 0).then_some(event.point);
            rec.obj.ready(point, false)
        };
        if due {
            event.fired = true;
            // One increment, saturating, which is what wakes whoever is in
            // `poll` on the eventfd - the reader an event loop already has. The
            // write is the counter node's own, so it carries the same wake the
            // eventfd's `write` syscall does rather than a second one beside it.
            let _ = Inode::write_at(&*event.counter, 0, &1u64.to_ne_bytes());
        }
    }
}

/// `SYNCOBJ_CREATE`: a new object, and the handle that names it.
///
/// `DRM_SYNCOBJ_CREATE_SIGNALED` is the difference between an object every
/// request about a fence will refuse and one with a fence already behind it -
/// measured, and the opposite reading from the upstream sources, which treat
/// *both* as signaled. `NOTES.md` §7.4 has the measurement.
fn syncobj_create(owner: u64, arg: usize) -> isize {
    // struct drm_syncobj_create: handle@0, flags@4.
    let flags = unsafe { ((arg + 4) as *const u32).read_unaligned() };
    if flags & !SYNCOBJ_CREATE_SIGNALED != 0 {
        return libc_einval();
    }
    let handle = critical_section::with(|cs| {
        let mut st = STATE.borrow(cs).borrow_mut();
        let st = &mut *st;
        if st.syncobjs.len() >= MAX_SYNCOBJS {
            return 0;
        }
        // Lowest free number *for this description*, which is what Linux's idr
        // gives and what a test that creates and destroys in a loop sees.
        let mut handle = 1u32;
        while syncobj_lookup(st, owner, handle).is_some() {
            handle += 1;
        }
        st.syncobjs.push(Some(SyncObjRec {
            owner,
            handle,
            obj: crate::gpu::sync::SyncObj::new(flags & SYNCOBJ_CREATE_SIGNALED != 0),
            events: Vec::new(),
        }));
        handle
    });
    if handle == 0 {
        return libc_einval(); // the table is full
    }
    unsafe { (arg as *mut u32).write_unaligned(handle) };
    0
}

/// `SYNCOBJ_DESTROY`: the handle stops naming anything.
///
/// The object itself lives on while a `SYNCOBJ_HANDLE_TO_FD` descriptor names it,
/// which is exactly the sharing the descriptor exists for. A registration that
/// has not fired never will, which is what the measured card does: destroying the
/// object leaves the eventfd at zero rather than handing it an increment for a
/// point nobody will reach.
fn syncobj_destroy(owner: u64, arg: usize) -> isize {
    let handle = unsafe { (arg as *const u32).read_unaligned() };
    critical_section::with(|cs| {
        let mut st = STATE.borrow(cs).borrow_mut();
        let st = &mut *st;
        // A handle that names nothing is `EINVAL` here and `ENOENT` from the
        // requests that *operate* on an object. Both were measured that way.
        let Some(slot) = st.syncobjs.iter_mut().find(|rec| {
            rec.as_ref()
                .is_some_and(|rec| rec.owner == owner && rec.handle == handle)
        }) else {
            return libc_einval();
        };
        *slot = None;
        st.syncobjs.retain(Option::is_some);
        0
    })
}

/// `SYNCOBJ_HANDLE_TO_FD`: the object behind a descriptor.
///
/// Two descriptors come out of this, and which one is asked for is
/// `DRM_SYNCOBJ_HANDLE_TO_FD_FLAGS_EXPORT_SYNC_FILE`. Without it the descriptor
/// *is* the object and another description can name the same timeline; with it
/// the descriptor is a `sync_file` over the fence the object holds, so a caller
/// that only wants to wait on one gets nothing that could be handed on. The
/// export refuses an object with no fence, which the measured card does and the
/// sources do not - there is nothing to export.
fn syncobj_handle_to_fd(owner: u64, arg: usize) -> isize {
    // struct drm_syncobj_handle: handle@0, flags@4, fd@8, pad@12, point@16.
    let handle = unsafe { (arg as *const u32).read_unaligned() };
    let flags = unsafe { ((arg + 4) as *const u32).read_unaligned() };
    let point = unsafe { ((arg + 16) as *const u64).read_unaligned() };
    if flags & !(SYNCOBJ_EXPORT_SYNC_FILE | SYNCOBJ_FLAGS_TIMELINE | DRM_CLOEXEC) != 0 {
        return libc_einval();
    }
    // What `point` means, measured on the real card for every combination of
    // flags and points (§7.4) rather than read off the sources, which say
    // nothing about it:
    //
    //   * **A non-zero point needs `EXPORT_SYNC_FILE`.** That is the only form
    //     whose descriptor can carry one: `flags = 0` and `TIMELINE` alone with
    //     point 5 are both `EINVAL`, while `EXPORT_SYNC_FILE` with the same
    //     point on the same object succeeds.
    //   * **With `TIMELINE` the point is a *timeline* point**, so it must be one
    //     the object has reached: a binary object - which has no timeline and so
    //     no point at all - refuses any non-zero point, and an object at 5
    //     answers 0, 1, 3 and 5 and refuses 6 and 100. That last pair is the
    //     part worth having measured: the rule is `point <= the object's`, not
    //     merely "the object is a timeline".
    //   * **Without `TIMELINE` the point is not read at all.** `EXPORT_SYNC_FILE`
    //     with point 5 on a *binary* object succeeds, which is what says the flag
    //     is what turns the number into a timeline point rather than the number
    //     by itself.
    if point != 0 && flags & SYNCOBJ_EXPORT_SYNC_FILE == 0 {
        return libc_einval();
    }
    let obj = critical_section::with(|cs| {
        let st = STATE.borrow(cs).borrow();
        syncobj_lookup(&st, owner, handle)
    });
    let Some(obj) = obj else {
        return -2; // ENOENT
    };
    if point != 0
        && flags & SYNCOBJ_FLAGS_TIMELINE != 0
        && !(obj.is_timeline() && point <= obj.point())
    {
        return libc_einval();
    }
    let want_file = flags & SYNCOBJ_EXPORT_SYNC_FILE != 0;
    let inode: Arc<dyn Inode> = if want_file {
        match obj.fence() {
            Some(fence) => SyncFileFd::new(fence),
            None => return libc_einval(),
        }
    } else {
        SyncObjFd::new(obj)
    };
    let file = Arc::new(crate::vfs::File::new(inode));
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

/// `SYNCOBJ_FD_TO_HANDLE`: a descriptor's object, under a handle in the caller's
/// own table.
///
/// The two directions take the two descriptors `HANDLE_TO_FD` makes, and they do
/// not take each other's: without the flag the descriptor must *be* the object
/// (and `handle` must be 0, because a new name is the only thing this can
/// produce), while with `IMPORT_SYNC_FILE` it must be a `sync_file` and `handle`
/// must name an object that already exists, whose fence is then replaced. Asking
/// for a new handle with the import flag is `ENOENT` on the measured card - and
/// that is the whole difference between the two, so it is pinned rather than
/// smoothed over.
fn syncobj_fd_to_handle(owner: u64, arg: usize) -> isize {
    let handle = unsafe { (arg as *const u32).read_unaligned() };
    let flags = unsafe { ((arg + 4) as *const u32).read_unaligned() };
    let fd = unsafe { ((arg + 8) as *const i32).read_unaligned() };
    let point = unsafe { ((arg + 16) as *const u64).read_unaligned() };
    if flags & !(SYNCOBJ_IMPORT_SYNC_FILE | SYNCOBJ_FLAGS_TIMELINE) != 0 {
        return libc_einval();
    }
    // The other half of the export side's first rule, and measured the same way:
    // a non-zero point is the *import* form's to name, and with `flags = 0` or
    // `TIMELINE` alone it is refused - importing a plain object with point 5 is
    // `EINVAL` where point 0 succeeds. What the import form does with the point
    // is the gap named below.
    if point != 0 && flags & SYNCOBJ_IMPORT_SYNC_FILE == 0 {
        return libc_einval();
    }
    let file = match crate::sched::current_fds().clone_at(fd) {
        Ok(file) => file,
        Err(_) => return -9, // EBADF
    };
    let inode = file.inode();
    if flags & SYNCOBJ_IMPORT_SYNC_FILE != 0 {
        // The object is named first, so that a caller which has neither hears
        // the same answer the measured card gives rather than the fd's.
        let Some(target) = critical_section::with(|cs| {
            let st = STATE.borrow(cs).borrow();
            syncobj_lookup(&st, owner, handle)
        }) else {
            return -2; // ENOENT
        };
        let Some(sync_file) = inode.as_any().downcast_ref::<SyncFileFd>() else {
            return libc_einval();
        };
        // The fence is handed on and the destination's *point* is not set from
        // it, which is where this is still narrower than the measured card -
        // and the measurement is the part worth having, because it says the flag
        // is not the no-op the constant's old note called it (§7.4). Importing
        // the `sync_file` of a timeline at 9 into an object at 4 leaves that
        // object at **9**; adding `TIMELINE` to the same request leaves it at
        // the caller's **7** instead. So the flag's meaning on this side is
        // "the point in *this request* is the point to land on, rather than the
        // one the fence was made at", and this card cannot express either yet: a
        // [`Fence`] here is a retired flag with no point to inherit
        // (`crates/gpu/src/sync.rs`), so `submit` with `None` leaves the
        // destination where it was. Closing it means giving a fence the point it
        // was made at, which is the change this note is here to make cheap for
        // whoever measures the rest of the matrix.
        target.submit(sync_file.fence(), None, &mut SchedWaker);
        critical_section::with(|cs| {
            let mut st = STATE.borrow(cs).borrow_mut();
            sync_events_left(&mut st, owner, handle);
        });
        return 0;
    }
    if handle != 0 {
        // Without the import flag there is nothing to replace.
        return libc_einval();
    }
    let Some(syncobj_fd) = inode.as_any().downcast_ref::<SyncObjFd>() else {
        return libc_einval();
    };
    let object = syncobj_fd.object();
    let handle = critical_section::with(|cs| {
        let mut st = STATE.borrow(cs).borrow_mut();
        let st = &mut *st;
        if st.syncobjs.len() >= MAX_SYNCOBJS {
            return 0;
        }
        let mut handle = 1u32;
        while syncobj_lookup(st, owner, handle).is_some() {
            handle += 1;
        }
        st.syncobjs.push(Some(SyncObjRec {
            owner,
            handle,
            obj: object,
            events: Vec::new(),
        }));
        handle
    });
    if handle == 0 {
        return libc_einval();
    }
    unsafe { (arg as *mut u32).write_unaligned(handle) };
    0
}

/// `SYNCOBJ_WAIT`: wait for the fences behind a list of objects.
///
/// The measured rule that makes this unlike the sources: **an object with no
/// fence is refused**, not treated as already signaled, unless the caller said
/// `WAIT_FOR_SUBMIT` - in which case there is something to wait for after all and
/// the wait parks until another task submits a fence. Without that flag a wait on
/// an empty object could never end, which is why the card answers `EINVAL` rather
/// than blocking.
fn syncobj_wait(owner: u64, arg: usize) -> isize {
    // struct drm_syncobj_wait: handles@0, timeout_nsec@8, count_handles@16,
    // flags@20, first_signaled@24, pad@28, deadline_nsec@32.
    let ptr = unsafe { (arg as *const u64).read_unaligned() } as usize;
    let timeout = unsafe { ((arg + 8) as *const i64).read_unaligned() };
    let count = unsafe { ((arg + 16) as *const u32).read_unaligned() } as usize;
    let flags = unsafe { ((arg + 20) as *const u32).read_unaligned() };
    const KNOWN: u32 =
        SYNCOBJ_WAIT_ALL | SYNCOBJ_WAIT_FOR_SUBMIT | SYNCOBJ_WAIT_AVAILABLE | SYNCOBJ_WAIT_DEADLINE;
    if flags & !KNOWN != 0 {
        return libc_einval();
    }
    let handles = match syncobj_handles(ptr, count) {
        Ok(handles) => handles,
        Err(e) => return e,
    };
    if handles.is_empty() {
        return 0;
    }
    let objs = critical_section::with(|cs| {
        let st = STATE.borrow(cs).borrow();
        syncobj_resolve(&st, owner, &handles)
    });
    let objs = match objs {
        Ok(objs) => objs,
        Err(e) => return e,
    };
    let all = flags & SYNCOBJ_WAIT_ALL != 0;
    let for_submit = flags & SYNCOBJ_WAIT_FOR_SUBMIT != 0;
    // No point is named: a plain object's whole question is whether a fence is
    // behind it.
    let points = alloc::vec![0u64; objs.len()];
    let satisfied = || syncobj_any_ready(&objs, &points, for_submit, all);
    if !for_submit && !satisfied() {
        // An object with no fence, and no promise that one is coming.
        return libc_einval();
    }
    if !satisfied() {
        let rc = syncobj_park(&objs, timeout, satisfied);
        if rc != 0 {
            return rc;
        }
    }
    if !all {
        let first = syncobj_first_ready(&objs, &points, for_submit);
        unsafe { ((arg + 24) as *mut u32).write_unaligned(first) };
    }
    0
}

/// `SYNCOBJ_TIMELINE_WAIT`: the same, for a point on each object.
///
/// Two rules are this request's own. An object that has never had a point
/// submitted is **not a timeline**, and asking it about a point is `EINVAL`
/// rather than a wait that could never end. And `WAIT_FOR_SUBMIT` again settles
/// on the fence's presence: the measured card answers `ok` to a point it has
/// never reached, as soon as something is behind the object.
fn syncobj_timeline_wait(owner: u64, arg: usize) -> isize {
    // struct drm_syncobj_timeline_wait: handles@0, points@8, timeout_nsec@16,
    // count_handles@24, flags@28, first_signaled@32, pad@36, deadline_nsec@40.
    let handles_ptr = unsafe { (arg as *const u64).read_unaligned() } as usize;
    let points_ptr = unsafe { ((arg + 8) as *const u64).read_unaligned() } as usize;
    let timeout = unsafe { ((arg + 16) as *const i64).read_unaligned() };
    let count = unsafe { ((arg + 24) as *const u32).read_unaligned() } as usize;
    let flags = unsafe { ((arg + 28) as *const u32).read_unaligned() };
    const KNOWN: u32 =
        SYNCOBJ_WAIT_ALL | SYNCOBJ_WAIT_FOR_SUBMIT | SYNCOBJ_WAIT_AVAILABLE | SYNCOBJ_WAIT_DEADLINE;
    if flags & !KNOWN != 0 {
        return libc_einval();
    }
    let handles = match syncobj_handles(handles_ptr, count) {
        Ok(handles) => handles,
        Err(e) => return e,
    };
    if handles.is_empty() {
        return 0;
    }
    if points_ptr == 0 || !user_array(points_ptr, count, 8, false) {
        return -14; // EFAULT
    }
    let points = unsafe { core::slice::from_raw_parts(points_ptr as *const u64, count) }.to_vec();
    let objs = critical_section::with(|cs| {
        let st = STATE.borrow(cs).borrow();
        syncobj_resolve(&st, owner, &handles)
    });
    let objs = match objs {
        Ok(objs) => objs,
        Err(e) => return e,
    };
    if objs.iter().any(|obj| !obj.is_timeline()) {
        return libc_einval(); // a point was never submitted, so there is none to wait on
    }
    let all = flags & SYNCOBJ_WAIT_ALL != 0;
    let for_submit = flags & SYNCOBJ_WAIT_FOR_SUBMIT != 0;
    let satisfied = || syncobj_any_ready(&objs, &points, for_submit, all);
    if !for_submit && !satisfied() {
        return libc_einval();
    }
    if !satisfied() {
        let rc = syncobj_park(&objs, timeout, satisfied);
        if rc != 0 {
            return rc;
        }
    }
    if !all {
        let first = syncobj_first_ready(&objs, &points, for_submit);
        unsafe { ((arg + 32) as *mut u32).write_unaligned(first) };
    }
    0
}

/// `SYNCOBJ_RESET`: the objects stop holding anything.
///
/// A handle that names nothing is `ENOENT` here, unlike `DESTROY`'s `EINVAL` -
/// both measured, and the difference is the one Linux draws: destroying a name
/// that is not there is an argument error, while operating on a name that is not
/// there is a missing object.
fn syncobj_reset(owner: u64, arg: usize) -> isize {
    // struct drm_syncobj_array: handles@0, count_handles@8.
    let ptr = unsafe { (arg as *const u64).read_unaligned() } as usize;
    let count = unsafe { ((arg + 8) as *const u32).read_unaligned() } as usize;
    let handles = match syncobj_handles(ptr, count) {
        Ok(handles) => handles,
        Err(e) => return e,
    };
    let objs = critical_section::with(|cs| {
        let st = STATE.borrow(cs).borrow();
        syncobj_resolve(&st, owner, &handles)
    });
    let objs = match objs {
        Ok(objs) => objs,
        Err(e) => return e,
    };
    for obj in &objs {
        obj.reset();
    }
    0
}

/// `SYNCOBJ_SIGNAL`: a fence is submitted to each object, retired already.
///
/// On this card that is all a signal can be - there is no engine to hand the
/// work to, so the fence it leaves is one that has retired by the time the
/// request returns. `TIMELINE_SIGNAL` is the same write with a point attached.
fn syncobj_signal(owner: u64, arg: usize) -> isize {
    let ptr = unsafe { (arg as *const u64).read_unaligned() } as usize;
    let count = unsafe { ((arg + 8) as *const u32).read_unaligned() } as usize;
    let handles = match syncobj_handles(ptr, count) {
        Ok(handles) => handles,
        Err(e) => return e,
    };
    let objs = critical_section::with(|cs| {
        let st = STATE.borrow(cs).borrow();
        syncobj_resolve(&st, owner, &handles)
    });
    let objs = match objs {
        Ok(objs) => objs,
        Err(e) => return e,
    };
    for obj in &objs {
        obj.submit(crate::gpu::sync::Fence::retired(), None, &mut SchedWaker);
    }
    critical_section::with(|cs| {
        let mut st = STATE.borrow(cs).borrow_mut();
        for &handle in &handles {
            sync_events_left(&mut st, owner, handle);
        }
    });
    0
}

/// `SYNCOBJ_TIMELINE_SIGNAL`: submit a fence at a point.
///
/// This is the request that *makes* an object a timeline, which is why there is
/// no gate here for one that is not: the measured card accepts it on a plain
/// object and answers its `QUERY` with the point afterwards. Points are accepted
/// below the object's own too, and move nothing - a point that could go backwards
/// would let a wait on a point already passed fail again.
fn syncobj_timeline_signal(owner: u64, arg: usize) -> isize {
    // struct drm_syncobj_timeline_array: handles@0, points@8, count@16, flags@20.
    let handles_ptr = unsafe { (arg as *const u64).read_unaligned() } as usize;
    let points_ptr = unsafe { ((arg + 8) as *const u64).read_unaligned() } as usize;
    let count = unsafe { ((arg + 16) as *const u32).read_unaligned() } as usize;
    let flags = unsafe { ((arg + 20) as *const u32).read_unaligned() };
    if flags != 0 {
        return libc_einval();
    }
    let handles = match syncobj_handles(handles_ptr, count) {
        Ok(handles) => handles,
        Err(e) => return e,
    };
    if count == 0 {
        return 0;
    }
    if points_ptr == 0 || !user_array(points_ptr, count, 8, false) {
        return -14; // EFAULT
    }
    let points = unsafe { core::slice::from_raw_parts(points_ptr as *const u64, count) }.to_vec();
    let objs = critical_section::with(|cs| {
        let st = STATE.borrow(cs).borrow();
        syncobj_resolve(&st, owner, &handles)
    });
    let objs = match objs {
        Ok(objs) => objs,
        Err(e) => return e,
    };
    for (obj, &point) in objs.iter().zip(points.iter()) {
        obj.submit(
            crate::gpu::sync::Fence::retired(),
            Some(point),
            &mut SchedWaker,
        );
    }
    critical_section::with(|cs| {
        let mut st = STATE.borrow(cs).borrow_mut();
        for &handle in &handles {
            sync_events_left(&mut st, owner, handle);
        }
    });
    0
}

/// `SYNCOBJ_QUERY`: how far each timeline has got.
///
/// An object that has never had a point reports 0, and so does one that was
/// reset, which is the same statement: `RESET` takes the point back to zero.
/// This is the one request in the family that does not need a fence, because
/// "how far has this got" has an answer even when the answer is nothing.
fn syncobj_query(owner: u64, arg: usize) -> isize {
    let handles_ptr = unsafe { (arg as *const u64).read_unaligned() } as usize;
    let points_ptr = unsafe { ((arg + 8) as *const u64).read_unaligned() } as usize;
    let count = unsafe { ((arg + 16) as *const u32).read_unaligned() } as usize;
    let flags = unsafe { ((arg + 20) as *const u32).read_unaligned() };
    if flags & !SYNCOBJ_QUERY_LAST_SUBMITTED != 0 {
        return libc_einval();
    }
    let handles = match syncobj_handles(handles_ptr, count) {
        Ok(handles) => handles,
        Err(e) => return e,
    };
    if count == 0 {
        return 0;
    }
    if points_ptr == 0 || !user_array(points_ptr, count, 8, true) {
        return -14; // EFAULT
    }
    let objs = critical_section::with(|cs| {
        let st = STATE.borrow(cs).borrow();
        syncobj_resolve(&st, owner, &handles)
    });
    let objs = match objs {
        Ok(objs) => objs,
        Err(e) => return e,
    };
    // Every point is written, including the zeros: the array is the caller's
    // output and a point left alone would be a number the card never said.
    unsafe {
        for (i, obj) in objs.iter().enumerate() {
            (points_ptr as *mut u64).add(i).write_unaligned(obj.point());
        }
    }
    0
}

/// `SYNCOBJ_TRANSFER`: move a fence from one object to another.
///
/// Measured on the real card: the request needs *one* fence between its two ends
/// and refuses it with `EINVAL` when neither has one, which is also the honest
/// answer - a transfer that carries nothing and reports success is the silent
/// no-op `AGENTS.md` §4.2 forbids. The destination's point moves to `dst_point`
/// when one was named, with no check that it went up.
fn syncobj_transfer(owner: u64, arg: usize) -> isize {
    // struct drm_syncobj_transfer: src@0, dst@4, src_point@8, dst_point@16,
    // flags@24.
    let src = unsafe { (arg as *const u32).read_unaligned() };
    let dst = unsafe { ((arg + 4) as *const u32).read_unaligned() };
    let dst_point = unsafe { ((arg + 16) as *const u64).read_unaligned() };
    let flags = unsafe { ((arg + 24) as *const u32).read_unaligned() };
    if flags != 0 {
        return libc_einval();
    }
    let (src_obj, dst_obj) = critical_section::with(|cs| {
        let st = STATE.borrow(cs).borrow();
        (
            syncobj_lookup(&st, owner, src),
            syncobj_lookup(&st, owner, dst),
        )
    });
    let (Some(src_obj), Some(dst_obj)) = (src_obj, dst_obj) else {
        return -2; // ENOENT
    };
    let some_fence = src_obj.has_fence() || dst_obj.has_fence();
    if !some_fence {
        return libc_einval();
    }
    match src_obj.fence() {
        Some(fence) => dst_obj.submit(
            fence,
            (dst_point != 0).then_some(dst_point),
            &mut SchedWaker,
        ),
        // The source carries nothing, so only the destination's own point moves;
        // its fence (the one that made this request legal) is left where it is.
        None => {
            if dst_point != 0 {
                dst_obj.submit_point(dst_point, &mut SchedWaker);
            }
        }
    }
    critical_section::with(|cs| {
        let mut st = STATE.borrow(cs).borrow_mut();
        sync_events_left(&mut st, owner, dst);
    });
    0
}

/// `SYNCOBJ_EVENTFD`: a sync object's arrival, as an increment on a counter.
///
/// This is the request an event loop uses, and the one Smithay probes before it
/// will expose `wp_linux_drm_syncobj_v1` at all - a compositor that has to wait
/// for a client's timeline cannot afford a thread per wait. Each call registers
/// *one* increment: two registrations and one signal read back as 2, and a
/// registration whose point is already covered fires before the request returns.
fn syncobj_eventfd(owner: u64, arg: usize) -> isize {
    // struct drm_syncobj_eventfd: handle@0, flags@4, point@8, fd@16, pad@20.
    let handle = unsafe { (arg as *const u32).read_unaligned() };
    let flags = unsafe { ((arg + 4) as *const u32).read_unaligned() };
    let point = unsafe { ((arg + 8) as *const u64).read_unaligned() };
    let fd = unsafe { ((arg + 16) as *const i32).read_unaligned() };
    // `WAIT_AVAILABLE` is meaningful here; the rest of the wait flags are not,
    // and the measured card refuses anything else with `EINVAL`.
    if flags & !SYNCOBJ_WAIT_AVAILABLE != 0 {
        return libc_einval();
    }
    let file = match crate::sched::current_fds().clone_at(fd) {
        Ok(file) => file,
        Err(_) => return -9, // EBADF
    };
    // The node, not the number: the increment this registration may owe later
    // has to reach a counter that is still there, which is the reference
    // Linux's `eventfd_ctx_fdget` takes for the same reason.
    let counter = file.inode();
    if counter.as_eventfd().is_none() {
        return libc_einval(); // not a counter anything could increment
    }
    critical_section::with(|cs| {
        let mut st = STATE.borrow(cs).borrow_mut();
        let st = &mut *st;
        let Some(rec) = st
            .syncobjs
            .iter_mut()
            .flatten()
            .find(|rec| rec.owner == owner && rec.handle == handle)
        else {
            return -2; // ENOENT
        };
        rec.events.push(SyncEvent {
            counter,
            point,
            available: flags & SYNCOBJ_WAIT_AVAILABLE != 0,
            fired: false,
        });
        // A point that is already covered, or a fence already behind it, owes
        // its increment now rather than at the next signal.
        fire_sync_events(rec);
        0
    })
}

/// Fire the registrations a request may have made due, by handle.
fn sync_events_left(st: &mut DrmState, owner: u64, handle: u32) {
    for rec in st.syncobjs.iter_mut().flatten() {
        if rec.owner == owner && rec.handle == handle {
            fire_sync_events(rec);
        }
    }
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

/// `MODE_CURSOR`/`MODE_CURSOR2`: bind the cursor image and move it - the legacy
/// door to the cursor plane, and the one a compositor that has not found the plane
/// in the plane list still has.
///
/// One handler for both requests, because `struct drm_mode_cursor2` is
/// `drm_mode_cursor` with the hotspot appended and nothing else differs.
/// `MODE_CURSOR`'s hotspot is 0,0, which is exactly the arrow-tip convention the
/// older struct was written around: its top-left pixel is the point.
///
/// `flags` says which of the two things is being asked for - `CURSOR_BO` binds an
/// image, `CURSOR_MOVE` moves it - and Linux refuses a request that asks for
/// neither, or for a bit outside the pair, so this does too. The two may be asked
/// for together, which is what a compositor does on the frame it first shows a
/// pointer.
///
/// **The `handle` is a framebuffer id here.** The UAPI calls that field a "driver
/// specific handle", which is the header declining to define one: there is no
/// Linux-wide answer to match, and this card's answer is the id `ADDFB`/`ADDFB2`
/// handed back - the object it has to composite. Inventing a second name for the
/// same framebuffer would be the private dialect the UAPI avoided creating.
/// `width`/`height` are the caller's own statement of the image's size, so they are
/// checked against the framebuffer rather than believed.
///
/// A cursor the plane cannot show is refused *here*, where it is bound, rather
/// than silently not drawn later: an image larger than the 64x64
/// `DRM_CAP_CURSOR_WIDTH`/`HEIGHT` answers for, or one that is not `ARGB8888`
/// (the format a cursor carries its alpha in). An unknown CRTC is `EINVAL`, which
/// is what this file has always answered for an object id the request cannot use.
/// Linux's `drm_crtc_find` says `ENOENT` instead, and the whole file is the other
/// convention, so the cursor follows the file rather than introducing a second
/// answer for the same mistake inside one driver.
fn mode_cursor(arg: usize, hot: bool, lease: Option<&LeaseSet>) -> isize {
    // struct drm_mode_cursor: flags@0, crtc_id@4, x@8(s32), y@12(s32),
    // width@16, height@20, handle@24; drm_mode_cursor2 adds hot_x@28, hot_y@32.
    let flags = unsafe { (arg as *const u32).read_unaligned() };
    let crtc_id = unsafe { ((arg + 4) as *const u32).read_unaligned() };
    let x = unsafe { ((arg + 8) as *const i32).read_unaligned() };
    let y = unsafe { ((arg + 12) as *const i32).read_unaligned() };
    let width = unsafe { ((arg + 16) as *const u32).read_unaligned() };
    let height = unsafe { ((arg + 20) as *const u32).read_unaligned() };
    let handle = unsafe { ((arg + 24) as *const u32).read_unaligned() };
    let hotspot = if hot {
        (
            unsafe { ((arg + 28) as *const i32).read_unaligned() },
            unsafe { ((arg + 32) as *const i32).read_unaligned() },
        )
    } else {
        // The older struct has no hotspot at all, and its convention is the
        // image's top-left pixel.
        (0, 0)
    };
    if flags == 0 || flags & !CURSOR_FLAGS != 0 {
        return libc_einval();
    }
    if crtc_id != CRTC_ID || !leased(lease, crtc_id) {
        return libc_einval();
    }

    if flags & CURSOR_BO != 0 {
        // A zero-sized image is "turn the cursor off", which is how Linux spells
        // it and how libdrm's `drmModeSetCursor(fd, crtc, 0, 0, 0)` says it.
        if width == 0 && height == 0 {
            critical_section::with(|cs| STATE.borrow(cs).borrow_mut().cursor_fb = 0);
        } else {
            let showable = critical_section::with(|cs| {
                let st = STATE.borrow(cs).borrow();
                cursor_fb_ok(&st, handle)
                    && st
                        .fbs
                        .iter()
                        .filter_map(|f| f.as_ref())
                        .any(|f| f.id == handle && f.width == width && f.height == height)
            });
            if !showable {
                return libc_einval();
            }
            critical_section::with(|cs| {
                let mut st = STATE.borrow(cs).borrow_mut();
                st.cursor_fb = handle;
            });
        }
    }
    // A move carries the position whether or not it also carried an image, and
    // `MODE_CURSOR2`'s hotspot moves with the pointer for the same reason.
    if flags & CURSOR_MOVE != 0 {
        critical_section::with(|cs| {
            let mut st = STATE.borrow(cs).borrow_mut();
            st.cursor_pos = (x, y);
            st.cursor_hotspot = hotspot;
        });
    } else if flags & CURSOR_BO != 0 {
        // An image and no move: the pointer stays where it was and the new image
        // lands under it, but a hotspot that was only just supplied still has to
        // be taken, or the image would sit at the old one's offset.
        critical_section::with(|cs| {
            STATE.borrow(cs).borrow_mut().cursor_hotspot = hotspot;
        });
    }

    // Present. Nothing was committed, so nothing else would rebuild the scanout -
    // and the pointer is drawn *in* that rebuild, so the master's frame is copied
    // again here, with the new image and position, and flushed. This is what
    // `SETCRTC` and `PAGE_FLIP` do for their own frames, and it is why a cursor
    // move is as prompt as a commit: the compositor does not redraw, and the
    // kernel does not wait for one to.
    let rc = critical_section::with(|cs| {
        let st = STATE.borrow(cs).borrow();
        if st.current_fb == 0 {
            // Nothing is on the screen to draw a pointer over. The image and
            // position are recorded, so the pointer appears with the next commit -
            // the honest outcome rather than inventing a screen to draw on.
            return 0;
        }
        copy_to_scanout(&st, st.current_fb)
    });
    if rc == 0 {
        crate::drivers::video::render::request_flush_blocking();
    }
    0
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

    /// A dumb buffer is real memory the card scans out; its mapping must be
    /// those pages, so the mapping is frame-backed.
    fn frame_backed(&self) -> bool {
        true
    }
}
