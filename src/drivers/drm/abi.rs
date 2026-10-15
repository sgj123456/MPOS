pub(crate) const CRTC_ID: u32 = 34;
pub(crate) const ENCODER_ID: u32 = 33;
pub(crate) const CONNECTOR_ID: u32 = 35;
pub(crate) const PLANE_ID: u32 = 36;
/// The cursor plane, the card's second plane.
///
/// It is a *plane* rather than only the legacy `MODE_CURSOR` door because that is
/// what a compositor looks for: weston picks its cursor plane out of the plane
/// list by the `type` property and drives it atomically, and only falls back to
/// drawing the pointer into its own framebuffer when there is none. A card with
/// cursor *hardware* and no cursor plane is a real configuration - the UAPI
/// documents it - but it is not this one, and the difference matters because a
/// compositor that cannot find the plane will not ask for it.
pub(crate) const CURSOR_PLANE_ID: u32 = 37;
/// DRM's major number - `DRM_MAJOR` in the kernel's own headers, and the number
/// every `/sys/class/drm/cardN/dev` prints and every uevent about a card
/// carries. Fixed since XFree86 4.x.
pub(crate) const DRM_MAJOR: u32 = 226;
/// Fixed property ids, allocated the same way: `OBJ_GETPROPERTIES` and
/// `GETPROPERTY` have to agree on them, and nothing else cares what they are.
/// `CRTC_ID` and `DPMS` live on the connector, `ACTIVE` on the CRTC, and
/// `type` on the plane - the set weston's DRM backend looks every one of
/// them up by *name* for.
pub(crate) const PROP_CRTC_ID: u32 = 70;
pub(crate) const PROP_DPMS: u32 = 71;
pub(crate) const PROP_ACTIVE: u32 = 72;
pub(crate) const PROP_TYPE: u32 = 73;
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
pub(crate) const PROP_MODE_ID: u32 = 74;
pub(crate) const PROP_PLANE_FB_ID: u32 = 75;
pub(crate) const PROP_PLANE_CRTC_ID: u32 = 76;
pub(crate) const PROP_SRC_X: u32 = 77;
pub(crate) const PROP_SRC_Y: u32 = 78;
pub(crate) const PROP_SRC_W: u32 = 79;
pub(crate) const PROP_SRC_H: u32 = 80;
pub(crate) const PROP_CRTC_X: u32 = 81;
pub(crate) const PROP_CRTC_Y: u32 = 82;
pub(crate) const PROP_CRTC_W: u32 = 83;
pub(crate) const PROP_CRTC_H: u32 = 84;
pub(crate) const PROP_BLEND: u32 = 85;
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
pub(crate) const PROP_IN_FENCE_FD: u32 = 86;
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
pub(crate) const PROP_EDID: u32 = 87;
pub(crate) const PROP_NON_DESKTOP: u32 = 88;
pub(crate) const PROP_MAX_BPC: u32 = 89;
pub(crate) const PROP_VRR_CAPABLE: u32 = 90;
pub(crate) const PROP_IN_FORMATS: u32 = 94;
/// The plane's `OUT_FENCE_PTR`: a *pointer* in the caller's memory where the
/// commit writes back a fence descriptor for the frame it just committed.
///
/// It is the other half of `IN_FENCE_FD`, and the half this card was missing
/// entirely - the list in `NOTES.md` §7.4 has it absent, because amdgpu does not
/// publish one either. The measured shape on a card that does is a range whose
/// two ends are 0 and the whole 64-bit span, since the value is an address and
/// not a number with a meaning of its own.
pub(crate) const PROP_OUT_FENCE_PTR: u32 = 95;
/// The cursor plane's `HOTSPOT_X`/`HOTSPOT_Y`: the pixel of the cursor image that
/// is the actual pointer point, which a compositor sets when it has bound an image
/// and reads back as part of the plane's state.
///
/// These exist because this card *is* one of the virtualized drivers the cap is
/// named for: Linux's virtio-gpu publishes them on its cursor plane, and a client
/// only sees them once it has set `DRM_CLIENT_CAP_CURSOR_PLANE_HOTSPOT`. They are
/// gated per open file exactly like [`ATOMIC_PROPS`] - see [`PROP_HOTSPOT_X`]'s
/// use in the two list doors - so a client that never asked is not shown them.
/// The §7.4 record explains why an earlier note here refused the cap on the
/// strength of a non-virtualized amdgpu measurement that did not apply to this
/// device.
pub(crate) const PROP_HOTSPOT_X: u32 = 96;
pub(crate) const PROP_HOTSPOT_Y: u32 = 97;
/// The two hotspot properties, listed together so the two enumeration doors can
/// hide both at once and cannot drift apart on one of them.
pub(crate) const HOTSPOT_PROPS: [u32; 2] = [PROP_HOTSPOT_X, PROP_HOTSPOT_Y];
/// The connector's property list, in one place: `GETCONNECTOR` and
/// `OBJ_GETPROPERTIES` both report it, and a client that read it from one and
/// not the other would see two different connectors.
pub(crate) const CONNECTOR_PROPS: [u32; 6] = [
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
pub(crate) const BPC_HERE: u64 = 8;
/// The formats the one plane scans out, in the order `GETPLANE` reports them -
/// which is the order the `IN_FORMATS` bitmask indexes, so the plane's list and
/// the blob are built from this one array and cannot drift apart. `ADDFB2`
/// accepts exactly these two, which is the third place the same set has to
/// agree.
pub(crate) const PLANE_FORMATS: [u32; 2] = [DRM_FORMAT_XRGB8888, DRM_FORMAT_ARGB8888];
/// The cursor plane's formats, which is a shorter list for a measured reason:
/// `modetest -p` on the real amdgpu card reports `formats: AR24` for every one
/// of its cursor planes. A cursor is alpha-blended, so the format that carries
/// alpha is the one it is in; the opaque spelling is what a primary plane has.
pub(crate) const CURSOR_FORMATS: [u32; 1] = [DRM_FORMAT_ARGB8888];
/// `DRM_MODE_CURSOR_BO` and `DRM_MODE_CURSOR_MOVE`: the two things a cursor
/// request can ask for, and `DRM_MODE_CURSOR_FLAGS` the mask of both.
pub(crate) const CURSOR_BO: u32 = 0x01;
pub(crate) const CURSOR_MOVE: u32 = 0x02;
pub(crate) const CURSOR_FLAGS: u32 = 0x03;
/// `DRM_MODE_CONNECTOR_VIRTUAL` / `DRM_MODE_ENCODER_VIRTUAL`.
pub(crate) const CONNECTOR_TYPE: u32 = 15;
pub(crate) const ENCODER_TYPE: u32 = 5;
/// `DRM_MODE_CONNECTED`.
pub(crate) const CONNECTED: u32 = 1;
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
pub(crate) const EVENT_FLIP_COMPLETE: u32 = 0x02;

/// The ioctl `nr` byte (bits 8..15) for the KMS + core requests we serve.
pub(crate) const NR_GET_CAP: u32 = 0x0c;
pub(crate) const NR_SET_CLIENT_CAP: u32 = 0x0d;
pub(crate) const NR_GETRESOURCES: u32 = 0xa0;
pub(crate) const NR_GETCRTC: u32 = 0xa1;
pub(crate) const NR_SETCRTC: u32 = 0xa2;
pub(crate) const NR_GETENCODER: u32 = 0xa6;
pub(crate) const NR_GETCONNECTOR: u32 = 0xa7;
pub(crate) const NR_GETPROPERTY: u32 = 0xaa;
pub(crate) const NR_SETPROPERTY: u32 = 0xab;
pub(crate) const NR_GETPROPBLOB: u32 = 0xac;
pub(crate) const NR_GETFB: u32 = 0xad;
pub(crate) const NR_ADDFB: u32 = 0xae;
pub(crate) const NR_RMFB: u32 = 0xaf;
pub(crate) const NR_PAGE_FLIP: u32 = 0xb0;
/// The four requests §7.4's list names that this card answers by *refusing*
/// rather than by falling off the end of the dispatch `match`, plus `GETFB2`.
/// Naming them is the difference between "not implemented" and "deliberately
/// not this card's": a caller is owed the second, and `NOTES.md` §7.4 owes the
/// reason beside it. See `gamma_refused` and `sequence_refused`.
pub(crate) const NR_MODE_GETGAMMA: u32 = 0xa4;
pub(crate) const NR_MODE_SETGAMMA: u32 = 0xa5;
pub(crate) const NR_CRTC_GET_SEQUENCE: u32 = 0x3b;
pub(crate) const NR_CRTC_QUEUE_SEQUENCE: u32 = 0x3c;
pub(crate) const NR_GETFB2: u32 = 0xce;
/// The two spellings of the cursor request. `MODE_CURSOR2` is the same struct
/// with the hotspot at the end, which is why both are served by one handler.
pub(crate) const NR_MODE_CURSOR: u32 = 0xa3;
pub(crate) const NR_MODE_CURSOR2: u32 = 0xbb;
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
pub(crate) const NR_MODE_CREATE_LEASE: u32 = 0xc6;
pub(crate) const NR_MODE_LIST_LESSES: u32 = 0xc7;
pub(crate) const NR_MODE_REVOKE_LEASE: u32 = 0xc9;
pub(crate) const NR_MODE_GET_LEASE: u32 = 0xc8;
/// `DRM_MODE_LEASE_IMMUTABLE`: the lessor promises not to revoke. Recorded and
/// reported, *not* enforced below - see [`LeaseSet`].
///
/// Both this value and [`LEASE_SEAL_ALL`] are worth a second look for a reason
/// that is not about this card: **no header on the machine this was written on
/// defines either name**. The request numbers beside them were once written down
/// thirteen too high and every caller reached the dispatch catch-all as a result
/// (§7.4) - and that mistake was *loud*, because a request this card does not
/// serve answers `ENOTTY`. A wrong **flag** is quiet: the value is inside the
/// accepted mask, something is stored, and a caller reads back what it asked for.
/// So these two are accepted on a shape argument rather than a measurement, and
/// they are the last thing in this file that is. Enforcing them - which is what
/// would make `IMMUTABLE` mean anything - needs the values first, and this card
/// cannot supply them: `MODE_CREATE_LEASE` is the master's (`EACCES` otherwise),
/// so the accepting host's card is out of reach.
pub(crate) const LEASE_IMMUTABLE: u32 = 1;
/// `DRM_MODE_SEAL_ALL`: seal every sealing option there is. Same honesty: the
/// flag is stored so a caller reads back what it asked for, and nothing more.
/// See [`LEASE_IMMUTABLE`] for why the value itself is unverifiable here.
pub(crate) const LEASE_SEAL_ALL: u32 = 4;
pub(crate) const NR_CREATE_DUMB: u32 = 0xb2;
pub(crate) const NR_MAP_DUMB: u32 = 0xb3;
pub(crate) const NR_DESTROY_DUMB: u32 = 0xb4;
pub(crate) const NR_GETPLANERESOURCES: u32 = 0xb5;
pub(crate) const NR_GETPLANE: u32 = 0xb6;
pub(crate) const NR_MODE_ATOMIC: u32 = 0xbc;
pub(crate) const NR_CREATE_BLOB: u32 = 0xbd;
pub(crate) const NR_DESTROY_BLOB: u32 = 0xbe;
/// `DRM_IOCTL_MODE_ADDFB2` / `..._OBJ_GETPROPERTIES` / `..._OBJ_SETPROPERTY`:
/// the fourcc form of ADDFB and the object-typed property interface.
pub(crate) const NR_ADDFB2: u32 = 0xb8;
pub(crate) const NR_OBJ_GETPROPERTIES: u32 = 0xb9;
pub(crate) const NR_OBJ_SETPROPERTY: u32 = 0xba;
/// The core (non-KMS) requests: version, auth, and the PRIME pair that hands
/// a GEM handle to a dma-buf descriptor and back. `SET_VERSION` and the
/// magic/auth pair are what libdrm's setup probes; PRIME is what GBM builds
/// its buffers through.
pub(crate) const NR_VERSION: u32 = 0x00;
pub(crate) const NR_GET_MAGIC: u32 = 0x02;
pub(crate) const NR_SET_VERSION: u32 = 0x07;
pub(crate) const NR_GEM_CLOSE: u32 = 0x09;
pub(crate) const NR_AUTH_MAGIC: u32 = 0x11;
pub(crate) const NR_PRIME_HANDLE_TO_FD: u32 = 0x2d;
pub(crate) const NR_PRIME_FD_TO_HANDLE: u32 = 0x2e;

/// The sync object family, `<drm.h>`'s numbers, in the two runs the header puts
/// them in. `CREATE`..`SIGNAL` sit between the lease family and `GETFB2`, and the
/// later four were added years after them - which is why a request number is
/// never inferred from the one next to it.
///
/// The whole family is decided by `crate::gpu::sync`, where the rules are written
/// down with a host test each; what is here is the argument marshalling and the
/// table of objects.
pub(crate) const NR_SYNCOBJ_CREATE: u32 = 0xbf;
pub(crate) const NR_SYNCOBJ_DESTROY: u32 = 0xc0;
pub(crate) const NR_SYNCOBJ_HANDLE_TO_FD: u32 = 0xc1;
pub(crate) const NR_SYNCOBJ_FD_TO_HANDLE: u32 = 0xc2;
pub(crate) const NR_SYNCOBJ_WAIT: u32 = 0xc3;
pub(crate) const NR_SYNCOBJ_RESET: u32 = 0xc4;
pub(crate) const NR_SYNCOBJ_SIGNAL: u32 = 0xc5;
pub(crate) const NR_SYNCOBJ_TIMELINE_WAIT: u32 = 0xca;
pub(crate) const NR_SYNCOBJ_QUERY: u32 = 0xcb;
pub(crate) const NR_SYNCOBJ_TRANSFER: u32 = 0xcc;
pub(crate) const NR_SYNCOBJ_TIMELINE_SIGNAL: u32 = 0xcd;
pub(crate) const NR_SYNCOBJ_EVENTFD: u32 = 0xcf;

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
pub(crate) const NR_VIRTGPU_MAP: u32 = 0x41;
pub(crate) const NR_VIRTGPU_EXECBUFFER: u32 = 0x42;
pub(crate) const NR_VIRTGPU_GETPARAM: u32 = 0x43;
pub(crate) const NR_VIRTGPU_RESOURCE_CREATE: u32 = 0x44;
pub(crate) const NR_VIRTGPU_RESOURCE_INFO: u32 = 0x45;
pub(crate) const NR_VIRTGPU_TRANSFER_FROM_HOST: u32 = 0x46;
pub(crate) const NR_VIRTGPU_TRANSFER_TO_HOST: u32 = 0x47;
pub(crate) const NR_VIRTGPU_WAIT: u32 = 0x48;
pub(crate) const NR_VIRTGPU_GET_CAPS: u32 = 0x49;
pub(crate) const NR_VIRTGPU_RESOURCE_CREATE_BLOB: u32 = 0x4a;
pub(crate) const NR_VIRTGPU_CONTEXT_INIT: u32 = 0x4b;

/// `DRM_SYNCOBJ_CREATE_SIGNALED`: the object starts with a fence behind it.
pub(crate) const SYNCOBJ_CREATE_SIGNALED: u32 = 1 << 0;
/// `DRM_SYNCOBJ_HANDLE_TO_FD_FLAGS_EXPORT_SYNC_FILE` and its import twin: the
/// descriptor is a `sync_file` over a fence rather than the object itself.
pub(crate) const SYNCOBJ_EXPORT_SYNC_FILE: u32 = 1 << 0;
pub(crate) const SYNCOBJ_IMPORT_SYNC_FILE: u32 = 1 << 0;
/// `DRM_SYNCOBJ_*_FLAGS_TIMELINE` on both directions. This host's header has it
/// and the older upstream one does not.
///
/// The note here used to read "the measured card accepts it and does not
/// require it, so it is accepted and changes nothing", from an instrument that
/// sent the flag with a `point` of zero. Measured again with every combination
/// of flags and points (§7.4), that is half right and misses the half the flag
/// is *for*: it is what makes the request's `point` a **timeline** point rather
/// than a number the operation may ignore, and the two directions read it
/// differently. Both are honoured below, and on the import side the flag is what
/// chooses between two rules rather than one - "the request names the point, and
/// it raises" against "the fence's own point lands, exactly". `SyncFileFd`'s
/// `point` field is what carries the second.
pub(crate) const SYNCOBJ_FLAGS_TIMELINE: u32 = 1 << 1;
/// The wait flags. `ALL` and `FOR_SUBMIT` are the two with behaviour;
/// `AVAILABLE` and `DEADLINE` are accepted and mean nothing to a card with no
/// engine to schedule against.
pub(crate) const SYNCOBJ_WAIT_ALL: u32 = 1 << 0;
pub(crate) const SYNCOBJ_WAIT_FOR_SUBMIT: u32 = 1 << 1;
pub(crate) const SYNCOBJ_WAIT_AVAILABLE: u32 = 1 << 2;
pub(crate) const SYNCOBJ_WAIT_DEADLINE: u32 = 1 << 3;
/// `DRM_SYNCOBJ_QUERY_FLAGS_LAST_SUBMITTED`. Nothing here is submitted and
/// unsignaled, so the last submitted point and the last signaled one are the
/// same number and the flag changes no answer - which is why it is accepted
/// rather than refused.
pub(crate) const SYNCOBJ_QUERY_LAST_SUBMITTED: u32 = 1 << 0;
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
pub(crate) const NR_AUTH: u32 = 0x1c;
pub(crate) const NR_SET_MASTER: u32 = 0x1e;
pub(crate) const NR_DROP_MASTER: u32 = 0x1f;

/// `DRM_CAP_DUMB_BUFFER`; `GET_CAP` answers 1 for it and 0 elsewhere.
pub(crate) const CAP_DUMB_BUFFER: u64 = 0x1;
/// `DRM_CAP_PRIME`: both directions of PRIME work here (the dma-buf of
/// `prime_handle_to_fd`), which is what tells Mesa's GBM that the render
/// node can hand buffers to the card.
pub(crate) const CAP_PRIME: u64 = 0x5;
/// `DRM_PRIME_CAP_IMPORT | DRM_PRIME_CAP_EXPORT`.
pub(crate) const PRIME_CAPS: u64 = 0x3;
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
pub(crate) const CAP_TIMESTAMP_MONOTONIC: u64 = 0x6;
/// `DRM_CAP_CURSOR_WIDTH` / `_HEIGHT`: the largest cursor the hardware takes.
///
/// This card has a cursor plane, and the size is *consumed*: a framebuffer
/// larger than [`CURSOR_SIZE`] in either direction cannot be bound to that plane
/// ([`cursor_fb_ok`]) and a `MODE_CURSOR2` image past it is refused as well, so
/// the cap answers the same number the checks enforce rather than a fallback.
/// The note here used to say "this card has no cursor plane, so nothing here
/// consumes the size" - true when it was written and false from the moment the
/// plane landed, which makes it worse than a stale number: it is the sentence a
/// reader trusts.
pub(crate) const CAP_CURSOR_WIDTH: u64 = 0x8;
pub(crate) const CAP_CURSOR_HEIGHT: u64 = 0x9;
/// The cursor's largest size in each direction: what the cap answers, and what
/// the two checks that take a cursor image enforce.
pub(crate) const CURSOR_SIZE: u64 = 64;
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
pub(crate) const CAP_CRTC_IN_VBLANK_EVENT: u64 = 0x12;
/// `DRM_CAP_ADDFB2_MODIFIERS`: whether `ADDFB2` honours `DRM_MODE_FB_MODIFIERS`.
/// Answered 1, and `add_fb2` below is where that is made true - it takes the
/// flag with the linear modifier and refuses any other.
pub(crate) const CAP_ADDFB2_MODIFIERS: u64 = 0x10;
/// `DRM_CAP_ASYNC_PAGE_FLIP`: whether a flip can be requested that tears.
///
/// 0, and named rather than left to the catch-all, because the difference
/// matters to a reader: this is not a capability that is missing yet, it is one
/// this card does not have. There is no engine presenting outside the frame
/// boundary, so the `PAGE_FLIP_ASYNC` flag it gates has nothing to mean.
pub(crate) const CAP_ASYNC_PAGE_FLIP: u64 = 0x07;
/// `DRM_CAP_SYNCOBJ`: whether the card has the sync object family at all. The
/// real card answers 1, so answering 1 here is a promise the `SYNCOBJ_*` arms
/// below have to keep rather than a capability this card can decline.
pub(crate) const CAP_SYNCOBJ: u64 = 0x13;
/// `DRM_CAP_SYNCOBJ_TIMELINE`: whether those objects can carry a point.
///
/// It is the one Smithay asks before it will expose `wp_linux_drm_syncobj_v1`,
/// and the protocol is *only* about timelines - a client hands the compositor a
/// point on its own timeline rather than a fence per frame. So this is the cap
/// that decides whether explicit sync exists on this machine at all.
pub(crate) const CAP_SYNCOBJ_TIMELINE: u64 = 0x14;
/// `DRM_CLIENT_CAP_UNIVERSAL_PLANES`: the client wants the plane objects the
/// card has, which is how a compositor finds a primary plane to scan out on.
pub(crate) const CLIENT_CAP_UNIVERSAL_PLANES: u64 = 2;
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
pub(crate) const CLIENT_CAP_ATOMIC: u64 = 3;
/// The rest of `include/uapi/drm/drm.h`'s client capabilities, named so the
/// handler can tell "the card does not do this" from "no such capability" -
/// Linux answers the first with `EOPNOTSUPP` and the second with `EINVAL`, and
/// a client that gets the second wrong can never tell a typo from a driver
/// that lacks a feature.
pub(crate) const CLIENT_CAP_STEREO_3D: u64 = 1;
pub(crate) const CLIENT_CAP_ASPECT_RATIO: u64 = 4;
pub(crate) const CLIENT_CAP_WRITEBACK_CONNECTORS: u64 = 5;
pub(crate) const CLIENT_CAP_CURSOR_PLANE_HOTSPOT: u64 = 6;
pub(crate) const CLIENT_CAP_PLANE_COLOR_PIPELINE: u64 = 7;
/// `DRM_CLOEXEC == O_CLOEXEC` on Linux: mark the handed-out descriptor the
/// way `open(O_CLOEXEC)` would.
pub(crate) const DRM_CLOEXEC: u32 = 0x80000;

/// `DRM_MODE_OBJECT_CRTC` / `_CONNECTOR` / `_ENCODER` / `_PLANE` / `_FB`: the
/// object-type tags of `OBJ_GETPROPERTIES`/`OBJ_SETPROPERTY`, and the single
/// *value* every `OBJECT`-typed property carries. The numbers are
/// `<drm_mode.h>`'s, and they are also what a caller compares against: drm-rs
/// maps them to the `ValueType` a property reports (`0xfbfbfbfb` is
/// `Framebuffer`, `0xcccccccc` is `CRTC`), so a tag answered wrong is a
/// property the caller has no type for.
pub(crate) const OBJECT_CRTC: u32 = 0xcccc_cccc;
pub(crate) const OBJECT_CONNECTOR: u32 = 0xc0c0_c0c0;
pub(crate) const OBJECT_PLANE: u32 = 0xeeee_eeee;
pub(crate) const OBJECT_FB: u32 = 0xfbfb_fbfb;
/// `DRM_MODE_OBJECT_ENCODER`. Named next to the four above for the reason the
/// encoder's own arm states: a card that *advertises* an encoder has to answer
/// for it, and refusing the one object the card lists that has no properties to
/// give is the difference between a compositor enumerating the card and one
/// that gives up on its primary node (`docs/notes/` §7.4).
pub(crate) const OBJECT_ENCODER: u32 = 0xe0e0_e0e0;
/// `DRM_MODE_OBJECT_ANY`: "the *id* is what I am naming, not the kind".
///
/// Linux's `__drm_mode_object_find` looks the id up in the card's one object
/// table and checks the type **only when the caller named one**, so this tag is
/// answered for whatever the id is. It is not a corner: a client reads a
/// property's *value* through `drmModeObjectGetProperties(fd, obj,
/// DRM_MODE_OBJECT_ANY)` - Aquamarine's `getDRMProp` is exactly that call - and
/// a card that refused the tag failed its plane init outright, which took
/// Hyprland's whole DRM backend down (`docs/notes/` §7.4).
pub(crate) const OBJECT_ANY: u32 = 0;
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
pub(crate) const PROP_RANGE: u32 = 1 << 1;
pub(crate) const PROP_IMMUTABLE: u32 = 1 << 2;
pub(crate) const PROP_ENUM: u32 = 1 << 3;
pub(crate) const PROP_BLOB: u32 = 1 << 4;
pub(crate) const PROP_OBJECT: u32 = 1 << 6;
pub(crate) const PROP_SIGNED_RANGE: u32 = 1 << 7;
/// `DRM_MODE_PROP_ATOMIC`: not a *type* like the bits above, but a statement
/// about who may see the property. Linux sets it on the properties an atomic
/// commit is made of and leaves those out of the lists it answers a client that
/// has not set `DRM_CLIENT_CAP_ATOMIC` - so the flag `GETPROPERTY` reports and
/// the skip the two list doors make are one fact, and [`ATOMIC_PROPS`] is where
/// this file keeps it.
pub(crate) const PROP_ATOMIC: u32 = 1 << 31;
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
pub(crate) const ATOMIC_PROPS: [u32; 15] = [
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
pub(crate) const PLANE_TYPE_PRIMARY: u64 = 1;
/// `DRM_PLANE_TYPE_CURSOR`, the `type` value that makes the second plane the
/// cursor rather than an overlay.
pub(crate) const PLANE_TYPE_CURSOR: u64 = 2;
/// `DRM_MODE_PROP_BLEND`'s two enum values, as the kernel spells them:
/// pre-multiplied alpha, and straight "coverage" alpha.
pub(crate) const BLEND_PREMULT: u64 = 0;
pub(crate) const BLEND_COVERAGE: u64 = 1;
/// The values `DPMS` and `ACTIVE` carry, in *one* list, because two readers
/// need the same answer: [`get_property`] advertises it and the write path
/// refuses what it does not hold.
///
/// A property whose range advertises 0..3 and whose write path records 9 is
/// advertising a shape it does not keep to, and the read-back then hands the
/// caller a value its own enum has no name for - the same self-contradiction
/// `max bpc` refuses a 16 over, which is the precedent this follows rather than
/// a new rule.
pub(crate) const DPMS_MODES: &[u64] = &[0, 1, 2, 3];
pub(crate) const ACTIVE_VALUES: &[u64] = &[0, 1];

/// `DRM_MODE_ATOMIC_*`: the commit is a rehearsal (`TEST_ONLY`), a commit that
/// must be allowed to change the mode (`ALLOW_MODESET`), or one that may
/// return before it completes (`NONBLOCK` - this card completes it before it
/// returns either way, which is what Linux permits).
pub(crate) const ATOMIC_TEST_ONLY: u32 = 0x0100;
pub(crate) const ATOMIC_ALLOW_MODESET: u32 = 0x0400;
pub(crate) const ATOMIC_NONBLOCK: u32 = 0x0200;
/// `DRM_MODE_PAGE_FLIP_EVENT`: the commit's completion arrives as an event on
/// the card fd, which is how a compositor paces its frames.
pub(crate) const PAGE_FLIP_EVENT: u32 = 0x1;
/// The largest blob this card will store - a mode is 68 bytes and a damage
/// rectangle list a few hundred; a megabyte is a ceiling no caller reaches
/// and no attacker can use to fill the kernel's heap through one ioctl.
pub(crate) const MAX_BLOB: usize = 1 << 20;

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
pub(crate) const DRM_FORMAT_XRGB8888: u32 = 0x3432_5258;
pub(crate) const DRM_FORMAT_ARGB8888: u32 = 0x3432_5241;
/// `DRM_MODE_FB_MODIFIERS`: the bit in `struct drm_mode_fb_cmd2.flags` that says
/// the `modifier[]` array is meaningful. Named at module scope because both
/// directions read it - `ADDFB2` refuses a modifier that arrives without it, and
/// `GETFB2` sets it only for a framebuffer that was made with one, which is the
/// same statement about the same array.
pub(crate) const FB_MODIFIERS: u32 = 1 << 1;

/// A dumb buffer's pages are found in the mmap address space at
/// `handle * PAGES_PER_BUFFER * PAGE`, so `mmap_frame` can decode the buffer
/// back out of an absolute page index. 65536 pages (256 MiB) per slot is
/// comfortably above any single frame.
pub(crate) const PAGES_PER_BUFFER: usize = 1 << 16;
/// Cap on buffers and framebuffers the card will track.
pub(crate) const MAX_HANDLES: usize = 64;
