//! t-drm - a compositor's KMS bring-up, end to end on the card.
//!
//! Exercises the exact sequence `libdrm`/weston drives a DRM backend with, so
//! this pins the ABI rather than an internal-only notion: open `/dev/dri/card0`,
//! ask for `DRM_CAP_DUMB_BUFFER`, enumerate resources (one CRTC / connector /
//! encoder), make a dumb buffer, `MAP_DUMB` it and `mmap` the returned offset
//! (the buffer must alias real pages - a write through the mapping reads back),
//! `ADDFB` it to the CRTC, `SETCRTC`, then `PAGE_FLIP` with the event flag and
//! read the flip-completion `drmEvent` off the card fd. The request numbers and
//! struct offsets are the Linux UAPI's, so a green run means a real compositor
//! speaking libdrm would find the same shapes.
//!
//! Past the flip, the bracket a GL compositor adds around it: `GETFB` reading
//! the binding back, `ADDFB2` binding by fourcc, the connector's property list
//! (`OBJ_GETPROPERTIES`), the `SET_VERSION` handshake, the `GET_MAGIC`/
//! `AUTH_MAGIC` pair, `GEM_CLOSE` releasing a handle - and the render node
//! (`renderD128`) making and mapping a dumb buffer of its own, the door GBM
//! allocates through.
//!
//! Before any of that, the identity walk a Rust compositor makes with nothing
//! but the card's device number: `/sys/dev/char/226:0/device/drm` has to
//! resolve and has to name the two nodes of the one GPU, because a card that
//! cannot answer is refused as "the provided file descriptor does not refer to
//! a DRM node" - which is where niri stopped, not libdrm, whose test for the
//! same question is a different and weaker one. See `src/vfs/sysfs.rs`.
//!
//! # What the move to the std side changed
//!
//! The door, and the two records that were undersized.
//!
//! The ioctl door is `libc::syscall(SYS_ioctl, ...)` through this program's own
//! `ioctl`, as `t-drm-args` and `t-drm-lease` do it, and the card and render
//! node are `rustix` `OwnedFd`s rather than numbers closed by hand. The
//! namespace walk is `std::fs` (`metadata`/`read_dir`) instead of the raw
//! `getdents64`.
//!
//! Every record is spelled as `drm.h`/`drm_mode.h` lay it out, with the offsets
//! as compile-time asserts - `t-drm-args`'s rule, and load-bearing for the same
//! reason: neither library binds these structs. Doing so found **two records the
//! no-std version handed the kernel four bytes short of what the kernel's own
//! `arg_shape` table copies**: `drm_mode_obj_get_properties` is `sizeof` 32
//! (the `__u64` pointers round the two `__u32`s up), and it was passed `28`;
//! `drm_mode_fb_cmd2` is `sizeof` 104 and its first `ADDFB2` was passed `100`.
//! Both worked on the no-std side only because a stack buffer reads four bytes
//! into its neighbour without faulting; a caller who allocated exactly `len`
//! would have had the kernel read past the end. The sizes here are `size_of`,
//! so the request word states the same length the kernel validates against
//! (`src/drivers/drm.rs`, `arg_shape`).
//!
//! Not made on a host, for `t-drm-args`'s and `t-drm-lease`'s reason: every
//! claim below is about *this* card's answers - one CRTC, one connector, two
//! planes, a vblank-less flip, this driver's `GET_MAGIC` - and a host's own
//! card answers its own topology. So a host run says so and makes no claim at
//! all.

use std::ffi::c_void;
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::fs::MetadataExt;

use mpos_user_tests::{Report, host_run};
use rustix::fs::{Mode, OFlags};
use rustix::io::Errno;
use rustix::mm::{MapFlags, ProtFlags};

/// `_IOWR('d', nr, size)` as libdrm builds it: the direction pair at bits 30..31,
/// the argument's size at 16..31, the family byte at 8..15, the request number
/// below that. Identical on both machines - an ioctl request word is not a
/// syscall number (`t-drm-args` spells its whole family the same way).
const fn ioc_d(nr: u32, size: u32) -> u64 {
    ((3u64) << 30) | ((size as u64) << 16) | ((b'd' as u64) << 8) | (nr as u64)
}
/// `_IO('d', nr)` - no argument at all, which is how the master requests are
/// spelled and why their third ioctl argument is a number rather than a pointer.
const fn io_d(nr: u32) -> u64 {
    (nr as u64) | ((b'd' as u64) << 8)
}

const NR_VERSION: u32 = 0x00;
const NR_GET_MAGIC: u32 = 0x02;
const NR_SET_VERSION: u32 = 0x07;
const NR_GEM_CLOSE: u32 = 0x09;
const NR_GET_CAP: u32 = 0x0c;
const NR_SET_CLIENT_CAP: u32 = 0x0d;
const NR_AUTH_MAGIC: u32 = 0x11;
const NR_SET_MASTER: u32 = 0x1e;
const NR_DROP_MASTER: u32 = 0x1f;
const NR_GETRESOURCES: u32 = 0xa0;
const NR_SETCRTC: u32 = 0xa2;
const NR_MODE_CURSOR: u32 = 0xa3;
const NR_MODE_CURSOR2: u32 = 0xbb;
const NR_GETPROPERTY: u32 = 0xaa;
const NR_GETFB: u32 = 0xad;
const NR_ADDFB: u32 = 0xae;
const NR_PAGE_FLIP: u32 = 0xb0;
const NR_CREATE_DUMB: u32 = 0xb2;
const NR_MAP_DUMB: u32 = 0xb3;
const NR_GETPLANERESOURCES: u32 = 0xb5;
const NR_GETPLANE: u32 = 0xb6;
const NR_ADDFB2: u32 = 0xb8;
const NR_OBJ_GETPROPERTIES: u32 = 0xb9;
const NR_GETFB2: u32 = 0xce;
/// The `DRM_VIRTGPU_*` pair a dumb buffer has to answer as Linux's does: the
/// resource it names, and the copy into the device that this request makes.
/// Both are refused for a handle this card never made, and both answer for a
/// dumb buffer now.
const NR_RESOURCE_INFO: u32 = 0x45;
const NR_TRANSFER_TO_HOST: u32 = 0x47;

/// `DRM_CLIENT_CAP_ATOMIC`. Set below rather than only asserted: a client that
/// has not set it is not shown the atomic properties in a list, and the reads
/// further down this file are the reads a *compositor* makes - weston 16 has no
/// legacy path left and must set this cap before it can read a single one.
const CLIENT_CAP_ATOMIC: u64 = 3;
/// `DRM_CAP_DUMB_BUFFER`.
const CAP_DUMB_BUFFER: u64 = 1;
/// `DRM_EVENT_FLIP_COMPLETE`, as the UAPI spells it. libdrm dispatches on
/// this field, so its value is not the card's to choose - a test that took
/// the card's word for it agreed with a card whose flip events no compositor
/// would ever act on.
const EVENT_FLIP_COMPLETE: u32 = 0x02;
/// The card's fixed object ids: the one CRTC, the connector, the primary plane
/// and the cursor plane, as `src/drivers/drm.rs` allocates them.
const CRTC_ID: u32 = 34;
const CURSOR_PLANE_ID: u32 = 37;
/// `DRM_MODE_PAGE_FLIP_EVENT`.
const PAGE_FLIP_EVENT: u32 = 0x1;
/// `DRM_FORMAT_XRGB8888` and `DRM_FORMAT_ARGB8888`, the two fourccs a
/// compositor's scanout and cursor buffers carry.
const FORMAT_XRGB8888: u32 = 0x3432_5258;
const FORMAT_ARGB8888: u32 = 0x3432_5241;
/// The DRM character-device major, and the card's two minor numbers - the
/// number `stat` of `/dev/dri/card0` answers, and the render node beside it.
const DRM_MAJOR: u32 = 226;
const CARD_MINOR: u32 = 0;
const RENDER_MINOR: u32 = 128;

