//! t-drm-props - the connector and plane properties a compositor reads *before*
//! it decides it can drive this card, and the two blobs they point at.
//!
//! `NOTES.md` §7.4 lists the property families a second compositor family looks
//! up by name. This card publishes the four its hardware can back - `EDID`,
//! `non-desktop`, `max bpc`, `vrr_capable` on the connector, `IN_FORMATS` on the
//! plane - and the shapes here are Linux's as *measured* (`modetest -c -p`
//! against a real card), not as the sources describe them: `non-desktop` and
//! `vrr_capable` are immutable **ranges** with the two values 0 and 1, not bools.
//!
//! The rest of the file is the two blobs and the promise they make together.
//! `EDID` is a real 128-byte EDID 1.4 base block - header, checksum, a name, one
//! detailed timing - and the connector's `mm_width`/`mm_height` are its own
//! centimetre fields times ten. `IN_FORMATS` says which format-and-modifier
//! pairs the plane scans out, and `DRM_CAP_ADDFB2_MODIFIERS` says the same thing
//! in the other direction: the cap is answered 1, so `ADDFB2` must take
//! `DRM_MODE_FB_MODIFIERS` with the linear modifier and must refuse any other.
//! Both halves are checked here, because a cap answered 1 over an `ADDFB2` that
//! refuses every modifier is a caller building a buffer it cannot then bind.
//!
//! # What the move to the std side changed
//!
//! The door, and one record the no-std version built by hand.
//!
//! The ioctl door is `libc::syscall(SYS_ioctl, ...)` through this program's own
//! `ioctl`, and the two card descriptors are `rustix` `OwnedFd`s - `t-drm`'s
//! rule, and load-bearing here for a reason the no-std side could not have: the
//! second descriptor is the *control* for the whole first section, and a number
//! closed by hand is a number whose close is a line someone can drop.
//!
//! Every record is spelled as `drm.h`/`drm_mode.h` lay it out, with the sizes
//! and offsets as compile-time asserts taken from a `cc` probe against the
//! installed uapi headers. Doing so found **a request word and a field offset
//! that were both wrong in the original**:
//!
//! * `struct drm_mode_create_blob` is `sizeof` 16, and `CREATE_BLOB` was called
//!   with 24 in its request word.
//! * `drm_mode_fb_cmd2`'s `modifier[4]` is at offset **72**, not 68: `offsets[4]`
//!   ends at 68 and a `__u64` cannot begin there, so 68..72 is the padding the
//!   uapi carries. The kernel read and wrote the modifier at 68 too, and the
//!   no-std test named its offsets by hand in the same wrong place - so the two
//!   agreed with each other and with nothing else. That is the pair this move
//!   separates: the test now states the uapi's number, and the kernel was moved
//!   to answer it (`src/drivers/drm.rs`, `add_fb2` and `get_fb2`). It was
//!   invisible for as long as only `LINEAR` - which is 0 - was ever named,
//!   because reading the padding instead of the field answers 0 either way; this
//!   file's "refuses a modifier the plane does not list" is the claim that names
//!   a nonzero one and so makes the offset observable.
//!
//! Not made on a host, for `t-drm`'s and `t-drm-lease`'s reason: every claim
//! below is about *this* card's properties and *this* card's blobs, and a host's
//! own `/dev/dri/card0` publishes a different set off a different EDID.

use std::ffi::c_void;
use std::mem::{offset_of, size_of};
use std::os::fd::{AsRawFd, OwnedFd};

use mpos_user_tests::{Report, host_run};
use rustix::fs::{Mode, OFlags};
use rustix::io::Errno;

/// `_IOWR('d', nr, size)` as libdrm builds it: the direction pair at bits 30..31,
/// the argument's size at 16..31, the family byte at 8..15, the request number
/// below that.
const fn ioc_d(nr: u32, size: u32) -> u64 {
    ((3u64) << 30) | ((size as u64) << 16) | ((b'd' as u64) << 8) | (nr as u64)
}

const NR_GET_CAP: u32 = 0x0c;
const NR_SET_CLIENT_CAP: u32 = 0x0d;
const NR_GETCONNECTOR: u32 = 0xa7;
const NR_GETPROPERTY: u32 = 0xaa;
const NR_GETPROPBLOB: u32 = 0xac;
const NR_CREATE_BLOB: u32 = 0xbd;
const NR_DESTROY_BLOB: u32 = 0xbe;
const NR_GETPLANE: u32 = 0xb6;
const NR_MODE_CURSOR2: u32 = 0xbb;
const NR_ADDFB2: u32 = 0xb8;
const NR_OBJ_GETPROPERTIES: u32 = 0xb9;
const NR_OBJ_SETPROPERTY: u32 = 0xba;
const NR_CREATE_DUMB: u32 = 0xb2;

/// The connector, CRTC and planes this card has, and the object-type tags.
const CONNECTOR_ID: u32 = 35;
const PLANE_ID: u32 = 36;
/// The cursor plane, the card's second one.
const CURSOR_PLANE_ID: u32 = 37;
/// The one CRTC - named here for the same reason the others are: the write
/// checks below name an object, and a bare `34` in an argument tells a reader
/// nothing about which door it is aimed at.
const CRTC_ID: u32 = 34;
const OBJECT_CONNECTOR: u32 = 0xc0c0_c0c0;
const OBJECT_CRTC: u32 = 0xcccc_cccc;
const OBJECT_PLANE: u32 = 0xeeee_eeee;
const OBJECT_FB: u32 = 0xfbfb_fbfb;

const DRM_CAP_ADDFB2_MODIFIERS: u64 = 0x10;
const DRM_CAP_ASYNC_PAGE_FLIP: u64 = 0x07;
const DRM_CAP_DUMB_BUFFER: u64 = 0x1;

/// `DRM_MODE_PROP_*`, on the bit numbers `drm_mode.h` gives them.
const PROP_RANGE: u32 = 1 << 1;
const PROP_ENUM: u32 = 1 << 3;
const PROP_BLOB: u32 = 1 << 4;
const PROP_IMMUTABLE: u32 = 1 << 2;
/// `DRM_MODE_PROP_OBJECT` (`DRM_MODE_PROP_TYPE(n) = n << 6` with n = 1).
const PROP_OBJECT: u32 = 1 << 6;
/// `DRM_MODE_PROP_ATOMIC`: the flag that says a property belongs to an atomic
/// commit - and the one that decides who may *see* it, since a client that has
/// not set `DRM_CLIENT_CAP_ATOMIC` is not shown the flagged properties in a list.
const PROP_ATOMIC: u32 = 1 << 31;
/// `DRM_CLIENT_CAP_ATOMIC`, the capability number in `drm.h`.
const CLIENT_CAP_ATOMIC: u64 = 3;
/// `DRM_CLIENT_CAP_CURSOR_PLANE_HOTSPOT`, the cap that makes a virtualized card's
/// cursor plane publish its hotspot properties (`drm.h`). The cursor plane's
/// `HOTSPOT_X`/`HOTSPOT_Y` are enumerated only for a client that has set it.
const CLIENT_CAP_CURSOR_PLANE_HOTSPOT: u64 = 6;
/// `PROP_PLANE_FB_ID` and `PROP_CRTC_ID`: this card's numbering for the two
/// atomic properties the pin below looks for (`src/drivers/drm.rs`). The *name*
/// is the ABI and the id is this card's, so each is used only after the
/// assertion that the list's id and the name lookup's id are the same one.
const PROP_PLANE_FB_ID: u32 = 75;
const PROP_CRTC_ID: u32 = 70;
/// `DRM_MODE_FB_MODIFIERS`, which enables the `modifier[]` array of an `ADDFB2`.
const FB_MODIFIERS: u32 = 1 << 1;
/// `DRM_FORMAT_XRGB8888`, the layout the guest test's dumb buffer is in.
const FORMAT_XRGB8888: u32 = 0x3432_5258;
/// `DRM_FORMAT_ARGB8888`, the layout a cursor buffer is in.
const FORMAT_ARGB8888: u32 = 0x3432_5241;
/// `DRM_FORMAT_MOD_LINEAR`: the only modifier a dumb buffer is ever in.
const FORMAT_MOD_LINEAR: u64 = 0;
/// A modifier from a vendor namespace this card has never heard of - vendor
/// 0x01's, with the value 0, so it is `0x0100_0000_0000_0000`: a real-shaped
/// modifier from `drm_fourcc.h`'s vendor field, and one this plane does not
/// list.
///
/// The zero in its low 32 bits is the point. A kernel that read `modifier[0]`
/// four bytes early - at 68, which is the padding after `offsets[4]` - would
/// answer the padding plus this value's *low* half, and that is 0, so it would
/// take a modifier it cannot scan out and call it `LINEAR`. With a nonzero low
/// half the refusal comes out the same either way and the offset stays
/// invisible, which is exactly why this is 0 in the low half and not 1.
const FORMAT_MOD_BOGUS: u64 = 0x0100_0000_0000_0000;

