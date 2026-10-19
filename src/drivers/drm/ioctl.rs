use super::card::{DrmCard, create_dumb, map_dumb};
use super::fb::{
    BufferGone, add_fb, add_fb2, destroy_handle, drop_fb_hold, gamma_refused, get_fb, get_fb2,
    hide_host_cursor, prime_fd_to_handle, prime_handle_to_fd, sequence_refused, tell_buffer_gone,
};
use super::flip::{mode_cursor, page_flip};
use super::master::{
    auth_magic, create_lease, get_lease, list_lessees, master_ok, master_request, revoke_lease,
    take_magic,
};
use super::props::{
    create_blob, destroy_blob, get_cap, get_connector, get_crtc, get_encoder, get_plane,
    get_propblob, get_property, get_resources, mode_atomic, obj_get_properties, obj_set_property,
    set_client_cap, set_crtc, set_property, set_version,
};
use super::state::STATE;
use super::sync::{
    syncobj_create, syncobj_destroy, syncobj_eventfd, syncobj_fd_to_handle, syncobj_handle_to_fd,
    syncobj_query, syncobj_reset, syncobj_signal, syncobj_timeline_signal, syncobj_timeline_wait,
    syncobj_transfer, syncobj_wait,
};
use super::virtgpu::{
    virtgpu_context_init, virtgpu_execbuffer, virtgpu_get_caps, virtgpu_getparam, virtgpu_map,
    virtgpu_resource_create, virtgpu_resource_info, virtgpu_transfer_from_host,
    virtgpu_transfer_to_host, virtgpu_wait,
};

use super::abi::{
    CURSOR_PLANE_ID, NR_ADDFB, NR_ADDFB2, NR_AUTH, NR_AUTH_MAGIC, NR_CREATE_BLOB, NR_CREATE_DUMB,
    NR_DESTROY_BLOB, NR_DESTROY_DUMB, NR_DROP_MASTER, NR_GEM_CLOSE, NR_GET_CAP, NR_GET_MAGIC,
    NR_GETCONNECTOR, NR_GETCRTC, NR_GETENCODER, NR_GETFB, NR_GETFB2, NR_GETPLANE,
    NR_GETPLANERESOURCES, NR_GETPROPBLOB, NR_GETPROPERTY, NR_GETRESOURCES, NR_MAP_DUMB,
    NR_MODE_ATOMIC, NR_MODE_CREATE_LEASE, NR_MODE_CURSOR, NR_MODE_CURSOR2, NR_MODE_GET_LEASE,
    NR_MODE_LIST_LESSES, NR_MODE_REVOKE_LEASE, NR_OBJ_GETPROPERTIES, NR_OBJ_SETPROPERTY,
    NR_PAGE_FLIP, NR_PRIME_FD_TO_HANDLE, NR_PRIME_HANDLE_TO_FD, NR_RMFB, NR_SET_CLIENT_CAP,
    NR_SET_MASTER, NR_SET_VERSION, NR_SETCRTC, NR_SETPROPERTY, NR_SYNCOBJ_CREATE,
    NR_SYNCOBJ_DESTROY, NR_SYNCOBJ_EVENTFD, NR_SYNCOBJ_FD_TO_HANDLE, NR_SYNCOBJ_HANDLE_TO_FD,
    NR_SYNCOBJ_QUERY, NR_SYNCOBJ_RESET, NR_SYNCOBJ_SIGNAL, NR_SYNCOBJ_TIMELINE_SIGNAL,
    NR_SYNCOBJ_TIMELINE_WAIT, NR_SYNCOBJ_TRANSFER, NR_SYNCOBJ_WAIT, NR_VERSION,
    NR_VIRTGPU_CONTEXT_INIT, NR_VIRTGPU_EXECBUFFER, NR_VIRTGPU_GET_CAPS, NR_VIRTGPU_GETPARAM,
    NR_VIRTGPU_MAP, NR_VIRTGPU_RESOURCE_CREATE, NR_VIRTGPU_RESOURCE_CREATE_BLOB,
    NR_VIRTGPU_RESOURCE_INFO, NR_VIRTGPU_TRANSFER_FROM_HOST, NR_VIRTGPU_TRANSFER_TO_HOST,
    NR_VIRTGPU_WAIT, PLANE_ID,
};

use alloc::vec::Vec;

