//! t-drm-props - the connector and plane properties a compositor reads *before*
//! it decides it can drive this card, and the two blobs they point at.
//!
//! `PLAN.md` §7.4 lists the property families a second compositor family looks
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

#![no_std]
#![no_main]

#[path = "../../../lib/lin.rs"]
mod lin;

use core::panic::PanicInfo;

/// `_IOWR('d', nr, size)` as libdrm builds it: the family byte at bits 8..15.
const fn ioc_d(nr: usize, size: usize) -> usize {
    (3 << 30) | (size << 16) | ((b'd' as usize) << 8) | nr
}

const NR_GET_CAP: usize = 0x0c;
const NR_GETCONNECTOR: usize = 0xa7;
const NR_GETPROPERTY: usize = 0xaa;
const NR_GETPROPBLOB: usize = 0xac;
const NR_DESTROY_BLOB: usize = 0xbe;
const NR_GETPLANE: usize = 0xb6;
const NR_MODE_CURSOR2: usize = 0xbb;
const NR_ADDFB2: usize = 0xb8;
const NR_OBJ_GETPROPERTIES: usize = 0xb9;
const NR_OBJ_SETPROPERTY: usize = 0xba;
const NR_CREATE_DUMB: usize = 0xb2;

/// The connector and plane this card has, and the object-type tags.
const CONNECTOR_ID: u32 = 35;
const PLANE_ID: u32 = 36;
/// The cursor plane, the card's second one.
const CURSOR_PLANE_ID: u32 = 37;
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
/// `DRM_MODE_FB_MODIFIERS`, which enables the `modifier[]` array of an `ADDFB2`.
const FB_MODIFIERS: u32 = 1 << 1;
/// `DRM_FORMAT_XRGB8888`, the layout the guest test's dumb buffer is in.
const FORMAT_XRGB8888: u32 = 0x3432_5258;
/// `DRM_FORMAT_MOD_LINEAR`: the only modifier a dumb buffer is ever in.
const FORMAT_MOD_LINEAR: u64 = 0;
/// A modifier from a vendor namespace this card has never heard of.
const FORMAT_MOD_BOGUS: u64 = 0x0100_0000_0000_0001;

const EINVAL: isize = -22;
const O_RDWR: usize = 2;

