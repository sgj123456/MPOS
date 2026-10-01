//! t-virtgpu - what the `DRM_VIRTGPU_*` family answers on a card with no 3D.
//!
//! The family is how Mesa's virgl driver and every Vulkan-on-virtio-gpu stack
//! reach the device, and this card cannot carry it yet. What it *can* do is
//! answer each request the way a real Linux card without virgl behind it
//! answers - and that is not one refusal: a card with no 3D **succeeds** at some
//! of these, refuses others `ENOSYS` and others `EINVAL`. The measurement behind
//! the expectations below is `user/staging/virtgpu-probe/` (Linux 6.12 on
//! `virtio-gpu-pci`), and `src/drm_virtgpu.rs` carries it into the kernel with a
//! comment and a reason per entry.
//!
//! Three things this pins that a table of constants cannot:
//!
//! * **The width of `GETPARAM`'s write.** Linux's `struct drm_virtgpu_getparam`
//!   spells `value` as a `u64` and the copy is `sizeof(int)`, so the caller's
//!   high half is left exactly as it was. The sentinel below is what makes a
//!   kernel that wrote eight bytes fail.
//! * **`EFAULT` before a refusal.** Linux's ioctl layer copies every DRM struct
//!   in before the handler can decline, so a request this card refuses with a
//!   pointer that is not the caller's hears `EFAULT` instead - which is why the
//!   four requests with no handler still have a size row.
//! * **The two answers inside `RESOURCE_CREATE`.** Bad arguments are `EINVAL`
//!   (a card without 3D refuses a 3D target and any array/mip/sample count above
//!   one) and good arguments are `ENOSYS` - this card cannot allocate, which is
//!   a different problem from the caller's arguments being wrong.

#![no_std]
#![no_main]

#[path = "../../../lib/lin.rs"]
mod lin;

use core::panic::PanicInfo;

/// `DRM_IOCTL_BASE` is `'d'` (0x64), and the command number sits at bits 8..15
/// of the request word - see `t-drm.rs` for why this kernel accepts both this
/// spelling and the one a real libdrm builds.
const fn ioc_d(nr: usize, size: usize) -> usize {
    (3 << 30) | (size << 16) | ((b'd' as usize) << 8) | nr
}

// The family's numbers, from `virtgpu_drm.h`: `DRM_COMMAND_BASE` is 0x40 and the
// requests run from 1.
const NR_MAP: usize = 0x41;
const NR_EXECBUFFER: usize = 0x42;
const NR_GETPARAM: usize = 0x43;
const NR_RESOURCE_CREATE: usize = 0x44;
const NR_TRANSFER_FROM_HOST: usize = 0x46;
const NR_GET_CAPS: usize = 0x49;
const NR_RESOURCE_CREATE_BLOB: usize = 0x4a;
const NR_CONTEXT_INIT: usize = 0x4b;

const O_RDWR: usize = 2;

/// What `GETPARAM` leaves in the caller's word before it runs, so that a kernel
/// writing more than Linux does is visible: the low half is the answer's and the
/// high half must come back untouched.
const SENTINEL: u64 = 0xdead_beef_feed_face;

fn put_u32(b: &mut [u8], at: usize, v: u32) {
    b[at..at + 4].copy_from_slice(&v.to_ne_bytes());
}

/// `GETPARAM(param)`, with the destination pre-filled.
fn getparam(fd: i32, param: u64, value: &mut u64) -> isize {
    *value = SENTINEL;
    let mut arg = [param, value as *mut u64 as u64];
    lin::ioctl(fd, ioc_d(NR_GETPARAM, 16), arg.as_mut_ptr() as usize)
}