// --- the records, spelled as `drm.h`/`drm_mode.h` lay them out --------------
//
// Every one is handed over at the size the kernel's own ioctl table names for
// its request (`src/drivers/drm.rs`, the `arg_shape` rows), because the handler
// copies the whole record - a shorter buffer is not a short copy, it is a read
// past the caller's allocation.

/// `struct drm_get_cap`: a capability word in, its value out.
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct GetCap {
    capability: u64,
    value: u64,
}

/// `struct drm_set_client_cap`: a capability word and whether to set it.
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct SetClientCap {
    capability: u64,
    value: u64,
}

/// `struct drm_mode_card_res` - the two-call enumeration: four id arrays and
/// the four counts that size them, then the mode limits.
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct CardRes {
    fb_id_ptr: u64,
    crtc_id_ptr: u64,
    connector_id_ptr: u64,
    encoder_id_ptr: u64,
    count_fbs: u32,
    count_crtcs: u32,
    count_connectors: u32,
    count_encoders: u32,
    min_width: u32,
    max_width: u32,
    min_height: u32,
    max_height: u32,
}

/// `struct drm_mode_get_plane_res`: the one KMS list whose pointer comes
/// *before* its count - the layout a kernel can get wrong and still look
/// plausible.
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct GetPlaneRes {
    plane_id_ptr: u64,
    count_planes: u32,
}

/// `struct drm_mode_create_dumb`.
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct CreateDumb {
    height: u32,
    width: u32,
    bpp: u32,
    flags: u32,
    handle: u32,
    pitch: u32,
    size: u64,
}

/// `struct drm_mode_map_dumb`: a handle in, the mmap offset out.
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct MapDumb {
    handle: u32,
    pad: u32,
    offset: u64,
}

/// `struct drm_virtgpu_resource_info`: a handle in, the device resource and its
/// size out.
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct ResourceInfo {
    bo_handle: u32,
    res_handle: u32,
    size: u64,
}

/// `struct drm_virtgpu_3d_box`: the half-open rectangle a transfer names.
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct VirtgpuBox {
    x: u32,
    y: u32,
    z: u32,
    w: u32,
    h: u32,
    d: u32,
}

/// `struct drm_virtgpu_3d_transfer_to_host`: a handle, the box, and the strides
/// a caller would use for a layered resource.
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct TransferToHost {
    bo_handle: u32,
    box_: VirtgpuBox,
    level: u32,
    offset: u32,
    stride: u32,
    layer_stride: u32,
}

/// `struct drm_mode_fb_cmd`: the legacy framebuffer binding, bpp plus depth.
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct FbCmd {
    fb_id: u32,
    width: u32,
    height: u32,
    pitch: u32,
    bpp: u32,
    depth: u32,
    handle: u32,
}

/// `struct drm_mode_fb_cmd2`: the fourcc binding, four planes of which a caller
/// that inherited the framebuffer reads as many as are non-zero. In and out at
/// once - the request goes back with `fb_id` in the bytes the answer comes in.
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct FbCmd2 {
    fb_id: u32,
    width: u32,
    height: u32,
    pixel_format: u32,
    flags: u32,
    handles: [u32; 4],
    pitches: [u32; 4],
    offsets: [u32; 4],
    modifier: [u64; 4],
}

/// `struct drm_mode_crtc`: the CRTC's own state, set and read back. The trailing
/// `drm_mode_modeinfo` is the current mode, which no claim here reads - it is
/// carried for the size, not spelled out.
#[repr(C)]
#[derive(Clone, Copy)]
struct ModeCrtc {
    set_connectors_ptr: u64,
    count_connectors: u32,
    crtc_id: u32,
    fb_id: u32,
    x: u32,
    y: u32,
    gamma_size: u32,
    mode_valid: u32,
    _mode: [u8; 68],
}

impl Default for ModeCrtc {
    fn default() -> Self {
        unsafe { core::mem::zeroed() }
    }
}

/// `struct drm_mode_crtc_page_flip`: the buffer that becomes the scanout.
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct PageFlip {
    crtc_id: u32,
    fb_id: u32,
    flags: u32,
    reserved: u32,
    user_data: u64,
}

/// `struct drm_mode_get_plane`: the plane's own state and its format list.
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct GetPlane {
    plane_id: u32,
    crtc_id: u32,
    fb_id: u32,
    possible_crtcs: u32,
    gamma_size: u32,
    count_format_types: u32,
    format_type_ptr: u64,
}

/// `struct drm_mode_cursor`: bind or move the pointer, its value in the third
/// ioctl argument rather than behind a pointer.
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct ModeCursor {
    flags: u32,
    crtc_id: u32,
    x: i32,
    y: i32,
    width: u32,
    height: u32,
    handle: u32,
}

/// `struct drm_mode_cursor2`: `drm_mode_cursor` with the hotspot.
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct ModeCursor2 {
    flags: u32,
    crtc_id: u32,
    x: i32,
    y: i32,
    width: u32,
    height: u32,
    handle: u32,
    hot_x: i32,
    hot_y: i32,
}

/// `struct drm_mode_obj_get_properties`: the two-call property list of any KMS
/// object, keyed by its id and type.
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct ObjGetProperties {
    props_ptr: u64,
    prop_values_ptr: u64,
    count_props: u32,
    obj_id: u32,
    obj_type: u32,
    _pad: u32,
}

/// `struct drm_mode_get_property`: a property id in, its name, flags and value
/// list out. `name` is a fixed `DRM_PROP_NAME_LEN` array at offset 24.
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct GetProperty {
    values_ptr: u64,
    enum_blob_ptr: u64,
    prop_id: u32,
    flags: u32,
    name: [u8; 32],
    count_values: u32,
    count_enum_blobs: u32,
}

/// `struct drm_version`: the three numbers, then three (length, pointer) pairs
/// whose strings the caller allocates. `name_len` is a `__kernel_size_t`, which
/// is why it sits at 16 rather than at 12.
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Version {
    version_major: i32,
    version_minor: i32,
    version_patchlevel: i32,
    name_len: u64,
    name: u64,
    date_len: u64,
    date: u64,
    desc_len: u64,
    desc: u64,
}

/// `struct drm_set_version`: the interface the caller wants, the driver's used.
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct SetVersion {
    drm_di_major: i32,
    drm_di_minor: i32,
    drm_dd_major: i32,
    drm_dd_minor: i32,
}

/// `struct drm_gem_close`: one handle, released.
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct GemClose {
    handle: u32,
    pad: u32,
}

/// `struct drm_auth`: one magic word, handed out or taken back.
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Auth {
    magic: u32,
}