/// `struct drm_mode_modeinfo`, measured at 68 bytes against the installed uapi:
/// the mode the connector hands back, carried into a blob whole.
const MODEINFO_BYTES: usize = 68;

// --- the records -----------------------------------------------------------------
//
// Spelled as `drm_mode.h` lays them out, with the sizes and offsets this host's
// headers report as compile-time asserts. Neither library binds these structs,
// so this is the only place the shape is written down - and the request word
// below each call is `size_of`, which is the same length the kernel's own
// `arg_shape` table validates against.

/// `struct drm_mode_get_property`, `sizeof` 64.
#[repr(C)]
#[derive(Default, Clone, Copy)]
struct GetProperty {
    values_ptr: u64,
    enum_blob_ptr: u64,
    prop_id: u32,
    flags: u32,
    name: [u8; 32],
    count_values: u32,
    count_enum_blobs: u32,
}

/// `struct drm_mode_obj_get_properties`, `sizeof` 32 - the two `__u64` pointers
/// round the trailing `__u32`s up past the 28 a hand count gives.
#[repr(C)]
#[derive(Default)]
struct ObjGetProperties {
    props_ptr: u64,
    prop_values_ptr: u64,
    count_props: u32,
    obj_id: u32,
    obj_type: u32,
}

/// `struct drm_mode_get_blob`, `sizeof` 16.
#[repr(C)]
#[derive(Default)]
struct GetBlob {
    blob_id: u32,
    length: u32,
    data: u64,
}

/// `struct drm_mode_create_blob`, `sizeof` 16.
#[repr(C)]
#[derive(Default)]
struct CreateBlob {
    data: u64,
    length: u32,
    blob_id: u32,
}

/// `struct drm_mode_destroy_blob`, `sizeof` 4.
#[repr(C)]
#[derive(Default)]
struct DestroyBlob {
    blob_id: u32,
}

/// `struct drm_mode_obj_set_property`, `sizeof` 24.
#[repr(C)]
#[derive(Default)]
struct ObjSetProperty {
    value: u64,
    prop_id: u32,
    obj_id: u32,
    obj_type: u32,
}

/// `struct drm_mode_get_connector`, `sizeof` 80.
#[repr(C)]
#[derive(Default)]
struct GetConnector {
    encoders_ptr: u64,
    modes_ptr: u64,
    props_ptr: u64,
    prop_values_ptr: u64,
    count_modes: u32,
    count_props: u32,
    count_encoders: u32,
    encoder_id: u32,
    connector_id: u32,
    connector_type: u32,
    connector_type_id: u32,
    connection: u32,
    mm_width: u32,
    mm_height: u32,
    subpixel: u32,
}

/// `struct drm_mode_get_plane`, `sizeof` 32.
#[repr(C)]
#[derive(Default)]
struct GetPlane {
    plane_id: u32,
    crtc_id: u32,
    fb_id: u32,
    possible_crtcs: u32,
    gamma_size: u32,
    count_format_types: u32,
    format_type_ptr: u64,
}