pub(crate) fn fill_version(buf: usize) -> isize {
    // struct drm_version: three ints (then four bytes of padding), and a
    // length/pointer pair each for the name, the date and the description.
    const FIELDS: [(&[u8], usize, usize); 3] =
        [(b"virtio_gpu", 16, 24), (b"", 32, 40), (b"", 48, 56)];

    unsafe {
        let w = |off: usize, v: i32| ((buf + off) as *mut i32).write_unaligned(v);
        w(0, 0); // major
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
            let to_copy = room.min(text.len());
            given[field] = if dst.is_null() {
                // The sizing call: it wants the length and brings no room.
                text.len()
            } else if to_copy == 0 {
                // Nothing to copy, so there is no address to *check*. A caller
                // that asked for a length and was told zero reserves no room,
                // and the pointer it then hands this field is the one an empty
                // `Vec` carries - non-null, and nowhere near a mapping. Linux's
                // `copy_to_user` of zero bytes cannot fault, so answering
                // `EFAULT` for it is a refusal the caller has no way to act on:
                // it is what stopped niri with `Failed to query drm driver on
                // device Some("/dev/dri/card0") (Bad address (os error 14))`
                // on its way from the connector it had just found to a surface
                // (`docs/notes/` §7.4). The same shape `get_resources` already
                // carries for its own zero-length lists.
                0
            } else if !user_array(dst as usize, to_copy, 1, true) {
                // Room was offered, but not at that address: refuse the copy
                // and report the length, which is what Linux's failed
                // `copy_to_user` leaves the caller with.
                return -14; // EFAULT
            } else {
                core::ptr::copy_nonoverlapping(text.as_ptr(), dst, to_copy);
                to_copy
            };
            len.write_unaligned(given[field]);
        }
    }
    0
}

pub(crate) fn libc_einval() -> isize {
    -22
}