fn put32(buf: &mut [u8], off: usize, v: u32) {
    buf[off..off + 4].copy_from_slice(&v.to_ne_bytes());
}
fn get32(buf: &[u8], off: usize) -> u32 {
    u32::from_ne_bytes([buf[off], buf[off + 1], buf[off + 2], buf[off + 3]])
}
fn put64(buf: &mut [u8], off: usize, v: u64) {
    buf[off..off + 8].copy_from_slice(&v.to_ne_bytes());
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

/// One `GETPROPERTY`: the flags, the name, the value list and the enums.
struct Property {
    flags: u32,
    name: [u8; 32],
    values: [u64; 4],
    count_values: u32,
    count_enums: u32,
}

fn property(fd: i32, id: u32) -> Option<Property> {
    let mut arg = [0u8; 64];
    let mut values = [0u64; 4];
    put32(&mut arg, 16, id);
    put64(&mut arg, 0, values.as_mut_ptr() as usize as u64);
    put32(&mut arg, 56, 4);
    if lin::ioctl(fd, ioc_d(NR_GETPROPERTY, 64), arg.as_mut_ptr() as usize) != 0 {
        return None;
    }
    let mut name = [0u8; 32];
    name.copy_from_slice(&arg[24..56]);
    Some(Property {
        flags: get32(&arg, 20),
        name,
        values,
        count_values: get32(&arg, 56),
        count_enums: get32(&arg, 60),
    })
}

/// The property id one of an object's properties has, found by name.
fn property_id(fd: i32, obj_id: u32, obj_type: u32, want: &[u8]) -> Option<(u32, u64)> {
    let mut arg = [0u8; 32];
    let mut ids = [0u32; 32];
    let mut values = [0u64; 32];
    put64(&mut arg, 0, ids.as_mut_ptr() as usize as u64);
    put64(&mut arg, 8, values.as_mut_ptr() as usize as u64);
    put32(&mut arg, 16, 32);
    put32(&mut arg, 20, obj_id);
    put32(&mut arg, 24, obj_type);
    if lin::ioctl(
        fd,
        ioc_d(NR_OBJ_GETPROPERTIES, 32),
        arg.as_mut_ptr() as usize,
    ) != 0
    {
        return None;
    }
    let count = get32(&arg, 16) as usize;
    for index in 0..count.min(32) {
        let prop = property(fd, ids[index])?;
        let end = prop.name.iter().position(|b| *b == 0).unwrap_or(32);
        if &prop.name[..end] == want {
            return Some((ids[index], values[index]));
        }
    }
    None
}

/// Read a blob whole, answering its bytes.
fn blob(fd: i32, id: u32) -> Option<[u8; 256]> {
    let mut arg = [0u8; 16];
    let mut data = [0u8; 256];
    put32(&mut arg, 0, id);
    put32(&mut arg, 4, 256);
    put64(&mut arg, 8, data.as_mut_ptr() as usize as u64);
    if lin::ioctl(fd, ioc_d(NR_GETPROPBLOB, 16), arg.as_mut_ptr() as usize) != 0 {
        return None;
    }
    if get32(&arg, 4) == 0 || get32(&arg, 4) > 256 {
        return None;
    }
    Some(data)
}

fn blob_len(fd: i32, id: u32) -> Option<u32> {
    let mut arg = [0u8; 16];
    put32(&mut arg, 0, id);
    if lin::ioctl(fd, ioc_d(NR_GETPROPBLOB, 16), arg.as_mut_ptr() as usize) != 0 {
        return None;
    }
    Some(get32(&arg, 4))
}

#[unsafe(no_mangle)]
extern "C" fn mpos_user_main(_argc: usize, _argv: *const *const u8) -> ! {
    lin::puts(b"t-drm-props\n");
    let mut failed = false;

    let fd = lin::openat(lin::AT_FDCWD, b"/dev/dri/card0\0", O_RDWR, 0);
    lin::check_true(&mut failed, b"the card opens", fd >= 0);
    let fd = fd as i32;
    // No `SET_MASTER`: on this card an *unclaimed* session is served its
    // presentation requests (`master_ok`'s comment says why - a compositor's
    // first commit can arrive before its seat manager's `SET_MASTER`), and a
    // session that has taken the card on a boot with nothing on the framebuffer
    // is bound to a console that does not hold the screen, so every write below
    // would come back `EACCES` before reaching the handler this file is about.

    // --- the connector's property list -------------------------------------------------
    //
    // Read through GETCONNECTOR, which is the path libdrm's drmModeGetConnector
    // takes: the ids *and* the values, in the order the card reports them.
    let mut conn = [0u8; 80];
    let mut ids = [0u32; 16];
    let mut values = [0u64; 16];
    put64(&mut conn, 16, ids.as_mut_ptr() as usize as u64);
    put64(&mut conn, 24, values.as_mut_ptr() as usize as u64);
    put32(&mut conn, 36, 16);
    put32(&mut conn, 48, CONNECTOR_ID);
    lin::check_eq(
        &mut failed,
        b"the connector asks for its properties",
        lin::ioctl(fd, ioc_d(NR_GETCONNECTOR, 80), conn.as_mut_ptr() as usize),
        0,
    );
    let count = get32(&conn, 36) as usize;
    lin::check_true(&mut failed, b"it reports six of them", count == 6);
    let mm_w = get32(&conn, 64);
    let mm_h = get32(&conn, 68);
    lin::check_true(
        &mut failed,
        b"and a physical size in millimetres",
        mm_w > 0 && mm_h > 0,
    );
    lin::check_eq(
        &mut failed,
        b"with the sub-pixel order a real connector reports (unknown, 0)",
        get32(&conn, 72) as isize,
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
    lin::check_true(
        &mut failed,
        b"every property id in the list answers GETPROPERTY",
        all_queryable,
    );

    // --- the four named properties, with the shapes Linux gives them -------------------
    let edid = property_id(fd, CONNECTOR_ID, OBJECT_CONNECTOR, b"EDID");
    match edid {
        Some((id, blob_id)) => {
            let prop = property(fd, id).expect("the EDID property");
            lin::check_true(
                &mut failed,
                b"EDID is a blob and immutable",
                prop.flags & PROP_BLOB != 0 && prop.flags & PROP_IMMUTABLE != 0,
            );
            let bytes = blob(fd, blob_id as u32).expect("the EDID bytes");
            lin::check_true(
                &mut failed,
                b"and its value names a blob that reads back",
                bytes[..8] == [0x00, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x00],
            );
            lin::check_eq(
                &mut failed,
                b"the EDID is one 128-byte block",
                blob_len(fd, blob_id as u32).unwrap_or(0) as isize,
                128,
            );
            let sum = bytes[..128].iter().fold(0u8, |a, b| a.wrapping_add(*b));
            lin::check_eq(
                &mut failed,
                b"whose bytes add to zero, as an EDID's must",
                sum as isize,
                0,
            );
            lin::check_eq(&mut failed, b"it is EDID 1.4", bytes[18] as isize, 1);
            lin::check_eq(&mut failed, b"revision 4", bytes[19] as isize, 4);
            // The size the connector reported is this block's own centimetres
            // times ten - the relation the measurement found between the two, and
            // the reason `size_mm` is derived from `size_cm` rather than computed
            // separately.
            lin::check_eq(
                &mut failed,
                b"the connector's millimetres are the EDID's centimetres times ten",
                mm_w as isize,
                bytes[21] as isize * 10,
            );
            lin::check_eq(
                &mut failed,
                b"and so is the height",
                mm_h as isize,
                bytes[22] as isize * 10,
            );
            // The name descriptor: tag 0xFC in the third of the four 18-byte
            // descriptors, with printable ASCII after it.
            let name = &bytes[54 + 2 * 18..54 + 3 * 18];
            lin::check_true(
                &mut failed,
                b"it carries a monitor name descriptor",
                name[3] == 0xfc && name[5].is_ascii_graphic(),
            );
            // And the first descriptor is a *timing*, which is what its first two
            // bytes say: a descriptor opens with two zeroes, a timing with a clock.
            let timing = &bytes[54..72];
            lin::check_true(
                &mut failed,
                b"and a detailed timing that opens like a timing",
                (timing[0], timing[1]) != (0, 0),
            );
            lin::check_true(
                &mut failed,
                b"the other two descriptors open like descriptors",
                bytes[54 + 18] == 0
                    && bytes[54 + 19] == 0
                    && bytes[54 + 3 * 18] == 0
                    && bytes[54 + 3 * 18 + 1] == 0,
            );
            // Destroying the id a property holds is a no-op, not a way to take
            // the connector's EDID away: Linux keeps the blob because the
            // property owns a reference.
            let mut destroy = [0u8; 4];
            put32(&mut destroy, 0, blob_id as u32);
            lin::check_eq(
                &mut failed,
                b"destroying a driver blob's id succeeds",
                lin::ioctl(fd, ioc_d(NR_DESTROY_BLOB, 4), destroy.as_mut_ptr() as usize),
                0,
            );
            lin::check_true(
                &mut failed,
                b"and the blob is still there afterwards",
                blob(fd, blob_id as u32).is_some(),
            );
        }
        None => lin::check_true(&mut failed, b"the connector has an EDID property", false),
    }

    // The already-existing property, so that a card which lost its old answer
    // while gaining new ones is caught here rather than in a compositor.
    match property_id(fd, CONNECTOR_ID, OBJECT_CONNECTOR, b"DPMS") {
        Some((id, _)) => {
            let prop = property(fd, id).expect("the DPMS property");
            lin::check_true(
                &mut failed,
                b"DPMS is still an enum of four modes",
                prop.flags & PROP_ENUM != 0 && prop.count_enums == 4,
            );
        }
        None => lin::check_true(&mut failed, b"the connector still has DPMS", false),
    }

    match property_id(fd, CONNECTOR_ID, OBJECT_CONNECTOR, b"non-desktop") {
        Some((id, _)) => {
            let prop = property(fd, id).expect("the non-desktop property");
            let end = prop.name.iter().position(|b| *b == 0).unwrap_or(32);
            lin::check_true(
                &mut failed,
                b"non-desktop is a range, not a bool",
                prop.flags & PROP_RANGE != 0 && prop.flags & PROP_ENUM == 0,
            );
            lin::check_true(
                &mut failed,
                b"and it is immutable",
                prop.flags & PROP_IMMUTABLE != 0,
            );
            lin::check_true(
                &mut failed,
                b"with the two values 0 and 1",
                prop.count_values == 2 && prop.values[0] == 0 && prop.values[1] == 1,
            );
            lin::check_true(
                &mut failed,
                b"and it is named non-desktop",
                &prop.name[..end] == b"non-desktop",
            );
        }
        None => lin::check_true(&mut failed, b"the connector has non-desktop", false),
    }

    match property_id(fd, CONNECTOR_ID, OBJECT_CONNECTOR, b"vrr_capable") {
        Some((id, value)) => {
            let prop = property(fd, id).expect("the vrr_capable property");
            lin::check_true(
                &mut failed,
                b"vrr_capable is an immutable range of 0 and 1",
                prop.flags & PROP_RANGE != 0
                    && prop.flags & PROP_IMMUTABLE != 0
                    && prop.count_values == 2
                    && prop.values == [0, 1, 0, 0],
            );
            lin::check_eq(
                &mut failed,
                b"and this display cannot vary its rate, so it reads 0",
                value as isize,
                0,
            );
        }
        None => lin::check_true(&mut failed, b"the connector has vrr_capable", false),
    }

    match property_id(fd, CONNECTOR_ID, OBJECT_CONNECTOR, b"max bpc") {
        Some((id, value)) => {
            let prop = property(fd, id).expect("the max bpc property");
            lin::check_true(
                &mut failed,
                b"max bpc is a range of exactly one value",
                prop.flags & PROP_RANGE != 0 && prop.count_values == 2 && prop.values[0] == 8,
            );
            lin::check_eq(
                &mut failed,
                b"the scanout is eight bits per channel",
                value as isize,
                8,
            );
            // A range of one value means a write outside it is refused, which is
            // what keeps "16" from being a success that changes no pixels.
            let mut arg = [0u8; 24];
            put64(&mut arg, 0, 16);
            put32(&mut arg, 8, id);
            put32(&mut arg, 12, CONNECTOR_ID);
            put32(&mut arg, 16, OBJECT_CONNECTOR);
            lin::check_eq(
                &mut failed,
                b"setting max bpc to 16 is refused",
                lin::ioctl(fd, ioc_d(NR_OBJ_SETPROPERTY, 24), arg.as_mut_ptr() as usize),
                EINVAL,
            );
            put64(&mut arg, 0, 8);
            lin::check_eq(
                &mut failed,
                b"and setting it to what it is succeeds",
                lin::ioctl(fd, ioc_d(NR_OBJ_SETPROPERTY, 24), arg.as_mut_ptr() as usize),
                0,
            );
        }
        None => lin::check_true(&mut failed, b"the connector has max bpc", false),
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
            lin::check_true(
                &mut failed,
                b"the second plane carries an enumerated type",
                prop.flags & PROP_ENUM != 0,
            );
            lin::check_eq(
                &mut failed,
                b"and its type is Cursor, which is how a compositor finds it",
                value as isize,
                2,
            );
        }
        None => lin::check_true(
            &mut failed,
            b"the cursor plane carries a type property",
            false,
        ),
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
    for name in [
        b"SRC_X".as_slice(),
        b"SRC_Y",
        b"SRC_W",
        b"SRC_H",
        b"CRTC_W",
        b"CRTC_H",
    ] {
        let found = property_id(fd, CURSOR_PLANE_ID, OBJECT_PLANE, name);
        lin::check_true(
            &mut failed,
            match name {
                b"SRC_X" => b"the cursor plane has a source offset in x",
                b"SRC_Y" => b"the cursor plane has a source offset in y",
                b"SRC_W" => b"the cursor plane has a source width",
                b"SRC_H" => b"the cursor plane has a source height",
                b"CRTC_W" => b"the cursor plane has a destination width",
                _ => b"the cursor plane has a destination height",
            },
            found.is_some(),
        );
    }
    // `CRTC_X`/`CRTC_Y` are the pointer itself, and they are acted on rather than
    // recorded - the same two the legacy `MODE_CURSOR` moves.
    lin::check_true(
        &mut failed,
        b"the cursor plane's CRTC_X reads back where the pointer is",
        property_id(fd, CURSOR_PLANE_ID, OBJECT_PLANE, b"CRTC_X").is_some(),
    );

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
            &b"FB_ID"[..],
            OBJECT_FB,
        ),
        (
            "the primary plane's CRTC_ID is an object property with one value",
            PLANE_ID,
            OBJECT_PLANE,
            &b"CRTC_ID"[..],
            OBJECT_CRTC,
        ),
        (
            "the cursor plane's FB_ID is an object property with one value",
            CURSOR_PLANE_ID,
            OBJECT_PLANE,
            &b"FB_ID"[..],
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
        lin::check_true(&mut failed, label.as_bytes(), ok);
    }

    // A move with no image bound still moves the pointer, and where it landed is
    // readable - `CRTC_X`/`CRTC_Y` are the plane's steering state rather than a
    // copy of it, which is the one place this plane differs in kind from the
    // primary plane's recorded rectangles.
    let mut cur = [0u8; 36];
    put32(&mut cur, 0, 0x2); // flags: DRM_MODE_CURSOR_MOVE
    put32(&mut cur, 4, 34); // crtc_id
    put32(&mut cur, 8, 77); // x
    put32(&mut cur, 12, 88); // y
    put32(&mut cur, 28, 1); // hot_x
    put32(&mut cur, 32, 1); // hot_y
    lin::check_eq(
        &mut failed,
        b"MODE_CURSOR2 moves the pointer",
        lin::ioctl(fd, ioc_d(NR_MODE_CURSOR2, 36), cur.as_mut_ptr() as usize),
        0,
    );
    match property_id(fd, CURSOR_PLANE_ID, OBJECT_PLANE, b"CRTC_X") {
        Some((_, value)) => lin::check_eq(
            &mut failed,
            b"and the plane reports where it landed",
            value as isize,
            77,
        ),
        None => lin::check_true(
            &mut failed,
            b"CRTC_X is one of the plane's properties",
            false,
        ),
    }

    // --- the plane's formats and its IN_FORMATS blob -----------------------------------
    let mut plane = [0u8; 32];
    let mut formats = [0u32; 8];
    put32(&mut plane, 0, PLANE_ID);
    put64(&mut plane, 24, formats.as_mut_ptr() as usize as u64);
    put32(&mut plane, 20, 8);
    lin::check_eq(
        &mut failed,
        b"the plane reports its format list",
        lin::ioctl(fd, ioc_d(NR_GETPLANE, 32), plane.as_mut_ptr() as usize),
        0,
    );
    let format_count = get32(&plane, 20) as usize;
    lin::check_eq(
        &mut failed,
        b"two formats: the two byte orders this scanout can actually show",
        format_count as isize,
        2,
    );
    lin::check_true(
        &mut failed,
        b"and the first is XRGB8888",
        formats[0] == FORMAT_XRGB8888,
    );
    lin::check_eq(
        &mut failed,
        b"and the second is ARGB8888, the layout a cursor buffer is in",
        formats[1] as isize,
        0x3432_5241,
    );

    match property_id(fd, PLANE_ID, OBJECT_PLANE, b"IN_FORMATS") {
        Some((id, blob_id)) => {
            let prop = property(fd, id).expect("the IN_FORMATS property");
            lin::check_true(
                &mut failed,
                b"IN_FORMATS is an immutable blob",
                prop.flags & PROP_BLOB != 0 && prop.flags & PROP_IMMUTABLE != 0,
            );
            let bytes = blob(fd, blob_id as u32).expect("the IN_FORMATS bytes");
            lin::check_eq(
                &mut failed,
                b"the blob is version 1",
                get32(&bytes, 0) as isize,
                1,
            );
            lin::check_eq(
                &mut failed,
                b"it lists as many formats as the plane reports",
                get32(&bytes, 8) as isize,
                format_count as isize,
            );
            lin::check_eq(
                &mut failed,
                b"the format array starts after the 24-byte header",
                get32(&bytes, 12) as isize,
                24,
            );
            lin::check_eq(
                &mut failed,
                b"there is one modifier",
                get32(&bytes, 16) as isize,
                1,
            );
            // 24 + 4 * n, rounded up to eight - which for two formats is 32.
            let modifiers = get32(&bytes, 20) as usize;
            lin::check_eq(
                &mut failed,
                b"and it starts where the header and the format array say",
                modifiers as isize,
                (24 + 4 * format_count).next_multiple_of(8) as isize,
            );
            let bitmask = get64(&bytes, modifiers);
            let expect = if format_count == 64 {
                u64::MAX
            } else {
                (1u64 << format_count) - 1
            };
            lin::check_eq(
                &mut failed,
                b"its bitmask covers every format the plane lists, from offset 0",
                bitmask as isize,
                expect as isize,
            );
            lin::check_eq(
                &mut failed,
                b"and the offset into the format array is zero",
                get32(&bytes, modifiers + 8) as isize,
                0,
            );
            lin::check_eq(
                &mut failed,
                b"the modifier is LINEAR, the only one a dumb buffer is in",
                get64(&bytes, modifiers + 16) as isize,
                FORMAT_MOD_LINEAR as isize,
            );
            let mut same_formats = true;
            for index in 0..format_count {
                if get32(&bytes, 24 + 4 * index) != formats[index] {
                    same_formats = false;
                }
            }
            lin::check_true(
                &mut failed,
                b"and the blob's format array is the plane's own list, entry for entry",
                same_formats,
            );
        }
        None => lin::check_true(&mut failed, b"the plane has IN_FORMATS", false),
    }

    // --- the cap and the promise it makes ---------------------------------------------
    let mut cap_arg = [0u8; 16];
    put64(&mut cap_arg, 0, DRM_CAP_ADDFB2_MODIFIERS);
    lin::check_eq(
        &mut failed,
        b"ADDFB2_MODIFIERS is queryable",
        lin::ioctl(fd, ioc_d(NR_GET_CAP, 16), cap_arg.as_mut_ptr() as usize),
        0,
    );
    lin::check_eq(
        &mut failed,
        b"and answered 1",
        get64(&cap_arg, 8) as isize,
        1,
    );
    put64(&mut cap_arg, 0, DRM_CAP_ASYNC_PAGE_FLIP);
    lin::ioctl(fd, ioc_d(NR_GET_CAP, 16), cap_arg.as_mut_ptr() as usize);
    lin::check_eq(
        &mut failed,
        b"while tearing flips are answered 0, as this card has no engine for them",
        get64(&cap_arg, 8) as isize,
        0,
    );

    // A dumb buffer to bind, so ADDFB2 has something real to take.
    let mut dumb = [0u8; 32];
    put32(&mut dumb, 4, 256); // width
    put32(&mut dumb, 0, 256); // height
    put32(&mut dumb, 8, 32); // bpp
    lin::check_eq(
        &mut failed,
        b"a dumb buffer is created",
        lin::ioctl(fd, ioc_d(NR_CREATE_DUMB, 32), dumb.as_mut_ptr() as usize),
        0,
    );
    let handle = get32(&dumb, 16);
    let pitch = get32(&dumb, 20);

    // ADDFB2 with the modifier flag and the linear modifier: the cap above says
    // this works, so it has to.
    let mut fb = [0u8; 104];
    put32(&mut fb, 4, 256);
    put32(&mut fb, 8, 256);
    put32(&mut fb, 12, FORMAT_XRGB8888);
    put32(&mut fb, 16, FB_MODIFIERS);
    put32(&mut fb, 20, handle);
    put32(&mut fb, 36, pitch);
    put64(&mut fb, 68, FORMAT_MOD_LINEAR);
    lin::check_eq(
        &mut failed,
        b"ADDFB2 takes LINEAR with the modifier flag set",
        lin::ioctl(fd, ioc_d(NR_ADDFB2, 104), fb.as_mut_ptr() as usize),
        0,
    );
    let fb_id = get32(&fb, 0);
    lin::check_true(&mut failed, b"and answers a framebuffer id", fb_id != 0);

    // And it refuses a modifier this plane cannot scan out, which is what the
    // IN_FORMATS list above said in the other direction.
    let mut bogus = [0u8; 104];
    bogus.copy_from_slice(&fb);
    put32(&mut bogus, 0, 0);
    put32(&mut bogus, 16, FB_MODIFIERS);
    put64(&mut bogus, 68, FORMAT_MOD_BOGUS);
    lin::check_eq(
        &mut failed,
        b"ADDFB2 refuses a modifier the plane does not list",
        lin::ioctl(fd, ioc_d(NR_ADDFB2, 104), bogus.as_mut_ptr() as usize),
        EINVAL,
    );

    // The three capabilities that were already true are still true, so this file
    // cannot pass by making the card refuse everything.
    for (cap, want) in [(DRM_CAP_DUMB_BUFFER, 1isize), (DRM_CAP_ADDFB2_MODIFIERS, 1)] {
        let mut arg = [0u8; 16];
        put64(&mut arg, 0, cap);
        lin::ioctl(fd, ioc_d(NR_GET_CAP, 16), arg.as_mut_ptr() as usize);
        lin::check_eq(
            &mut failed,
            b"a capability this card does have still reads 1",
            get64(&arg, 8) as isize,
            want,
        );
    }

    // An object-typed property write against the wrong object is still refused,
    // and so is a property this card does not have: the refusal that keeps a
    // "set what you like" card from looking healthy.
    let mut arg = [0u8; 24];
    put64(&mut arg, 0, 0);
    put32(&mut arg, 8, 0xdead_beef);
    put32(&mut arg, 12, CONNECTOR_ID);
    put32(&mut arg, 16, OBJECT_CONNECTOR);
    lin::check_eq(
        &mut failed,
        b"a property id the card never issued is refused",
        lin::ioctl(fd, ioc_d(NR_OBJ_SETPROPERTY, 24), arg.as_mut_ptr() as usize),
        EINVAL,
    );

    lin::close(fd);
    lin::done(failed)
}

#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    lin::die(b"t-drm-props: panicked\n")
}
