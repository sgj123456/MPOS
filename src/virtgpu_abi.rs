//! The virtio-gpu protocol, as the wire spells it.
//!
//! The device is driven through one *control queue* of little request/response
//! pairs: a `CtrlHeader` naming the command, then the command's own fields, and
//! back a `CtrlHeader` naming a response code. This module holds that vocabulary
//! and nothing else - no queue, no device, no I/O - so the shapes every driver
//! and every caller must agree on are pinned by a host test rather than by the
//! one machine that happens to boot.
//!
//! It exists because the kernel needs commands the `virtio-drivers` crate does
//! not have. That crate's GPU device stops at `GET_EDID`; the 3D family
//! (`CTX_CREATE`, `RESOURCE_CREATE_3D`, `SUBMIT_3D`, the capset queries) is not
//! in it, and its control queue has no accessor, so a driver that needs those
//! commands has to own the queue - and therefore the encoding. `PLAN.md` §7.4
//! carries the measurement this work started from and the reason the crate was
//! not simply vendored.
//!
//! # Layout, and the one assumption
//!
//! Every struct is `#[repr(C)]` and every field is a `u32`, a `u64` or an array
//! of one, with no implicit padding - the sizes asserted at the bottom of this
//! file are the wire's, and a field added carelessly would fail them rather than
//! silently shift every field after it. The header spells its integers
//! `__le32`/`__le64`; this kernel is x86_64-only (`PLAN.md` §9.1 lists what a
//! port would need) and x86_64 is little-endian, so the native types *are* the
//! wire types here. A big-endian port would have to byte-swap in `as_bytes` and
//! `read_struct`, which is the only two places that would change.
//!
//! # Reading, not parsing
//!
//! A response is a fixed-size struct read out of a receive buffer the driver
//! allocated, so `read_struct` is a bounds-checked copy rather than a parser:
//! it answers `None` when the buffer is shorter than the struct, and the caller
//! turns that into `EIO`.
//!
//! There is exactly one reply whose length the *device* decides rather than the
//! driver - `RESP_OK_CAPSET`, whose struct ends in a flexible array - and
//! [`resp_capset_payload`] is the reader that bounds it. That is the one thing
//! here a fuzzer can usefully attack, and `fuzz/fuzz_targets/virtgpu_capset.rs`
//! does.
//!
//! `SUBMIT_3D`'s command stream looks like a second one and is not: it is the
//! *caller's* bytes, forwarded to the device verbatim. Linux hands them over
//! with `vmemdup_user` without looking at them, this kernel does the same, and
//! nothing here walks a length the caller supplied - so there is nothing to
//! fuzz on that side.

use virtio_drivers::config::{ReadOnly, WriteOnly};

bitflags::bitflags! {
    /// The virtio feature bits this device's command set is gated on.
    ///
    /// A `bitflags` type rather than a set of loose `u64`s, because the
    /// transport's bring-up takes and returns one (`Transport::begin_init` is
    /// generic over `Flags`), so the negotiation and the names have to be the
    /// same value. The device-independent bits are in here too: the queue
    /// features are negotiated in the same word as the device's own.
    #[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
    pub struct Features: u64 {
        /// The device can run a 3D context (`CTX_*`, `*_3D`).
        const VIRGL = 1 << 0;
        /// `GET_EDID` is answered. This kernel does not ask - the DRM layer
        /// synthesizes its own EDID block - but the bit is named because the
        /// request it gates is.
        const EDID = 1 << 1;
        /// A resource can be given a UUID so another virtio device can share it.
        const RESOURCE_UUID = 1 << 2;
        /// Blob resources, which Mesa's newer winsys asks for.
        const RESOURCE_BLOB = 1 << 3;
        /// A context can be created explicitly, with several rings, rather than
        /// implicitly by the first command that needs one.
        const CONTEXT_INIT = 1 << 4;
        /// The device reports a blob alignment its blob requests must respect.
        const BLOB_ALIGNMENT = 1 << 5;

        /// Descriptors may point at a descriptor table.
        const RING_INDIRECT_DESC = 1 << 28;
        /// Notification suppression through an event index.
        const RING_EVENT_IDX = 1 << 29;
        /// The modern (non-transitional) device. Always negotiated, and the
        /// reason a legacy register layout is never used.
        const VERSION_1 = 1 << 32;
    }
}

/// The features this kernel asks for, and no others.
///
/// Deliberately not "everything the device offers": a feature accepted and then
/// unused is a promise this kernel makes to a device that will act on it. The
/// device's own command-gating bits are absent because the commands they gate do
/// not exist here yet - `VIRGL` is the one that matters, and asking for it
/// before there is a context to run would have the device offer a 3D path this
/// kernel cannot drive. When those commands land they add their bits here, and
/// the test below is what makes forgetting them fail.
pub const WANTED_FEATURES: Features = Features::RING_INDIRECT_DESC
    .union(Features::RING_EVENT_IDX)
    .union(Features::VERSION_1);

/// `VIRTIO_GPU_EVENT_DISPLAY`: bit 0 of `events_read`, raised by the host when
/// the display it wants this machine to drive changes shape - a window resize
/// or a fullscreen toggle. It is the only asynchronously-set bit the device
/// configuration has, and the only event this driver takes.
pub const EVENT_DISPLAY: u32 = 1 << 0;

