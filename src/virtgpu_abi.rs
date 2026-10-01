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
//! turns that into `EIO`. Nothing here walks a length the device supplied, which
//! is why there is no fuzz target beside this module - the variable-length
//! replies (`GET_CAPSET`'s blob, `RESOURCE_CREATE_BLOB`'s descriptor list) are
//! where a parser will appear, and they land with the commands that need them.

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

/// The response codes. Only the ones a request can be answered with are named;
/// `check` below turns anything else into an error, so an unnamed code is
/// reported as itself rather than mistaken for a success.
pub const RESP_OK_NODATA: u32 = 0x1100;
pub const RESP_OK_DISPLAY_INFO: u32 = 0x1101;
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
        assert_eq!(RESP_OK_NODATA, 0x1100);
        assert_eq!(RESP_OK_DISPLAY_INFO, 0x1101);
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
