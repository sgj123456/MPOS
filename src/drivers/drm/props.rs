pub(crate) fn get_cap(arg: usize) -> isize {
    // struct drm_get_cap { u64 capability; u64 value; }
    let cap = unsafe { (arg as *const u64).read_unaligned() };
    let value = match cap {
        super::abi::CAP_DUMB_BUFFER => 1,
        super::abi::CAP_PRIME => PRIME_CAPS,
        super::abi::CAP_TIMESTAMP_MONOTONIC => 1,
        super::abi::CAP_CRTC_IN_VBLANK_EVENT => 1,
        super::abi::CAP_CURSOR_WIDTH | super::abi::CAP_CURSOR_HEIGHT => CURSOR_SIZE,
        // True, and only because `ADDFB2` honours the flag this advertises: a
        // commit may carry `DRM_MODE_FB_MODIFIERS` with the linear modifier,
        // which is the only one a dumb buffer is ever in. A cap answered 1 over
        // an `ADDFB2` that refused every modifier would be the caller building
        // a buffer it cannot then bind.
        super::abi::CAP_ADDFB2_MODIFIERS => 1,
        // Both answered 1, and made true below rather than merely advertised:
        // the family is dispatched, `SYNCOBJ_EVENTFD` is the probe a compositor
        // makes before it will expose the explicit-sync protocol, and a timeline
        // is a shape these objects really have once a point is submitted.
        super::abi::CAP_SYNCOBJ | super::abi::CAP_SYNCOBJ_TIMELINE => 1,
        // Named rather than left to the catch-all: this is *not* "unsupported
        // for now". A tearing flip is a mode of presentation this card does not
        // have - there is no engine to present outside the frame boundary - so
        // a compositor looking for it is told no, and one that ignores the cap
        // and asks anyway is refused the `PAGE_FLIP_ASYNC` flag it would need.
        super::abi::CAP_ASYNC_PAGE_FLIP => 0,
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
pub(crate) fn set_client_cap(arg: usize, ofd: u64) -> isize {
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
        super::abi::CLIENT_CAP_ATOMIC => {
            critical_section::with(|cs| {
                let mut st = STATE.borrow(cs).borrow_mut();
                st.atomic_ofds.retain(|&owner| owner != ofd);
                if value == 1 {
                    st.atomic_ofds.push(ofd);
                }
            });
            0
        }
        // The cursor plane's hotspot pair, recorded per description exactly like
        // `CLIENT_CAP_ATOMIC`: it is a second cap with its own reader (the two list
        // doors hide [`HOTSPOT_PROPS`] for a client that has not set it), so a
        // compositor that asks sees `HOTSPOT_X`/`HOTSPOT_Y` on the cursor plane and
        // one that does not, does not. A `value` of 0 unsets it, the same teardown
        // request weston makes. This card accepts the cap because it *is* one of the
        // virtualized drivers the cap names: Linux's virtio-gpu publishes the hotspot
        // properties behind it, and an earlier note here refused it on the strength of
        // a non-virtualized amdgpu measurement - §7.4 records why that was the wrong
        // instrument.
        super::abi::CLIENT_CAP_CURSOR_PLANE_HOTSPOT => {
            critical_section::with(|cs| {
                let mut st = STATE.borrow(cs).borrow_mut();
                st.hotspot_ofds.retain(|&owner| owner != ofd);
                if value == 1 {
                    st.hotspot_ofds.push(ofd);
                }
            });
            0
        }
        // Refused because the object behind it does not exist here: no plane has a
        // color pipeline, so there is nothing for the plane-color cap to be about.
        // `EOPNOTSUPP` is Linux's answer for a cap the driver declares it does not do.
        super::abi::CLIENT_CAP_PLANE_COLOR_PIPELINE => -95,
        // A capability number the driver has never heard of is not "not
        // supported", it is not a request at all, and Linux says so.
        _ => libc_einval(),
    }
}

/// `SET_VERSION`: the caller names the DRM *ioctl interface* it wants; the
/// reply reports the interface in force and this driver's own version,
/// 1.1.0 - the same version `fill_version` answers by name.
pub(crate) fn set_version(arg: usize) -> isize {
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

pub(crate) fn get_resources(arg: usize, ofd: u64) -> isize {
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
            //
            // The list is the *description's* own, not the card's: Linux's
            // `drm_mode_getresources` walks `file_priv->fbs`, and every other
            // list in the reply is device-wide (the three below are, with the
            // lease filter). Measured on a current host
            // (`.workbuddy/probe_drm_fb_owner.c`, Linux 7.2.11): a child that
            // made fb 464 read 1 fb - its own - while the parent holding a
            // second description answered 0; after the parent made fb 465 its
            // own answer became 1 and the child's remained 464. The
            // device-wide answer is what niri stopped on: it walked the ids
            // `GETRESOURCES` gave it and found kmscube's framebuffer (fb 100),
            // which is another client's object and not one it may name. The
            // ownership is the one `ADDFB`/`ADDFB2` already record
            // (`Framebuffer::owner`, the open file description), and the
            // release path already prunes it, so this filter and that table
            // are one fact rather than two.
            let fbs: Vec<u32> = st
                .fbs
                .iter()
                .filter_map(|f| f.as_ref())
                .filter(|f| f.owner == ofd)
                .map(|f| f.id)
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
pub(crate) fn write_mode(buf: *mut u8, clock_khz: u32, w: u32, h: u32) {
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
/// asked for, whether or not the scanout has been driven to it. `GETCONNECTOR`'s
/// mode list and the connector's `EDID` report this, because it is the mode a
/// compositor re-probes for after a hotplug uevent and may then commit.
///
/// `GETCRTC` deliberately does *not*: it answers what the scanout is driving
/// ([`programmed_mode_dims`]), and the difference between the two is the whole of
/// why a compositor can tell that it still has to modeset.
use super::abi::{
    ACTIVE_VALUES, ATOMIC_ALLOW_MODESET, ATOMIC_NONBLOCK, ATOMIC_PROPS, ATOMIC_TEST_ONLY,
    BLEND_COVERAGE, BLEND_PREMULT, BPC_HERE, CLIENT_CAP_ASPECT_RATIO, CLIENT_CAP_STEREO_3D,
    CLIENT_CAP_UNIVERSAL_PLANES, CLIENT_CAP_WRITEBACK_CONNECTORS, CONNECTED, CONNECTOR_ID,
    CONNECTOR_PROPS, CONNECTOR_TYPE, CRTC_ID, CURSOR_FORMATS, CURSOR_PLANE_ID, CURSOR_SIZE,
    DPMS_MODES, DRM_FORMAT_ARGB8888, DRM_MAJOR, ENCODER_ID, ENCODER_TYPE, HOTSPOT_PROPS, MAX_BLOB,
    OBJECT_CONNECTOR, OBJECT_CRTC, OBJECT_ENCODER, OBJECT_FB, OBJECT_PLANE, PAGE_FLIP_EVENT,
    PLANE_FORMATS, PLANE_ID, PLANE_TYPE_CURSOR, PLANE_TYPE_PRIMARY, PRIME_CAPS, PROP_ACTIVE,
    PROP_ATOMIC, PROP_BLEND, PROP_BLOB, PROP_CRTC_H, PROP_CRTC_ID, PROP_CRTC_W, PROP_CRTC_X,
    PROP_CRTC_Y, PROP_DPMS, PROP_EDID, PROP_ENUM, PROP_HOTSPOT_X, PROP_HOTSPOT_Y, PROP_IMMUTABLE,
    PROP_IN_FENCE_FD, PROP_IN_FORMATS, PROP_MAX_BPC, PROP_MODE_ID, PROP_NON_DESKTOP, PROP_OBJECT,
    PROP_OUT_FENCE_PTR, PROP_PLANE_CRTC_ID, PROP_PLANE_FB_ID, PROP_RANGE, PROP_SIGNED_RANGE,
    PROP_SRC_H, PROP_SRC_W, PROP_SRC_X, PROP_SRC_Y, PROP_TYPE, PROP_VRR_CAPABLE,
};
use super::flip::show_cursor;
use super::ioctl::{libc_einval, user_array};
use super::master::{LeaseSet, leased, queue_flip_event};
use super::state::{DrmState, STATE};
use super::sync::SyncFileFd;
use super::virtgpu::card;

use crate::mm::phys_to_virt;
use alloc::sync::Arc;
use alloc::vec::Vec;

pub(crate) fn advertised_mode_dims() -> (u32, u32) {
    match crate::drivers::virtio::gpu::global() {
        Some(gpu) => gpu.advertised_dims(),
        None => (1024, 768),
    }
}

/// The size the scanout is actually driven at: the size of the resource on the
/// screen - a client compositor's, when one has presented its own, and this
/// driver's own framebuffer otherwise - and the ceiling a committed framebuffer
/// has to fit. It equals [`advertised_mode_dims`] except in the window between
/// the host asking for a new size and a compositor committing it.
///
/// That it is the *presented* resource's size and not the driver's private
/// framebuffer's is load-bearing, and it was measured rather than reasoned
/// (`docs/notes/07` §7.4). A compositor presents its own buffer at whatever size
/// it drew - the size the host asked for - so a card that keeps answering with
/// its private buffer's geometry reports a size it is not driving. The
/// compositor's own re-read then agrees with the connector, `connect`'s
/// `memcmp` says "no change", no mode is ever committed, and each frame it does
/// send is refused by [`fb_fits_at`] for not fitting a size the screen is not
/// at: `mpos-dbg: EINVAL from fb_fits_at fb=100 fit=1280x800 programmed=1280x800
/// advertised=(952, 1135) mode_blob=None`, one line per frame, forever.
pub(crate) fn programmed_mode_dims() -> (u32, u32) {
    match crate::drivers::virtio::gpu::global() {
        Some(gpu) => gpu.driven_dims(),
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
/// `GETCONNECTOR`'s mode list carries the size the connector is offering, which
/// is what [`advertised_mode_dims`] answers. What `GETCRTC` reports is the size
/// the scanout *drives*, and the compositor needs the two to differ before it
/// will send the modeset that closes the gap.
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
pub(crate) fn cursor_fb_ok(st: &DrmState, fb_id: u32) -> bool {
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
///
/// This asks about the *buffer*, not about the client's name for it, and the two
/// are deliberately different questions: a framebuffer names its buffer by the
/// handle number, and it keeps naming it after the client releases that name
/// (Linux's `drm_framebuffer_init` holds the GEM object, so a framebuffer is a
/// holder in its own right). A released name is the other question, and
/// [`handle_names_buffer`] is where it is asked.
fn handle_is_dumb(st: &DrmState, handle: u32) -> bool {
    st.dumb
        .get(handle as usize - 1)
        .is_some_and(|s| s.is_some())
}

/// Whether `handle` still *names* a buffer: there is one, and the client has not
/// released the name.
///
/// What every request that takes a handle has to ask. A framebuffer holds its
/// buffer past the release of its name - that is what makes a compositor's
/// `ADDFB2`-then-`GEM_CLOSE` work at all - so "the buffer is there" is not the
/// same as "this number is a name a caller may use", and answering the second
/// with the first would hand a client a buffer it has already let go of.
pub(crate) fn handle_names_buffer(st: &DrmState, handle: usize) -> bool {
    st.dumb
        .get(handle.wrapping_sub(1))
        .and_then(|slot| slot.as_ref())
        .is_some_and(|d| !d.holders.is_empty())
}

/// Put the pixels of framebuffer `fb_id` on the scanout. A framebuffer the
/// card did not make, or one larger than the scanout's mode, is refused with
/// the same errno Linux uses.
/// Whether the framebuffer `fb_id`'s pixels are the *device's* rather than the
/// guest's - see [`Resource::host_pixels`] for the fact itself and how it was
/// measured.
///
/// No condition on who holds the card, and that is deliberate: this card's own rule
/// for a presentation request is that an *unbound* card serves its holder
/// (`master_ok` passes when no console is bound), which is what lets a client that
/// never calls `SET_MASTER` - kmscube is one, and it is how the first frames on a
/// fresh boot reach the screen at all - set a mode and flip. Requiring a master
/// here turned "nothing holds the card" into "the compositor's frame is not what
/// belongs on the screen", and the screen then kept the kernel's own (blank, after
/// a resize) framebuffer: measured as `[dbg] present: painted=false resent=false
/// pending=true client=None master=false` while kmscube was drawing.
///
/// What keeps a *lost* master from holding the screen is not this either: its
/// presentation requests are refused from then on (`master_ok`), and the console it
/// lost the screen to paints - and a console that painted wins the present step
/// (`video::render`), which is where the screen goes back to it.
fn fb_pixels_are_the_devices(st: &DrmState, fb_id: u32) -> bool {
    if fb_id == 0 {
        return false;
    }
    let Some(fb) = st
        .fbs
        .iter()
        .filter_map(|f| f.as_ref())
        .find(|f| f.id == fb_id)
    else {
        return false;
    };
    st.dumb
        .get(fb.handle.wrapping_sub(1) as usize)
        .and_then(|slot| slot.as_ref())
        .and_then(|d| d.resource.as_ref())
        .is_some_and(|r| r.host_pixels)
}

/// The compositor's own resource, when *it* is what the display should be showing.
///
/// `Some((resource_id, width, height))` exactly when the framebuffer the CRTC is on
/// is one whose pixels the device holds ([`fb_pixels_are_the_devices`]), and `None`
/// means this driver's own framebuffer is the thing to put up: a text console, a
/// card with no 3D, or a client whose frame really is in guest memory - a dumb
/// buffer drawn by a software rasteriser - which the scanout copy carries correctly.
///
/// The render task reads this at the one moment it presents (`video::render`'s own
/// call), so the answer is taken where the pixels are about to move rather than
/// cached somewhere it could go stale, and `None` is the answer for every state the
/// task can be in except that one.
pub fn client_scanout() -> Option<(u32, u32, u32)> {
    critical_section::with(|cs| {
        let st = STATE.borrow(cs).borrow();
        if !fb_pixels_are_the_devices(&st, st.current_fb) {
            return None;
        }
        let fb = st
            .fbs
            .iter()
            .filter_map(|f| f.as_ref())
            .find(|f| f.id == st.current_fb)?;
        let resource = st
            .dumb
            .get(fb.handle.wrapping_sub(1) as usize)
            .and_then(|slot| slot.as_ref())
            .and_then(|d| d.resource.as_ref())?;
        Some((resource.id, fb.width, fb.height))
    })
}

pub(crate) fn copy_to_scanout(st: &DrmState, fb_id: u32) -> isize {
    if !fb_fits(st, fb_id) {
        return libc_einval();
    }
    // A framebuffer whose pixels the *device* holds has nothing here to copy: the
    // pages below it were never written, so this would carry black onto the screen.
    // That frame reaches the display the other way - the render task scans the
    // resource out ([`client_scanout`], `Gpu::scan_out_client`) - so this answers
    // what the callers use it for ("the frame is accepted") and copies nothing,
    // exactly as it does for a screen with no framebuffer on it. `fb_fits` above
    // still ran, because an fb that does not fit the mode is refused whatever its
    // pixels are.
    if fb_pixels_are_the_devices(st, fb_id) {
        return 0;
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
    // The pointer goes on last, over the frame that was just copied - *unless*
    // this device renders the pointer itself. When it does ([`host_cursor_live`]),
    // the scanout carries only the compositor's frame and the host composites the
    // cursor over it, so blending here would draw a second pointer into the frame
    // the host already has one on. See [`paint_cursor`] for why this is the right
    // place for the software path.
    if !host_cursor_live() {
        paint_cursor(st, scanout, screen_w, screen_h);
    }
    0
}

/// Whether the host renders the pointer, so the kernel must not blend one into the
/// scanout. True exactly when the device published a cursor queue at bring-up;
/// false on a card with no second queue, where [`paint_cursor`] is the presentation
/// path. One predicate, read at the single blend call site above, which is what
/// makes the two paths auditable against each other rather than scattered.
pub(crate) fn host_cursor_live() -> bool {
    card().cursor_q
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

pub(crate) fn get_crtc(arg: usize) -> isize {
    // struct drm_mode_crtc: set_connectors_ptr@0,count_connectors@8,
    // crtc_id@12, fb_id@16, x@20, y@24, gamma_size@28, mode_valid@32, mode@36.
    // The mode reported here is the one the scanout is *driving*, not the one the
    // connector is offering. Those are the same size except in the window between
    // the host asking for a new one and a compositor committing it, and reporting
    // the offered size in that window is a lie with a deadlock behind it:
    // Aquamarine's `SDRMConnector::connect` takes the CRTC's current mode
    // (`getCurrentMode`, which falls back to this ioctl when the CRTC's `MODE_ID`
    // is unset) and compares it against each mode the connector lists - our single
    // advertised one - and *skips `setMode` when they match*. So the very commit
    // that would drive the scanout to the host's new size was the one it never
    // sent: it believed the CRTC already ran at that size, and page-flipped a
    // buffer of that size instead, which `fb_fits_at` then refused against the
    // size the scanout *was* driving. Measured on the GL machine: `fit=1280x800
    // programmed=1280x800 advertised=(1910, 1135) mode_blob=None` on every commit,
    // four `TEST_ONLY` then the real ones, with the compositor's own
    // "got error from kernel - expect bad rendering 2" underneath.
    critical_section::with(|cs| {
        let st = STATE.borrow(cs).borrow();
        let (w, h) = programmed_mode_dims();
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

pub(crate) fn set_crtc(arg: usize, lease: Option<&LeaseSet>) -> isize {
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

pub(crate) fn get_encoder(arg: usize) -> isize {
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

pub(crate) fn get_connector(arg: usize, ofd: u64) -> isize {
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
        without_props(&mut visible_props, &mut visible_values, all, &ATOMIC_PROPS)
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
pub(crate) fn get_plane(arg: usize) -> isize {
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
pub(crate) fn get_property(arg: usize) -> isize {
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
            DPMS_MODES,
            &[("On", 0), ("Standby", 1), ("Suspend", 2), ("Off", 3)],
        ),
        PROP_ACTIVE => ("ACTIVE", PROP_RANGE, ACTIVE_VALUES, &[]),
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
        // The cursor image's hotspot: the pixel of the image that is the pointer
        // point. Signed ranges bounded by the largest cursor this card takes, which
        // is what a real virtio-gpu cursor plane carries and why they are signed -
        // a compositor may name a hotspot at or beyond an edge. Only the *cursor*
        // plane has them; see [`HOTSPOT_PROPS`] for the cap that gates their
        // enumeration.
        PROP_HOTSPOT_X => (
            "HOTSPOT_X",
            PROP_RANGE | PROP_SIGNED_RANGE,
            &[0, CURSOR_SIZE],
            &[],
        ),
        PROP_HOTSPOT_Y => (
            "HOTSPOT_Y",
            PROP_RANGE | PROP_SIGNED_RANGE,
            &[0, CURSOR_SIZE],
            &[],
        ),
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
        // The two enum-shaped properties that are *recorded* and read back, so a
        // write outside the values their own list advertises is refused rather
        // than stored: storing it would hand the caller back a number its enum
        // has no name for, which is a property contradicting itself.
        (CONNECTOR_ID, PROP_DPMS) => {
            if !DPMS_MODES.contains(&value) {
                return libc_einval();
            }
            st.dpms = value as u32
        }
        (CRTC_ID, PROP_ACTIVE) => {
            if !ACTIVE_VALUES.contains(&value) {
                return libc_einval();
            }
            st.crtc_active = value != 0
        }
        // The CRTC's mode, as a blob id - and the value is judged by the *same*
        // test the commit path judges it by ([`mode_fits`], at the top of
        // `mode_atomic`), so the two doors cannot disagree about what a mode is.
        // They did: a commit naming a blob that holds no mode is `EINVAL`, while
        // this door recorded any number at all, so a caller could write an id
        // naming nothing and then read it back as the CRTC's current mode - a
        // value the card had just called its own state and one `GETPROPBLOB`
        // answers `EINVAL` for (`SETPROPERTY` and `OBJ_SETPROPERTY` being one
        // handler is not enough; sharing the *test* is what makes them agree).
        (CRTC_ID, PROP_MODE_ID) => {
            if value != 0 && !mode_fits(st, value as u32) {
                return libc_einval();
            }
            st.crtc_mode_id = value as u32
        }
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
        // The cursor's hotspot: the pixel of the image that is the pointer point,
        // recorded so a read-back reports what was written and so the host-cursor
        // path (`Job::SetCursor`) can subtract it from `cursor_pos` to place the
        // image's top-left where the device expects. A signed write outside the
        // largest cursor is refused rather than stored, matching the range
        // [`get_property`] advertises - the same "an accepted write is what the
        // read says" rule every ranged property here follows.
        (CURSOR_PLANE_ID, PROP_HOTSPOT_X) => {
            let v = value as i64;
            if !(0..=(CURSOR_SIZE as i64)).contains(&v) {
                return libc_einval();
            }
            st.cursor_hotspot.0 = v as i32
        }
        (CURSOR_PLANE_ID, PROP_HOTSPOT_Y) => {
            let v = value as i64;
            if !(0..=(CURSOR_SIZE as i64)).contains(&v) {
                return libc_einval();
            }
            st.cursor_hotspot.1 = v as i32
        }
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
        // Pre-multiplied or coverage alpha. Nothing here *composites* - the
        // scanout carries the compositor's finished frame, so there is nothing
        // below it to blend against and the pixels are pre-multiplied by
        // construction - but the value is still **recorded**, which it was not:
        // the write used to be dropped while the read path answered
        // `Pre-multiplied` unconditionally, so a caller that wrote `Coverage`
        // was told the write succeeded and then read its own answer contradicted.
        // That is a worse shape than ignoring the write, and it made the sentence
        // on [`set_object_prop`] - "every property here is state a compositor
        // reads back" - false of the one property it was written for.
        (PLANE_ID, PROP_BLEND) => {
            if value != BLEND_PREMULT && value != BLEND_COVERAGE {
                return libc_einval();
            }
            st.plane_blend = value
        }
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
pub(crate) fn set_property(arg: usize) -> isize {
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

/// The kind of the object `id` names, which is everything a caller that asked
/// under [`OBJECT_ANY`] left to the card.
///
/// This is Linux's `__drm_mode_object_find` against the card's one object
/// table: the four fixed ids are that table, plus the framebuffers, whose ids
/// are the caller's own to allocate and so are known only to the state. `None`
/// is an id nothing here holds, which either door turns into the same `EINVAL`
/// a wrong kind gets.
fn object_kind(st: &DrmState, id: u32) -> Option<u32> {
    match id {
        CRTC_ID => Some(OBJECT_CRTC),
        CONNECTOR_ID => Some(OBJECT_CONNECTOR),
        ENCODER_ID => Some(OBJECT_ENCODER),
        super::abi::PLANE_ID | super::abi::CURSOR_PLANE_ID => Some(OBJECT_PLANE),
        _ if st
            .fbs
            .iter()
            .filter_map(|fb| fb.as_ref())
            .any(|fb| fb.id == id) =>
        {
            Some(OBJECT_FB)
        }
        _ => None,
    }
}

/// `OBJ_SETPROPERTY`: the object-typed spelling of the same write.
///
/// The object *type* is what the caller says it is talking to, so it is
/// checked against the object's real type - a plane's id under
/// `OBJECT_CONNECTOR` is the mismatch Linux refuses. The one write that
/// moves the scanout is the plane's `FB_ID`, and it is followed by the
/// pixel copy here, because a caller that sets it outside a commit is asking
/// for the buffer to be shown now.
pub(crate) fn obj_set_property(arg: usize) -> isize {
    // struct drm_mode_obj_set_property: value@0(u64), prop_id@8, obj_id@12,
    // obj_type@16.
    let value = unsafe { (arg as *const u64).read_unaligned() };
    let prop_id = unsafe { ((arg + 8) as *const u32).read_unaligned() };
    let obj_id = unsafe { ((arg + 12) as *const u32).read_unaligned() };
    let obj_type = unsafe { ((arg + 16) as *const u32).read_unaligned() };
    // The *primary* plane's buffer is the one write this door acts on by
    // copying: a caller that sets it outside a commit is asking for that
    // buffer to be on the screen now. The cursor plane's buffer is a different
    // present - the pointer is the host's to draw on a device with a cursor
    // queue, and a blend over the master's frame on one without - so it is
    // presented below the way `MODE_CURSOR` presents its own image, never by
    // copying the cursor image over the whole scanout.
    let shown = prop_id == PROP_PLANE_FB_ID && obj_id == PLANE_ID;
    let cursor_image = prop_id == PROP_PLANE_FB_ID && obj_id == CURSOR_PLANE_ID;
    let rc = critical_section::with(|cs| {
        let mut st = STATE.borrow(cs).borrow_mut();
        // `DRM_MODE_OBJECT_ANY` is the id deciding rather than the kind, so it
        // is resolved first - Linux's `drm_mode_obj_set_property_ioctl` asks
        // `drm_mode_object_find` the same way, and a write under that tag is a
        // write to whatever the id names.
        let obj_type = match obj_type {
            super::abi::OBJECT_ANY => match object_kind(&st, obj_id) {
                Some(kind) => kind,
                None => return libc_einval(),
            },
            named => named,
        };
        // Both planes are named here, not just the primary: the cursor plane is
        // a plane to every caller that reads it, and its properties are
        // settable, so a type check that knew only `PLANE_ID` turned *every*
        // write to a cursor property into `EINVAL` while the read of the same
        // property went on reporting the old value. That is the shape
        // `t-drm-props` caught as `a HOTSPOT_X write reads back (got 0, want 4)`.
        if !matches!(
            (obj_type, obj_id),
            (OBJECT_CONNECTOR, CONNECTOR_ID)
                | (OBJECT_CRTC, CRTC_ID)
                | (OBJECT_PLANE, PLANE_ID)
                | (OBJECT_PLANE, CURSOR_PLANE_ID)
        ) {
            return libc_einval();
        }
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
        return rc;
    }
    if rc == 0 && cursor_image {
        // The same two presents `MODE_CURSOR` makes, for the same reason: on a
        // device with a cursor queue one small command moves the pointer, and
        // on one without, the frame is rebuilt around the new image and
        // flushed. Nothing is drawn when there is no frame to draw over - the
        // image and position are recorded either way, so it appears with the
        // next commit.
        if host_cursor_live() {
            let _ = show_cursor();
        } else {
            let rc = critical_section::with(|cs| {
                let st = STATE.borrow(cs).borrow();
                if st.current_fb == 0 {
                    return 0;
                }
                copy_to_scanout(&st, st.current_fb)
            });
            if rc == 0 {
                crate::drivers::video::render::request_flush_blocking();
            }
            return rc;
        }
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
pub(crate) fn mode_atomic(arg: usize) -> isize {
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
                // The earliest refusal there is, and the one that hides the most:
                // a property this card does not have takes the whole commit down
                // before any of the judgement below is reached, so the caller sees
                // only `EINVAL` and a compositor's log says nothing about which
                // property it was.
                crate::println!(
                    "mpos-dbg: commit refused at parse: obj={obj} prop={prop} value={value}: {e}"
                );
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
                // The one refusal a compositor cannot tell from any other: it
                // named a mode this card will not show, and the answer is `EINVAL`
                // with nothing said. So it is said here - the blob, its size (when
                // it has one) and the two sizes a mode is judged against.
                _ => {
                    crate::println!(
                        "mpos-dbg: EINVAL from mode_fits blob={blob} size={:?} \
                         advertised={:?} programmed={:?}",
                        mode_size(&st, blob),
                        advertised_mode_dims(),
                        programmed_mode_dims(),
                    );
                    return (libc_einval(), None);
                }
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
            // This refusal is the one a compositor cannot see the reason for: its
            // commit comes back `EINVAL` and its own log can only say so. The
            // whole of what was judged is printed for that reason - the
            // framebuffer's own shape, its handle, whether that handle still names
            // a buffer, and the sizes the fit is measured against. Three separate
            // facts can produce `EINVAL` here (a framebuffer larger than the mode,
            // one bound to a handle that has been released, or one that is not 32
            // bpp) and only the first is visible from the caller's side.
            let rec = st
                .fbs
                .iter()
                .filter_map(|f| f.as_ref())
                .find(|f| f.id == fb);
            let live = rec.is_some_and(|f| handle_is_dumb(&st, f.handle));
            crate::println!(
                "mpos-dbg: EINVAL from fb_fits_at fb={fb} fit={fit_w}x{fit_h} \
                 programmed={cur_w}x{cur_h} advertised={:?} mode_blob={:?} test_only={} \
                 fb=(handle,width,height,pitch,bpp,format)={:?} handle_live={live}",
                advertised_mode_dims(),
                plan.mode_blob,
                flags & ATOMIC_TEST_ONLY != 0,
                rec.map(|f| (f.handle, f.width, f.height, f.pitch, f.bpp, f.format)),
            );
            return (libc_einval(), None);
        }
        // The cursor's rectangle, which has to be the identity for the image
        // the commit is binding - see `Commit::cursor_rect_ok`.
        if !plan.cursor_rect_ok(&st) {
            crate::println!(
                "mpos-dbg: EINVAL from cursor_rect_ok src={:?} dst={:?} cfb={:?}",
                plan.cursor_src,
                plan.cursor_dst,
                st.cursor_fb
            );
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
        crate::println!("mpos-dbg: EINVAL from resize_blocking: the host will not drive {w}x{h}");
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
                    crate::println!(
                        "mpos-dbg: commit refused copying framebuffer {fb} to the scanout: {e}"
                    );
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
                crate::println!(
                    "mpos-dbg: commit refused property obj={obj} prop={prop} value={value}: {rc}"
                );
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
        // A commit that touched the pointer has to make it visible. On a device with
        // a cursor queue that means handing the image/position to the host
        // (`show_cursor`), and *not* re-copying the master's framebuffer - the host
        // composites the pointer over whatever frame is already there, so a move costs
        // one cursor-queue command rather than a full gather. On a device without one,
        // the pointer is drawn in the copy, so the frame is rebuilt around it exactly
        // as before. Same reasoning as `mode_cursor`, and the reason an atomic cursor
        // move and a legacy one are equally prompt on either path.
        if cursor_touched {
            if host_cursor_live() {
                let _ = show_cursor();
            } else {
                critical_section::with(|cs| {
                    let st = STATE.borrow(cs).borrow();
                    if st.current_fb != 0 {
                        let _ = copy_to_scanout(&st, st.current_fb);
                    }
                });
            }
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
    /// The hotspot this commit names for the cursor plane, if it names one. Kept
    /// so a commit that sets an image and its hotspot in one atomic request lands
    /// both together - the same coupling that makes `MODE_CURSOR2`'s struct carry
    /// them as a pair - rather than binding the image at a stale offset.
    cursor_hotspot: Option<(i32, i32)>,
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
                | (CURSOR_PLANE_ID, PROP_HOTSPOT_X)
                | (CURSOR_PLANE_ID, PROP_HOTSPOT_Y)
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
                super::abi::PROP_SRC_X
                | super::abi::PROP_SRC_Y
                | super::abi::PROP_SRC_W
                | super::abi::PROP_SRC_H => {
                    let mut r = self.cursor_src.unwrap_or((0, 0, 0, 0));
                    match prop {
                        PROP_SRC_X => r.0 = value as u32 >> 16,
                        PROP_SRC_Y => r.1 = value as u32 >> 16,
                        PROP_SRC_W => r.2 = value as u32 >> 16,
                        _ => r.3 = value as u32 >> 16,
                    }
                    self.cursor_src = Some(r);
                }
                super::abi::PROP_CRTC_W | super::abi::PROP_CRTC_H => {
                    let mut d = self.cursor_dst.unwrap_or((0, 0));
                    if prop == PROP_CRTC_W {
                        d.0 = value as u32;
                    } else {
                        d.1 = value as u32;
                    }
                    self.cursor_dst = Some(d);
                }
                // The hotspot, kept so a commit that binds an image and names its
                // pointer point in one request lands both together. Signed like the
                // property's range; the write path validates it against the same
                // bound `set_object_prop` uses, so the two doors agree.
                PROP_HOTSPOT_X => {
                    let mut h = self.cursor_hotspot.unwrap_or((0, 0));
                    h.0 = value as i64 as i32;
                    self.cursor_hotspot = Some(h);
                }
                PROP_HOTSPOT_Y => {
                    let mut h = self.cursor_hotspot.unwrap_or((0, 0));
                    h.1 = value as i64 as i32;
                    self.cursor_hotspot = Some(h);
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
pub(crate) fn create_blob(arg: usize) -> isize {
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
pub(crate) const DRIVER_BLOB_EDID: u32 = 0x4000_0001;
pub(crate) const DRIVER_BLOB_IN_FORMATS: u32 = 0x4000_0002;
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
pub(crate) fn get_propblob(arg: usize) -> isize {
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
pub(crate) fn destroy_blob(arg: usize) -> isize {
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

/// The properties of a list a client that has not set the cap behind them is
/// shown: the ones in `hidden` are taken out, in place, and the count comes back
/// with them. `values` runs parallel to `ids` and is compacted the same way,
/// because a caller reads the two as pairs.
///
/// The filter sits over an already-built list rather than inside every arm that
/// builds one, because an object's *contents* and who may see them are two facts
/// and only the second is what the cap changes. It serves both gated families -
/// [`ATOMIC_PROPS`] for `DRM_CLIENT_CAP_ATOMIC` and [`HOTSPOT_PROPS`] for
/// `DRM_CLIENT_CAP_CURSOR_PLANE_HOTSPOT` - from one code path, which is what keeps
/// the two enumeration doors (`GETCONNECTOR` and `OBJ_GETPROPERTIES`) agreeing on
/// any given object; a client reading a connector from one door and the other would
/// otherwise be handed two different connectors. The §7.4 measurement saw exactly
/// this on a connector: seventeen properties to a description without the cap,
/// eighteen with it, and per *description*.
fn without_props(ids: &mut [u32], values: &mut [u64], count: usize, hidden: &[u32]) -> usize {
    let mut kept = 0;
    for index in 0..count {
        if hidden.contains(&ids[index]) {
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
pub(crate) fn obj_get_properties(arg: usize, ofd: u64) -> isize {
    // struct drm_mode_obj_get_properties: props_ptr@0, prop_values_ptr@8,
    // count_props@16, obj_id@20, obj_type@24.
    let obj_id = unsafe { ((arg + 20) as *const u32).read_unaligned() };
    let obj_type = unsafe { ((arg + 24) as *const u32).read_unaligned() };
    // (ids, values, count) - the arrays are fixed-size so the lock only
    // guards a few word reads. Sixteen slots is the longest list any object
    // here carries, and the count is what says how much of it is real.
    let Some((ids, values, count)) = critical_section::with(|cs| {
        let st = STATE.borrow(cs).borrow();
        // `DRM_MODE_OBJECT_ANY` is the id deciding rather than the kind, so a
        // caller that named only an id is answered for whatever that id is -
        // Linux's `__drm_mode_object_find`, and the shape a client reads a
        // property's *value* with. An id nothing here holds comes back `None`
        // from that same lookup, and this door's answer to it is the `EINVAL` a
        // wrong kind has always got.
        let obj_type = match obj_type {
            super::abi::OBJECT_ANY => match object_kind(&st, obj_id) {
                Some(kind) => kind,
                None => return None,
            },
            named => named,
        };
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
                        st.plane_blend,
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
                        PROP_HOTSPOT_X,
                        PROP_HOTSPOT_Y,
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
                        st.cursor_hotspot.0 as i64 as u64,
                        st.cursor_hotspot.1 as i64 as u64,
                        0,
                        0,
                    ],
                    // Both hotspots are in the list, so both are in the count.
                    // Thirteen enumerated `HOTSPOT_X` (index 12) and left
                    // `HOTSPOT_Y` (13) sitting past the end of its own array -
                    // which measured as a client that had set the cap being
                    // shown one of the two properties the cap is for.
                    14usize,
                ))
            }
            // The encoder this card advertises - `GETRESOURCES` names it and
            // `GETCONNECTOR` names it back - and it carries no properties.
            //
            // Answering *that* rather than refusing is the whole of this arm.
            // A compositor enumerates what the card listed and asks each object
            // for its properties, and a refusal is not an empty list to it: it
            // is the primary node failing to open. Measured on niri, which said
            // `error adding primary node device, display-only devices may not
            // work: DRM access error: Error reading properties on device
            // Some("/dev/dri/card0") (Invalid argument (os error 22))` - the
            // EINVAL this door used to answer for the one object it did not
            // recognise, on both cards, renderer or not (`docs/notes/` §7.4).
            // Linux's own answer for an object that exists is a count and
            // nothing to fill, which is what this arm gives.
            (OBJECT_ENCODER, ENCODER_ID) => Some(([0; 16], [0u64; 16], 0)),
            // A **framebuffer** is an object a client can name, but it carries
            // no property list: Linux's `drm_mode_obj_get_properties_ioctl`
            // resolves the id and then refuses an object whose
            // `base.properties` is null, which every framebuffer's is. Measured
            // on a current host (`.workbuddy/probe_fb_props.c` and
            // `probe_drm_fb_owner.c`, Linux 7.2.11): an fb this description
            // made, an fb another description made, and the same id under
            // `OBJECT_ANY` all answer **`EINVAL`** - the reply this card used
            // to give only for an unrecognised type, and which it now gives
            // for the fb too. The empty list this arm used to return was a
            // workaround for the device-wide `GETRESOURCES` list above: once
            // that list is the description's own, a client is never handed an
            // fb it did not make, and Linux's answer is the honest one.
            //
            // What is *not* true is the shape this file's note used to carry -
            // that Linux publishes `width`, `height`, `format` and `modifier`
            // on an fb. It does not: those are `struct drm_framebuffer`
            // fields, and `GETFB2` is the request that reports them. The
            // measurement corrected that paragraph rather than building it.
            _ => None,
        };
        let (mut ids, mut values, count) = list?;
        // What this client is shown. A description that has not set a cap is not
        // shown the properties behind it *at all* - not a smaller list of values,
        // but a shorter list of ids - which is what the measurement on this host's
        // card found on a CRTC (six properties uncapped, nine capped) and on a
        // connector (seventeen and eighteen, the difference being `CRTC_ID`). Two
        // gated families run through the same filter here: the atomic properties for
        // `DRM_CLIENT_CAP_ATOMIC`, and the cursor plane's hotspot pair for
        // `DRM_CLIENT_CAP_CURSOR_PLANE_HOTSPOT`. Asking for one by id is a different
        // door and is not filtered here; see [`get_property`].
        let mut count = count;
        if !st.atomic_ofds.contains(&ofd) {
            count = without_props(&mut ids, &mut values, count, &ATOMIC_PROPS);
        }
        if !st.hotspot_ofds.contains(&ofd) {
            count = without_props(&mut ids, &mut values, count, &HOTSPOT_PROPS);
        }
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