/// The device configuration space, `struct virtio_gpu_config`.
///
/// Five `u32` words, read once at bring-up except for `events_read`, which the
/// driver polls: the host raises `EVENT_DISPLAY` there and the driver
/// acknowledges it by writing the bit back to `events_clear`. That is the whole
/// of the resize protocol's device half - the size itself is a `GET_DISPLAY_INFO`
/// command, and only worth spending once this says there is a reason to.
///
/// The registers are wrapped the way the crate's configuration reader requires
/// (`ReadOnly`/`WriteOnly`), because the queue's transport is what actually
/// reads them and it needs the direction to be part of the type.
#[repr(C)]
pub struct Config {
    pub events_read: ReadOnly<u32>,
    pub events_clear: WriteOnly<u32>,
    pub num_scanouts: ReadOnly<u32>,
    pub num_capsets: ReadOnly<u32>,
    pub blob_alignment: ReadOnly<u32>,
}

/// A control-queue command: `enum virtio_gpu_ctrl_type`'s request half.
pub const CMD_GET_DISPLAY_INFO: u32 = 0x0100;
pub const CMD_RESOURCE_CREATE_2D: u32 = 0x0101;
pub const CMD_RESOURCE_UNREF: u32 = 0x0102;
pub const CMD_SET_SCANOUT: u32 = 0x0103;
pub const CMD_RESOURCE_FLUSH: u32 = 0x0104;
pub const CMD_TRANSFER_TO_HOST_2D: u32 = 0x0105;
pub const CMD_RESOURCE_ATTACH_BACKING: u32 = 0x0106;
pub const CMD_RESOURCE_DETACH_BACKING: u32 = 0x0107;
/// Ask the device to describe one of its cap sets: its id, the highest version
/// the device can answer with, and how many bytes the set is.
pub const CMD_GET_CAPSET_INFO: u32 = 0x0108;
/// Ask for the cap set itself, `.capset_version` bytes of it.
pub const CMD_GET_CAPSET: u32 = 0x0109;

/// The 3D commands, which a device that negotiated no `VIRGL` answers with
/// `RESP_ERR_*` - the family is reachable either way, which is what the
/// measurement in `user/staging/virtgpu-probe/` shows.
pub const CMD_CTX_CREATE: u32 = 0x0200;
pub const CMD_CTX_DESTROY: u32 = 0x0201;
pub const CMD_CTX_ATTACH_RESOURCE: u32 = 0x0202;
pub const CMD_CTX_DETACH_RESOURCE: u32 = 0x0203;
pub const CMD_RESOURCE_CREATE_3D: u32 = 0x0204;
pub const CMD_TRANSFER_TO_HOST_3D: u32 = 0x0205;
pub const CMD_TRANSFER_FROM_HOST_3D: u32 = 0x0206;
pub const CMD_SUBMIT_3D: u32 = 0x0207;

/// The response codes. Only the ones a request can be answered with are named;
/// `check` below turns anything else into an error, so an unnamed code is
/// reported as itself rather than mistaken for a success.
pub const RESP_OK_NODATA: u32 = 0x1100;
pub const RESP_OK_DISPLAY_INFO: u32 = 0x1101;
/// The answer to `GET_CAPSET_INFO`, which describes a set rather than carrying
/// it.
pub const RESP_OK_CAPSET_INFO: u32 = 0x1102;
/// The answer to `GET_CAPSET`: a header and then the set's own bytes, with no
/// length field of its own - see [`resp_capset_payload`].
pub const RESP_OK_CAPSET: u32 = 0x1103;
pub const RESP_ERR_UNSPEC: u32 = 0x1200;
pub const RESP_ERR_OUT_OF_MEMORY: u32 = 0x1201;
pub const RESP_ERR_INVALID_SCANOUT_ID: u32 = 0x1202;
pub const RESP_ERR_INVALID_RESOURCE_ID: u32 = 0x1203;
pub const RESP_ERR_INVALID_CONTEXT_ID: u32 = 0x1204;
pub const RESP_ERR_INVALID_PARAMETER: u32 = 0x1205;

/// `VIRTIO_GPU_FLAG_FENCE`: the response is not delivered until the command's
/// work is done, and `fence_id` says which fence. Set on every command that has
/// work behind it, so that its response ordering is a promise rather than a
/// coincidence - a `RESOURCE_FLUSH` answered early would put a `write(2)`'s
/// pixels on screen after the syscall that claimed they were there.
pub const FLAG_FENCE: u32 = 1 << 0;

/// The resource this driver's scanout always uses.
///
/// One, because this kernel drives exactly one scanout and one framebuffer: the
/// device supports 16, the DRM layer exposes one connector, and a second
/// resource would be a second screen nothing can reach. It is a constant rather
/// than a counter because its lifetime is the machine's - `change_resolution`
/// tears it down and builds it again under the same id.
pub const RESOURCE_FB: u32 = 1;

/// Scanout 0 - the only one `num_scanouts` is asked about, and the only one
/// the DRM layer's single connector names.
pub const SCANOUT_ID: u32 = 0;

/// `VIRTIO_GPU_FORMAT_B8G8R8A8_UNORM`, the 2D scanout format: one 32-bit word
/// per pixel, blue in the low byte. The DRM layer's `XRGB8888`/`ARGB8888` are
/// this same layout, which is why a dumb buffer can be copied row by row into
/// it without a channel swizzle.
pub const FORMAT_B8G8R8A8_UNORM: u32 = 1;

