use super::abi::{CRTC_ID, CURSOR_BO, CURSOR_FLAGS, CURSOR_MOVE, PAGE_FLIP_EVENT};
use super::ioctl::libc_einval;
use super::master::{LeaseSet, leased, queue_flip_event};
use super::props::{copy_to_scanout, cursor_fb_ok, host_cursor_live};
use super::state::STATE;

use crate::mm::PAGE_SIZE;
use alloc::vec::Vec;

pub(crate) fn page_flip(arg: usize, lease: Option<&LeaseSet>) -> isize {
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
pub(crate) fn mode_cursor(arg: usize, hot: bool, lease: Option<&LeaseSet>) -> isize {
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

    // Present. On a device with a cursor queue the pointer is the host's to draw,
    // so this hands the image and position over (`show_cursor`) and does *not*
    // re-copy the master's framebuffer or flush it - a move costs one small
    // cursor-queue command, not a full gather. On a device without one, the pointer
    // is blended into the scanout, which means the frame has to be rebuilt around it
    // exactly as before: the master's framebuffer is copied again with the new image
    // and position, then flushed. This is what `SETCRTC` and `PAGE_FLIP` do for their
    // own frames, and it is why a software-path cursor move is as prompt as a commit.
    if host_cursor_live() {
        let _ = show_cursor();
        return 0;
    }
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

/// Hand the cursor plane's current state to the host as a hardware pointer, on the
/// device's cursor queue. This is what makes a pointer appear on a virtualized
/// display: the host composites it over whatever the guest scanout shows, so the
/// kernel neither blends it into the frame nor re-copies the master's framebuffer to
/// move it. [`host_cursor_live`] gates this against the software path.
///
/// Three cases, decided by what changed since the last call and read off
/// [`DrmState::cursor_resource`]:
///   * **no image bound** (`cursor_fb == 0`) - hide: `UPDATE_CURSOR` with resource 0,
///     and drop any tracked resource.
///   * **same image, moved** - `MOVE_CURSOR` alone: no resource work, which is the
///     whole point of keeping the two commands apart. A pointer-motion event costs
///     one small cursor-queue command, not a framebuffer gather and flush.
///   * **new image** (a different `fb_id`, or none shown yet) - create + attach +
///     transfer the resource on the control queue, then `UPDATE_CURSOR` on the
///     cursor queue. The three control commands are pushed before the update so
///     `drain_jobs`' FIFO order fences them; the device has the pixels by the time
///     it is told to load them.
///
/// The position sent is `cursor_pos - cursor_hotspot`: the device places the image's
/// top-left at that point, and `cursor_pos` is the pointer point, so the hotspot is
/// subtracted here rather than in [`crate::gpu::cursor::compose`] (which keeps doing
/// its own subtraction for the software fallback). That is why accepting the hotspot
/// cap does not shift the pointer: the coordinate convention moves with the path, and
/// both land the tip where the compositor meant it.
///
/// Runs entirely outside the STATE critical section except for reading the fields it
/// needs and recording the new resource id - the device round trips park the caller,
/// and parking while holding `STATE` is forbidden (`back_dumb_with_a_resource` says
/// the same). Returns an errno the caller may ignore: a failed host cursor leaves the
/// recorded state intact and the next commit retries.
pub(crate) fn show_cursor() -> isize {
    if !host_cursor_live() {
        return 0;
    }
    // Read everything the decision needs under one lock, including the buffer's
    // frames materialized for a fresh binding, so no device command is built from
    // state that can change underneath it.
    enum Want {
        Hide,
        Move {
            x: u32,
            y: u32,
        },
        Show {
            fb_id: u32,
            entries: Vec<(u64, u32)>,
            width: u32,
            height: u32,
            x: u32,
            y: u32,
            hot_x: u32,
            hot_y: u32,
        },
    }
    let want = critical_section::with(|cs| -> Result<Want, isize> {
        let mut st = STATE.borrow(cs).borrow_mut();
        if st.cursor_fb == 0 {
            return Ok(Want::Hide);
        }
        // Copy out everything the framebuffer contributes *before* touching the
        // dumb table mutably below; `fb` borrows `st.fbs` and the page-growing path
        // needs `&mut st.dumb`, which cannot overlap it.
        let (handle, width, height) = match st
            .fbs
            .iter()
            .filter_map(|f| f.as_ref())
            .find(|f| f.id == st.cursor_fb)
        {
            Some(fb) => (fb.handle as usize - 1, fb.width, fb.height),
            None => return Ok(Want::Hide),
        };
        let fb_id = st.cursor_fb;
        // The pointer point minus the hotspot is where the image's top-left goes.
        let (px, py) = st.cursor_pos;
        let (hx, hy) = st.cursor_hotspot;
        let (x, y) = (px - hx, py - hy);
        // A move only if the very same framebuffer is already showing on the
        // host, and a `MOVE_CURSOR` when it is - which is Linux's rule
        // (`virtio_gpu_cursor_plane_update`: the transfer and the
        // `UPDATE_CURSOR` are both gated on `fb != old_fb`, and the unchanged
        // case sends `MOVE_CURSOR`).
        //
        // Re-posting `UPDATE_CURSOR` instead was tried and measured not to
        // work, which is why the comment on the device side says so: the
        // host's `update_cursor` copies the resource's *host-side* image, and
        // only `TRANSFER_TO_HOST_2D` refills that image - so an update without
        // a transfer re-sends whatever the last transfer left, exactly as a
        // bare move would, at the cost of a command that names the resource
        // again. `.workbuddy/curprobe.c` is the measurement: a 64x64 buffer
        // refilled blue and re-posted kept showing the red it was bound with,
        // on both the 2D and the GL card.
        if st
            .cursor_resource
            .is_some_and(|(_, shown_fb)| shown_fb == fb_id)
        {
            return Ok(Want::Move {
                x: x.max(0) as u32,
                y: y.max(0) as u32,
            });
        }
        // Otherwise bind afresh. A GBM/PRIME-imported cursor buffer arrives with its
        // pages *promised but not yet backed* - weston maps and writes it through the
        // dma-buf, and demand paging fills a frame only when a byte is touched - so
        // the device would read nowhere for any page not yet materialized. Grow them
        // here, exactly as `back_dumb_with_a_resource` does for a plain dumb buffer:
        // this is the mutable borrow that path needs and an immutable one could not
        // take. Refusing instead was the bug: the first SHOW saw "0 of 4 pages", made
        // no resource, and every later event was a MOVE against a host that had never
        // been given an image - a pointer that moved and clicked and stayed invisible.
        let dumb = match st.dumb.get_mut(handle).and_then(|s| s.as_mut()) {
            Some(d) => d,
            None => return Ok(Want::Hide),
        };
        let pages = (dumb.size() as usize).div_ceil(PAGE_SIZE);
        for page in 0..pages {
            if dumb.ensure_page(page) == 0 {
                return Err(-12); // ENOMEM - the allocator refused, retry next commit
            }
        }
        let entries: Vec<(u64, u32)> = dumb
            .frames
            .iter()
            .take(pages)
            .map(|&p| (p as u64, PAGE_SIZE as u32))
            .collect();
        Ok(Want::Show {
            fb_id,
            entries,
            width,
            height,
            x: x.max(0) as u32,
            y: y.max(0) as u32,
            hot_x: hx.max(0) as u32,
            hot_y: hy.max(0) as u32,
        })
    });
    let want = match want {
        Ok(w) => w,
        Err(errno) => return errno,
    };

    match want {
        Want::Hide => {
            let old = critical_section::with(|cs| {
                let mut st = STATE.borrow(cs).borrow_mut();
                st.cursor_resource.take()
            });
            // Hide the host pointer first, then release its resource - the device
            // must never be left naming a resource it no longer has.
            let _ =
                crate::drivers::video::render::job(crate::drivers::video::render::Job::SetCursor {
                    id: 0,
                    x: 0,
                    y: 0,
                    hot_x: 0,
                    hot_y: 0,
                });
            if let Some((id, _)) = old {
                let _ = crate::drivers::video::render::job(
                    crate::drivers::video::render::Job::DestroyCursor { id },
                );
            }
            0
        }
        Want::Move { x, y } => {
            crate::drivers::video::render::job(crate::drivers::video::render::Job::MoveCursor {
                x,
                y,
            })
        }
        Want::Show {
            fb_id,
            entries,
            width,
            height,
            x,
            y,
            hot_x,
            hot_y,
        } => {
            // The cursor's resource id is this driver's own constant, in the
            // high range beside the scanout's framebuffer - not a number from
            // the client's id space, which a command stream can name. One id is
            // enough because the discipline below is destroy-before-create: the
            // device holds at most one cursor resource at a time, and the next
            // image reuses the same id the moment the old one is gone.
            let (id, old) = critical_section::with(|cs| {
                let mut st = STATE.borrow(cs).borrow_mut();
                let old = st.cursor_resource.take();
                (crate::gpu::abi::RESOURCE_CURSOR, old)
            });
            if let Some((old_id, _)) = old {
                let _ = crate::drivers::video::render::job(
                    crate::drivers::video::render::Job::DestroyCursor { id: old_id },
                );
            }
            // Control queue: make the resource, back it, fill it. Each completes
            // synchronously before the next job drains, so these three land before
            // the cursor-queue update below names the resource.
            if crate::drivers::video::render::job(
                crate::drivers::video::render::Job::CreateCursor2d {
                    id,
                    width,
                    height,
                    entries,
                },
            ) != 0
            {
                return -5; // EIO
            }
            if crate::drivers::video::render::job(
                crate::drivers::video::render::Job::TransferCursor { id, width, height },
            ) != 0
            {
                let _ = crate::drivers::video::render::job(
                    crate::drivers::video::render::Job::DestroyCursor { id },
                );
                return -5; // EIO
            }
            // Cursor queue: load and place it.
            let rc =
                crate::drivers::video::render::job(crate::drivers::video::render::Job::SetCursor {
                    id,
                    x,
                    y,
                    hot_x,
                    hot_y,
                });
            if rc == 0 {
                critical_section::with(|cs| {
                    STATE.borrow(cs).borrow_mut().cursor_resource = Some((id, fb_id));
                });
            } else {
                let _ = crate::drivers::video::render::job(
                    crate::drivers::video::render::Job::DestroyCursor { id },
                );
            }
            rc
        }
    }
}