// The layout claims, as a *build* claim rather than a printed one: a drifting
// record is a compile failure, not a silently misread word. The sizes are the
// kernel's `arg_shape` rows, which are the header's `sizeof`.
const _: () = assert!(core::mem::size_of::<GetCap>() == 16);
const _: () = assert!(core::mem::size_of::<SetClientCap>() == 16);
const _: () = assert!(core::mem::size_of::<CardRes>() == 64);
const _: () = assert!(core::mem::offset_of!(CardRes, count_crtcs) == 36);
const _: () = assert!(core::mem::offset_of!(CardRes, count_connectors) == 40);
const _: () = assert!(core::mem::offset_of!(CardRes, count_encoders) == 44);
const _: () = assert!(core::mem::size_of::<GetPlaneRes>() == 16);
const _: () = assert!(core::mem::offset_of!(GetPlaneRes, plane_id_ptr) == 0);
const _: () = assert!(core::mem::offset_of!(GetPlaneRes, count_planes) == 8);
const _: () = assert!(core::mem::size_of::<CreateDumb>() == 32);
const _: () = assert!(core::mem::offset_of!(CreateDumb, handle) == 16);
const _: () = assert!(core::mem::offset_of!(CreateDumb, size) == 24);
const _: () = assert!(core::mem::size_of::<MapDumb>() == 16);
const _: () = assert!(core::mem::offset_of!(MapDumb, offset) == 8);
const _: () = assert!(core::mem::size_of::<ResourceInfo>() == 16);
const _: () = assert!(core::mem::offset_of!(ResourceInfo, res_handle) == 4);
const _: () = assert!(core::mem::offset_of!(ResourceInfo, size) == 8);
const _: () = assert!(core::mem::size_of::<VirtgpuBox>() == 24);
const _: () = assert!(core::mem::size_of::<TransferToHost>() == 44);
const _: () = assert!(core::mem::offset_of!(TransferToHost, box_) == 4);
const _: () = assert!(
    core::mem::offset_of!(TransferToHost, box_) + core::mem::offset_of!(VirtgpuBox, w) == 16
);
const _: () = assert!(
    core::mem::offset_of!(TransferToHost, box_) + core::mem::offset_of!(VirtgpuBox, d) == 24
);
const _: () = assert!(core::mem::size_of::<FbCmd>() == 28);
const _: () = assert!(core::mem::offset_of!(FbCmd, handle) == 24);
const _: () = assert!(core::mem::size_of::<FbCmd2>() == 104);
const _: () = assert!(core::mem::offset_of!(FbCmd2, pixel_format) == 12);
const _: () = assert!(core::mem::offset_of!(FbCmd2, handles) == 20);
const _: () = assert!(core::mem::offset_of!(FbCmd2, pitches) == 36);
const _: () = assert!(core::mem::offset_of!(FbCmd2, offsets) == 52);
const _: () = assert!(core::mem::offset_of!(FbCmd2, modifier) == 72);
const _: () = assert!(core::mem::size_of::<ModeCrtc>() == 104);
const _: () = assert!(core::mem::offset_of!(ModeCrtc, crtc_id) == 12);
const _: () = assert!(core::mem::offset_of!(ModeCrtc, fb_id) == 16);
const _: () = assert!(core::mem::size_of::<PageFlip>() == 24);
const _: () = assert!(core::mem::offset_of!(PageFlip, user_data) == 16);
const _: () = assert!(core::mem::size_of::<GetPlane>() == 32);
const _: () = assert!(core::mem::offset_of!(GetPlane, format_type_ptr) == 24);
const _: () = assert!(core::mem::size_of::<ModeCursor>() == 28);
const _: () = assert!(core::mem::offset_of!(ModeCursor, handle) == 24);
const _: () = assert!(core::mem::size_of::<ModeCursor2>() == 36);
const _: () = assert!(core::mem::offset_of!(ModeCursor2, hot_x) == 28);
const _: () = assert!(core::mem::size_of::<ObjGetProperties>() == 32);
const _: () = assert!(core::mem::offset_of!(ObjGetProperties, count_props) == 16);
const _: () = assert!(core::mem::offset_of!(ObjGetProperties, obj_id) == 20);
const _: () = assert!(core::mem::offset_of!(ObjGetProperties, obj_type) == 24);
const _: () = assert!(core::mem::size_of::<GetProperty>() == 64);
const _: () = assert!(core::mem::offset_of!(GetProperty, prop_id) == 16);
const _: () = assert!(core::mem::offset_of!(GetProperty, name) == 24);
const _: () = assert!(core::mem::size_of::<Version>() == 64);
const _: () = assert!(core::mem::offset_of!(Version, name_len) == 16);
const _: () = assert!(core::mem::offset_of!(Version, name) == 24);
const _: () = assert!(core::mem::size_of::<SetVersion>() == 16);
const _: () = assert!(core::mem::offset_of!(SetVersion, drm_dd_major) == 8);
const _: () = assert!(core::mem::size_of::<GemClose>() == 8);
const _: () = assert!(core::mem::size_of::<Auth>() == 4);

/// One `ioctl`, answering the negative errno a raw syscall hands back.
///
/// Through `libc::syscall` rather than `libc::ioctl`, for `t-drm-args`'s reason:
/// the request is an `unsigned long` word and `ioctl`'s binding of it is not the
/// same width on both libcs. The number comes from `libc::SYS_ioctl`, the
/// maintained per-machine table.
///
/// # Safety
/// `arg` must be null, a value the request takes by value, or an address the
/// caller owns that is at least as long as the request's size.
unsafe fn ioctl(fd: i32, request: u64, arg: *mut c_void) -> isize {
    let rc = unsafe { libc::syscall(libc::SYS_ioctl, fd as libc::c_long, request, arg) };
    if rc < 0 {
        -(std::io::Error::last_os_error().raw_os_error().unwrap_or(0) as isize)
    } else {
        rc as isize
    }
}

/// The errno a refusal answers, as the negative value the door hands back.
fn refused(err: Errno) -> isize {
    -(err.raw_os_error() as isize)
}

/// A claim that a request was refused with a particular errno. The value that
/// arrived is named on failure, so a red run says which door and with what.
fn check_refused(r: &mut Report, name: &str, got: isize, want: Errno) {
    if got == refused(want) {
        r.check(name, true);
    } else {
        r.check(
            &format!("{name} (got {got}, want {})", refused(want)),
            false,
        );
    }
}

/// A claim about a value the kernel wrote into the caller's buffer, or read out
/// of it.
fn check_is(r: &mut Report, name: &str, got: isize, want: isize) {
    if got == want {
        r.check(name, true);
    } else {
        r.check(&format!("{name} (got {got}, want {want})"), false);
    }
}

/// A claim about a condition the program computed itself.
fn check_true(r: &mut Report, name: &str, ok: bool) {
    if ok {
        r.check(name, true);
    } else {
        r.check(name, false);
    }
}