#[unsafe(no_mangle)]
extern "C" fn mpos_user_main(_argc: usize, _argv: *const *const u8) -> ! {
    lin::puts(b"t-virtgpu\n");
    let mut failed = false;

    let fd = lin::openat(lin::AT_FDCWD, b"/dev/dri/card0\0", O_RDWR, 0) as i32;
    lin::check_true(&mut failed, b"/dev/dri/card0 opens", fd >= 0);
    if fd < 0 {
        lin::done(failed);
    }

    // ---- GETPARAM: the measured table, value for value ----------------------
    let mut v = 0u64;
    let r = getparam(fd, 1, &mut v);
    lin::check_eq(&mut failed, b"GETPARAM(3D_FEATURES) succeeds", r, 0);
    lin::check_eq(
        &mut failed,
        b"3D_FEATURES is 0 - this kernel negotiates no VIRGL",
        v as u32 as isize,
        0,
    );
    lin::check_eq(
        &mut failed,
        b"GETPARAM wrote four bytes and left the caller's high half alone",
        (v >> 32) as isize,
        (SENTINEL >> 32) as isize,
    );

    let r = getparam(fd, 2, &mut v);
    lin::check_eq(&mut failed, b"GETPARAM(CAPSET_QUERY_FIX) succeeds", r, 0);
    lin::check_eq(
        &mut failed,
        b"CAPSET_QUERY_FIX is 1 - Linux answers it whatever the card is",
        v as u32 as isize,
        1,
    );

    // 3..8 are all "no": no blobs, no host-visible mapping, no cross-device
    // sharing, no explicit context creation, no capsets, no debug names.
    for (param, name) in [
        (3u64, b"RESOURCE_BLOB is 0".as_slice()),
        (4, b"HOST_VISIBLE is 0"),
        (5, b"CROSS_DEVICE is 0"),
        (6, b"CONTEXT_INIT is 0"),
        (7, b"SUPPORTED_CAPSET_IDs is an empty mask"),
        (8, b"EXPLICIT_DEBUG_NAME is 0"),
    ] {
        let r = getparam(fd, param, &mut v);
        lin::check_eq(&mut failed, b"GETPARAM succeeds", r, 0);
        lin::check_eq(&mut failed, name, v as u32 as isize, 0);
    }

    // A parameter outside the table is EINVAL - including 9, which a newer
    // header calls BLOB_ALIGNMENT and this card has no blobs to align.
    for param in [0u64, 9, 100] {
        let r = getparam(fd, param, &mut v);
        lin::check_eq(&mut failed, b"an unknown parameter is EINVAL", r, lin::EINVAL);
    }

    // A destination that is not the caller's memory is EFAULT, and the check
    // comes *after* the parameter's, as Linux's does: an unknown parameter is
    // EINVAL even with a null destination.
    let mut arg = [1u64, 0u64];
    let r = lin::ioctl(fd, ioc_d(NR_GETPARAM, 16), arg.as_mut_ptr() as usize);
    lin::check_eq(&mut failed, b"a null destination is EFAULT", r, lin::EFAULT);
    let mut arg = [999u64, 0u64];
    let r = lin::ioctl(fd, ioc_d(NR_GETPARAM, 16), arg.as_mut_ptr() as usize);
    lin::check_eq(
        &mut failed,
        b"an unknown parameter is EINVAL before the destination is looked at",
        r,
        lin::EINVAL,
    );

    // ---- the refusals, and they are not one gate ----------------------------
    let mut caps = [0u8; 24];
    let r = lin::ioctl(fd, ioc_d(NR_GET_CAPS, 24), caps.as_mut_ptr() as usize);
    lin::check_eq(
        &mut failed,
        b"GET_CAPS is ENOSYS - no capset is fetchable",
        r,
        lin::ENOSYS,
    );

    let mut execbuf = [0u8; 64];
    let r = lin::ioctl(fd, ioc_d(NR_EXECBUFFER, 64), execbuf.as_mut_ptr() as usize);
    lin::check_eq(
        &mut failed,
        b"EXECBUFFER is ENOSYS - there is no context to submit to",
        r,
        lin::ENOSYS,
    );

    let mut from_host = [0u8; 44];
    let r = lin::ioctl(
        fd,
        ioc_d(NR_TRANSFER_FROM_HOST, 44),
        from_host.as_mut_ptr() as usize,
    );
    lin::check_eq(
        &mut failed,
        b"TRANSFER_FROM_HOST is ENOSYS - only the 3D command goes that way",
        r,
        lin::ENOSYS,
    );

    let mut context = [0u8; 16];
    let r = lin::ioctl(fd, ioc_d(NR_CONTEXT_INIT, 16), context.as_mut_ptr() as usize);
    lin::check_eq(
        &mut failed,
        b"CONTEXT_INIT is EINVAL, which is Linux's answer without virgl",
        r,
        lin::EINVAL,
    );

    let mut blob = [0u8; 56];
    let r = lin::ioctl(
        fd,
        ioc_d(NR_RESOURCE_CREATE_BLOB, 56),
        blob.as_mut_ptr() as usize,
    );
    lin::check_eq(
        &mut failed,
        b"RESOURCE_CREATE_BLOB is EINVAL - no blob feature",
        r,
        lin::EINVAL,
    );

    // ---- RESOURCE_CREATE: two answers, and they stay apart ------------------
    // `struct drm_virtgpu_resource_create`: target@0, depth@20, array_size@24,
    // last_level@28, nr_samples@32.
    let mut rc = [0u8; 56];
    put_u32(&mut rc, 0, 2); // the 2D target
    put_u32(&mut rc, 12, 64); // width
    put_u32(&mut rc, 16, 64); // height
    put_u32(&mut rc, 20, 1);
    put_u32(&mut rc, 24, 1);
    put_u32(&mut rc, 28, 1);
    put_u32(&mut rc, 32, 1);
    let r = lin::ioctl(fd, ioc_d(NR_RESOURCE_CREATE, 56), rc.as_mut_ptr() as usize);
    lin::check_eq(
        &mut failed,
        b"a 2D resource is ENOSYS - this card cannot allocate one yet",
        r,
        lin::ENOSYS,
    );

    put_u32(&mut rc, 0, 4); // the 3D target
    let r = lin::ioctl(fd, ioc_d(NR_RESOURCE_CREATE, 56), rc.as_mut_ptr() as usize);
    lin::check_eq(
        &mut failed,
        b"a 3D target is EINVAL, as a card without 3D answers it",
        r,
        lin::EINVAL,
    );

    put_u32(&mut rc, 0, 2);
    put_u32(&mut rc, 20, 2); // depth
    let r = lin::ioctl(fd, ioc_d(NR_RESOURCE_CREATE, 56), rc.as_mut_ptr() as usize);
    lin::check_eq(
        &mut failed,
        b"a depth above one is EINVAL",
        r,
        lin::EINVAL,
    );

    put_u32(&mut rc, 20, 1);
    put_u32(&mut rc, 32, 2); // nr_samples
    let r = lin::ioctl(fd, ioc_d(NR_RESOURCE_CREATE, 56), rc.as_mut_ptr() as usize);
    lin::check_eq(&mut failed, b"multisampling is EINVAL", r, lin::EINVAL);

    // ---- EFAULT comes before any of the refusals ----------------------------
    // `MAP` is one of the four requests with no handler, and it must still
    // refuse a pointer that is not the caller's memory rather than answer
    // ENOTTY without looking at it - which is what Linux's ioctl layer does by
    // copying the struct in first.
    let r = lin::ioctl(fd, ioc_d(NR_MAP, 16), 0);
    lin::check_eq(
        &mut failed,
        b"a request this card cannot answer still faults a bad pointer first",
        r,
        lin::EFAULT,
    );

    let _ = lin::close(fd);
    lin::done(failed)
}

#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    lin::die(b"t-virtgpu: panicked\n")
}
