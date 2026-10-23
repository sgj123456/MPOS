use super::abi::{
    DRM_CLOEXEC, DRM_FORMAT_ARGB8888, DRM_FORMAT_XRGB8888, FB_MODIFIERS, MAX_HANDLES,
};
use super::ioctl::libc_einval;
use super::props::host_cursor_live;
use super::state::{DrmState, Dumb, Framebuffer, PrimeRecord, STATE};
use super::virtgpu::CONTEXTS;

use crate::mm::{PAGE_SIZE, clone_frame_ref};
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;

pub(crate) struct FbShape {
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
pub(crate) fn insert_fb(st: &mut DrmState, handle: u32, shape: FbShape, owner: u64) -> Option<u32> {
    let slot = handle as usize;
    // The handle's buffer must exist - and the geometry it comes with has to
    // be a shape the scanout can show: the handle's own bpp (the scanout is
    // a 32-bit BGRA surface, whatever the ADDFB claims), nonzero, with a
    // pitch wide enough for a row. The name has to be *live* as well: an
    // `ADDFB2` naming a handle its file has released is `ENOENT` on Linux
    // (`drm_mode_addfb2` looks it up in the file's handle table), so a buffer
    // kept alive by some *other* framebuffer is not a buffer this request may
    // bind.
    let dumb = st
        .dumb
        .get_mut(slot.checked_sub(1)?)
        .and_then(|d| d.as_mut())?;
    if dumb.bpp != 32
        || shape.bpp != 32
        || shape.width == 0
        || shape.height == 0
        || shape.pitch < shape.width * 4
        || dumb.holders.is_empty()
    {
        return None;
    }
    // The framebuffer holds the buffer from here: this is what keeps a
    // compositor's scanout alive across the release of the handle it was made
    // from, and it is the hold [`drop_fb_hold`] gives back.
    dumb.fb_refs += 1;
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
pub(crate) fn legacy_fb_format(bpp: u32, depth: u32) -> u32 {
    match (bpp, depth) {
        (32, 24) => DRM_FORMAT_XRGB8888,
        (32, 32) => DRM_FORMAT_ARGB8888,
        _ => 0,
    }
}

pub(crate) fn add_fb(arg: usize, owner: u64) -> isize {
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

pub(crate) fn add_fb2(arg: usize, owner: u64) -> isize {
    // struct drm_mode_fb_cmd2: fb_id@0,width@4,height@8,pixel_format@12,
    // flags@16, handles[4]@20, pitches[4]@36, offsets[4]@52, modifier[4]@72.
    // The four `__u64` start at 72 and not at 68, because `offsets` ends at 68
    // and a `u64` cannot begin there - the gap is the padding `drm_mode.h` has
    // at 68..72, measured (`sizeof` 104, `offsetof(modifier)` 72) rather than
    // counted. Reading at 68 answers the *padding* plus the low half of
    // `modifier[0]`, which is zero for every modifier a caller writes at the
    // offset Linux reads - so the bug was invisible to a caller that only ever
    // names LINEAR, and only a non-linear modifier put at 72 showed it.
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
    let modifier = unsafe { ((arg + 72) as *const u64).read_unaligned() };
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
pub(crate) fn get_fb(arg: usize) -> isize {
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
pub(crate) fn get_fb2(arg: usize) -> isize {
    // struct drm_mode_fb_cmd2: fb_id@0, width@4, height@8, pixel_format@12,
    // flags@16, handles[4]@20, pitches[4]@36, offsets[4]@52, modifier[4]@72 -
    // the offsets `add_fb2` reads, which this answers in the other direction.
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
                w64(72 + plane * 8, if first { fb.modifier } else { 0 });
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
pub(crate) fn gamma_refused() -> isize {
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
pub(crate) fn sequence_refused() -> isize {
    -25 // ENOTTY
}

/// The device's side of a buffer leaving the handle table, collected under the
/// lock and acted on outside it.
///
/// Every field is a *device command*'s argument, and a command parks - so the
/// caller has to be out of the critical section before it can run any of them.
/// [`take_buffer`] collects, [`tell_buffer_gone`] speaks.
pub(crate) struct BufferGone {
    /// The device resource the buffer was backed by, when it was one. A plain
    /// dumb buffer was never a resource and this is `None`.
    res: Option<u32>,
    /// The contexts that had been told about that resource, which have to be
    /// told it is gone before the resource itself is unref'd.
    holders: Vec<u32>,
}

/// Take `handle`'s buffer out of the table, with everything the device has to be
/// told about it.
///
/// The caller has checked the buffer is there. Called when its **last** holder
/// goes: the client's handle (`GEM_CLOSE`/`DESTROY_DUMB`), or the last
/// framebuffer made from it ([`drop_fb_hold`]). Linux spells the same rule with a
/// reference count on the GEM object - the file's handle is one holder and
/// `drm_framebuffer_init` is another - and the difference between "the handle is
/// gone" and "the buffer is gone" is exactly what [`destroy_handle`] records.
fn take_buffer(
    st: &mut DrmState,
    cs: critical_section::CriticalSection<'_>,
    handle: u32,
) -> BufferGone {
    let res = st
        .dumb
        .get(handle as usize - 1)
        .and_then(|slot| slot.as_ref())
        .and_then(|d| d.resource.as_ref())
        .map(|r| r.id);
    if let Some(slot) = st.dumb.get_mut(handle as usize - 1) {
        if let Some(d) = slot.as_mut() {
            d.release();
        }
        *slot = None;
    }
    // The dma-buf records that pointed at this object go with it: the number is
    // about to be handed to the next create, and a record left behind would answer
    // a later import with a name that now belongs to somebody else's buffer.
    st.prime.retain(|record| record.handle != handle);
    // The contexts that had been told about this resource forget it here, in the
    // same section that takes the resource out of the table: the id is about to
    // become free for the next create to claim, and a context that kept "told
    // about id N" would then never send the attach for the resource that lands on
    // N - the same dropped batch this card was fixed for, one reuse later.
    let holders = match res {
        Some(id) => CONTEXTS
            .borrow(cs)
            .borrow_mut()
            .iter_mut()
            .filter_map(|context| {
                let held = context.attached.contains(&id);
                context.attached.retain(|attached| *attached != id);
                held.then_some(context.id)
            })
            .collect(),
        None => Vec::new(),
    };
    BufferGone { res, holders }
}

/// Tell the device what [`take_buffer`] collected: the mirror of the attach a
/// context was given, then the unref of the resource itself.
///
/// The detach goes out first, so the device is never left holding a binding to an
/// object it has already let go of. Both answers are ignored for the same reason:
/// the buffer has left the table either way, and a device that refuses to forget
/// a resource whose pages are back in the allocator is not something a caller can
/// act on.
pub(crate) fn tell_buffer_gone(gone: BufferGone) {
    let Some(id) = gone.res else {
        // A plain dumb buffer was never a resource, so the device holds nothing
        // to let go of.
        return;
    };
    for ctx in gone.holders {
        let _ = crate::drivers::video::render::job(crate::drivers::video::render::Job::CtxDetach {
            ctx,
            ids: [id].to_vec(),
        });
    }
    let _ = crate::drivers::video::render::job(crate::drivers::video::render::Job::Unref { id });
}

/// Give back the hold one framebuffer had on its buffer, and answer the device's
/// side of it when that was the buffer's last hold.
///
/// The other half of [`insert_fb`]'s `fb_refs += 1`, and the half that makes a
/// compositor's scanout outlive its handle: the buffer leaves the table when the
/// framebuffer that named it goes, *provided* the client has already released the
/// handle. `None` means something still holds it - either another framebuffer, or
/// the client's own name.
pub(crate) fn drop_fb_hold(
    st: &mut DrmState,
    cs: critical_section::CriticalSection<'_>,
    handle: u32,
) -> Option<BufferGone> {
    let slot = st.dumb.get_mut(handle.checked_sub(1)? as usize)?.as_mut()?;
    slot.fb_refs = slot.fb_refs.saturating_sub(1);
    if slot.fb_refs > 0 || !slot.holders.is_empty() {
        return None;
    }
    Some(take_buffer(st, cs, handle))
}

/// Take the host pointer down for a cursor resource that is going away: the
/// cursor-queue hide first, then the control-queue unref of the resource it drew
/// from, in that order, so the device never loads an image whose pages are
/// already back in the allocator.
///
/// A no-op on a card with no cursor queue: the software path draws the pointer
/// into the frame and keeps no resource of its own to release.
pub(crate) fn hide_host_cursor(id: u32) {
    if !host_cursor_live() {
        return;
    }
    // A task in its own exit path cannot park for these: `job` posts them
    // fire-and-forget when the caller has already exited (the exit-path hide
    // parked a dying task forever, and the parent's `wait` never returned).
    let _ = crate::drivers::video::render::job(crate::drivers::video::render::Job::SetCursor {
        id: 0,
        x: 0,
        y: 0,
        hot_x: 0,
        hot_y: 0,
    });
    let _ = crate::drivers::video::render::job(crate::drivers::video::render::Job::DestroyCursor {
        id,
    });
}

/// Release a GEM handle - the one path `GEM_CLOSE` and `DESTROY_DUMB` both spell.
/// A name *this file* does not hold is `EINVAL`, the refusal Linux makes too; the
/// dma-buf records that name the object are not touched, because they are the
/// object's (`take_buffer` drops them with it) and a later import of the same
/// descriptor has to find it.
///
/// What does *not* happen here is the buffer going away while anything else still
/// refers to it. Linux's `drm_gem_handle_delete` drops the file's handle and
/// nothing else: the object lives while anything references it, and both a
/// framebuffer made from it (`drm_framebuffer_init` holds the GEM object) and
/// **another file's name** are such references - the handle table is per
/// `drm_file`. A client that closes the handle after `ADDFB` is therefore doing
/// nothing wrong - it is the idiom Mesa's GBM allocator and Aquamarine use,
/// because the framebuffer is all the kernel needs to flip - and this card got it
/// wrong by freeing the buffer: the framebuffer id stayed listed by `GETRESOURCES`
/// while every commit naming it was refused (the fit test asks whether the buffer
/// is there), so the compositor came up with no picture and nothing to read. Now
/// the *name* goes ([`Dumb::holders`]), the buffer stays, and [`drop_fb_hold`]
/// frees it when the last framebuffer that holds it goes.
pub(crate) fn destroy_handle(handle: usize, ofd: u64) -> isize {
    if handle == 0 {
        return libc_einval();
    }
    // The device resource id, read while the buffer is in the table. Taken out
    // of the critical section rather than dropped with the rest because telling
    // the device is a *job*, and a job parks: parking with interrupts off is
    // sleeping while holding a lock, which this kernel forbids.
    //
    // `None` answers "there is no such handle" (`EINVAL`, as before); `Some` with
    // a `None` buffer answers "the name went, the buffer is still held"; `Some`
    // with a buffer is the buffer leaving, which is what the caller has to tell
    // the device about.
    let mut closed_a_held_name: Option<usize> = None;
    let found = critical_section::with(|cs| {
        let mut st = STATE.borrow(cs).borrow_mut();
        // What the number is, before anything else: a handle this card never made,
        // and a name *this file* does not hold, are the same answer (`EINVAL`),
        // because Linux asks the file's own handle table - a released name is not
        // in it, while the buffer it named may still be held by a framebuffer or by
        // another file's name, which are facts about the buffer and not about this
        // name. The prime records are *not* dropped here: they name the object, so
        // a later import of the same dma-buf still finds it, and they go with the
        // object in `take_buffer`.
        let Some(buffer) = st.dumb.get(handle - 1).and_then(|slot| slot.as_ref()) else {
            return None;
        };
        if !buffer.holders.contains(&ofd) {
            return None;
        }
        if let Some(d) = st.dumb.get_mut(handle - 1).and_then(|slot| slot.as_mut()) {
            d.holders.retain(|holder| *holder != ofd);
        }
        // Somebody still names the buffer - another file that imported the same
        // dma-buf, or the client's own second open - or a framebuffer made from it
        // does: the name goes and nothing else does. Not the host cursor either,
        // because the binding belongs to the framebuffer (see `set_cursor_prop`)
        // and that framebuffer is still there; the fb's own release takes it down
        // (`drop_fb_hold`).
        let held_by_fb = st
            .fbs
            .iter()
            .filter_map(|f| f.as_ref())
            .any(|f| f.handle as usize == handle);
        let named_still = st
            .dumb
            .get(handle - 1)
            .and_then(|slot| slot.as_ref())
            .is_some_and(|d| !d.holders.is_empty());
        if held_by_fb || named_still {
            if held_by_fb {
                closed_a_held_name = Some(handle);
            }
            return Some((None, None));
        }
        // If this handle backs the framebuffer the host pointer is drawn from, the
        // cursor binding and its separate device resource go with it. Cleared here so
        // the caller hides + releases the cursor *before* the `Unref` below - push
        // order keeps the device from ever seeing an `UPDATE_CURSOR` name a resource
        // whose pages are already gone. The cursor's own resource id differs from the
        // dumb buffer's `res`, so the two releases do not collide.
        let cursor_died = st
            .fbs
            .iter()
            .filter_map(|f| f.as_ref())
            .any(|f| f.id == st.cursor_fb && f.handle as usize == handle)
            .then(|| st.cursor_resource.take().map(|(id, _)| id))
            .flatten();
        if cursor_died.is_some() {
            st.cursor_fb = 0;
        }
        Some((cursor_died, Some(take_buffer(&mut st, cs, handle as u32))))
    });
    let Some((cursor_died, gone)) = found else {
        return libc_einval();
    };
    // A name released while a framebuffer holds the buffer. Nothing else records
    // it and it matters twice over: the buffer stays (which is the point of the
    // hold), and this number is one a later `EXECBUFFER` has to answer about - the
    // submit path refuses a released name with `ENOENT`, and Mesa turns that into
    // `got error from kernel - expect bad rendering 2` and marks its winsys dead.
    if let Some(handle) = closed_a_held_name {
        crate::println!(
            "mpos-dbg: name {handle} released, a framebuffer is holding the buffer (ofd={ofd})"
        );
    }
    // Hide and release the host cursor first (a cursor-queue command, then the
    // control-queue unref of its resource), before the buffer's own unref below.
    if let Some(cur_id) = cursor_died {
        hide_host_cursor(cur_id);
    }
    if let Some(gone) = gone {
        tell_buffer_gone(gone);
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
pub(crate) fn prime_handle_to_fd(arg: usize, ofd: u64) -> isize {
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
        let dumb = st
            .dumb
            .get_mut(handle - 1)
            .and_then(|slot| slot.as_mut())
            // A released name exports nothing. The descriptor the handle already
            // gave away keeps its own lifetime, but this number names no buffer for
            // anyone - `drm_gem_prime_handle_to_fd` looks the handle up first.
            .filter(|d| !d.holders.is_empty())?;
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
    crate::println!(
        "mpos-dbg: PRIME export <- handle {handle} (ofd={ofd}, ino={})",
        buf.ino()
    );
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
pub(crate) fn prime_fd_to_handle(arg: usize, ofd: u64) -> isize {
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
    let (handle, reused) = critical_section::with(|cs| {
        let mut st = STATE.borrow(cs).borrow_mut();
        let st = &mut *st;
        // The same object the card already knows: its handle, so repeated
        // imports never stack up aliases of one buffer - and the importing file
        // gets its *own* name on it, because a name is the file's. That is what
        // lets one file release its handle while another goes on submitting the
        // batch that names it.
        let known = st
            .prime
            .iter()
            .find(|record| record.ino == buf.ino())
            .map(|record| record.handle);
        if let Some(handle) = known {
            if let Some(d) = st
                .dumb
                .get_mut(handle as usize - 1)
                .and_then(|slot| slot.as_mut())
                && !d.holders.contains(&ofd)
            {
                d.holders.push(ofd);
            }
            return (handle, true);
        }
        if st.dumb.len() >= MAX_HANDLES {
            return (0u32, false);
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
            // This file's own name for it: nobody has made a framebuffer from
            // this number yet, and *this* file has not released it.
            fb_refs: 0,
            holders: vec![ofd],
        }));
        let handle = st.dumb.len() as u32;
        st.prime.push(PrimeRecord {
            ino: buf.ino(),
            handle,
            buf: None,
        });
        (handle, false)
    });
    if handle == 0 {
        return libc_einval(); // the handle table is full
    }
    crate::println!(
        "mpos-dbg: PRIME import -> handle {handle} (ofd={ofd}, ino={}, {}x{}, reused={reused})",
        buf.ino(),
        buf.width(),
        buf.height()
    );
    unsafe { (arg as *mut u32).write_unaligned(handle) };
    0
}