fn main() {
    println!("t-drm");
    let mut r = Report::new();

    // See the header: a host's own card answers its own topology, so a hosted
    // run makes no claim at all - `t-drm-args`'s rule.
    if host_run() {
        println!(
            "note: not made on a host - every claim below is about this kernel's card \
             (one CRTC, one connector, two planes, a vblank-less flip, this driver's \
             GET_MAGIC), and a host's own /dev/dri/card0 answers its own"
        );
        r.finish();
    }

    let card: OwnedFd = match rustix::fs::open("/dev/dri/card0", OFlags::RDWR, Mode::empty()) {
        Ok(fd) => {
            r.check("/dev/dri/card0 opens", true);
            fd
        }
        Err(e) => {
            r.check(&format!("/dev/dri/card0 opens ({e:?})"), false);
            r.finish();
        }
    };
    let fd = card.as_raw_fd();

    // The cap a compositor sets before it reads anything. This file reads the
    // connector's and the plane's property lists further down - and the
    // connector's `CRTC_ID` and the plane's `FB_ID`/`SRC_W` are properties only a
    // capped client is shown - so the cap belongs here, where a compositor puts
    // it, rather than at each read. What the cap *gates* is pinned in
    // `t-drm-props`, which is the file about the property lists.
    let mut client_cap = SetClientCap {
        capability: CLIENT_CAP_ATOMIC,
        value: 1,
    };
    let got = unsafe {
        ioctl(
            fd,
            ioc_d(NR_SET_CLIENT_CAP, 16),
            (&raw mut client_cap) as *mut c_void,
        )
    };
    check_is(&mut r, "DRM_CLIENT_CAP_ATOMIC is accepted", got, 0);

    // GET_CAP(DUMB_BUFFER) -> value 1.
    let mut cap = GetCap {
        capability: CAP_DUMB_BUFFER,
        value: 0,
    };
    let got = unsafe { ioctl(fd, ioc_d(NR_GET_CAP, 16), (&raw mut cap) as *mut c_void) };
    check_is(&mut r, "GET_CAP succeeds", got, 0);
    check_true(&mut r, "DRM_CAP_DUMB_BUFFER is advertised", cap.value == 1);

    // GETRESOURCES reports one crtc, connector, encoder.
    let mut res = CardRes::default();
    let got = unsafe {
        ioctl(
            fd,
            ioc_d(NR_GETRESOURCES, 64),
            (&raw mut res) as *mut c_void,
        )
    };
    check_is(&mut r, "GETRESOURCES succeeds", got, 0);
    check_true(&mut r, "one CRTC", res.count_crtcs == 1);
    check_true(&mut r, "one connector", res.count_connectors == 1);
    check_true(&mut r, "one encoder", res.count_encoders == 1);

    // And the second call of that same enumeration, which is the one a
    // compositor actually reads its ids out of: the counts come back to the
    // kernel beside buffers reserved at exactly those counts - a real one for
    // the CRTC, and for the framebuffers this card has not created yet the
    // dangling non-null pointer an empty `Vec` carries (address 4), because
    // that is what `drm-ffi`'s `map_ptr!` hands over. A count of zero means
    // nothing is copied and the pointer is never read; a kernel that validates
    // it anyway answers `EFAULT` and the device is not added at all. This is
    // also the only place the empty case can be pinned: it needs a card with no
    // framebuffer, and this program runs before anything here creates one -
    // `t-drm-args` runs after, by which time the card has one.
    let mut crtc_ids = [0u32; 1];
    let mut fill = CardRes {
        fb_id_ptr: 4, // the empty-Vec pointer
        crtc_id_ptr: crtc_ids.as_mut_ptr() as u64,
        count_crtcs: 1,
        count_fbs: 0,
        ..CardRes::default()
    };
    let got = unsafe {
        ioctl(
            fd,
            ioc_d(NR_GETRESOURCES, 64),
            (&raw mut fill) as *mut c_void,
        )
    };
    check_is(
        &mut r,
        "the fill call succeeds with an empty array beside a dangling pointer",
        got,
        0,
    );
    check_true(
        &mut r,
        "and it hands back the CRTC id it counted",
        crtc_ids[0] != 0,
    );

    // GETPLANERESOURCES: the one KMS list whose pointer comes *before* its
    // count (`plane_id_ptr@0`, `count_planes@8`), which is exactly the order
    // a card can get wrong and still look plausible - libdrm sizes its array
    // from the count and hands the pointer back in the second call, so a
    // kernel that reads the count from offset 0 reads half of a pointer and
    // then writes through a zero-length allocation. Pinned here so the layout
    // cannot drift again.
    let mut pres = GetPlaneRes::default();
    let got = unsafe {
        ioctl(
            fd,
            ioc_d(NR_GETPLANERESOURCES, 16),
            (&raw mut pres) as *mut c_void,
        )
    };
    check_is(&mut r, "GETPLANERESOURCES succeeds", got, 0);
    check_is(
        &mut r,
        "two planes, counted at offset 8",
        pres.count_planes as isize,
        2,
    );
    let mut plane_ids = [0u32; 2];
    let mut pres = GetPlaneRes {
        plane_id_ptr: plane_ids.as_mut_ptr() as u64,
        count_planes: plane_ids.len() as u32,
    };
    let got = unsafe {
        ioctl(
            fd,
            ioc_d(NR_GETPLANERESOURCES, 16),
            (&raw mut pres) as *mut c_void,
        )
    };
    check_is(&mut r, "the plane list fills", got, 0);
    check_is(
        &mut r,
        "the first is the primary plane",
        plane_ids[0] as isize,
        36,
    );

    // CREATE_DUMB: 64x64 @ 32bpp -> pitch 256, size 16384, handle >= 1.
    let mut dumb = CreateDumb {
        height: 64,
        width: 64,
        bpp: 32,
        ..CreateDumb::default()
    };
    let got = unsafe {
        ioctl(
            fd,
            ioc_d(NR_CREATE_DUMB, 32),
            (&raw mut dumb) as *mut c_void,
        )
    };
    check_is(&mut r, "CREATE_DUMB succeeds", got, 0);
    let handle = dumb.handle;
    let size = dumb.size;
    check_true(&mut r, "a dumb buffer handle is returned", handle >= 1);
    check_is(
        &mut r,
        "the dumb size is pitch*height",
        size as isize,
        16384,
    );

    // MAP_DUMB -> a nonzero mmap offset.
    let mut map = MapDumb {
        handle,
        ..MapDumb::default()
    };
    let got = unsafe { ioctl(fd, ioc_d(NR_MAP_DUMB, 16), (&raw mut map) as *mut c_void) };
    check_is(&mut r, "MAP_DUMB succeeds", got, 0);
    let offset = map.offset;
    check_true(&mut r, "MAP_DUMB returns an offset", offset != 0);

    // mmap the buffer and round-trip a value through the real pages.
    let m = unsafe {
        rustix::mm::mmap(
            core::ptr::null_mut(),
            size as usize,
            ProtFlags::READ | ProtFlags::WRITE,
            MapFlags::SHARED,
            &card,
            offset,
        )
    };
    check_true(&mut r, "the dumb buffer maps", m.is_ok());
    if let Ok(m) = m {
        unsafe { (m as *mut u32).write_volatile(0xdead_beef) };
        let back = unsafe { (m as *const u32).read_volatile() };
        check_true(
            &mut r,
            "the mapped buffer holds what was written",
            back == 0xdead_beef,
        );
        let _ = unsafe { rustix::mm::munmap(m, size as usize) };
    }

    // RESOURCE_INFO on it: on Linux a dumb buffer *is* a device resource
    // (`virtio_gpu_mode_dumb_create` goes through the same object creation a
    // RESOURCE_CREATE does), so `res_handle` names one and the size is the
    // buffer's own. This kernel used to answer 0 here and `EINVAL` for the
    // transfer below, which were the two divergences named in
    // `crates/gpu/src/virtgpu.rs`'s header; both are Linux's answers now, and
    // the resource is made on the first request that asks for it rather than at
    // CREATE_DUMB (that function's own reasoning says why).
    let mut info = ResourceInfo {
        bo_handle: handle,
        ..ResourceInfo::default()
    };
    let got = unsafe {
        ioctl(
            fd,
            ioc_d(NR_RESOURCE_INFO, 16),
            (&raw mut info) as *mut c_void,
        )
    };
    check_is(&mut r, "RESOURCE_INFO succeeds for a dumb buffer", got, 0);
    let res_handle = info.res_handle;
    check_true(
        &mut r,
        "a dumb buffer names a device resource",
        res_handle != 0,
    );
    check_is(
        &mut r,
        "the resource size is the dumb buffer's own",
        info.size as isize,
        size as isize,
    );
    // Asking twice answers the same resource: it is made once, not per request.
    let mut info2 = ResourceInfo {
        bo_handle: handle,
        ..ResourceInfo::default()
    };
    let got = unsafe {
        ioctl(
            fd,
            ioc_d(NR_RESOURCE_INFO, 16),
            (&raw mut info2) as *mut c_void,
        )
    };
    check_is(&mut r, "RESOURCE_INFO is idempotent", got, 0);
    check_true(
        &mut r,
        "and names the same resource twice",
        info2.res_handle == res_handle,
    );

    // TRANSFER_TO_HOST of the whole buffer: Linux completes it, and so does this
    // now. The box is the identity rectangle because a 2D resource has one
    // layer and no mips - a caller asking for another shape is asking about
    // something the resource does not have.
    let mut xfer = TransferToHost {
        bo_handle: handle,
        box_: VirtgpuBox {
            w: 64,
            h: 64,
            d: 1, // one layer
            ..VirtgpuBox::default()
        },
        ..TransferToHost::default()
    };
    let got = unsafe {
        ioctl(
            fd,
            ioc_d(NR_TRANSFER_TO_HOST, 44),
            (&raw mut xfer) as *mut c_void,
        )
    };
    check_is(
        &mut r,
        "TRANSFER_TO_HOST completes for a dumb buffer",
        got,
        0,
    );
    // And an unknown handle is still ENOENT rather than a resource conjured for
    // it - the answer the lookup gives before any device work.
    let mut unknown = ResourceInfo {
        bo_handle: 0xffff_ff00,
        ..ResourceInfo::default()
    };
    let got = unsafe {
        ioctl(
            fd,
            ioc_d(NR_RESOURCE_INFO, 16),
            (&raw mut unknown) as *mut c_void,
        )
    };
    check_refused(
        &mut r,
        "RESOURCE_INFO refuses a handle this card never made",
        got,
        Errno::NOENT,
    );

    // ADDFB the buffer: width/height/pitch/bpp/depth/handle -> fb_id.
    let mut fb = FbCmd {
        width: 64,
        height: 64,
        pitch: 256,
        bpp: 32,
        depth: 24,
        handle,
        ..FbCmd::default()
    };
    let got = unsafe { ioctl(fd, ioc_d(NR_ADDFB, 28), (&raw mut fb) as *mut c_void) };
    check_is(&mut r, "ADDFB succeeds", got, 0);
    let fb_id = fb.fb_id;
    check_true(&mut r, "a framebuffer id is returned", fb_id != 0);

    // SETCRTC crtc_id/fb_id, 64x64 at 0,0. The trailing `drm_mode_modeinfo` is
    // left zeroed: this is the legacy door, and a zeroed mode is "no resize" -
    // the kernel's `set_crtc` reads the mode's `hdisplay`/`vdisplay` only to
    // decide whether the caller is asking for a resize (`src/drivers/drm.rs`).
    let mut crtc = ModeCrtc {
        crtc_id: CRTC_ID,
        fb_id,
        x: 0,
        y: 0,
        ..ModeCrtc::default()
    };
    let got = unsafe { ioctl(fd, ioc_d(NR_SETCRTC, 104), (&raw mut crtc) as *mut c_void) };
    check_is(&mut r, "SETCRTC succeeds", got, 0);

    // PAGE_FLIP with the event flag, then read the flip completion back.
    let mut flip = PageFlip {
        crtc_id: CRTC_ID,
        fb_id,
        flags: PAGE_FLIP_EVENT,
        ..PageFlip::default()
    };
    let got = unsafe { ioctl(fd, ioc_d(NR_PAGE_FLIP, 24), (&raw mut flip) as *mut c_void) };
    check_is(&mut r, "PAGE_FLIP succeeds", got, 0);

    let mut ev = [0u8; 32];
    let n = rustix::io::read(&card, &mut ev).unwrap_or(0);
    check_true(&mut r, "a drm event is readable", n >= 8);
    let etype = u32::from_ne_bytes([ev[0], ev[1], ev[2], ev[3]]);
    check_is(
        &mut r,
        "the event is a flip completion",
        etype as isize,
        EVENT_FLIP_COMPLETE as isize,
    );

    // --- the UAPI a GL compositor asks for beyond the flip itself ---

    // GETFB: the geometry and handle ADDFB bound, answered back.
    let mut getfb = FbCmd {
        fb_id,
        ..FbCmd::default()
    };
    let got = unsafe { ioctl(fd, ioc_d(NR_GETFB, 28), (&raw mut getfb) as *mut c_void) };
    check_is(&mut r, "GETFB succeeds", got, 0);
    check_is(&mut r, "GETFB reports the width", getfb.width as isize, 64);
    check_is(
        &mut r,
        "GETFB reports the height",
        getfb.height as isize,
        64,
    );
    check_is(&mut r, "GETFB reports the pitch", getfb.pitch as isize, 256);
    check_is(&mut r, "GETFB reports the bpp", getfb.bpp as isize, 32);
    check_is(
        &mut r,
        "GETFB reports the handle",
        getfb.handle as isize,
        handle as isize,
    );

    // ADDFB2: the fourcc spelling of the same binding - XRGB8888, one
    // plane, no offsets or modifiers - answered with a second fb id.
    let mut fb2 = FbCmd2 {
        width: 64,
        height: 64,
        pixel_format: FORMAT_XRGB8888,
        pitches: [256, 0, 0, 0],
        handles: [handle, 0, 0, 0],
        ..FbCmd2::default()
    };
    let got = unsafe { ioctl(fd, ioc_d(NR_ADDFB2, 104), (&raw mut fb2) as *mut c_void) };
    check_is(&mut r, "ADDFB2 succeeds", got, 0);
    let fb2_id = fb2.fb_id;
    check_true(&mut r, "ADDFB2 returns a framebuffer id", fb2_id != 0);

    // GETFB2: the same binding read back *with its format*, which is the whole
    // reason libdrm has a second call. `struct drm_mode_fb_cmd2` is in and out
    // at once, so the request goes back with `fb_id` in the bytes the answer
    // comes back in.
    //
    // Every field below is one a caller that inherited this framebuffer needs,
    // and the three *unused* planes are checked as zeros: libdrm reads
    // `handles`/`pitches`/`offsets`/`modifier` as four-element arrays whose
    // length is "however many are non-zero", so a kernel that left them alone
    // would be describing a four-plane buffer over addresses the caller never
    // provided - the same class of mistake as the mode name weston's `strdup`
    // once walked off the end of.
    let mut g2 = FbCmd2 {
        fb_id: fb2_id,
        ..FbCmd2::default()
    };
    let got = unsafe { ioctl(fd, ioc_d(NR_GETFB2, 104), (&raw mut g2) as *mut c_void) };
    check_is(&mut r, "GETFB2 succeeds", got, 0);
    check_is(&mut r, "GETFB2 reports the width", g2.width as isize, 64);
    check_is(&mut r, "GETFB2 reports the height", g2.height as isize, 64);
    check_is(
        &mut r,
        "GETFB2 reports the fourcc ADDFB2 bound",
        g2.pixel_format as isize,
        FORMAT_XRGB8888 as isize,
    );
    check_is(
        &mut r,
        "and no modifier flag, since none was carried",
        g2.flags as isize,
        0,
    );
    check_is(
        &mut r,
        "and the handle behind it",
        g2.handles[0] as isize,
        handle as isize,
    );
    check_is(&mut r, "and the pitch", g2.pitches[0] as isize, 256);
    check_is(
        &mut r,
        "and a zero offset, since the buffer starts at its start",
        g2.offsets[0] as isize,
        0,
    );
    check_is(
        &mut r,
        "and a single plane, with the second handle zeroed",
        g2.handles[1] as isize,
        0,
    );
    check_is(
        &mut r,
        "and the fourth pitch zeroed",
        g2.pitches[3] as isize,
        0,
    );
    check_is(
        &mut r,
        "and the second modifier zeroed",
        g2.modifier[1] as isize,
        0,
    );

    // The legacy ADDFB names its layout as bpp plus depth, because its struct
    // predates fourcc codes - so GETFB2 on such a framebuffer is answering with
    // a format the card *derived*. Depth 24 over 32bpp is XRGB8888, the same
    // pair ADDFB2 spells directly, which is what makes the two doors agree.
    let mut leg = FbCmd {
        width: 64,
        height: 64,
        pitch: 256,
        bpp: 32,
        depth: 24,
        handle,
        ..FbCmd::default()
    };
    let got = unsafe { ioctl(fd, ioc_d(NR_ADDFB, 28), (&raw mut leg) as *mut c_void) };
    check_is(&mut r, "a legacy ADDFB succeeds too", got, 0);
    let leg_id = leg.fb_id;
    let mut g3 = FbCmd2 {
        fb_id: leg_id,
        ..FbCmd2::default()
    };
    let got = unsafe { ioctl(fd, ioc_d(NR_GETFB2, 104), (&raw mut g3) as *mut c_void) };
    check_is(&mut r, "GETFB2 reads a legacy framebuffer back", got, 0);
    check_is(
        &mut r,
        "and derives XRGB8888 from 32bpp depth 24, as Linux's legacy mapping does",
        g3.pixel_format as isize,
        FORMAT_XRGB8888 as isize,
    );

    // A framebuffer id the card never made: `EINVAL`, the same answer the
    // legacy GETFB gives, so the two spellings of the question agree.
    let mut g4 = FbCmd2 {
        fb_id: 0xffff,
        ..FbCmd2::default()
    };
    check_refused(
        &mut r,
        "GETFB2 of an unknown framebuffer is EINVAL",
        unsafe { ioctl(fd, ioc_d(NR_GETFB2, 104), (&raw mut g4) as *mut c_void) },
        Errno::INVAL,
    );

    // --- the cursor plane ----------------------------------------------------
    // The plane list is two now, and the second one has to be *described* as the
    // cursor: a compositor finds its cursor plane by asking about each id here and
    // reading the `type` property back. Before this, `GETPLANE` ignored the id it
    // was given and answered with the primary plane for every question, which is
    // why the test asks about both ids and about one that is not there.
    let mut plane_ids = [0u32; 4];
    let mut pl_res = GetPlaneRes {
        plane_id_ptr: plane_ids.as_mut_ptr() as u64,
        count_planes: plane_ids.len() as u32,
    };
    let got = unsafe {
        ioctl(
            fd,
            ioc_d(NR_GETPLANERESOURCES, 16),
            (&raw mut pl_res) as *mut c_void,
        )
    };
    check_is(&mut r, "the plane list is read", got, 0);
    check_is(
        &mut r,
        "two planes, where there used to be one",
        pl_res.count_planes as isize,
        2,
    );
    check_is(
        &mut r,
        "and the second is the cursor plane",
        plane_ids[1] as isize,
        CURSOR_PLANE_ID as isize,
    );

    // The cursor plane's own format list: `ARGB8888` alone, which is the layout a
    // cursor buffer is in and the format this card used to refuse.
    let mut cp_formats = [0u32; 4];
    let mut cp = GetPlane {
        plane_id: CURSOR_PLANE_ID,
        count_format_types: cp_formats.len() as u32,
        format_type_ptr: cp_formats.as_mut_ptr() as u64,
        ..GetPlane::default()
    };
    let got = unsafe { ioctl(fd, ioc_d(NR_GETPLANE, 32), (&raw mut cp) as *mut c_void) };
    check_is(&mut r, "the cursor plane is described", got, 0);
    check_is(
        &mut r,
        "it reports the id it was asked about",
        cp.plane_id as isize,
        CURSOR_PLANE_ID as isize,
    );
    check_is(
        &mut r,
        "one format, not the primary plane's two",
        cp.count_format_types as isize,
        1,
    );
    check_is(
        &mut r,
        "and it is ARGB8888, the layout a cursor carries its alpha in",
        cp_formats[0] as isize,
        FORMAT_ARGB8888 as isize,
    );

    // A plane id the card does not have: refused, rather than answered with
    // whichever plane happened to be looked at first.
    let mut bogus_plane = GetPlane {
        plane_id: 9999,
        ..GetPlane::default()
    };
    check_refused(
        &mut r,
        "GETPLANE of a plane that is not there is ENOENT",
        unsafe {
            ioctl(
                fd,
                ioc_d(NR_GETPLANE, 32),
                (&raw mut bogus_plane) as *mut c_void,
            )
        },
        Errno::NOENT,
    );

    // The cursor requests themselves. `flags` has to name one of the two things -
    // bind or move - and a request that says neither is EINVAL.
    let mut cur = ModeCursor {
        crtc_id: 34,
        ..ModeCursor::default()
    };
    check_refused(
        &mut r,
        "MODE_CURSOR with no flags at all is EINVAL",
        unsafe { ioctl(fd, ioc_d(NR_MODE_CURSOR, 28), (&raw mut cur) as *mut c_void) },
        Errno::INVAL,
    );
    // A bit outside the pair, which is a request this card cannot be asked.
    cur.flags = 0x8;
    check_refused(
        &mut r,
        "and a flag outside the pair is EINVAL too",
        unsafe { ioctl(fd, ioc_d(NR_MODE_CURSOR, 28), (&raw mut cur) as *mut c_void) },
        Errno::INVAL,
    );
    // An image that is not a framebuffer: the `handle` is an fb id here, and the
    // UAPI calls that field "driver specific", so this card's choice *is* the ABI
    // - which is exactly why it needs pinning.
    cur.flags = 0x1; // BO
    cur.width = 64;
    cur.height = 64;
    cur.handle = 0xffff; // no such fb
    check_refused(
        &mut r,
        "MODE_CURSOR binding a framebuffer that does not exist is EINVAL",
        unsafe { ioctl(fd, ioc_d(NR_MODE_CURSOR, 28), (&raw mut cur) as *mut c_void) },
        Errno::INVAL,
    );

    // A real cursor image: ARGB8888 over the same dumb buffer, which is what a
    // compositor makes. It binds, and `GETPLANE` reports it bound.
    let mut cfb = FbCmd2 {
        width: 64,
        height: 64,
        pixel_format: FORMAT_ARGB8888,
        pitches: [256, 0, 0, 0],
        handles: [handle, 0, 0, 0],
        ..FbCmd2::default()
    };
    let got = unsafe { ioctl(fd, ioc_d(NR_ADDFB2, 104), (&raw mut cfb) as *mut c_void) };
    check_is(&mut r, "an ARGB8888 framebuffer binds", got, 0);
    let cursor_fb = cfb.fb_id;
    cur.handle = cursor_fb;
    let got = unsafe { ioctl(fd, ioc_d(NR_MODE_CURSOR, 28), (&raw mut cur) as *mut c_void) };
    check_is(
        &mut r,
        "and MODE_CURSOR binds it to the cursor plane",
        got,
        0,
    );
    let mut cp2 = GetPlane {
        plane_id: CURSOR_PLANE_ID,
        ..GetPlane::default()
    };
    let got = unsafe { ioctl(fd, ioc_d(NR_GETPLANE, 32), (&raw mut cp2) as *mut c_void) };
    check_is(&mut r, "the cursor plane reads back", got, 0);
    check_is(
        &mut r,
        "with that framebuffer bound to it",
        cp2.fb_id as isize,
        cursor_fb as isize,
    );

    // `MODE_CURSOR2` moves it, hotspot and all. Where it landed is read back by
    // `t-drm-props`, which has the property-name lookup this file does not.
    let mut cur2 = ModeCursor2 {
        flags: 0x2, // MOVE
        crtc_id: 34,
        x: 10,
        y: 20,
        hot_x: 1,
        hot_y: 1,
        ..ModeCursor2::default()
    };
    let got = unsafe {
        ioctl(
            fd,
            ioc_d(NR_MODE_CURSOR2, 36),
            (&raw mut cur2) as *mut c_void,
        )
    };
    check_is(&mut r, "MODE_CURSOR2 moves the pointer", got, 0);

    // And a zero-sized image turns it off, which is how libdrm spells it.
    cur.flags = 0x1; // BO
    cur.width = 0;
    cur.height = 0;
    let got = unsafe { ioctl(fd, ioc_d(NR_MODE_CURSOR, 28), (&raw mut cur) as *mut c_void) };
    check_is(&mut r, "a zero-sized cursor turns it off", got, 0);
    let mut cp3 = GetPlane {
        plane_id: CURSOR_PLANE_ID,
        ..GetPlane::default()
    };
    let got = unsafe { ioctl(fd, ioc_d(NR_GETPLANE, 32), (&raw mut cp3) as *mut c_void) };
    check_is(&mut r, "the cursor plane reads back again", got, 0);
    check_is(
        &mut r,
        "with nothing bound, so the pointer is gone",
        cp3.fb_id as isize,
        0,
    );

    // OBJ_GETPROPERTIES on the connector: the two-call list shape, and the
    // CRTC_ID/DPMS pair a compositor drives the connector by.
    let mut props = ObjGetProperties {
        obj_id: 35,            // the connector
        obj_type: 0xc0c0_c0c0, // DRM_MODE_OBJECT_CONNECTOR
        ..ObjGetProperties::default()
    };
    let got = unsafe {
        ioctl(
            fd,
            ioc_d(NR_OBJ_GETPROPERTIES, 32),
            (&raw mut props) as *mut c_void,
        )
    };
    check_is(&mut r, "OBJ_GETPROPERTIES succeeds", got, 0);
    let count = props.count_props;
    // The *set* is the card's business and its own test's: `t-drm-props` pins
    // the six a real connector carries and the shape of each. What this file
    // needs is that the two it reads are there at all, and that the list can be
    // walked - so it asks for the two by index and no more.
    check_true(
        &mut r,
        "the connector carries at least the two properties this file reads",
        count >= 2,
    );
    let mut prop_ids = [0u32; 16];
    let mut prop_values = [0u64; 16];
    let mut props = ObjGetProperties {
        props_ptr: prop_ids.as_mut_ptr() as u64,
        prop_values_ptr: prop_values.as_mut_ptr() as u64,
        // The kernel copies only when the caller's capacity covers the *whole*
        // list (Linux's rule, and this card's: `want >= count`, all or nothing -
        // `src/drivers/drm.rs`'s `obj_get_properties`). The no-std original left
        // this field at the total the sizing call answered and handed two-element
        // arrays, so the copy ran the total's length into the caller's two slots
        // and read the first two back out of an overflowed stack frame. Sized
        // here like the plane's list below, which is the shape a caller uses.
        count_props: prop_ids.len() as u32,
        obj_id: 35,
        obj_type: 0xc0c0_c0c0,
        ..ObjGetProperties::default()
    };
    let got = unsafe {
        ioctl(
            fd,
            ioc_d(NR_OBJ_GETPROPERTIES, 32),
            (&raw mut props) as *mut c_void,
        )
    };
    check_is(&mut r, "OBJ_GETPROPERTIES fills the arrays", got, 0);
    check_is(
        &mut r,
        "the connector's first property is CRTC_ID",
        prop_ids[0] as isize,
        70,
    );
    check_is(
        &mut r,
        "the connector is bound to the one CRTC",
        prop_values[0] as isize,
        CRTC_ID as isize,
    );
    check_is(
        &mut r,
        "the connector's second property is DPMS",
        prop_ids[1] as isize,
        71,
    );
    check_is(
        &mut r,
        "the connector's DPMS mode is on",
        prop_values[1] as isize,
        0,
    );

    // OBJ_GETPROPERTIES on the plane: the `type` property that classifies it
    // - what weston picks its primary plane by - and the frame properties an
    // atomic commit writes (`FB_ID`, the two rectangles, the blend mode).
    // The list is sized from the count the first call answered, which is the
    // two-call contract every KMS list ioctl follows.
    let mut pprops = ObjGetProperties {
        obj_id: 36,            // the plane
        obj_type: 0xeeee_eeee, // DRM_MODE_OBJECT_PLANE
        ..ObjGetProperties::default()
    };
    let got = unsafe {
        ioctl(
            fd,
            ioc_d(NR_OBJ_GETPROPERTIES, 32),
            (&raw mut pprops) as *mut c_void,
        )
    };
    check_is(&mut r, "the plane carries properties", got, 0);
    let plane_count = pprops.count_props as usize;
    check_true(
        &mut r,
        "the plane carries a frame's worth of properties",
        plane_count >= 5,
    );
    let mut pids = [0u32; 16];
    let mut pvals = [0u64; 16];
    let mut pprops = ObjGetProperties {
        props_ptr: pids.as_mut_ptr() as u64,
        prop_values_ptr: pvals.as_mut_ptr() as u64,
        count_props: pids.len() as u32,
        obj_id: 36,
        obj_type: 0xeeee_eeee,
        ..ObjGetProperties::default()
    };
    let got = unsafe {
        ioctl(
            fd,
            ioc_d(NR_OBJ_GETPROPERTIES, 32),
            (&raw mut pprops) as *mut c_void,
        )
    };
    check_is(&mut r, "the plane's properties fill", got, 0);
    check_is(
        &mut r,
        "the plane's property is its type",
        pids[0] as isize,
        73,
    );
    check_is(
        &mut r,
        "the plane is a primary surface",
        pvals[0] as isize,
        1,
    );
    let seen = &pids[..plane_count.min(16)];
    check_true(
        &mut r,
        "the plane carries FB_ID and the source rectangle",
        seen.contains(&75) && seen.contains(&79),
    );

    // GETPROPERTY: weston finds its properties by *name*, so the id the
    // connector's list advertised must answer under the name weston asks
    // for - and the name field is at 24 in the 64-byte struct.
    let mut gp = GetProperty {
        prop_id: 70, // CRTC_ID
        ..GetProperty::default()
    };
    let got = unsafe { ioctl(fd, ioc_d(NR_GETPROPERTY, 64), (&raw mut gp) as *mut c_void) };
    check_is(&mut r, "GETPROPERTY succeeds", got, 0);
    check_true(
        &mut r,
        "GETPROPERTY names the property CRTC_ID",
        &gp.name[..7] == b"CRTC_ID",
    );

    // VERSION: the two-call shape libdrm's `drmGetVersion` drives the request
    // in, and the reason a card cannot answer it however it likes. The first
    // call brings a null pointer and a zero length and asks how long the name
    // is; the caller then allocates exactly that many bytes *plus one*, hands
    // the block back with the same length in the field - where it means "this
    // much room" - and after the second call writes its own terminator at
    // `name[name_len]`. So an answer that outgrows the first one, or a name
    // written into the pointer field instead of through the pointer, puts that
    // terminator outside the caller's block: weston died in libdrm, on a write
    // to a page nobody had mapped, over exactly this.
    let mut ver = Version::default();
    let got = unsafe { ioctl(fd, ioc_d(NR_VERSION, 64), (&raw mut ver) as *mut c_void) };
    check_is(&mut r, "VERSION sizing call succeeds", got, 0);
    let name_len = ver.name_len as usize;
    check_true(&mut r, "VERSION answers a name length", name_len > 0);
    check_true(
        &mut r,
        "VERSION answers no date or description",
        ver.date_len == 0 && ver.desc_len == 0,
    );

    let mut block = [0u8; 64];
    let block_ptr = block.as_mut_ptr() as usize;
    check_true(
        &mut r,
        "the name fits the block the caller would allocate",
        name_len + 1 <= block.len(),
    );
    ver.name = block_ptr as u64;
    let got = unsafe { ioctl(fd, ioc_d(NR_VERSION, 64), (&raw mut ver) as *mut c_void) };
    check_is(&mut r, "VERSION fill call succeeds", got, 0);
    let name_len2 = ver.name_len as usize;
    check_true(
        &mut r,
        "VERSION does not outgrow the room the caller brought",
        name_len2 <= name_len,
    );
    // The name is the *driver's*, as Linux answers it: `virtio_gpu` for the
    // card this machine has, not a private spelling. Mesa's renderer choice is
    // keyed on this string (`NOTES.md` §7.4), so a name no caller's table has is
    // a card a compositor has no driver for.
    check_true(
        &mut r,
        "VERSION wrote the name through the pointer, not over it",
        &block[..name_len2] == b"virtio_gpu" && ver.name == block_ptr as u64,
    );

    // SET_VERSION: request the 1.4 KMS interface, read the driver's 1.1.
    let mut dver = SetVersion {
        drm_di_major: 1,
        drm_di_minor: 4,
        ..SetVersion::default()
    };
    let got = unsafe {
        ioctl(
            fd,
            ioc_d(NR_SET_VERSION, 16),
            (&raw mut dver) as *mut c_void,
        )
    };
    check_is(&mut r, "SET_VERSION succeeds", got, 0);
    check_is(
        &mut r,
        "SET_VERSION confirms the interface",
        dver.drm_di_major as isize,
        1,
    );
    check_is(
        &mut r,
        "SET_VERSION reports the driver",
        dver.drm_dd_major as isize,
        1,
    );

    // The magic handshake, as a master authenticating itself. The master is
    // taken first because Linux's `AUTH_MAGIC` is *the master's* operation: a
    // card with no master refuses it `EACCES` rather than quietly letting a
    // random descriptor mint an authorisation for itself. `SET_MASTER` is `_IO`
    // - no argument, no size - so the request word carries a zero length and
    // the third ioctl argument is a plain 0, not a pointer.
    check_is(
        &mut r,
        "SET_MASTER succeeds",
        unsafe { ioctl(fd, io_d(NR_SET_MASTER), core::ptr::null_mut()) },
        0,
    );
    let mut magic = Auth::default();
    let got = unsafe { ioctl(fd, ioc_d(NR_GET_MAGIC, 4), (&raw mut magic) as *mut c_void) };
    check_is(&mut r, "GET_MAGIC succeeds", got, 0);
    let token = magic.magic;
    check_true(&mut r, "GET_MAGIC hands out a token", token != 0);
    let mut auth = Auth { magic: token };
    let got = unsafe { ioctl(fd, ioc_d(NR_AUTH_MAGIC, 4), (&raw mut auth) as *mut c_void) };
    check_is(&mut r, "AUTH_MAGIC accepts the token", got, 0);

    // GEM_CLOSE: a handle released once, then refused the second time.
    let mut dumb2 = CreateDumb {
        height: 48,
        width: 48,
        bpp: 32,
        ..CreateDumb::default()
    };
    let got = unsafe {
        ioctl(
            fd,
            ioc_d(NR_CREATE_DUMB, 32),
            (&raw mut dumb2) as *mut c_void,
        )
    };
    check_is(&mut r, "a second dumb buffer is made", got, 0);
    let mut close = GemClose {
        handle: dumb2.handle,
        ..GemClose::default()
    };
    let got = unsafe { ioctl(fd, ioc_d(NR_GEM_CLOSE, 8), (&raw mut close) as *mut c_void) };
    check_is(&mut r, "GEM_CLOSE releases the handle", got, 0);
    let got = unsafe { ioctl(fd, ioc_d(NR_GEM_CLOSE, 8), (&raw mut close) as *mut c_void) };
    check_refused(&mut r, "GEM_CLOSE refuses a dead handle", got, Errno::INVAL);

    // The identity walk a Rust compositor makes before it opens anything, and
    // the one no `/dev` node can answer. `DrmNode::from_path("/dev/dri/card0")`
    // is a `stat` of the node - which is where 226:0 comes from - and then two
    // questions only sysfs answers, either of which a card that fails them is
    // refused for: "the provided file descriptor does not refer to a DRM
    // node". First, whether `/sys/dev/char/226:0/device/drm` exists; then what
    // that directory lists, since its entries are the node *names* of the one
    // GPU, matched by prefix (`card`, `renderD`) to find the render node the
    // compositor goes on to open. niri stopped exactly here, with the node, its
    // `st_rdev` and the class entry all correct, so the chain is pinned as a
    // rule rather than as a spelling: it must resolve, it must be a directory,
    // and it must name the two nodes this machine really has.
    let card_drm = format!("/sys/dev/char/{DRM_MAJOR}:{CARD_MINOR}/device/drm");
    let meta = std::fs::metadata(&card_drm);
    check_is(
        &mut r,
        "the card's number reaches its GPU's drm/",
        if meta.is_ok() { 0 } else { -1 },
        0,
    );
    check_true(
        &mut r,
        "and what it reaches is a directory",
        meta.as_ref().map(|m| m.is_dir()).unwrap_or(false),
    );

    let listing = std::fs::read_dir(&card_drm);
    check_true(&mut r, "the GPU's drm/ lists", listing.is_ok());
    if let Ok(listing) = listing {
        let mut saw_card = false;
        let mut saw_render = false;
        for entry in listing.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name == "card0" {
                saw_card = true;
            }
            if name == "renderD128" {
                saw_render = true;
            }
        }
        // The names, not just the count: the crate takes the first entry with
        // the type's prefix and then requires `/dev/dri/<name>` to exist, so a
        // listing that named a node this machine does not have would send a
        // compositor after a render node it cannot open.
        check_true(&mut r, "the listing names card0", saw_card);
        check_true(&mut r, "and renderD128", saw_render);
    }
    // The render node's own number walks the same chain to the same place: a
    // compositor that opened the render node is handed 226:128 and asks again.
    // The two `device` links have to be the *same* directory - one card carries
    // both names - or the parent a compositor reads the sibling node's name
    // from is a different GPU than the one it opened.
    let render_drm = format!("/sys/dev/char/{DRM_MAJOR}:{RENDER_MINOR}/device/drm");
    let rmeta = std::fs::metadata(&render_drm);
    check_is(
        &mut r,
        "the render node's number reaches drm/ too",
        if rmeta.is_ok() { 0 } else { -1 },
        0,
    );
    let a = std::fs::metadata(format!("/sys/dev/char/{DRM_MAJOR}:{CARD_MINOR}/device"));
    let b = std::fs::metadata(format!("/sys/dev/char/{DRM_MAJOR}:{RENDER_MINOR}/device"));
    check_true(
        &mut r,
        "both numbers hang from one GPU",
        match (a, b) {
            (Ok(a), Ok(b)) => a.ino() == b.ino(),
            _ => false,
        },
    );

    // The render node: the same card under GBM's allocation door, where a
    // client that is not the master makes its buffers.
    let render = rustix::fs::open("/dev/dri/renderD128", OFlags::RDWR, Mode::empty());
    check_true(&mut r, "/dev/dri/renderD128 opens", render.is_ok());
    if let Ok(render) = render {
        let rfd = render.as_raw_fd();
        let mut rdumb = CreateDumb {
            height: 32,
            width: 32,
            bpp: 32,
            ..CreateDumb::default()
        };
        let got = unsafe {
            ioctl(
                rfd,
                ioc_d(NR_CREATE_DUMB, 32),
                (&raw mut rdumb) as *mut c_void,
            )
        };
        check_is(&mut r, "the render node makes a dumb buffer", got, 0);
        let rhandle = rdumb.handle;
        let rsize = rdumb.size as usize;
        check_true(&mut r, "the render node returns a handle", rhandle >= 1);
        if got == 0 && rhandle >= 1 {
            let mut rmap = MapDumb {
                handle: rhandle,
                ..MapDumb::default()
            };
            let got = unsafe { ioctl(rfd, ioc_d(NR_MAP_DUMB, 16), (&raw mut rmap) as *mut c_void) };
            check_is(&mut r, "the render node maps the buffer", got, 0);
            let m = unsafe {
                rustix::mm::mmap(
                    core::ptr::null_mut(),
                    rsize,
                    ProtFlags::READ | ProtFlags::WRITE,
                    MapFlags::SHARED,
                    &render,
                    rmap.offset,
                )
            };
            check_true(&mut r, "the render node's buffer maps", m.is_ok());
            if let Ok(m) = m {
                unsafe { (m as *mut u32).write_volatile(0xfeed_face) };
                let back = unsafe { (m as *const u32).read_volatile() };
                check_is(
                    &mut r,
                    "the render node's mapping round-trips",
                    back as isize,
                    0xfeed_face,
                );
                let _ = unsafe { rustix::mm::munmap(m, rsize) };
            }
            let mut rclose = GemClose {
                handle: rhandle,
                ..GemClose::default()
            };
            let _ = unsafe {
                ioctl(
                    rfd,
                    ioc_d(NR_GEM_CLOSE, 8),
                    (&raw mut rclose) as *mut c_void,
                )
            };
        }
    }

    // Hand the card back. `SET_MASTER` above took the card's one master and
    // bound it to a console, and the kernel clears that only on `DROP_MASTER`
    // (not when the descriptor closes), so a master left held would be the next
    // DRM program's fixture: the sweep boots several of them in one boot, and
    // `master_ok` refuses a *presentation* request from any description but the
    // authorized holder once the card is console-bound. `t-drm-lease` hands the
    // console back the same way and for the same reason.
    check_is(
        &mut r,
        "DROP_MASTER releases the card",
        unsafe { ioctl(fd, io_d(NR_DROP_MASTER), core::ptr::null_mut()) },
        0,
    );

    r.finish();
}