/// `VIRTIO_GPU_CAPSET_VIRGL`: the cap set Mesa's virgl driver asks for, and the
/// one a `virtio-gpu-gl-pci` device offers as its first.
pub const CAPSET_VIRGL: u32 = 1;
/// `VIRTIO_GPU_CAPSET_VIRGL2`: the same set with the second version's additions.
/// Both are offered by the measured GL card - `GETPARAM(7)` answered the mask
/// `6`, which is bit 1 and bit 2 - and the two are told apart by the *version*
/// a caller asks for, not by two separate fetch paths.
pub const CAPSET_VIRGL2: u32 = 2;

/// `VIRTIO_GPU_CONTEXT_INIT_CAPSET_ID_MASK`: the low byte of a context's
/// `context_init` word is the cap set the context runs. The bits above it carry
/// the ring count and the per-ring priorities, which this kernel leaves zero -
/// see `drm_virtgpu::CONTEXT_INIT`'s note.
pub const CONTEXT_INIT_CAPSET_ID_MASK: u32 = 0x0000_00ff;

/// `VIRTIO_GPU_RESOURCE_FLAG_Y_0_TOP`: the resource's rows run top to bottom.
///
/// Mesa sets it for a rendering target, because a GL framebuffer's origin is
/// the lower left and the flag is how the guest tells the device which way the
/// rows it is handed are ordered. This kernel copies it through rather than
/// interpreting it - the 2D scanout path has its own orientation and the DRM
/// layer's transfer commands carry the box, not the flag.
pub const RESOURCE_FLAG_Y_0_TOP: u32 = 1;

/// The control header every request and response starts with.
///
/// `ctx_id` and `ring_idx` address a 3D context and one of its command rings;
/// every 2D command leaves them zero, and a device that has no contexts ignores
/// them. Both are named rather than folded into padding because the 3D family
/// puts them in the same place, and a header that spelled them differently from
/// the device would fail in the one command family that reads them.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CtrlHeader {
    /// The command being sent, or the response code coming back.
    pub kind: u32,
    pub flags: u32,
    /// Which fence this command's completion is reported under; meaningful
    /// only with `FLAG_FENCE`.
    pub fence_id: u64,
    pub ctx_id: u32,
    pub ring_idx: u8,
    pub padding: [u8; 3],
}

impl CtrlHeader {
    /// A header for `kind` with no fence, no context and ring 0 - what every 2D
    /// command sends.
    pub fn new(kind: u32) -> Self {
        CtrlHeader {
            kind,
            ..Default::default()
        }
    }

    /// Whether the response to this command came back with the code the command
    /// expects.
    ///
    /// Named rather than left to each caller: a device answering a command it did
    /// not understand reports one of the `RESP_ERR_*` codes, and an equality
    /// check against the expected code is the only thing that distinguishes that
    /// from success. A caller reading fields out of a struct the device filled
    /// with an error would otherwise see zeros and believe them.
    pub fn is(&self, expected: u32) -> bool {
        self.kind == expected
    }
}

/// `struct virtio_gpu_rect`: a pixel rectangle, half-open in the usual way.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Rect {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

impl Rect {
    /// The whole of a `width` x `height` surface.
    pub fn whole(width: u32, height: u32) -> Self {
        Rect {
            x: 0,
            y: 0,
            width,
            height,
        }
    }
}

/// `struct virtio_gpu_resource_create_2d`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct ResourceCreate2d {
    pub header: CtrlHeader,
    pub resource_id: u32,
    pub format: u32,
    pub width: u32,
    pub height: u32,
}

/// `struct virtio_gpu_resource_unref`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct ResourceUnref {
    pub header: CtrlHeader,
    pub resource_id: u32,
    pub padding: u32,
}

/// `struct virtio_gpu_set_scanout`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct SetScanout {
    pub header: CtrlHeader,
    pub rect: Rect,
    pub scanout_id: u32,
    pub resource_id: u32,
}

/// `struct virtio_gpu_resource_flush`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct ResourceFlush {
    pub header: CtrlHeader,
    pub rect: Rect,
    pub resource_id: u32,
    pub padding: u32,
}

/// `struct virtio_gpu_transfer_to_host_2d`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct TransferToHost2d {
    pub header: CtrlHeader,
    pub rect: Rect,
    /// Byte offset into the resource the rectangle starts at.
    pub offset: u64,
    pub resource_id: u32,
    pub padding: u32,
}

/// `struct virtio_gpu_mem_entry`: one guest-physical extent behind a resource.
///
/// A resource's backing is a *list* of these. This driver always sends exactly
/// one entry covering the whole allocation, because the framebuffer is one
/// contiguous block - but the list is what the device reads, so the entry is
/// spelled out rather than the address being folded into the request.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct MemEntry {
    pub addr: u64,
    pub length: u32,
    pub padding: u32,
}

/// `struct virtio_gpu_resource_attach_backing`, followed on the wire by
/// `nr_entries` `MemEntry`s.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct ResourceAttachBacking {
    pub header: CtrlHeader,
    pub resource_id: u32,
    pub nr_entries: u32,
}