/// Serve a DRM ioctl on the card. `nr` selects the request - the syscall
/// layer has already decoded it out of the request word, whichever half of
/// it carried the command number; `arg` is the user pointer to the request's
/// struct. Every struct is copied in whole, filled per the fixed topology,
/// and (for the in-out KMS structs) copied back - matching how libdrm passes
/// them.
pub fn drm_request(fd: i32, nr: u32, arg: usize) -> isize {
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
        super::abi::NR_GEM_CLOSE | super::abi::NR_DESTROY_DUMB => {
            // struct drm_gem_close / drm_mode_destroy_dumb: handle@0. The
            // two spell one release - a GEM handle and a dumb buffer are the
            // same object here, and both names may close it.
            let handle = unsafe { (arg as *const u32).read_unaligned() } as usize;
            destroy_handle(handle, ofd)
        }
        NR_PRIME_HANDLE_TO_FD => prime_handle_to_fd(arg, ofd),
        NR_PRIME_FD_TO_HANDLE => prime_fd_to_handle(arg, ofd),
        NR_GETRESOURCES => get_resources(arg, ofd),
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
        super::abi::NR_MODE_GETGAMMA | super::abi::NR_MODE_SETGAMMA => gamma_refused(),
        super::abi::NR_CRTC_GET_SEQUENCE | super::abi::NR_CRTC_QUEUE_SEQUENCE => sequence_refused(),
        NR_ADDFB => add_fb(arg, ofd),
        NR_ADDFB2 => add_fb2(arg, ofd),
        NR_RMFB => {
            let id = unsafe { (arg as *const u32).read_unaligned() };
            let (cursor_died, gone) = critical_section::with(|cs| {
                let mut st = STATE.borrow(cs).borrow_mut();
                // What the framebuffers being removed each held, read before they
                // go: a handle the client released earlier stays alive only as long
                // as a framebuffer names it, and this is one of the two doors that
                // takes such a framebuffer away (`release_framebuffers` is the
                // other - the description closing).
                let held: Vec<u32> = st
                    .fbs
                    .iter()
                    .filter_map(|f| f.as_ref())
                    .filter(|f| f.id == id)
                    .map(|f| f.handle)
                    .collect();
                for fb in st.fbs.iter_mut() {
                    if fb.as_ref().is_some_and(|f| f.id == id) {
                        *fb = None;
                    }
                }
                let gone: Vec<BufferGone> = held
                    .iter()
                    .filter_map(|&handle| drop_fb_hold(&mut st, cs, handle))
                    .collect();
                // Removing the framebuffer the host pointer is drawn from takes the
                // binding down and hands back the resource, so the caller can hide +
                // release it outside the lock. Without this the device would keep an
                // `UPDATE_CURSOR` naming a released resource - the dangling-id case
                // `release_framebuffers` closes on the close path, and `RMFB` is the
                // other door to the same framebuffer going away.
                let cursor_died = if st.cursor_fb == id {
                    st.cursor_fb = 0;
                    st.cursor_resource.take()
                } else {
                    None
                };
                (cursor_died, gone)
            });
            // The pointer first, then the buffers: the cursor's binding is the
            // thing that must not be left naming a resource the device has been
            // told to drop.
            if let Some((res, _)) = cursor_died {
                hide_host_cursor(res);
            }
            for gone in gone {
                tell_buffer_gone(gone);
            }
            0
        }
        NR_PAGE_FLIP => page_flip(arg, card.lease()),
        // The cursor plane's legacy door. Both spellings carry the same request
        // with the hotspot appended to the second, so one handler serves them.
        super::abi::NR_MODE_CURSOR | super::abi::NR_MODE_CURSOR2 => {
            mode_cursor(arg, nr == NR_MODE_CURSOR2, card.lease())
        }
        NR_MODE_CREATE_LEASE => create_lease(card, minor, ofd, arg),
        NR_MODE_LIST_LESSES => list_lessees(card, arg),
        NR_MODE_REVOKE_LEASE => revoke_lease(card, ofd, arg),
        NR_MODE_GET_LEASE => get_lease(card, arg),
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
        NR_CREATE_DUMB => create_dumb(arg, ofd),
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

pub(crate) fn arg_shape(nr: u32) -> Option<(usize, bool)> {
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
        super::abi::NR_MODE_GETGAMMA | super::abi::NR_MODE_SETGAMMA => (32, true),
        // The CRTC-sequence pair, `struct drm_crtc_get_sequence` and
        // `struct drm_crtc_queue_sequence`, both `DRM_IOWR` as well.
        //
        // The row matters more than it looks: these two are *refused*, and
        // Linux's ioctl layer copies every DRM struct in before the handler can
        // refuse it, so a caller whose pointer is not its own memory hears
        // `EFAULT` rather than the refusal. Without the row this card answered
        // `ENOTTY` without ever looking at the struct.
        super::abi::NR_CRTC_GET_SEQUENCE | super::abi::NR_CRTC_QUEUE_SEQUENCE => (24, true),
        NR_ADDFB => (28, false),
        NR_ADDFB2 => (104, false),   // struct drm_mode_fb_cmd2
        NR_RMFB => (4, false),       // struct drm_mode_rmfb
        NR_PAGE_FLIP => (24, false), // struct drm_mode_crtc_page_flip
        // The lease family, at `drm_mode.h`'s own sizes, which are `sizeof` of
        // the structs and not the count of words this driver touches:
        // `create_lease` is the objects pointer, the count, the flags and the
        // two words it returns - 24 bytes, no trailing pad, read and written;
        // `list_lessees` and `get_lease` are a count, padding and a pointer
        // (16); `revoke_lease` is one id (4).
        //
        // These were once 24/16/8 - the sizes this driver's own handlers
        // happened to touch - and a `libdrm` caller sends the header's, so a
        // request whose words were in the right place was still validated
        // against a length the caller never allocated. The 28 written here on
        // the way past was that same mistake one step finer: the struct this
        // machine's `<drm_mode.h>` declares has *five* fields and no `pad`, so
        // `sizeof` is 24 and `DRM_IOWR(0xC6, struct drm_mode_create_lease)` is
        // 0xc01864c6 rather than 0xc01c64c6 - a size no alignment could
        // produce, being 28 after a `__u64` first member. The number was
        // invented from the field list rather than measured, which is why the
        // row below is what `cc` prints for the header on this machine.
        NR_MODE_CREATE_LEASE => (24, true), // struct drm_mode_create_lease
        NR_MODE_LIST_LESSES => (16, true),  // struct drm_mode_list_lessees
        NR_MODE_GET_LEASE => (16, true),    // struct drm_mode_get_lease
        NR_MODE_REVOKE_LEASE => (4, false), // struct drm_mode_revoke_lease
        NR_CREATE_DUMB => (32, true),       // struct drm_mode_create_dumb
        NR_MAP_DUMB => (16, true),          // struct drm_mode_map_dumb
        NR_DESTROY_DUMB => (4, false),      // struct drm_mode_destroy_dumb
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
        super::abi::NR_SYNCOBJ_RESET | super::abi::NR_SYNCOBJ_SIGNAL => (16, false), // drm_syncobj_array
        NR_SYNCOBJ_TIMELINE_WAIT => (48, true), // drm_syncobj_timeline_wait
        // `struct drm_syncobj_timeline_array` is read *and* written: `QUERY`
        // fills the points array, and the two signals read it.
        super::abi::NR_SYNCOBJ_QUERY
        | super::abi::NR_SYNCOBJ_TRANSFER
        | super::abi::NR_SYNCOBJ_TIMELINE_SIGNAL
        | super::abi::NR_SYNCOBJ_EVENTFD => match nr {
            super::abi::NR_SYNCOBJ_QUERY | super::abi::NR_SYNCOBJ_TIMELINE_SIGNAL => (24, true),
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
        super::abi::NR_VIRTGPU_TRANSFER_FROM_HOST | super::abi::NR_VIRTGPU_TRANSFER_TO_HOST => {
            (44, true)
        }
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
pub(crate) fn user_array(ptr: usize, count: usize, elem: usize, write: bool) -> bool {
    if count == 0 || ptr == 0 {
        return false;
    }
    match count.checked_mul(elem) {
        Some(bytes) => crate::uaccess::buffer_ok(ptr, bytes, write, true),
        None => false,
    }
}