/// `struct drm_mode_cursor2`, `sizeof` 36.
#[repr(C)]
#[derive(Default)]
struct Cursor2 {
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

/// `struct drm_mode_fb_cmd2`, `sizeof` 104. `modifier` is at 72 and not 68:
/// `offsets[4]` ends at 68 and the uapi pads to the `__u64` boundary.
#[repr(C)]
#[derive(Default, Clone, Copy)]
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

/// `struct drm_mode_create_dumb`, `sizeof` 32.
#[repr(C)]
#[derive(Default)]
struct CreateDumb {
    height: u32,
    width: u32,
    bpp: u32,
    flags: u32,
    handle: u32,
    pitch: u32,
    size: u64,
}

/// `struct drm_get_cap` / `struct drm_set_client_cap`, both `sizeof` 16.
#[repr(C)]
#[derive(Default)]
struct Cap {
    capability: u64,
    value: u64,
}

const _: () = assert!(size_of::<GetProperty>() == 64);
const _: () = assert!(offset_of!(GetProperty, values_ptr) == 0);
const _: () = assert!(offset_of!(GetProperty, prop_id) == 16);
const _: () = assert!(offset_of!(GetProperty, flags) == 20);
const _: () = assert!(offset_of!(GetProperty, name) == 24);
const _: () = assert!(offset_of!(GetProperty, count_values) == 56);
const _: () = assert!(offset_of!(GetProperty, count_enum_blobs) == 60);

const _: () = assert!(size_of::<ObjGetProperties>() == 32);
const _: () = assert!(offset_of!(ObjGetProperties, props_ptr) == 0);
const _: () = assert!(offset_of!(ObjGetProperties, prop_values_ptr) == 8);
const _: () = assert!(offset_of!(ObjGetProperties, count_props) == 16);
const _: () = assert!(offset_of!(ObjGetProperties, obj_id) == 20);
const _: () = assert!(offset_of!(ObjGetProperties, obj_type) == 24);

const _: () = assert!(size_of::<GetBlob>() == 16);
const _: () = assert!(offset_of!(GetBlob, blob_id) == 0);
const _: () = assert!(offset_of!(GetBlob, length) == 4);
const _: () = assert!(offset_of!(GetBlob, data) == 8);

const _: () = assert!(size_of::<CreateBlob>() == 16);
const _: () = assert!(offset_of!(CreateBlob, data) == 0);
const _: () = assert!(offset_of!(CreateBlob, length) == 8);
const _: () = assert!(offset_of!(CreateBlob, blob_id) == 12);

const _: () = assert!(size_of::<DestroyBlob>() == 4);
const _: () = assert!(size_of::<ObjSetProperty>() == 24);
const _: () = assert!(offset_of!(ObjSetProperty, value) == 0);
const _: () = assert!(offset_of!(ObjSetProperty, prop_id) == 8);
const _: () = assert!(offset_of!(ObjSetProperty, obj_id) == 12);
const _: () = assert!(offset_of!(ObjSetProperty, obj_type) == 16);

const _: () = assert!(size_of::<GetConnector>() == 80);
const _: () = assert!(offset_of!(GetConnector, modes_ptr) == 8);
const _: () = assert!(offset_of!(GetConnector, props_ptr) == 16);
const _: () = assert!(offset_of!(GetConnector, prop_values_ptr) == 24);
const _: () = assert!(offset_of!(GetConnector, count_modes) == 32);
const _: () = assert!(offset_of!(GetConnector, count_props) == 36);
const _: () = assert!(offset_of!(GetConnector, connector_id) == 48);
const _: () = assert!(offset_of!(GetConnector, mm_width) == 64);
const _: () = assert!(offset_of!(GetConnector, mm_height) == 68);
const _: () = assert!(offset_of!(GetConnector, subpixel) == 72);

const _: () = assert!(size_of::<GetPlane>() == 32);
const _: () = assert!(offset_of!(GetPlane, plane_id) == 0);
const _: () = assert!(offset_of!(GetPlane, count_format_types) == 20);
const _: () = assert!(offset_of!(GetPlane, format_type_ptr) == 24);

const _: () = assert!(size_of::<Cursor2>() == 36);
const _: () = assert!(offset_of!(Cursor2, flags) == 0);
const _: () = assert!(offset_of!(Cursor2, crtc_id) == 4);
const _: () = assert!(offset_of!(Cursor2, x) == 8);
const _: () = assert!(offset_of!(Cursor2, y) == 12);
const _: () = assert!(offset_of!(Cursor2, hot_x) == 28);
const _: () = assert!(offset_of!(Cursor2, hot_y) == 32);

const _: () = assert!(size_of::<FbCmd2>() == 104);
const _: () = assert!(offset_of!(FbCmd2, fb_id) == 0);
const _: () = assert!(offset_of!(FbCmd2, pixel_format) == 12);
const _: () = assert!(offset_of!(FbCmd2, flags) == 16);
const _: () = assert!(offset_of!(FbCmd2, handles) == 20);
const _: () = assert!(offset_of!(FbCmd2, pitches) == 36);
const _: () = assert!(offset_of!(FbCmd2, offsets) == 52);
const _: () = assert!(offset_of!(FbCmd2, modifier) == 72);

const _: () = assert!(size_of::<CreateDumb>() == 32);
const _: () = assert!(offset_of!(CreateDumb, height) == 0);
const _: () = assert!(offset_of!(CreateDumb, width) == 4);
const _: () = assert!(offset_of!(CreateDumb, bpp) == 8);
const _: () = assert!(offset_of!(CreateDumb, handle) == 16);
const _: () = assert!(offset_of!(CreateDumb, pitch) == 20);

const _: () = assert!(size_of::<Cap>() == 16);
const _: () = assert!(offset_of!(Cap, capability) == 0);
const _: () = assert!(offset_of!(Cap, value) == 8);

// --- the door --------------------------------------------------------------------

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

/// A claim that a request was refused with a particular errno.
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

// --- the property readers ---------------------------------------------------------

/// One `GETPROPERTY`: the flags, the name, the value list and the enums.
struct Property {
    flags: u32,
    name: [u8; 32],
    values: [u64; 4],
    count_values: u32,
    count_enums: u32,
}

/// The bytes of a property's name up to its terminator.
fn name_of(prop: &Property) -> &[u8] {
    let end = prop.name.iter().position(|b| *b == 0).unwrap_or(32);
    &prop.name[..end]
}

fn property(fd: i32, id: u32) -> Option<Property> {
    let mut values = [0u64; 4];
    let mut arg = GetProperty {
        prop_id: id,
        count_values: values.len() as u32,
        ..GetProperty::default()
    };
    arg.values_ptr = values.as_mut_ptr() as u64;
    let got = unsafe {
        ioctl(
            fd,
            ioc_d(NR_GETPROPERTY, size_of::<GetProperty>() as u32),
            (&raw mut arg) as *mut c_void,
        )
    };
    if got < 0 {
        return None;
    }
    Some(Property {
        flags: arg.flags,
        name: arg.name,
        values,
        count_values: arg.count_values,
        count_enums: arg.count_enum_blobs,
    })
}

/// The property id one of an object's properties has, found by name.
fn property_id(fd: i32, obj_id: u32, obj_type: u32, want: &[u8]) -> Option<(u32, u64)> {
    let mut ids = [0u32; 32];
    let mut values = [0u64; 32];
    let mut arg = ObjGetProperties {
        count_props: ids.len() as u32,
        obj_id,
        obj_type,
        ..ObjGetProperties::default()
    };
    arg.props_ptr = ids.as_mut_ptr() as u64;
    arg.prop_values_ptr = values.as_mut_ptr() as u64;
    let got = unsafe {
        ioctl(
            fd,
            ioc_d(NR_OBJ_GETPROPERTIES, size_of::<ObjGetProperties>() as u32),
            (&raw mut arg) as *mut c_void,
        )
    };
    if got < 0 {
        return None;
    }
    let count = arg.count_props as usize;
    for index in 0..count.min(ids.len()) {
        let prop = property(fd, ids[index])?;
        if name_of(&prop) == want {
            return Some((ids[index], values[index]));
        }
    }
    None
}

/// The property ids one object reports through `OBJ_GETPROPERTIES`, and how
/// many - the *list* door, as against [`property`]'s and [`property_id`]'s
/// by-id door. The cap below changes what exactly one of the two answers, which
/// is why the pin has to look through this one.
///
/// `count_props` is one field doing two jobs: it arrives as the capacity of
/// `ids` and leaves as the total the object has. The kernel copies the whole
/// list only when the capacity is enough for it (`t-drm`'s note on the family),
/// which is why the array is 16 and not 2.
fn prop_list(fd: i32, obj_id: u32, obj_type: u32, ids: &mut [u32]) -> usize {
    let mut values = [0u64; 32];
    let mut arg = ObjGetProperties {
        count_props: ids.len() as u32,
        obj_id,
        obj_type,
        ..ObjGetProperties::default()
    };
    arg.props_ptr = ids.as_mut_ptr() as u64;
    arg.prop_values_ptr = values.as_mut_ptr() as u64;
    let got = unsafe {
        ioctl(
            fd,
            ioc_d(NR_OBJ_GETPROPERTIES, size_of::<ObjGetProperties>() as u32),
            (&raw mut arg) as *mut c_void,
        )
    };
    if got < 0 {
        return 0;
    }
    arg.count_props as usize
}

/// Read a blob whole, answering its bytes.
fn blob(fd: i32, id: u32) -> Option<[u8; 256]> {
    let mut data = [0u8; 256];
    let mut arg = GetBlob {
        blob_id: id,
        length: data.len() as u32,
        ..GetBlob::default()
    };
    arg.data = data.as_mut_ptr() as u64;
    let got = unsafe {
        ioctl(
            fd,
            ioc_d(NR_GETPROPBLOB, size_of::<GetBlob>() as u32),
            (&raw mut arg) as *mut c_void,
        )
    };
    if got < 0 {
        return None;
    }
    if arg.length == 0 || arg.length > data.len() as u32 {
        return None;
    }
    Some(data)
}

/// A blob's length, asked with no data pointer: the sizing call.
fn blob_len(fd: i32, id: u32) -> Option<u32> {
    let mut arg = GetBlob {
        blob_id: id,
        ..GetBlob::default()
    };
    let got = unsafe {
        ioctl(
            fd,
            ioc_d(NR_GETPROPBLOB, size_of::<GetBlob>() as u32),
            (&raw mut arg) as *mut c_void,
        )
    };
    if got < 0 {
        return None;
    }
    Some(arg.length)
}

/// One `OBJ_SETPROPERTY`, through the same `#[repr(C)]` record the rest uses.
fn set_property(fd: i32, value: u64, prop_id: u32, obj_id: u32, obj_type: u32) -> isize {
    let mut arg = ObjSetProperty {
        value,
        prop_id,
        obj_id,
        obj_type,
    };
    unsafe {
        ioctl(
            fd,
            ioc_d(NR_OBJ_SETPROPERTY, size_of::<ObjSetProperty>() as u32),
            (&raw mut arg) as *mut c_void,
        )
    }
}

/// Read a `u32` the way the blob it sits in is laid out: native endianness,
/// unaligned in the sense that a byte array gives no alignment promise.
fn get32(buf: &[u8], off: usize) -> u32 {
    u32::from_ne_bytes([buf[off], buf[off + 1], buf[off + 2], buf[off + 3]])
}

fn get64(buf: &[u8], off: usize) -> u64 {
    u64::from_ne_bytes([
        buf[off],
        buf[off + 1],
        buf[off + 2],
        buf[off + 3],
        buf[off + 4],
        buf[off + 5],
        buf[off + 6],
        buf[off + 7],
    ])
}

fn main() {
    println!("t-drm-props");
    let mut r = Report::new();

    // See the header: every claim below is about this card's own property set
    // and this card's own EDID, so a hosted run makes no claim at all.
    if host_run() {
        println!(
            "note: not made on a host - every claim below is about this kernel's card, \
             whose properties and EDID a host's own /dev/dri/card0 does not publish"
        );
        r.finish();
    }

    let card: OwnedFd = match rustix::fs::open("/dev/dri/card0", OFlags::RDWR, Mode::empty()) {
        Ok(fd) => {
            r.check("the card opens", true);
            fd
        }
        Err(e) => {
            r.check(&format!("the card opens ({e:?})"), false);
            println!("FAILED");
            std::process::exit(1);
        }
    };
    let fd = card.as_raw_fd();
    // No `SET_MASTER`: on this card an *unclaimed* session is served its
    // presentation requests (`master_ok`'s comment says why - a compositor's
    // first commit can arrive before its seat manager's `SET_MASTER`), and a
    // session that has taken the card on a boot with nothing on the framebuffer
    // is bound to a console that does not hold the screen, so every write below
    // would come back `EACCES` before reaching the handler this file is about.

    // --- the cap decides what the list carries -----------------------------------------
    // `DRM_CLIENT_CAP_ATOMIC` is not only a promise about the client. Linux sets
    // `DRM_MODE_PROP_ATOMIC` on the properties an atomic commit is made of and
    // leaves those out of the lists `GETCONNECTOR` and `OBJ_GETPROPERTIES` build
    // for a client that has not asked for them, while `GETPROPERTY` keeps
    // answering for one of them *by id* - what is gated is the enumeration, not
    // the door.
    //
    // All of that is measured on the host's card before it was written here
    // (§7.4): a CRTC reports 6 properties to a description without the cap and 9
    // to one with it, a connector 17 and 18, the difference being the CRTC_ID a
    // compositor binds an output by - and a *second descriptor* of the same card
    // kept its own answer, which is what makes the state the file's.
    //
    // The pin is those facts on this card. It has to run before the cap is set
    // for the rest of the file, because everything below reads the card the way a
    // compositor does - weston 16 has no legacy path left and sets this cap
    // before it looks at a single property.
    let second: OwnedFd = match rustix::fs::open("/dev/dri/card0", OFlags::RDWR, Mode::empty()) {
        Ok(f) => {
            r.check("a second descriptor of the card opens", true);
            f
        }
        Err(e) => {
            r.check(
                &format!("a second descriptor of the card opens ({e:?})"),
                false,
            );
            println!("FAILED");
            std::process::exit(1);
        }
    };
    let fd2 = second.as_raw_fd();

    let mut uncapped_ids = [0u32; 16];
    let uncapped_count = prop_list(fd, PLANE_ID, OBJECT_PLANE, &mut uncapped_ids);
    check_true(
        &mut r,
        "an uncapped client is not shown the plane's FB_ID at all",
        !uncapped_ids[..uncapped_count.min(16)].contains(&PROP_PLANE_FB_ID),
    );
    // It is not *gone*, though: the same id is still answered, and the flag it
    // comes back with is the reason it was missing from the list. This is the
    // half a reader gets wrong from the sentence alone, and why the filter sits
    // in the list builders rather than in the by-id door.
    match property(fd, PROP_PLANE_FB_ID) {
        Some(prop) => check_true(
            &mut r,
            "and by id it is still answered, flagged ATOMIC",
            name_of(&prop) == b"FB_ID" && prop.flags & PROP_ATOMIC != 0,
        ),
        None => check_true(&mut r, "the plane's FB_ID answers GETPROPERTY by id", false),
    }

    let mut cap = Cap {
        capability: CLIENT_CAP_ATOMIC,
        value: 1,
    };
    check_is(
        &mut r,
        "DRM_CLIENT_CAP_ATOMIC is accepted",
        unsafe {
            ioctl(
                fd,
                ioc_d(NR_SET_CLIENT_CAP, size_of::<Cap>() as u32),
                (&raw mut cap) as *mut c_void,
            )
        },
        0,
    );

    let mut capped_ids = [0u32; 16];
    let capped_count = prop_list(fd, PLANE_ID, OBJECT_PLANE, &mut capped_ids);
    check_true(
        &mut r,
        "and now the plane's FB_ID is in the list",
        capped_ids[..capped_count.min(16)].contains(&PROP_PLANE_FB_ID),
    );
    // The id the list carries and the id the *name* lookup gives are the same
    // one, which is what lets the assertions above and below use the number at
    // all: the name is the ABI and the number is this card's.
    check_true(
        &mut r,
        "by the same id the name lookup finds",
        property_id(fd, PLANE_ID, OBJECT_PLANE, b"FB_ID").map(|(id, _)| id)
            == Some(PROP_PLANE_FB_ID),
    );
    // The change is exactly the atomic properties, in both directions: nothing
    // the uncapped list had is missing, and everything it gained is flagged.
    check_true(
        &mut r,
        "the cap takes nothing away from what was already listed",
        uncapped_ids[..uncapped_count.min(16)]
            .iter()
            .all(|id| capped_ids[..capped_count.min(16)].contains(id)),
    );
    let mut gained = 0;
    let mut gained_all_atomic = true;
    for &id in capped_ids[..capped_count.min(16)].iter() {
        if uncapped_ids[..uncapped_count.min(16)].contains(&id) {
            continue;
        }
        gained += 1;
        match property(fd, id) {
            Some(prop) => {
                if prop.flags & PROP_ATOMIC == 0 {
                    gained_all_atomic = false;
                }
            }
            None => gained_all_atomic = false,
        }
    }
    check_true(
        &mut r,
        "and everything it gained is flagged DRM_MODE_PROP_ATOMIC",
        gained > 0 && gained_all_atomic,
    );

    // The control: a second descriptor of the same card, which never set the cap,
    // sees what the first one saw before it did.
    let mut control_ids = [0u32; 16];
    let control_count = prop_list(fd2, PLANE_ID, OBJECT_PLANE, &mut control_ids);
    check_true(
        &mut r,
        "while a second descriptor of the same card is unaffected",
        !control_ids[..control_count.min(16)].contains(&PROP_PLANE_FB_ID),
    );
    // And on the connector, where the property the cap hides is `CRTC_ID` - so a
    // card that kept it from a *capped* client would be a compositor with no
    // output, and one that showed it to an uncapped client would be a list that
    // does not match Linux's.
    let mut conn_capped = [0u32; 16];
    let conn_capped_count = prop_list(fd, CONNECTOR_ID, OBJECT_CONNECTOR, &mut conn_capped);
    let mut conn_uncapped = [0u32; 16];
    let conn_uncapped_count = prop_list(fd2, CONNECTOR_ID, OBJECT_CONNECTOR, &mut conn_uncapped);
    check_true(
        &mut r,
        "the capped client sees the connector's CRTC_ID and the uncapped one does not",
        conn_capped[..conn_capped_count.min(16)].contains(&PROP_CRTC_ID)
            && !conn_uncapped[..conn_uncapped_count.min(16)].contains(&PROP_CRTC_ID),
    );
    drop(second);

    // --- the connector's property list -------------------------------------------------
    //
    // Read through GETCONNECTOR, which is the path libdrm's drmModeGetConnector
    // takes: the ids *and* the values, in the order the card reports them.
    let mut ids = [0u32; 16];
    let mut values = [0u64; 16];
    let mut conn = GetConnector {
        count_props: ids.len() as u32,
        connector_id: CONNECTOR_ID,
        ..GetConnector::default()
    };
    conn.props_ptr = ids.as_mut_ptr() as u64;
    conn.prop_values_ptr = values.as_mut_ptr() as u64;
    check_is(
        &mut r,
        "the connector asks for its properties",
        unsafe {
            ioctl(
                fd,
                ioc_d(NR_GETCONNECTOR, size_of::<GetConnector>() as u32),
                (&raw mut conn) as *mut c_void,
            )
        },
        0,
    );
    let count = conn.count_props as usize;
    check_true(&mut r, "it reports six of them", count == 6);
    let mm_w = conn.mm_width;
    let mm_h = conn.mm_height;
    check_true(
        &mut r,
        "and a physical size in millimetres",
        mm_w > 0 && mm_h > 0,
    );
    check_is(
        &mut r,
        "with the sub-pixel order a real connector reports (unknown, 0)",
        conn.subpixel as isize,
        0,
    );

    // Every property it named has to be queryable by id, or a compositor's walk
    // of the list fails part-way.
    let mut all_queryable = count == 6;
    for index in 0..count.min(16) {
        if property(fd, ids[index]).is_none() {
            all_queryable = false;
        }
    }
    check_true(
        &mut r,
        "every property id in the list answers GETPROPERTY",
        all_queryable,
    );

    // --- the four named properties, with the shapes Linux gives them -------------------
    match property_id(fd, CONNECTOR_ID, OBJECT_CONNECTOR, b"EDID") {
        Some((id, blob_id)) => {
            let prop = property(fd, id).expect("the EDID property");
            check_true(
                &mut r,
                "EDID is a blob and immutable",
                prop.flags & PROP_BLOB != 0 && prop.flags & PROP_IMMUTABLE != 0,
            );
            let bytes = blob(fd, blob_id as u32).expect("the EDID bytes");
            check_true(
                &mut r,
                "and its value names a blob that reads back",
                bytes[..8] == [0x00, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x00],
            );
            check_is(
                &mut r,
                "the EDID is one 128-byte block",
                blob_len(fd, blob_id as u32).unwrap_or(0) as isize,
                128,
            );
            let sum = bytes[..128].iter().fold(0u8, |a, b| a.wrapping_add(*b));
            check_is(
                &mut r,
                "whose bytes add to zero, as an EDID's must",
                sum as isize,
                0,
            );
            check_is(&mut r, "it is EDID 1.4", bytes[18] as isize, 1);
            check_is(&mut r, "revision 4", bytes[19] as isize, 4);
            // The size the connector reported is this block's own centimetres
            // times ten - the relation the measurement found between the two, and
            // the reason `size_mm` is derived from `size_cm` rather than computed
            // separately.
            check_is(
                &mut r,
                "the connector's millimetres are the EDID's centimetres times ten",
                mm_w as isize,
                bytes[21] as isize * 10,
            );
            check_is(
                &mut r,
                "and so is the height",
                mm_h as isize,
                bytes[22] as isize * 10,
            );
            // The name descriptor: tag 0xFC in the third of the four 18-byte
            // descriptors, with printable ASCII after it.
            let name = &bytes[54 + 2 * 18..54 + 3 * 18];
            check_true(
                &mut r,
                "it carries a monitor name descriptor",
                name[3] == 0xfc && name[5].is_ascii_graphic(),
            );
            // And the first descriptor is a *timing*, which is what its first two
            // bytes say: a descriptor opens with two zeroes, a timing with a clock.
            let timing = &bytes[54..72];
            check_true(
                &mut r,
                "and a detailed timing that opens like a timing",
                (timing[0], timing[1]) != (0, 0),
            );
            check_true(
                &mut r,
                "the other two descriptors open like descriptors",
                bytes[54 + 18] == 0
                    && bytes[54 + 19] == 0
                    && bytes[54 + 3 * 18] == 0
                    && bytes[54 + 3 * 18 + 1] == 0,
            );
            // Destroying the id a property holds is a no-op, not a way to take
            // the connector's EDID away: Linux keeps the blob because the
            // property owns a reference.
            let mut destroy = DestroyBlob {
                blob_id: blob_id as u32,
            };
            check_is(
                &mut r,
                "destroying a driver blob's id succeeds",
                unsafe {
                    ioctl(
                        fd,
                        ioc_d(NR_DESTROY_BLOB, size_of::<DestroyBlob>() as u32),
                        (&raw mut destroy) as *mut c_void,
                    )
                },
                0,
            );
            check_true(
                &mut r,
                "and the blob is still there afterwards",
                blob(fd, blob_id as u32).is_some(),
            );
        }
        None => check_true(&mut r, "the connector has an EDID property", false),
    }

    // The already-existing property, so that a card which lost its old answer
    // while gaining new ones is caught here rather than in a compositor.
    match property_id(fd, CONNECTOR_ID, OBJECT_CONNECTOR, b"DPMS") {
        Some((id, _)) => {
            let prop = property(fd, id).expect("the DPMS property");
            check_true(
                &mut r,
                "DPMS is still an enum of four modes",
                prop.flags & PROP_ENUM != 0 && prop.count_enums == 4,
            );
        }
        None => check_true(&mut r, "the connector still has DPMS", false),
    }

    match property_id(fd, CONNECTOR_ID, OBJECT_CONNECTOR, b"non-desktop") {
        Some((id, _)) => {
            let prop = property(fd, id).expect("the non-desktop property");
            check_true(
                &mut r,
                "non-desktop is a range, not a bool",
                prop.flags & PROP_RANGE != 0 && prop.flags & PROP_ENUM == 0,
            );
            check_true(
                &mut r,
                "and it is immutable",
                prop.flags & PROP_IMMUTABLE != 0,
            );
            check_true(
                &mut r,
                "with the two values 0 and 1",
                prop.count_values == 2 && prop.values[0] == 0 && prop.values[1] == 1,
            );
            check_true(
                &mut r,
                "and it is named non-desktop",
                name_of(&prop) == b"non-desktop",
            );
        }
        None => check_true(&mut r, "the connector has non-desktop", false),
    }

    match property_id(fd, CONNECTOR_ID, OBJECT_CONNECTOR, b"vrr_capable") {
        Some((id, value)) => {
            let prop = property(fd, id).expect("the vrr_capable property");
            check_true(
                &mut r,
                "vrr_capable is an immutable range of 0 and 1",
                prop.flags & PROP_RANGE != 0
                    && prop.flags & PROP_IMMUTABLE != 0
                    && prop.count_values == 2
                    && prop.values == [0, 1, 0, 0],
            );
            check_is(
                &mut r,
                "and this display cannot vary its rate, so it reads 0",
                value as isize,
                0,
            );
        }
        None => check_true(&mut r, "the connector has vrr_capable", false),
    }

    match property_id(fd, CONNECTOR_ID, OBJECT_CONNECTOR, b"max bpc") {
        Some((id, value)) => {
            let prop = property(fd, id).expect("the max bpc property");
            check_true(
                &mut r,
                "max bpc is a range of exactly one value",
                prop.flags & PROP_RANGE != 0 && prop.count_values == 2 && prop.values[0] == 8,
            );
            check_is(
                &mut r,
                "the scanout is eight bits per channel",
                value as isize,
                8,
            );
            // A range of one value means a write outside it is refused, which is
            // what keeps "16" from being a success that changes no pixels.
            check_refused(
                &mut r,
                "setting max bpc to 16 is refused",
                set_property(fd, 16, id, CONNECTOR_ID, OBJECT_CONNECTOR),
                Errno::INVAL,
            );
            check_is(
                &mut r,
                "and setting it to what it is succeeds",
                set_property(fd, 8, id, CONNECTOR_ID, OBJECT_CONNECTOR),
                0,
            );
        }
        None => check_true(&mut r, "the connector has max bpc", false),
    }

    // --- a write is what the read-back says --------------------------------------------
    // `max bpc` above refuses a value its range does not hold, and the rule that
    // makes that worth doing applies to every property that advertises a shape:
    // a write outside it is refused, and an accepted one is what a caller reading
    // the property back is told. Two properties broke the second half in
    // different ways before this.
    match property_id(fd, PLANE_ID, OBJECT_PLANE, b"pixel blend mode") {
        Some((id, value)) => {
            check_is(
                &mut r,
                "pixel blend mode starts pre-multiplied",
                value as isize,
                0,
            );
            check_is(
                &mut r,
                "writing Coverage succeeds",
                set_property(fd, 1, id, PLANE_ID, OBJECT_PLANE),
                0,
            );
            let (_, back) = property_id(fd, PLANE_ID, OBJECT_PLANE, b"pixel blend mode")
                .expect("the property is still in the plane's list");
            check_is(
                &mut r,
                "and the read-back says Coverage, not the constant it used to",
                back as isize,
                1,
            );
            check_refused(
                &mut r,
                "while a value the enum does not have is refused",
                set_property(fd, 2, id, PLANE_ID, OBJECT_PLANE),
                Errno::INVAL,
            );
            // Put it back, so the state the rest of this file reads is the one it
            // started with.
            set_property(fd, 0, id, PLANE_ID, OBJECT_PLANE);
        }
        None => check_true(&mut r, "the plane has pixel blend mode", false),
    }

    // The other way: a value the property's own enum has no name for used to be
    // *recorded and handed back*, which is a property contradicting itself rather
    // than merely ignoring the write.
    match property_id(fd, CONNECTOR_ID, OBJECT_CONNECTOR, b"DPMS") {
        Some((id, value)) => {
            check_is(&mut r, "DPMS starts On", value as isize, 0);
            check_refused(
                &mut r,
                "a mode the enum does not have is refused",
                set_property(fd, 9, id, CONNECTOR_ID, OBJECT_CONNECTOR),
                Errno::INVAL,
            );
            check_is(
                &mut r,
                "while Off, which it does have, is taken",
                set_property(fd, 3, id, CONNECTOR_ID, OBJECT_CONNECTOR),
                0,
            );
            let (_, back) = property_id(fd, CONNECTOR_ID, OBJECT_CONNECTOR, b"DPMS")
                .expect("the property is still in the connector's list");
            check_is(&mut r, "and reads back as Off", back as isize, 3);
            set_property(fd, 0, id, CONNECTOR_ID, OBJECT_CONNECTOR);
        }
        None => check_true(&mut r, "the connector has DPMS", false),
    }

    // And the CRTC's own one-bit range, which took any number at all.
    match property_id(fd, CRTC_ID, OBJECT_CRTC, b"ACTIVE") {
        Some((id, _)) => {
            check_refused(
                &mut r,
                "ACTIVE outside its two values is refused",
                set_property(fd, 2, id, CRTC_ID, OBJECT_CRTC),
                Errno::INVAL,
            );
            check_is(
                &mut r,
                "while 0 is taken",
                set_property(fd, 0, id, CRTC_ID, OBJECT_CRTC),
                0,
            );
            let (_, back) = property_id(fd, CRTC_ID, OBJECT_CRTC, b"ACTIVE")
                .expect("the property is still in the CRTC's list");
            check_is(&mut r, "and reads back as 0", back as isize, 0);
            set_property(fd, 1, id, CRTC_ID, OBJECT_CRTC);
        }
        None => check_true(&mut r, "the CRTC has ACTIVE", false),
    }

    // --- MODE_ID, and the two doors agreeing about what a mode is ----------------------
    // `MODE_ID` is a blob id, and the *commit* path refuses one this card cannot
    // show (the same `mode_fits` the write path now uses). This door recorded any
    // number at all, so a caller could write an id naming nothing and read it back
    // as the CRTC's current mode - a value the card had just called its own state,
    // and one `GETPROPBLOB` answers `EINVAL` for.
    //
    // The mode to name is the card's *own*, read rather than assumed: a real
    // card's `MODE_ID` is a blob holding the mode its connector advertises.
    let mut mode = [0u8; MODEINFO_BYTES];
    let mut conn_mode = GetConnector {
        count_modes: 1,
        connector_id: CONNECTOR_ID,
        ..GetConnector::default()
    };
    conn_mode.modes_ptr = mode.as_mut_ptr() as u64;
    check_is(
        &mut r,
        "the connector hands back its mode",
        unsafe {
            ioctl(
                fd,
                ioc_d(NR_GETCONNECTOR, size_of::<GetConnector>() as u32),
                (&raw mut conn_mode) as *mut c_void,
            )
        },
        0,
    );
    check_is(
        &mut r,
        "one mode, which the blob below is made of",
        conn_mode.count_modes as isize,
        1,
    );
    let mut blob_arg = CreateBlob {
        length: mode.len() as u32,
        ..CreateBlob::default()
    };
    blob_arg.data = mode.as_ptr() as u64;
    check_is(
        &mut r,
        "and it can be stored as a blob",
        unsafe {
            ioctl(
                fd,
                ioc_d(NR_CREATE_BLOB, size_of::<CreateBlob>() as u32),
                (&raw mut blob_arg) as *mut c_void,
            )
        },
        0,
    );
    let mode_blob = blob_arg.blob_id;
    check_true(&mut r, "with an id", mode_blob != 0);

    match property_id(fd, CRTC_ID, OBJECT_CRTC, b"MODE_ID") {
        Some((id, _)) => {
            // An id naming nothing: what the commit path already refuses, and so
            // what this door must refuse too.
            check_refused(
                &mut r,
                "a mode id naming no blob is refused",
                set_property(fd, 0xdead_beef, id, CRTC_ID, OBJECT_CRTC),
                Errno::INVAL,
            );
            // The card's own mode, which it has just handed out.
            check_is(
                &mut r,
                "while the card's own mode is taken",
                set_property(fd, mode_blob as u64, id, CRTC_ID, OBJECT_CRTC),
                0,
            );
            let (_, back) = property_id(fd, CRTC_ID, OBJECT_CRTC, b"MODE_ID")
                .expect("the property is still in the CRTC's list");
            check_is(
                &mut r,
                "and reads back as that id",
                back as isize,
                mode_blob as isize,
            );
            // 0 is "no mode", which is always allowed - the shape `FB_ID` 0 has
            // on the cursor plane, and what a CRTC that has been turned off
            // reports.
            check_is(
                &mut r,
                "as is 0, which is no mode at all",
                set_property(fd, 0, id, CRTC_ID, OBJECT_CRTC),
                0,
            );
        }
        None => check_true(&mut r, "the CRTC has MODE_ID", false),
    }

    // --- the cursor plane's type, and where the pointer is -----------------------------
    // `type` is how a compositor tells a cursor plane from an overlay, and the
    // difference is the whole reason the plane exists: with `type` = Cursor it
    // drives the pointer through the kernel, and without it, it draws the pointer
    // into its own framebuffer. Read by name rather than by id, because the id is
    // this card's numbering and the name is the ABI.
    match property_id(fd, CURSOR_PLANE_ID, OBJECT_PLANE, b"type") {
        Some((id, value)) => {
            let prop = property(fd, id).expect("the cursor plane's type property");
            check_true(
                &mut r,
                "the second plane carries an enumerated type",
                prop.flags & PROP_ENUM != 0,
            );
            check_is(
                &mut r,
                "and its type is Cursor, which is how a compositor finds it",
                value as isize,
                2,
            );
        }
        None => check_true(&mut r, "the cursor plane carries a type property", false),
    }

    // --- the cursor plane's rectangle --------------------------------------------------
    // A compositor builds one atomic state per plane and adds `SRC_X`/`SRC_Y`/
    // `SRC_W`/`SRC_H` and `CRTC_W`/`CRTC_H` to *every* one of them, whether or
    // not the plane has them: a handle of 0 reaches the card as property 0 and
    // takes the whole commit with it. So a cursor plane without them is a plane
    // no compositor can put a cursor on, which is a pointer that moves and
    // clicks and is never seen.
    //
    // The values are the identity for whatever image is bound: this card's
    // cursor has no scaler and no crop, so the image is drawn whole at its own
    // size, and that is what the rectangle says. With no image bound they are
    // all zero, which is the same statement.
    for (name, label) in [
        (&b"SRC_X"[..], "the cursor plane has a source offset in x"),
        (b"SRC_Y", "the cursor plane has a source offset in y"),
        (b"SRC_W", "the cursor plane has a source width"),
        (b"SRC_H", "the cursor plane has a source height"),
        (b"CRTC_W", "the cursor plane has a destination width"),
        (b"CRTC_H", "the cursor plane has a destination height"),
    ] {
        let found = property_id(fd, CURSOR_PLANE_ID, OBJECT_PLANE, name);
        check_true(&mut r, label, found.is_some());
    }
    // `CRTC_X`/`CRTC_Y` are the pointer itself, and they are acted on rather than
    // recorded - the same two the legacy `MODE_CURSOR` moves.
    check_true(
        &mut r,
        "the cursor plane's CRTC_X reads back where the pointer is",
        property_id(fd, CURSOR_PLANE_ID, OBJECT_PLANE, b"CRTC_X").is_some(),
    );

    // --- the cursor plane's hotspot, behind its cap ------------------------------------
    // A virtualized card's cursor plane carries `HOTSPOT_X`/`HOTSPOT_Y`, and a
    // client sees them only once it has set `DRM_CLIENT_CAP_CURSOR_PLANE_HOTSPOT`.
    // Before that cap this cursor plane must *not* list them - the same per-client
    // gating `DRM_CLIENT_CAP_ATOMIC` gives the atomic properties, and the reason a
    // compositor that never asked is not handed a property it will not set.
    let mut pre_hotspot_ids = [0u32; 16];
    let pre_hotspot_count = prop_list(fd, CURSOR_PLANE_ID, OBJECT_PLANE, &mut pre_hotspot_ids);
    let pre_has_hotspot = ["HOTSPOT_X", "HOTSPOT_Y"]
        .iter()
        .any(|name| property_id(fd, CURSOR_PLANE_ID, OBJECT_PLANE, name.as_bytes()).is_some());
    check_true(
        &mut r,
        "the cursor plane hides its hotspot before the cap is set",
        !pre_has_hotspot,
    );

    let mut hot_cap = Cap {
        capability: CLIENT_CAP_CURSOR_PLANE_HOTSPOT,
        value: 1,
    };
    check_is(
        &mut r,
        "DRM_CLIENT_CAP_CURSOR_PLANE_HOTSPOT is accepted",
        unsafe {
            ioctl(
                fd,
                ioc_d(NR_SET_CLIENT_CAP, size_of::<Cap>() as u32),
                (&raw mut hot_cap) as *mut c_void,
            )
        },
        0,
    );

    let x_id = property_id(fd, CURSOR_PLANE_ID, OBJECT_PLANE, b"HOTSPOT_X");
    let y_id = property_id(fd, CURSOR_PLANE_ID, OBJECT_PLANE, b"HOTSPOT_Y");
    check_true(
        &mut r,
        "and now the cursor plane lists HOTSPOT_X",
        x_id.is_some(),
    );
    check_true(&mut r, "and HOTSPOT_Y", y_id.is_some());
    // The cap adds to the list rather than replacing it: nothing the uncapped view
    // showed is missing now (the same direction the ATOMIC cap is pinned on).
    if let Some((hx, _)) = x_id {
        let mut post_ids = [0u32; 16];
        let post_count = prop_list(fd, CURSOR_PLANE_ID, OBJECT_PLANE, &mut post_ids);
        check_true(
            &mut r,
            "the capped list still carries every id the uncapped one had",
            pre_hotspot_ids[..pre_hotspot_count.min(16)]
                .iter()
                .all(|id| post_ids[..post_count.min(16)].contains(id)),
        );
        // A write to the hotspot reads back what was written - an accepted property
        // whose value the read path reports, not one stored nowhere.
        let _ = set_property(fd, 4, hx, CURSOR_PLANE_ID, OBJECT_PLANE);
        check_is(
            &mut r,
            "and a HOTSPOT_X write reads back",
            property_id(fd, CURSOR_PLANE_ID, OBJECT_PLANE, b"HOTSPOT_X")
                .map(|(_, v)| v as i32)
                .unwrap_or(-1) as isize,
            4,
        );
    }

    // --- the object-typed properties carry their one value -----------------------------
    // An `OBJECT` property's value is the *kind* of object it points at, and it
    // has exactly one of them: Linux creates the three below through
    // `drm_property_create_object`, so `count_values` is 1 and `values[0]` is
    // `DRM_MODE_OBJECT_CRTC` (`0xcccccccc`) or `_FB` (`0xfbfbfbfb`). Measured
    // through libdrm on this host's card before it was written: the connector's
    // `CRTC_ID` and both planes' `CRTC_ID` answer `0xcccccccc`, the planes'
    // `FB_ID` answers `0xfbfbfbfb`, one value each.
    //
    // This is not a cosmetic count: a caller reads `values[0]` as soon as the
    // flag is set - drm-rs builds the `ValueType` a property reports that way -
    // so a plane advertising `FB_ID` with an empty list is not a smaller answer,
    // it is a panic in the caller's hands. That is what it was: niri died with
    // "index out of bounds: the len is 0 but the index is 0" in
    // `drm-0.14.1/src/control/mod.rs:497` one line after the primary node was
    // added.
    for (label, obj_id, obj_type, name, tag) in [
        (
            "the connector's CRTC_ID is an object property with one value",
            CONNECTOR_ID,
            OBJECT_CONNECTOR,
            &b"CRTC_ID"[..],
            OBJECT_CRTC,
        ),
        (
            "the primary plane's FB_ID is an object property with one value",
            PLANE_ID,
            OBJECT_PLANE,
            b"FB_ID",
            OBJECT_FB,
        ),
        (
            "the primary plane's CRTC_ID is an object property with one value",
            PLANE_ID,
            OBJECT_PLANE,
            b"CRTC_ID",
            OBJECT_CRTC,
        ),
        (
            "the cursor plane's FB_ID is an object property with one value",
            CURSOR_PLANE_ID,
            OBJECT_PLANE,
            b"FB_ID",
            OBJECT_FB,
        ),
    ] {
        let ok = match property_id(fd, obj_id, obj_type, name) {
            Some((id, _current)) => match property(fd, id) {
                Some(prop) => {
                    prop.flags & PROP_OBJECT != 0
                        && prop.count_values == 1
                        && prop.values[0] == tag as u64
                }
                None => false,
            },
            None => false,
        };
        check_true(&mut r, label, ok);
    }

    // A move with no image bound still moves the pointer, and where it landed is
    // readable - `CRTC_X`/`CRTC_Y` are the plane's steering state rather than a
    // copy of it, which is the one place this plane differs in kind from the
    // primary plane's recorded rectangles.
    let mut cur = Cursor2 {
        flags: 0x2, // DRM_MODE_CURSOR_MOVE
        crtc_id: 34,
        x: 77,
        y: 88,
        hot_x: 1,
        hot_y: 1,
        ..Cursor2::default()
    };
    check_is(
        &mut r,
        "MODE_CURSOR2 moves the pointer",
        unsafe {
            ioctl(
                fd,
                ioc_d(NR_MODE_CURSOR2, size_of::<Cursor2>() as u32),
                (&raw mut cur) as *mut c_void,
            )
        },
        0,
    );
    match property_id(fd, CURSOR_PLANE_ID, OBJECT_PLANE, b"CRTC_X") {
        Some((_, value)) => check_is(
            &mut r,
            "and the plane reports where it landed",
            value as isize,
            77,
        ),
        None => check_true(&mut r, "CRTC_X is one of the plane's properties", false),
    }

    // --- the plane's formats and its IN_FORMATS blob -----------------------------------
    let mut formats = [0u32; 8];
    let mut plane = GetPlane {
        plane_id: PLANE_ID,
        count_format_types: formats.len() as u32,
        ..GetPlane::default()
    };
    plane.format_type_ptr = formats.as_mut_ptr() as u64;
    check_is(
        &mut r,
        "the plane reports its format list",
        unsafe {
            ioctl(
                fd,
                ioc_d(NR_GETPLANE, size_of::<GetPlane>() as u32),
                (&raw mut plane) as *mut c_void,
            )
        },
        0,
    );
    let format_count = plane.count_format_types as usize;
    check_is(
        &mut r,
        "two formats: the two byte orders this scanout can actually show",
        format_count as isize,
        2,
    );
    check_true(
        &mut r,
        "and the first is XRGB8888",
        formats[0] == FORMAT_XRGB8888,
    );
    check_is(
        &mut r,
        "and the second is ARGB8888, the layout a cursor buffer is in",
        formats[1] as isize,
        FORMAT_ARGB8888 as isize,
    );

    match property_id(fd, PLANE_ID, OBJECT_PLANE, b"IN_FORMATS") {
        Some((id, blob_id)) => {
            let prop = property(fd, id).expect("the IN_FORMATS property");
            check_true(
                &mut r,
                "IN_FORMATS is an immutable blob",
                prop.flags & PROP_BLOB != 0 && prop.flags & PROP_IMMUTABLE != 0,
            );
            let bytes = blob(fd, blob_id as u32).expect("the IN_FORMATS bytes");
            check_is(
                &mut r,
                "the blob is version 1",
                get32(&bytes, 0) as isize,
                1,
            );
            check_is(
                &mut r,
                "it lists as many formats as the plane reports",
                get32(&bytes, 8) as isize,
                format_count as isize,
            );
            check_is(
                &mut r,
                "the format array starts after the 24-byte header",
                get32(&bytes, 12) as isize,
                24,
            );
            check_is(
                &mut r,
                "there is one modifier",
                get32(&bytes, 16) as isize,
                1,
            );
            // 24 + 4 * n, rounded up to eight - which for two formats is 32.
            let modifiers = get32(&bytes, 20) as usize;
            check_is(
                &mut r,
                "and it starts where the header and the format array say",
                modifiers as isize,
                (24 + 4 * format_count).next_multiple_of(8) as isize,
            );
            let bitmask = get64(&bytes, modifiers);
            let expect = if format_count == 64 {
                u64::MAX
            } else {
                (1u64 << format_count) - 1
            };
            check_is(
                &mut r,
                "its bitmask covers every format the plane lists, from offset 0",
                bitmask as isize,
                expect as isize,
            );
            check_is(
                &mut r,
                "and the offset into the format array is zero",
                get32(&bytes, modifiers + 8) as isize,
                0,
            );
            check_is(
                &mut r,
                "the modifier is LINEAR, the only one a dumb buffer is in",
                get64(&bytes, modifiers + 16) as isize,
                FORMAT_MOD_LINEAR as isize,
            );
            let mut same_formats = true;
            for index in 0..format_count {
                if get32(&bytes, 24 + 4 * index) != formats[index] {
                    same_formats = false;
                }
            }
            check_true(
                &mut r,
                "and the blob's format array is the plane's own list, entry for entry",
                same_formats,
            );
        }
        None => check_true(&mut r, "the plane has IN_FORMATS", false),
    }

    // --- the cap and the promise it makes ---------------------------------------------
    let mut cap_arg = Cap {
        capability: DRM_CAP_ADDFB2_MODIFIERS,
        ..Cap::default()
    };
    check_is(
        &mut r,
        "ADDFB2_MODIFIERS is queryable",
        unsafe {
            ioctl(
                fd,
                ioc_d(NR_GET_CAP, size_of::<Cap>() as u32),
                (&raw mut cap_arg) as *mut c_void,
            )
        },
        0,
    );
    check_is(&mut r, "and answered 1", cap_arg.value as isize, 1);
    let mut async_cap = Cap {
        capability: DRM_CAP_ASYNC_PAGE_FLIP,
        ..Cap::default()
    };
    unsafe {
        ioctl(
            fd,
            ioc_d(NR_GET_CAP, size_of::<Cap>() as u32),
            (&raw mut async_cap) as *mut c_void,
        )
    };
    check_is(
        &mut r,
        "while tearing flips are answered 0, as this card has no engine for them",
        async_cap.value as isize,
        0,
    );

    // A dumb buffer to bind, so ADDFB2 has something real to take.
    let mut dumb = CreateDumb {
        height: 256,
        width: 256,
        bpp: 32,
        ..CreateDumb::default()
    };
    check_is(
        &mut r,
        "a dumb buffer is created",
        unsafe {
            ioctl(
                fd,
                ioc_d(NR_CREATE_DUMB, size_of::<CreateDumb>() as u32),
                (&raw mut dumb) as *mut c_void,
            )
        },
        0,
    );
    let handle = dumb.handle;
    let pitch = dumb.pitch;

    // ADDFB2 with the modifier flag and the linear modifier: the cap above says
    // this works, so it has to. The modifier is written through the record's own
    // field, which is at 72 - the offset the uapi gives it, and the one a
    // nonzero modifier makes observable.
    let mut fb = FbCmd2 {
        width: 256,
        height: 256,
        pixel_format: FORMAT_XRGB8888,
        flags: FB_MODIFIERS,
        handles: [handle, 0, 0, 0],
        pitches: [pitch, 0, 0, 0],
        modifier: [FORMAT_MOD_LINEAR, 0, 0, 0],
        ..FbCmd2::default()
    };
    check_is(
        &mut r,
        "ADDFB2 takes LINEAR with the modifier flag set",
        unsafe {
            ioctl(
                fd,
                ioc_d(NR_ADDFB2, size_of::<FbCmd2>() as u32),
                (&raw mut fb) as *mut c_void,
            )
        },
        0,
    );
    let fb_id = fb.fb_id;
    check_true(&mut r, "and answers a framebuffer id", fb_id != 0);

    // And it refuses a modifier this plane cannot scan out, which is what the
    // IN_FORMATS list above said in the other direction. This is the claim the
    // padding bug hid: a modifier put at 68 was read as 0, so `LINEAR` was
    // accepted and a bogus one was read as `LINEAR` and accepted too.
    let mut bogus = fb;
    bogus.fb_id = 0;
    bogus.flags = FB_MODIFIERS;
    bogus.modifier = [FORMAT_MOD_BOGUS, 0, 0, 0];
    check_refused(
        &mut r,
        "ADDFB2 refuses a modifier the plane does not list",
        unsafe {
            ioctl(
                fd,
                ioc_d(NR_ADDFB2, size_of::<FbCmd2>() as u32),
                (&raw mut bogus) as *mut c_void,
            )
        },
        Errno::INVAL,
    );

    // The three capabilities that were already true are still true, so this file
    // cannot pass by making the card refuse everything.
    for (cap, want) in [(DRM_CAP_DUMB_BUFFER, 1isize), (DRM_CAP_ADDFB2_MODIFIERS, 1)] {
        let mut arg = Cap {
            capability: cap,
            ..Cap::default()
        };
        unsafe {
            ioctl(
                fd,
                ioc_d(NR_GET_CAP, size_of::<Cap>() as u32),
                (&raw mut arg) as *mut c_void,
            )
        };
        check_is(
            &mut r,
            "a capability this card does have still reads 1",
            arg.value as isize,
            want,
        );
    }

    // An object-typed property write against the wrong object is still refused,
    // and so is a property this card does not have: the refusal that keeps a
    // "set what you like" card from looking healthy.
    check_refused(
        &mut r,
        "a property id the card never issued is refused",
        set_property(fd, 0, 0xdead_beef, CONNECTOR_ID, OBJECT_CONNECTOR),
        Errno::INVAL,
    );

    r.finish();
}