/// `struct virtio_gpu_resource_detach_backing`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct ResourceDetachBacking {
    pub header: CtrlHeader,
    pub resource_id: u32,
    pub padding: u32,
}

/// `VIRTIO_GPU_MAX_SCANOUTS`: how many `pmodes` a display-info reply carries,
/// whatever the device's `num_scanouts` says.
pub const MAX_SCANOUTS: usize = 16;

/// One scanout's shape in a display-info reply.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct DisplayOne {
    pub rect: Rect,
    pub enabled: u32,
    pub flags: u32,
}

/// `struct virtio_gpu_resp_display_info`: the reply to `GET_DISPLAY_INFO`.
///
/// Fixed at 408 bytes whatever the device supports, because the array is the
/// UAPI's maximum and not the device's count. The driver reads scanout 0 out of
/// it and ignores the rest.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct RespDisplayInfo {
    pub header: CtrlHeader,
    pub pmodes: [DisplayOne; MAX_SCANOUTS],
}

/// `struct virtio_gpu_box`: the volume a 3D transfer names.
///
/// A separate type from [`Rect`] rather than one with unused fields, because
/// the two are genuinely different things on the wire: a `Rect` is two
/// dimensions and a `Box` is three plus a layer count, and the device reads
/// six words for one and four for the other. (`z`, `w`, `h`, `d` are the
/// header's own names, kept even though `w`/`h` sit beside a `d` that is a
/// depth rather than a distance.)
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Box3d {
    pub x: u32,
    pub y: u32,
    pub z: u32,
    pub w: u32,
    pub h: u32,
    pub d: u32,
}

/// `struct virtio_gpu_transfer_host_3d`: move a volume of a resource to or from
/// the host, at one mip level.
///
/// One struct for both directions, because the request half is identical and
/// only the command word differs - which is why the driver takes the direction
/// as an argument instead of having two near-copies of a 72-byte struct.
/// `stride` and `layer_stride` are the host's row and layer pitches; the guest
/// side is the contiguous extent `RESOURCE_ATTACH_BACKING` named.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct TransferHost3d {
    pub header: CtrlHeader,
    /// The header calls this `box`; `box3d` because `box` is a reserved word.
    pub box3d: Box3d,
    /// Byte offset into the resource the volume starts at.
    pub offset: u64,
    pub resource_id: u32,
    /// Which mip level of the resource.
    pub level: u32,
    pub stride: u32,
    pub layer_stride: u32,
}

/// `struct virtio_gpu_resource_create_3d`: allocate a resource a 3D context can
/// render into.
///
/// The 2D [`ResourceCreate2d`] plus everything a texture needs: `target` (the
/// pipe the resource belongs to - 2 is 2D, 4 is virgl), `bind`, the mip-level
/// count, the array size, the sample count and the flags. The measured card
/// accepts only `target = 2` with each of those at most 1 when no `VIRGL` was
/// negotiated, and forwards them when one was; that switch is the two branches
/// of `drm_virtgpu::resource_create_args_ok` and its caller.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct ResourceCreate3d {
    pub header: CtrlHeader,
    pub resource_id: u32,
    pub target: u32,
    pub format: u32,
    pub bind: u32,
    pub width: u32,
    pub height: u32,
    pub depth: u32,
    pub array_size: u32,
    pub last_level: u32,
    pub nr_samples: u32,
    pub flags: u32,
    pub padding: u32,
}

/// `struct virtio_gpu_ctx_create`: make a context, and name it.
///
/// `context_init` folds the DRM layer's `ctx_set_params` into one word - the
/// cap set id under [`CONTEXT_INIT_CAPSET_ID_MASK`], and the ring count and
/// per-ring priorities above it. This kernel sends only the cap set id and
/// leaves the rest zero, which is the single-ring path Mesa's virgl uses; see
/// `drm_virtgpu`'s note on `CONTEXT_INIT`.
///
/// `debug_name` is a fixed 64 bytes with `nlen` saying how much of it is the
/// name. It is a name rather than a string: the bytes after `nlen` are not a
/// terminator and are not read, which is why the field is an array and not a
/// pair of a pointer and a length.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct CtxCreate {
    pub header: CtrlHeader,
    /// How many bytes of `debug_name` are the name.
    pub nlen: u32,
    pub context_init: u32,
    pub debug_name: [u8; 64],
}

impl Default for CtxCreate {
    /// Written out rather than derived, and the only `Default` in this file
    /// that is: the standard derive covers arrays up to 32 elements, and the
    /// header's debug name is 64 bytes. A derived version would not compile,
    /// so the choice is this or none at all - and none at all would make this
    /// the one wire struct here that cannot be built by starting from zero.
    fn default() -> Self {
        CtxCreate {
            header: CtrlHeader::default(),
            nlen: 0,
            context_init: 0,
            debug_name: [0; 64],
        }
    }
}

/// `struct virtio_gpu_ctx_destroy`: release a context.
///
/// The header is the whole request - the context it names is in the header's
/// own `ctx_id`, so there is nothing left for a body to carry.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct CtxDestroy {
    pub header: CtrlHeader,
}

/// `struct virtio_gpu_ctx_resource`: attach or detach one resource from the
/// context the header's `ctx_id` names.
///
/// Both directions are one struct, as with the transfers: a resource is either
/// made reachable to the context's command stream or taken away from it, and
/// only the command word says which.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct CtxResource {
    pub header: CtrlHeader,
    pub resource_id: u32,
    pub padding: u32,
}

/// `struct virtio_gpu_cmd_submit`: the head of a `SUBMIT_3D`, which is a
/// command stream preceded by its length.
///
/// Only the head. On the wire it is followed by `size` bytes of the caller's
/// stream and then the resource ids the stream refers to - a stream names its
/// buffers by index, and the id list is how the device resolves them. That tail
/// is why `SUBMIT_3D` is the one request here that is neither a struct nor a
/// struct plus a fixed array, and `gpu_ctrl::submit_3d` is where its exact
/// layout is pinned against the kernel's own `virtio_gpu_cmd_submit`.
///
/// **The stream is not parsed, here or by Linux.** `virtio_gpu_init_submit`
/// hands the bytes to the device with `vmemdup_user` without looking at them,
/// and this kernel does the same: a length the caller supplied is never walked.
/// There is therefore no fuzz target on it, and the one this family does want
/// is on [`resp_capset_payload`], where the device supplies the length.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct CmdSubmit {
    pub header: CtrlHeader,
    pub size: u32,
    pub padding: u32,
}

/// `struct virtio_gpu_get_capset_info`: which of the device's cap sets to
/// describe.
///
/// By *index* rather than by id: the caller is asking what the device's nth set
/// is, and the reply is what says. A driver that wanted a set by id would walk
/// the indices until it found one - which is what `drm_virtgpu::capsets` does
/// once, at bring-up, so no ioctl has to.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct GetCapsetInfo {
    pub header: CtrlHeader,
    pub capset_index: u32,
    pub padding: u32,
}

/// `struct virtio_gpu_resp_capset_info`: the answer to the above.
///
/// `capset_max_version` is the highest version the device can answer with and
/// `capset_max_size` how many bytes a fetch at that version returns. Both are
/// what a caller needs before it can fetch anything at all: the version is what
/// it asks for and the size is what its `GET_CAPS` buffer is measured against -
/// which is the comparison `drm_virtgpu::caps_may_copy` makes.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RespCapsetInfo {
    pub header: CtrlHeader,
    pub capset_id: u32,
    pub capset_max_version: u32,
    pub capset_max_size: u32,
    pub padding: u32,
}

/// `struct virtio_gpu_get_capset`: fetch a cap set, at a version.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct GetCapset {
    pub header: CtrlHeader,
    pub capset_id: u32,
    pub capset_version: u32,
}

/// The cap set inside a `RESP_OK_CAPSET`, or `None` if these bytes are not one.
///
/// This is the module's only *parser*, and the shape of the reply is the reason
/// it needs one: `struct virtio_gpu_resp_capset` ends in a flexible array
/// (`__u8 capset_data[]`), so the reply carries no length of its own and the
/// only bound on it is the buffer the driver allocated. The driver sizes that
/// buffer from the `capset_max_size` the device reported and then reads exactly
/// that many bytes, so this reader's whole job is to refuse anything shorter
/// than a header and to name the payload without ever claiming more bytes than
/// are there.
///
/// A response code other than `RESP_OK_CAPSET` is `None` rather than an empty
/// payload. A device that answered an error would otherwise have the bytes of
/// its error struct handed to a caller as a cap set of zeroes - a capability
/// word that reads as a real answer, which is the false-shape failure this
/// family exists to avoid.
pub fn resp_capset_payload(bytes: &[u8]) -> Option<&[u8]> {
    let header = read_struct::<CtrlHeader>(bytes)?;
    if !header.is(RESP_OK_CAPSET) {
        return None;
    }
    bytes.get(size_of::<CtrlHeader>()..)
}

/// The bytes of a wire struct, for handing to a queue.
///
/// The one `unsafe` this module needs, and the reason it is safe is a property
/// of the module rather than of the call: every type here is `#[repr(C)]` and
/// made of integers and arrays of integers, so it has no padding byte that could
/// be uninitialised, and the assertions at the bottom of this file pin the size
/// of each. The lifetime is tied to the value, so a caller cannot keep the slice
/// past the struct it came from.
pub fn as_bytes<T: Copy>(value: &T) -> &[u8] {
    // SAFETY: `T` is a `#[repr(C)]` aggregate of integers (asserted below), so
    // every byte of it is initialised; the caller holds `value` for the life of
    // the borrow, so the pointer is valid and correctly aligned for
    // `size_of::<T>()` bytes.
    unsafe { core::slice::from_raw_parts(value as *const T as *const u8, size_of::<T>()) }
}

/// A wire struct read back out of a response buffer, or `None` if the buffer is
/// too short to hold one.
///
/// Reading rather than parsing: the caller knows which struct it asked for, and
/// the only thing that can be wrong with the buffer is that the device answered
/// with something shorter than the reply it owed. `None` is that case, and the
/// caller turns it into an I/O error.
pub fn read_struct<T: Copy>(bytes: &[u8]) -> Option<T> {
    if bytes.len() < size_of::<T>() {
        return None;
    }
    // SAFETY: the length is checked above, so the pointer is valid for
    // `size_of::<T>()` bytes; `T` is `Copy` and has no padding, so every bit
    // pattern of that length is a valid `T`. `read_unaligned` because the
    // receive buffer is a byte buffer with no alignment promise beyond its own.
    Some(unsafe { core::ptr::read_unaligned(bytes.as_ptr() as *const T) })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The command numbers are the header's, not this file's invention.
    ///
    /// Written as literals from `<linux/virtio_gpu.h>`'s own enumeration rather
    /// than by comparing the constants to each other, which is the check that
    /// caught the lease family's thirteen-too-high numbers
    /// (`drivers/drm.rs`). A caller and a driver agreeing on a number is not
    /// evidence that the number is right.
    #[test]
    fn the_command_numbers_are_the_headers() {
        assert_eq!(CMD_GET_DISPLAY_INFO, 0x0100);
        assert_eq!(CMD_RESOURCE_CREATE_2D, 0x0101);
        assert_eq!(CMD_RESOURCE_UNREF, 0x0102);
        assert_eq!(CMD_SET_SCANOUT, 0x0103);
        assert_eq!(CMD_RESOURCE_FLUSH, 0x0104);
        assert_eq!(CMD_TRANSFER_TO_HOST_2D, 0x0105);
        assert_eq!(CMD_RESOURCE_ATTACH_BACKING, 0x0106);
        assert_eq!(CMD_RESOURCE_DETACH_BACKING, 0x0107);
        assert_eq!(CMD_GET_CAPSET_INFO, 0x0108);
        assert_eq!(CMD_GET_CAPSET, 0x0109);
        assert_eq!(CMD_CTX_CREATE, 0x0200);
        assert_eq!(CMD_CTX_DESTROY, 0x0201);
        assert_eq!(CMD_CTX_ATTACH_RESOURCE, 0x0202);
        assert_eq!(CMD_CTX_DETACH_RESOURCE, 0x0203);
        assert_eq!(CMD_RESOURCE_CREATE_3D, 0x0204);
        assert_eq!(CMD_TRANSFER_TO_HOST_3D, 0x0205);
        assert_eq!(CMD_TRANSFER_FROM_HOST_3D, 0x0206);
        assert_eq!(CMD_SUBMIT_3D, 0x0207);
        assert_eq!(RESP_OK_NODATA, 0x1100);
        assert_eq!(RESP_OK_DISPLAY_INFO, 0x1101);
        assert_eq!(RESP_OK_CAPSET_INFO, 0x1102);
        assert_eq!(RESP_OK_CAPSET, 0x1103);
        assert_eq!(RESP_ERR_UNSPEC, 0x1200);
        assert_eq!(RESP_ERR_OUT_OF_MEMORY, 0x1201);
        assert_eq!(RESP_ERR_INVALID_SCANOUT_ID, 0x1202);
        assert_eq!(RESP_ERR_INVALID_RESOURCE_ID, 0x1203);
        assert_eq!(RESP_ERR_INVALID_CONTEXT_ID, 0x1204);
        assert_eq!(RESP_ERR_INVALID_PARAMETER, 0x1205);
        assert_eq!(SCANOUT_ID, 0);
        assert_eq!(RESOURCE_FB, 1);
        assert_eq!(FORMAT_B8G8R8A8_UNORM, 1);
        assert_eq!(MAX_SCANOUTS, 16);
        assert_eq!(EVENT_DISPLAY, 1);
    }

    #[test]
    fn the_feature_bits_are_the_headers() {
        assert_eq!(Features::VIRGL.bits(), 1);
        assert_eq!(Features::EDID.bits(), 2);
        assert_eq!(Features::RESOURCE_UUID.bits(), 4);
        assert_eq!(Features::RESOURCE_BLOB.bits(), 8);
        assert_eq!(Features::CONTEXT_INIT.bits(), 16);
        assert_eq!(Features::BLOB_ALIGNMENT.bits(), 32);
        assert_eq!(Features::RING_INDIRECT_DESC.bits(), 1 << 28);
        assert_eq!(Features::RING_EVENT_IDX.bits(), 1 << 29);
        assert_eq!(Features::VERSION_1.bits(), 1 << 32);
        assert_eq!(FLAG_FENCE, 1);
    }

    /// The cap set ids and the context word's mask are the header's too, and
    /// they are the two numbers a 3D client and this driver have to agree on
    /// without either being able to check the other: a cap set fetched under
    /// the wrong id is a blob of the wrong shape, and a cap set id written
    /// outside the mask is a context that will not run.
    #[test]
    fn the_capset_ids_and_the_context_mask_are_the_headers() {
        assert_eq!(CAPSET_VIRGL, 1);
        assert_eq!(CAPSET_VIRGL2, 2);
        assert_eq!(CONTEXT_INIT_CAPSET_ID_MASK, 0x0000_00ff);
        assert_eq!(RESOURCE_FLAG_Y_0_TOP, 1);
        // The two offered sets are exactly the mask the measured GL card
        // reported from `GETPARAM(7)`, which is what makes "mask 6" and "two
        // sets, ids 1 and 2" the same statement rather than two.
        assert_eq!((1 << CAPSET_VIRGL) | (1 << CAPSET_VIRGL2), 6);
        // A cap set id has to survive being folded into the context word,
        // which is the only place it is written.
        assert_eq!(CAPSET_VIRGL2 & CONTEXT_INIT_CAPSET_ID_MASK, CAPSET_VIRGL2);
    }

    /// Every size is the wire's, and a field added carelessly fails here rather
    /// than shifting every field after it on the wire.
    #[test]
    fn every_struct_is_the_size_the_device_reads() {
        assert_eq!(size_of::<CtrlHeader>(), 24);
        assert_eq!(size_of::<Rect>(), 16);
        assert_eq!(size_of::<MemEntry>(), 16);
        assert_eq!(size_of::<ResourceCreate2d>(), 40);
        assert_eq!(size_of::<ResourceUnref>(), 32);
        assert_eq!(size_of::<SetScanout>(), 48);
        assert_eq!(size_of::<ResourceFlush>(), 48);
        assert_eq!(size_of::<TransferToHost2d>(), 56);
        assert_eq!(size_of::<ResourceAttachBacking>(), 32);
        assert_eq!(size_of::<ResourceDetachBacking>(), 32);
        assert_eq!(size_of::<DisplayOne>(), 24);
        assert_eq!(size_of::<RespDisplayInfo>(), 24 + 16 * 24);
        assert_eq!(size_of::<Box3d>(), 24);
        assert_eq!(size_of::<TransferHost3d>(), 72);
        assert_eq!(size_of::<ResourceCreate3d>(), 72);
        assert_eq!(size_of::<CtxCreate>(), 96);
        // A context destroy really is the header alone, which is the check that
        // the context id is being read from the header rather than from a body
        // this struct does not have.
        assert_eq!(size_of::<CtxDestroy>(), size_of::<CtrlHeader>());
        assert_eq!(size_of::<CtxResource>(), 32);
        assert_eq!(size_of::<CmdSubmit>(), 32);
        assert_eq!(size_of::<GetCapsetInfo>(), 32);
        assert_eq!(size_of::<RespCapsetInfo>(), 40);
        assert_eq!(size_of::<GetCapset>(), 32);
        // `size` is at 24 and the stream begins at 32, which is the offset
        // `gpu_ctrl::submit_3d` appends the caller's bytes at.
        assert_eq!(size_of::<CmdSubmit>(), 24 + 8);
    }

    /// The header's fields sit where the device reads them. `ctx_id` at 16 and
    /// `ring_idx` at 20 are the ones worth pinning: the 3D family reads both,
    /// and a struct that put them elsewhere would be a header the 2D commands
    /// never notice and the 3D ones never survive.
    #[test]
    fn the_control_header_is_laid_out_the_way_the_device_reads_it() {
        assert_eq!(core::mem::offset_of!(CtrlHeader, kind), 0);
        assert_eq!(core::mem::offset_of!(CtrlHeader, flags), 4);
        assert_eq!(core::mem::offset_of!(CtrlHeader, fence_id), 8);
        assert_eq!(core::mem::offset_of!(CtrlHeader, ctx_id), 16);
        assert_eq!(core::mem::offset_of!(CtrlHeader, ring_idx), 20);
        assert_eq!(core::mem::offset_of!(Config, events_read), 0);
        assert_eq!(core::mem::offset_of!(Config, events_clear), 4);
        assert_eq!(core::mem::offset_of!(Config, num_scanouts), 8);
        assert_eq!(core::mem::offset_of!(Config, num_capsets), 12);
        assert_eq!(core::mem::offset_of!(Config, blob_alignment), 16);
        assert_eq!(size_of::<Config>(), 20);
        assert_eq!(core::mem::offset_of!(RespDisplayInfo, pmodes), 24);
        assert_eq!(core::mem::offset_of!(ResourceAttachBacking, nr_entries), 28);
    }

    /// The 3D structs' fields sit where the device reads them.
    ///
    /// Sizes alone would not catch the mistake these guard against: two `u32`s
    /// swapped inside a struct keep its size and move both values, and the
    /// device would read a width where a height belongs. The offsets worth
    /// pinning are the ones a plausible rearrangement gets wrong - the 8-byte
    /// `offset` in `TransferHost3d`, which has to land on an 8-byte boundary
    /// after a 24-byte `Box3d`, and `CmdSubmit`'s `size`, because the caller's
    /// stream begins the moment it ends.
    #[test]
    fn the_3d_structs_put_their_fields_where_the_device_reads_them() {
        assert_eq!(core::mem::offset_of!(TransferHost3d, box3d), 24);
        assert_eq!(core::mem::offset_of!(TransferHost3d, offset), 48);
        assert_eq!(core::mem::offset_of!(TransferHost3d, resource_id), 56);
        assert_eq!(core::mem::offset_of!(TransferHost3d, level), 60);
        assert_eq!(core::mem::offset_of!(TransferHost3d, stride), 64);
        assert_eq!(core::mem::offset_of!(TransferHost3d, layer_stride), 68);

        assert_eq!(core::mem::offset_of!(ResourceCreate3d, resource_id), 24);
        assert_eq!(core::mem::offset_of!(ResourceCreate3d, target), 28);
        assert_eq!(core::mem::offset_of!(ResourceCreate3d, bind), 36);
        assert_eq!(core::mem::offset_of!(ResourceCreate3d, width), 40);
        assert_eq!(core::mem::offset_of!(ResourceCreate3d, last_level), 56);
        assert_eq!(core::mem::offset_of!(ResourceCreate3d, nr_samples), 60);
        assert_eq!(core::mem::offset_of!(ResourceCreate3d, flags), 64);

        assert_eq!(core::mem::offset_of!(CtxCreate, nlen), 24);
        assert_eq!(core::mem::offset_of!(CtxCreate, context_init), 28);
        assert_eq!(core::mem::offset_of!(CtxCreate, debug_name), 32);

        assert_eq!(core::mem::offset_of!(CtxResource, resource_id), 24);
        assert_eq!(core::mem::offset_of!(CmdSubmit, size), 24);
        assert_eq!(core::mem::offset_of!(GetCapsetInfo, capset_index), 24);
        assert_eq!(core::mem::offset_of!(RespCapsetInfo, capset_id), 24);
        assert_eq!(
            core::mem::offset_of!(RespCapsetInfo, capset_max_version),
            28
        );
        assert_eq!(core::mem::offset_of!(RespCapsetInfo, capset_max_size), 32);
        assert_eq!(core::mem::offset_of!(GetCapset, capset_id), 24);
        assert_eq!(core::mem::offset_of!(GetCapset, capset_version), 28);
    }

    /// The cap set reader bounds the payload by the buffer and by nothing else.
    ///
    /// The reply carries no length of its own, so the only two facts the reader
    /// has are the buffer it was given and the response code in it. What this
    /// pins is the second: a reply that is not a `RESP_OK_CAPSET` is refused
    /// rather than reported as an empty set, and a payload is never longer than
    /// the bytes it was read from.
    #[test]
    fn a_capset_reply_is_read_only_when_it_is_one() {
        let mut bytes = [0u8; 24 + 308];
        bytes[..4].copy_from_slice(&RESP_OK_CAPSET.to_le_bytes());
        let payload = resp_capset_payload(&bytes).expect("a header and 308 bytes is a capset");
        assert_eq!(payload.len(), 308);
        // The payload is the reply's own bytes, not a copy with room to spare.
        assert_eq!(payload.as_ptr(), bytes[24..].as_ptr());

        // An error answer is refused. A device that declined would otherwise
        // have its error struct's zeroes handed over as a capability word.
        let mut declined = [0u8; 24 + 308];
        declined[..4].copy_from_slice(&RESP_ERR_INVALID_PARAMETER.to_le_bytes());
        assert!(resp_capset_payload(&declined).is_none());

        // Short of a header is short of everything: not even the code is there.
        assert!(resp_capset_payload(&[0u8; 23]).is_none());
        // A header with no payload is a legal empty set, not a failure - the
        // device may offer a set this kernel has nothing to say about.
        let mut bare = [0u8; 24];
        bare[..4].copy_from_slice(&RESP_OK_CAPSET.to_le_bytes());
        assert_eq!(resp_capset_payload(&bare), Some(&[][..]));
    }

    /// A request round-trips through the byte view unchanged, which is what
    /// makes `as_bytes` and `read_struct` a pair rather than two guesses.
    #[test]
    fn a_request_survives_the_byte_view() {
        let request = ResourceCreate2d {
            header: CtrlHeader::new(CMD_RESOURCE_CREATE_2D),
            resource_id: 7,
            format: FORMAT_B8G8R8A8_UNORM,
            width: 1280,
            height: 720,
        };
        let bytes = as_bytes(&request);
        assert_eq!(bytes.len(), 40);
        // The command word is first and little-endian, which is the one thing a
        // byte-order mistake would get wrong before anything else did.
        assert_eq!(&bytes[0..4], &[0x01, 0x01, 0x00, 0x00]);
        let back: ResourceCreate2d = read_struct(bytes).expect("40 bytes is one struct");
        assert_eq!(back.resource_id, 7);
        assert_eq!(back.width, 1280);
        assert_eq!(back.height, 720);
    }

    /// A short buffer is `None`, never a partially-filled struct.
    #[test]
    fn a_short_response_is_refused_rather_than_read() {
        let bytes = [0u8; 23];
        assert!(read_struct::<CtrlHeader>(&bytes).is_none());
        let bytes = [0u8; 24];
        assert!(read_struct::<CtrlHeader>(&bytes).is_some());
    }

    /// `is` distinguishes an error response from a success, which is the whole
    /// reason it exists.
    #[test]
    fn an_error_response_is_not_a_success() {
        let ok = CtrlHeader::new(RESP_OK_NODATA);
        assert!(ok.is(RESP_OK_NODATA));
        let bad = CtrlHeader::new(RESP_ERR_INVALID_RESOURCE_ID);
        assert!(!bad.is(RESP_OK_NODATA));
    }

    /// What this kernel asks for is what the queue is built to support, and
    /// nothing the device will act on unasked. VIRGL is deliberately absent:
    /// this change must not negotiate a feature whose commands do not exist yet.
    #[test]
    fn the_wanted_features_are_the_queue_ones_and_no_more() {
        assert_eq!(
            WANTED_FEATURES,
            Features::RING_INDIRECT_DESC
                .union(Features::RING_EVENT_IDX)
                .union(Features::VERSION_1)
        );
        for absent in [
            Features::VIRGL,
            Features::EDID,
            Features::RESOURCE_UUID,
            Features::RESOURCE_BLOB,
            Features::CONTEXT_INIT,
            Features::BLOB_ALIGNMENT,
        ] {
            assert!(
                !WANTED_FEATURES.contains(absent),
                "unasked-for feature {absent:?}"
            );
        }
    }
}
