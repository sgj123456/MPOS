//! t-virtgpu - what the `DRM_VIRTGPU_*` family answers, on either card.
//!
//! The family is how Mesa's virgl driver and every Vulkan-on-virtio-gpu stack
//! reach the device, and there are two machines to answer for: a
//! `virtio-gpu-pci` with no 3D behind it, and a `virtio-gpu-gl-pci` with virgl.
//! `GETPARAM(1)` says which one this is and the program takes one of two sets of
//! expectations from there - because the gate can only boot the first, and the
//! second would otherwise be verified by nothing but a hand-run boot.
//!
//! On the card **without** 3D, each request is answered the way a real Linux card
//! without virgl answers - and that is not one refusal: a card with no 3D
//! **succeeds** at some of these (it makes 2D resources, maps them, waits on
//! them, transfers into them), refuses others `ENOSYS` and others `EINVAL`. The
//! measurement behind the expectations is `user/staging/virtgpu-probe/`, which
//! was run on both cards, and `src/gpu/virtgpu.rs` carries it into the kernel with
//! a comment and a reason per entry.
//!
//! Four things this pins that a table of constants cannot:
//!
//! * **The width of `GETPARAM`'s write.** Linux's `struct drm_virtgpu_getparam`
//!   spells `value` as a `u64` and the copy is `sizeof(int)`, so the caller's
//!   high half is left exactly as it was. The sentinel below is what makes a
//!   kernel that wrote eight bytes fail.
//! * **`EFAULT` before a refusal.** Linux's ioctl layer copies every DRM struct
//!   in before the handler can decline, so a request this card refuses with a
//!   pointer that is not the caller's memory hears `EFAULT` instead.
//! * **A resource really exists.** The handle that comes back is one `MAP`,
//!   `RESOURCE_INFO` and `GEM_CLOSE` all take, the device id is not zero, and a
//!   transfer into it is accepted - across the mailbox to the render task, which
//!   is the only thing that may touch the device.
//! * **The two answers inside `RESOURCE_CREATE`, and a third this card adds.**
//!   Bad arguments are `EINVAL` (a card without 3D refuses a 3D target and any
//!   array/mip/sample count above one) and good ones make a resource. The third
//!   is the *over-read guard*: a transfer whose rectangle does not fit the
//!   backing the device was given is refused, where Linux would read past it.
//!   That one is this card's own, and it can only turn a broken caller's garbage
//!   into an error.
//!
//! # Why this one is not run on the host
//!
//! Every answer below is this kernel's `virtio-gpu` driver's, measured against
//! QEMU's two virtio-gpu cards, and one of the four things above is **this card's
//! own** rule rather than Linux's. A host's `/dev/dri/card0` is a different device
//! with different numbers, a different (usually larger) capability mask and no
//! over-read guard, so a hosted run measures the host's card and not this kernel's
//! - it would answer a mixed set of the two columns and fail for a reason that is
//! not a defect. So a hosted run prints what it is and makes **no claim at all**,
//! which is `t-drm-lease`'s rule for the same reason (`NOTES.md` §15.34: what a
//! boot may assert is bounded by what the kernel itself produces).
//!
//! # What the move changed
//!
//! Nothing about the family. The door changed - `libc::syscall(SYS_ioctl, ...)`
//! through this program's own `ioctl`, as `t-drm-args` and `t-drm-lease` do it,
//! rather than the no-std `lin::ioctl` - and the fd is a `rustix` `OwnedFd`
//! instead of a number closed by hand. The claim list, its order and its wording
//! are the original's; the two cards' expectations are its measurement.

use std::ffi::c_void;
use std::os::fd::AsRawFd;

use mpos_user_tests::{Report, host_run};
use rustix::fd::OwnedFd;
use rustix::fs::{CWD, Mode, OFlags, openat};
use rustix::io::Errno;
use rustix::mm::{MapFlags, ProtFlags};

/// `DRM_IOCTL_BASE` is `'d'` (0x64), and the command number sits at bits 8..15
/// of the request word - see `t-drm.rs` for why this kernel accepts both this
/// spelling and the one a real libdrm builds.
const fn ioc_d(nr: u32, size: u32) -> u64 {
    (3u64 << 30) | ((size as u64) << 16) | ((b'd' as u64) << 8) | (nr as u64)
}

// The family's numbers, from `virtgpu_drm.h`: `DRM_COMMAND_BASE` is 0x40 and the
// requests run from 1.
const NR_MAP: u32 = 0x41;
const NR_EXECBUFFER: u32 = 0x42;
const NR_GETPARAM: u32 = 0x43;
const NR_RESOURCE_CREATE: u32 = 0x44;
const NR_RESOURCE_INFO: u32 = 0x45;
const NR_TRANSFER_FROM_HOST: u32 = 0x46;
const NR_TRANSFER_TO_HOST: u32 = 0x47;
const NR_WAIT: u32 = 0x48;
const NR_GET_CAPS: u32 = 0x49;
const NR_RESOURCE_CREATE_BLOB: u32 = 0x4a;
const NR_CONTEXT_INIT: u32 = 0x4b;
const NR_GEM_CLOSE: u32 = 0x09;
const NR_CREATE_DUMB: u32 = 0xb2;
const NR_PRIME_HANDLE_TO_FD: u32 = 0x2d;
const NR_PRIME_FD_TO_HANDLE: u32 = 0x2e;

/// `VIRGL_CCMD_TRANSFER3D` - the one command that moves a resource's pixels
/// between the host's copy of it and the guest pages the device was given as its
/// backing. Its number is the position it holds in virgl's command enum, which
/// is why it is written here with the count it comes from rather than borrowed
/// from a header this program does not have: 43 entries precede it.
const VIRGL_CCMD_TRANSFER3D: u32 = 43;

/// A `TRANSFER3D`'s payload, in dwords, after the command word: the resource,
/// its level, the transfer usage, the two strides, the box, the offset, and the
/// direction. The layout is Mesa's own encoder's
/// (`virgl_encoder_transfer3d_common` then offset then direction), which is the
/// other end of this wire.
const VIRGL_TRANSFER3D_SIZE: u32 = 13;

/// The direction word's value for host-to-guest, where `1` is the other way.
/// This is the only direction whose *effect* a guest can read back: the pixels
/// land in the pages the device was given, so a caller that filled them first
/// can tell whether the command ran.
const VIRGL_TRANSFER_FROM_HOST: u32 = 2;

/// The resource a `transfer3d_from_host` below is built over: 64 x 64 at 32 bits
/// per pixel, the size and the pitch a GBM buffer of that shape has.
const PIN_BYTES: usize = 64 * 64 * 4;

/// The bottom of the range this driver keeps for its *own* device resources -
/// the scanout's framebuffer and the host cursor. A GEM handle a client holds
/// is always below it, which is what lets one number serve as both a handle
/// and a wire resource id.
const DRIVER_RESOURCE_BASE: u32 = 0x4000_0000;

/// What `GETPARAM` leaves in the caller's word before it runs, so that a kernel
/// writing more than Linux does is visible: the low half is the answer's and the
/// high half must come back untouched.
const SENTINEL: u64 = 0xdead_beef_feed_face;

/// One `ioctl`, answered as the signed return the kernel handed back.
///
/// Through `libc::syscall` rather than `libc::ioctl`, for `t-drm-args`'s reason:
/// the request is an `unsigned long` word and `ioctl`'s binding of it is not the
/// same width on both libcs. The number comes from `libc::SYS_ioctl`, the
/// maintained per-machine table.
///
/// # Safety
///
/// `arg` must be null or an address the caller owns that is at least as long as
/// the size the request word names.
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

/// A claim that a request succeeded.
fn check_ok(r: &mut Report, name: &str, got: isize) {
    if got >= 0 {
        r.check(name, true);
    } else {
        r.check(&format!("{name} (got {got})"), false);
    }
}

/// A claim about a value the kernel wrote into the caller's buffer.
fn check_is(r: &mut Report, name: &str, got: isize, want: isize) {
    if got == want {
        r.check(name, true);
    } else {
        r.check(&format!("{name} (got {got}, want {want})"), false);
    }
}

fn put_u32(b: &mut [u8], at: usize, v: u32) {
    b[at..at + 4].copy_from_slice(&v.to_ne_bytes());
}

/// A u32 out of a request/reply buffer, the UAPI structs being native-order.
fn u32_at(b: &[u8], at: usize) -> u32 {
    u32::from_ne_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

fn u64_at(b: &[u8], at: usize) -> u64 {
    let mut a = [0u8; 8];
    a.copy_from_slice(&b[at..at + 8]);
    u64::from_ne_bytes(a)
}

/// A command stream whose **one** command is `TRANSFER3D` from the host into
/// `res_handle`, over the whole of a `width` x `height` level.
///
/// The command word carries the length in its top half and the command in its
/// low byte (`VIRGL_CMD0`), with no object: a stream is a sequence of these, and
/// this one is a single command of exactly the size the decoder checks for.
///
/// The fields are in the order Mesa's encoder writes them, which is the order
/// vrend's decoder reads: resource, level, usage, stride, layer stride, the six
/// box words, the offset, the direction. Zero strides mean "the host infers
/// them", which is what Mesa sends for anything but a blob with guest memory.
fn transfer3d_from_host(res_handle: u32, width: u32, height: u32) -> [u32; 14] {
    let mut stream = [0u32; 14];
    stream[0] = VIRGL_CCMD_TRANSFER3D | (VIRGL_TRANSFER3D_SIZE << 16);
    stream[1] = res_handle;
    stream[2] = 0; // level 0, the only one this resource has
    stream[3] = 1; // PIPE_TRANSFER_READ: the usage a read-back asks for
    stream[4] = 0; // stride: inferred
    stream[5] = 0; // layer stride: inferred
    stream[9] = width; // box.w
    stream[10] = height; // box.h
    stream[11] = 1; // box.d
    stream[13] = VIRGL_TRANSFER_FROM_HOST;
    stream
}

/// `EXECBUFFER` with `stream` as its one command and `bo_handle` as the one
/// buffer it names - the shape a driver's submit has, and the whole of what the
/// request carries besides the flags this card refuses.
///
/// The answer is the *request's*, not the device's: QEMU answers every submit
/// `OK_NODATA` and discards what virglrenderer made of the stream, so a batch the
/// host dropped still comes back `0` here. That is why the claim below is about
/// pixels and not about this return value - see the block that uses it.
fn submit(fd: i32, stream: &[u32], bo_handle: u32) -> isize {
    let handles = [bo_handle];
    let mut eb = [0u8; 64];
    eb[8..16].copy_from_slice(&(stream.as_ptr() as u64).to_ne_bytes());
    eb[16..24].copy_from_slice(&(handles.as_ptr() as u64).to_ne_bytes());
    put_u32(&mut eb, 4, (stream.len() * 4) as u32);
    put_u32(&mut eb, 24, 1);
    unsafe { ioctl(fd, ioc_d(NR_EXECBUFFER, 64), eb.as_mut_ptr() as *mut c_void) }
}

/// `PRIME_HANDLE_TO_FD`: a handle this card made, out as a descriptor another
/// file can import. `struct drm_prime_handle`: handle@0, flags@4, fd@8.
fn prime_handle_to_fd(fd: i32, handle: u32) -> (isize, i32) {
    let mut arg = [0u8; 12];
    put_u32(&mut arg, 0, handle);
    let got = unsafe {
        ioctl(
            fd,
            ioc_d(NR_PRIME_HANDLE_TO_FD, 12),
            arg.as_mut_ptr() as *mut c_void,
        )
    };
    (got, i32::from_ne_bytes([arg[8], arg[9], arg[10], arg[11]]))
}

/// `PRIME_FD_TO_HANDLE`: that descriptor back in, on another file.
fn prime_fd_to_handle(fd: i32, descriptor: i32) -> (isize, u32) {
    let mut arg = [0u8; 12];
    arg[8..12].copy_from_slice(&descriptor.to_ne_bytes());
    let got = unsafe {
        ioctl(
            fd,
            ioc_d(NR_PRIME_FD_TO_HANDLE, 12),
            arg.as_mut_ptr() as *mut c_void,
        )
    };
    (got, u32_at(&arg, 0))
}

/// `RESOURCE_INFO`: the device resource a handle names, and its size.
fn resource_info(fd: i32, handle: u32) -> (isize, u32) {
    let mut info = [0u8; 16];
    put_u32(&mut info, 0, handle);
    let got = unsafe {
        ioctl(
            fd,
            ioc_d(NR_RESOURCE_INFO, 16),
            info.as_mut_ptr() as *mut c_void,
        )
    };
    (got, u32_at(&info, 4))
}

/// Write `byte` over a mapped buffer - the marker the transfer below destroys.
///
/// Volatile because the only reader that matters is the device: a plain store
/// loop is free to be elided, and the pages are written by something this
/// program cannot see.
fn fill_marker(map: *mut u8, len: usize, byte: u8) {
    for i in 0..len {
        unsafe { map.add(i).write_volatile(byte) };
    }
}

/// Whether every byte of the mapping is still the marker.
fn marker_intact(map: *const u8, len: usize, byte: u8) -> bool {
    (0..len).all(|i| unsafe { map.add(i).read_volatile() } == byte)
}

/// `GETPARAM(param)`, with the destination pre-filled.
fn getparam(fd: i32, param: u64, value: &mut u64) -> isize {
    *value = SENTINEL;
    let mut arg = [param, value as *mut u64 as u64];
    unsafe { ioctl(fd, ioc_d(NR_GETPARAM, 16), arg.as_mut_ptr() as *mut c_void) }
}

fn main() {
    println!("t-virtgpu");
    let mut r = Report::new();

    // See the header: a host's card is a different device, and one of the rules
    // below is this card's own, so a hosted run makes no claim at all.
    if host_run() {
        println!(
            "note: not made on a host - these answers are this kernel's virtio-gpu driver's, \
             measured against QEMU's two virtio-gpu cards, and the over-read guard among them \
             is this card's own rule rather than Linux's"
        );
        r.finish();
    }

    let card = match openat(CWD, "/dev/dri/card0", OFlags::RDWR, Mode::empty()) {
        Ok(card) => {
            r.check("/dev/dri/card0 opens", true);
            card
        }
        Err(e) => {
            r.check(&format!("/dev/dri/card0 opens ({e:?})"), false);
            r.finish();
        }
    };
    let fd = card.as_raw_fd();

    // ---- GETPARAM: the measured table, value for value ----------------------
    let mut v = 0u64;
    let got = getparam(fd, 1, &mut v);
    check_ok(&mut r, "GETPARAM(3D_FEATURES) succeeds", got);
    // This one answer says which machine the program is on, and the two sets of
    // expectations below meet here. Both values are measured: `0` on a
    // `virtio-gpu-pci`, `1` on a `virtio-gpu-gl-pci`.
    let three_d = v as u32 != 0;
    r.check(
        "3D_FEATURES is 0 or 1 - a card has a 3D context or it does not",
        v as u32 <= 1,
    );
    check_is(
        &mut r,
        "GETPARAM wrote four bytes and left the caller's high half alone",
        (v >> 32) as isize,
        (SENTINEL >> 32) as isize,
    );

    let got = getparam(fd, 2, &mut v);
    check_ok(&mut r, "GETPARAM(CAPSET_QUERY_FIX) succeeds", got);
    check_is(
        &mut r,
        "CAPSET_QUERY_FIX is 1 - Linux answers it whatever the card is",
        v as u32 as isize,
        1,
    );

    // 3..8 are the capability word, and the two measured rows differ in exactly
    // three of them. The blob three stay `0` on **both** cards, and not because
    // one of them is unimplemented: the GL device this was measured against
    // offers no blob feature (`-resource_blob`, `-host_visible` in the probe's
    // feature line), so Linux refuses `RESOURCE_CREATE_BLOB` there too.
    let row: [(u64, u32, &str); 6] = if three_d {
        [
            (
                3,
                0,
                "RESOURCE_BLOB is 0 - the device offers no blob feature",
            ),
            (4, 0, "HOST_VISIBLE is 0"),
            (5, 0, "CROSS_DEVICE is 0"),
            (
                6,
                1,
                "CONTEXT_INIT is 1 - the context can be made explicitly",
            ),
            (7, 6, "SUPPORTED_CAPSET_IDs is 6 - VIRGL and VIRGL2"),
            (8, 1, "EXPLICIT_DEBUG_NAME follows CONTEXT_INIT"),
        ]
    } else {
        [
            (3, 0, "RESOURCE_BLOB is 0"),
            (4, 0, "HOST_VISIBLE is 0"),
            (5, 0, "CROSS_DEVICE is 0"),
            (6, 0, "CONTEXT_INIT is 0"),
            (7, 0, "SUPPORTED_CAPSET_IDs is an empty mask"),
            (8, 0, "EXPLICIT_DEBUG_NAME is 0"),
        ]
    };
    for (param, want, name) in row {
        let got = getparam(fd, param, &mut v);
        check_ok(&mut r, "GETPARAM succeeds", got);
        check_is(&mut r, name, v as u32 as isize, want as isize);
    }

    // A parameter outside the table is EINVAL - including 9, which a newer
    // header calls BLOB_ALIGNMENT and this card has no blobs to align.
    for param in [0u64, 9, 100] {
        let got = getparam(fd, param, &mut v);
        check_refused(&mut r, "an unknown parameter is EINVAL", got, Errno::INVAL);
    }

    // A destination that is not the caller's memory is EFAULT, and the check
    // comes *after* the parameter's, as Linux's does: an unknown parameter is
    // EINVAL even with a null destination.
    let mut arg = [1u64, 0u64];
    let got = unsafe { ioctl(fd, ioc_d(NR_GETPARAM, 16), arg.as_mut_ptr() as *mut c_void) };
    check_refused(&mut r, "a null destination is EFAULT", got, Errno::FAULT);
    let mut arg = [999u64, 0u64];
    let got = unsafe { ioctl(fd, ioc_d(NR_GETPARAM, 16), arg.as_mut_ptr() as *mut c_void) };
    check_refused(
        &mut r,
        "an unknown parameter is EINVAL before the destination is looked at",
        got,
        Errno::INVAL,
    );

    // The two machines diverge here. Everything above is asked of both; what
    // follows is one card's answers or the other's, and the 3D set ends the
    // program rather than falling through into expectations it would fail.
    //
    // The 2D half of the family is deliberately *not* repeated on the 3D card.
    // The gate boots the non-3D one and pins it there, and the only part that
    // differs on a virgl card - that a create makes the context - is pinned by
    // the `CONTEXT_INIT` answer below, which is the first request after it.
    if three_d {
        three_d_assertions(fd, &card, &mut r);
        r.finish();
    }

    // ---- the refusals, and they are not one gate ----------------------------
    let mut caps = [0u8; 24];
    let got = unsafe { ioctl(fd, ioc_d(NR_GET_CAPS, 24), caps.as_mut_ptr() as *mut c_void) };
    check_refused(
        &mut r,
        "GET_CAPS is ENOSYS - no capset is fetchable",
        got,
        Errno::NOSYS,
    );

    let mut execbuf = [0u8; 64];
    let got = unsafe {
        ioctl(
            fd,
            ioc_d(NR_EXECBUFFER, 64),
            execbuf.as_mut_ptr() as *mut c_void,
        )
    };
    check_refused(
        &mut r,
        "EXECBUFFER is ENOSYS - there is no context to submit to",
        got,
        Errno::NOSYS,
    );

    let mut from_host = [0u8; 44];
    let got = unsafe {
        ioctl(
            fd,
            ioc_d(NR_TRANSFER_FROM_HOST, 44),
            from_host.as_mut_ptr() as *mut c_void,
        )
    };
    check_refused(
        &mut r,
        "TRANSFER_FROM_HOST is ENOSYS - only the 3D command goes that way",
        got,
        Errno::NOSYS,
    );

    let mut context = [0u8; 16];
    let got = unsafe {
        ioctl(
            fd,
            ioc_d(NR_CONTEXT_INIT, 16),
            context.as_mut_ptr() as *mut c_void,
        )
    };
    check_refused(
        &mut r,
        "CONTEXT_INIT is EINVAL, which is Linux's answer without virgl",
        got,
        Errno::INVAL,
    );

    let mut blob = [0u8; 56];
    let got = unsafe {
        ioctl(
            fd,
            ioc_d(NR_RESOURCE_CREATE_BLOB, 56),
            blob.as_mut_ptr() as *mut c_void,
        )
    };
    check_refused(
        &mut r,
        "RESOURCE_CREATE_BLOB is EINVAL - no blob feature",
        got,
        Errno::INVAL,
    );

    // ---- RESOURCE_CREATE: a resource is really made -------------------------
    // `struct drm_virtgpu_resource_create`: target@0, format@4, bind@8, width@12,
    // height@16, depth@20, array_size@24, last_level@28, nr_samples@32,
    // flags@36, bo_handle@40, res_handle@44, size@48, stride@52.
    let mut rc = [0u8; 56];
    put_u32(&mut rc, 0, 2); // the 2D target
    put_u32(&mut rc, 12, 64); // width
    put_u32(&mut rc, 16, 64); // height
    put_u32(&mut rc, 20, 1);
    put_u32(&mut rc, 24, 1);
    put_u32(&mut rc, 28, 1);
    put_u32(&mut rc, 32, 1);
    // A size that really backs 64x64 at 32 bits per pixel. A caller that passes
    // zero gets Linux's one-page object, and the transfer below is refused for it
    // - the over-read guard, which is this card's own addition and pinned further
    // down.
    put_u32(&mut rc, 48, 64 * 64 * 4);
    let got = unsafe {
        ioctl(
            fd,
            ioc_d(NR_RESOURCE_CREATE, 56),
            rc.as_mut_ptr() as *mut c_void,
        )
    };
    check_ok(&mut r, "a 2D resource is created", got);
    let handle = u32_at(&rc, 40);
    let res_handle = u32_at(&rc, 44);
    r.check("and a GEM handle comes back", handle != 0);
    // The resource id and the handle are **one number**, and that is what is
    // pinned here rather than merely "both are nonzero": the guest names a
    // resource inside its own command stream with the handle this request
    // answered, so a card that ran an id counter beside the handle table handed
    // the device ids it had never been told about. `vrend` answered exactly
    // that stream with `Illegal resource <n>` and Mesa's render process
    // stopped - no picture, for a reason three layers away from the number
    // that caused it. The two drifted apart at the first released handle,
    // because the counter only went up while handles went back into the table.
    check_is(
        &mut r,
        "and the device resource id is the handle it answered",
        res_handle as isize,
        handle as isize,
    );
    // The other half of the same statement: the driver's own resources (the
    // scanout's framebuffer, the host cursor) must sit outside the range a
    // handle can ever reach, or one of *them* is what a command stream names.
    r.check(
        "and no handle is ever a driver-owned resource id",
        handle < DRIVER_RESOURCE_BASE,
    );

    // The three requests that name it, which answered ENOTTY before a resource
    // could exist. They are the reason the previous pass withheld them: their
    // honest answer needs something to name.
    let mut info = [0u8; 16];
    put_u32(&mut info, 0, handle);
    let got = unsafe {
        ioctl(
            fd,
            ioc_d(NR_RESOURCE_INFO, 16),
            info.as_mut_ptr() as *mut c_void,
        )
    };
    check_ok(&mut r, "RESOURCE_INFO succeeds", got);
    check_is(
        &mut r,
        "and names the device resource",
        u32_at(&info, 4) as isize,
        res_handle as isize,
    );
    check_is(
        &mut r,
        "and the size the caller asked for",
        u32_at(&info, 8) as isize,
        (64 * 64 * 4) as isize,
    );
    check_is(&mut r, "and no blob memory", u32_at(&info, 12) as isize, 0);

    let mut map = [0u8; 16];
    put_u32(&mut map, 8, handle);
    let got = unsafe { ioctl(fd, ioc_d(NR_MAP, 16), map.as_mut_ptr() as *mut c_void) };
    check_ok(&mut r, "MAP succeeds for a resource", got);
    r.check("and names a nonzero mmap offset", u64_at(&map, 0) != 0);

    let mut wait = [0u8; 8];
    put_u32(&mut wait, 0, handle);
    let got = unsafe { ioctl(fd, ioc_d(NR_WAIT, 8), wait.as_mut_ptr() as *mut c_void) };
    check_ok(
        &mut r,
        "WAIT answers at once - every command is waited for before its ioctl returns",
        got,
    );

    // `struct drm_virtgpu_3d_transfer_to_host`: bo_handle@0, box (x@4, y@8, z@12,
    // w@16, h@20, d@24), level@28, offset@32, stride@36, layer_stride@40.
    let mut xfer = [0u8; 44];
    put_u32(&mut xfer, 0, handle);
    put_u32(&mut xfer, 16, 64); // box.w
    put_u32(&mut xfer, 20, 64); // box.h
    put_u32(&mut xfer, 24, 1); // box.d
    let got = unsafe {
        ioctl(
            fd,
            ioc_d(NR_TRANSFER_TO_HOST, 44),
            xfer.as_mut_ptr() as *mut c_void,
        )
    };
    check_ok(&mut r, "TRANSFER_TO_HOST is served for a resource", got);

    // A handle this card never made is ENOENT, which is Linux's own answer from
    // the handle lookup rather than a code chosen here.
    let mut unknown = [0u8; 16];
    put_u32(&mut unknown, 0, 9999);
    let got = unsafe {
        ioctl(
            fd,
            ioc_d(NR_RESOURCE_INFO, 16),
            unknown.as_mut_ptr() as *mut c_void,
        )
    };
    check_refused(
        &mut r,
        "RESOURCE_INFO on an unknown handle is ENOENT",
        got,
        Errno::NOENT,
    );

    // And the release tells the device, which is why it is not simply a table
    // slot going away.
    let mut close = [0u8; 8];
    put_u32(&mut close, 0, handle);
    let got = unsafe {
        ioctl(
            fd,
            ioc_d(NR_GEM_CLOSE, 8),
            close.as_mut_ptr() as *mut c_void,
        )
    };
    check_ok(&mut r, "GEM_CLOSE releases the resource handle", got);

    // ---- A dumb buffer's own resource is named by its handle too ------------
    // `CREATE_DUMB` normally makes a buffer the device never hears about: its
    // pixels reach the screen through this card's own scanout copy, so this
    // kernel backs it with a device resource lazily, the first time a caller
    // addresses the handle as one. That late resource has to answer the same
    // question the hand-made one above does, for the same reason - and it is
    // the harder case, because nothing forces the two paths to agree: the
    // asked-for resource takes its id from the slot it was given, and this one
    // reuses a slot another kind of buffer may already have been released from.
    //
    // This is where niri's picture was lost. A client that draws through GBM
    // holds dumb handles, and its command stream spells resources with those
    // handles; a late resource that took its id from a separate counter named
    // something the device had never created.
    let mut dumb2 = [0u8; 32];
    put_u32(&mut dumb2, 0, 64); // height
    put_u32(&mut dumb2, 4, 64); // width
    put_u32(&mut dumb2, 8, 32); // bpp
    let got = unsafe {
        ioctl(
            fd,
            ioc_d(NR_CREATE_DUMB, 32),
            dumb2.as_mut_ptr() as *mut c_void,
        )
    };
    check_ok(&mut r, "CREATE_DUMB succeeds", got);
    let handle = u32_at(&dumb2, 16);
    let mut info = [0u8; 16];
    put_u32(&mut info, 0, handle);
    let got = unsafe {
        ioctl(
            fd,
            ioc_d(NR_RESOURCE_INFO, 16),
            info.as_mut_ptr() as *mut c_void,
        )
    };
    check_ok(&mut r, "RESOURCE_INFO succeeds for a dumb buffer", got);
    check_is(
        &mut r,
        "and the resource it was backed with is named by its handle",
        u32_at(&info, 4) as isize,
        handle as isize,
    );
    let mut close = [0u8; 8];
    put_u32(&mut close, 0, handle);
    let got = unsafe {
        ioctl(
            fd,
            ioc_d(NR_GEM_CLOSE, 8),
            close.as_mut_ptr() as *mut c_void,
        )
    };
    check_ok(&mut r, "GEM_CLOSE releases the dumb handle", got);

    // ---- RESOURCE_CREATE: what a card without 3D still refuses --------------
    put_u32(&mut rc, 0, 4); // the 3D target
    let got = unsafe {
        ioctl(
            fd,
            ioc_d(NR_RESOURCE_CREATE, 56),
            rc.as_mut_ptr() as *mut c_void,
        )
    };
    check_refused(
        &mut r,
        "a 3D target is EINVAL, as a card without 3D answers it",
        got,
        Errno::INVAL,
    );

    put_u32(&mut rc, 0, 2);
    put_u32(&mut rc, 20, 2); // depth
    let got = unsafe {
        ioctl(
            fd,
            ioc_d(NR_RESOURCE_CREATE, 56),
            rc.as_mut_ptr() as *mut c_void,
        )
    };
    check_refused(&mut r, "a depth above one is EINVAL", got, Errno::INVAL);

    put_u32(&mut rc, 20, 1);
    put_u32(&mut rc, 32, 2); // nr_samples
    let got = unsafe {
        ioctl(
            fd,
            ioc_d(NR_RESOURCE_CREATE, 56),
            rc.as_mut_ptr() as *mut c_void,
        )
    };
    check_refused(&mut r, "multisampling is EINVAL", got, Errno::INVAL);

    // ---- EFAULT comes before any of the refusals ----------------------------
    // `MAP` is one of the four requests with no handler, and it must still
    // refuse a pointer that is not the caller's memory rather than answer
    // ENOTTY without looking at it - which is what Linux's ioctl layer does by
    // copying the struct in first.
    let got = unsafe { ioctl(fd, ioc_d(NR_MAP, 16), core::ptr::null_mut()) };
    check_refused(
        &mut r,
        "a request this card cannot answer still faults a bad pointer first",
        got,
        Errno::FAULT,
    );

    // The card stays open to here rather than being closed by hand: the `OwnedFd`
    // the open returned is still in scope, and `finish` leaves the process, so
    // the descriptor goes with it. A hosted run never reaches this line at all.
    r.finish()
}

/// What a card that **did** negotiate `VIRGL` answers.
///
/// The same program pins both machines because the gate can only boot the one
/// without 3D, and the other half of the family would otherwise be verified by
/// nothing but a hand-run boot. The values are the GL column of
/// `user/staging/virtgpu-probe/`'s table.
///
/// Two of the assertions below are deliberately weaker than an equality, and
/// each says so where it stands: a submit whose *stream contents* the device
/// refuses, and a transfer it may refuse, are answers the device gives rather
/// than decisions this kernel makes. What is pinned is that they are **served** -
/// that the request reaches the device and comes back - because that is what this
/// pass added; what the device then thinks of a four-byte stream of zeroes is
/// QEMU's business.
fn three_d_assertions(fd: i32, card: &OwnedFd, r: &mut Report) {
    // ---- GET_CAPS: real cap sets, where the other card has none ------------
    // `struct drm_virtgpu_get_caps`: cap_set_id@0, cap_set_ver@4, addr@8 (u64),
    // size@16. The card offers sets 1 (VIRGL) and 2 (VIRGL2), which is the mask
    // `GETPARAM(7)` reported a moment ago.
    let mut buf = [0u8; 4096];
    let mut caps = [0u8; 24];
    put_u32(&mut caps, 0, 1); // VIRGL
    put_u32(&mut caps, 4, 1); // the version the device named as its maximum
    put_u32(&mut caps, 16, buf.len() as u32);
    caps[8..16].copy_from_slice(&(buf.as_mut_ptr() as u64).to_ne_bytes());
    let got = unsafe { ioctl(fd, ioc_d(NR_GET_CAPS, 24), caps.as_mut_ptr() as *mut c_void) };
    check_ok(r, "GET_CAPS(VIRGL, v1) succeeds", got);
    // The set is the device's bytes and not the zeroes the caller left there: a
    // virgl cap set opens with its own version.
    r.check(
        "and fills the caller's buffer with the device's bytes",
        u32_at(&buf, 0) != 0,
    );

    put_u32(&mut caps, 0, 2); // VIRGL2
    put_u32(&mut caps, 4, 2);
    let got = unsafe { ioctl(fd, ioc_d(NR_GET_CAPS, 24), caps.as_mut_ptr() as *mut c_void) };
    check_ok(r, "GET_CAPS(VIRGL2, v2) succeeds", got);

    // A set the card does not offer, and a version past the one it has - the
    // same `EINVAL` for both, as Linux answers.
    put_u32(&mut caps, 0, 3);
    let got = unsafe { ioctl(fd, ioc_d(NR_GET_CAPS, 24), caps.as_mut_ptr() as *mut c_void) };
    check_refused(
        r,
        "a cap set the card does not offer is EINVAL",
        got,
        Errno::INVAL,
    );

    put_u32(&mut caps, 0, 1);
    put_u32(&mut caps, 4, 99);
    let got = unsafe { ioctl(fd, ioc_d(NR_GET_CAPS, 24), caps.as_mut_ptr() as *mut c_void) };
    check_refused(
        r,
        "a version past the set's maximum is EINVAL",
        got,
        Errno::INVAL,
    );

    // A caller that asked for nothing, and one whose buffer is smaller than the
    // set - the second is a **success**, which is the half of Linux's rule a
    // guess gets wrong.
    put_u32(&mut caps, 4, 1);
    put_u32(&mut caps, 16, 0);
    let got = unsafe { ioctl(fd, ioc_d(NR_GET_CAPS, 24), caps.as_mut_ptr() as *mut c_void) };
    check_refused(r, "a size of zero is EINVAL", got, Errno::INVAL);

    let mut small = [0u8; 16];
    put_u32(&mut caps, 16, small.len() as u32);
    caps[8..16].copy_from_slice(&(small.as_mut_ptr() as u64).to_ne_bytes());
    let got = unsafe { ioctl(fd, ioc_d(NR_GET_CAPS, 24), caps.as_mut_ptr() as *mut c_void) };
    check_ok(
        r,
        "a buffer smaller than the set is a success, not a truncation error",
        got,
    );

    // ---- RESOURCE_CREATE: the target the other card refuses ----------------
    // `struct drm_virtgpu_resource_create`: target@0, format@4, bind@8, width@12,
    // height@16, depth@20, array_size@24, last_level@28, nr_samples@32,
    // flags@36, bo_handle@40, res_handle@44, size@48, stride@52.
    let mut rc = [0u8; 56];
    put_u32(&mut rc, 0, 4); // the 3D target
    put_u32(&mut rc, 4, 1); // VIRTIO_GPU_FORMAT_B8G8R8A8_UNORM
    put_u32(&mut rc, 8, 2); // bind: scanout
    put_u32(&mut rc, 12, 64);
    put_u32(&mut rc, 16, 64);
    put_u32(&mut rc, 20, 1);
    put_u32(&mut rc, 24, 1);
    put_u32(&mut rc, 28, 1);
    put_u32(&mut rc, 32, 1);
    put_u32(&mut rc, 48, 64 * 64 * 4);
    let got = unsafe {
        ioctl(
            fd,
            ioc_d(NR_RESOURCE_CREATE, 56),
            rc.as_mut_ptr() as *mut c_void,
        )
    };
    check_ok(
        r,
        "a 3D target is served, where a card without 3D answers EINVAL",
        got,
    );
    let handle = u32_at(&rc, 40);
    let res_handle = u32_at(&rc, 44);
    r.check("and a GEM handle comes back", handle != 0);
    // The resource id and the handle are **one number**, and that is what is
    // pinned here rather than merely "both are nonzero": the guest names a
    // resource inside its own command stream with the handle this request
    // answered, so a card that ran an id counter beside the handle table handed
    // the device ids it had never been told about. `vrend` answered exactly
    // that stream with `Illegal resource <n>` and Mesa's render process
    // stopped - no picture, for a reason three layers away from the number
    // that caused it. The two drifted apart at the first released handle,
    // because the counter only went up while handles went back into the table.
    check_is(
        r,
        "and the device resource id is the handle it answered",
        res_handle as isize,
        handle as isize,
    );
    // The other half of the same statement: the driver's own resources (the
    // scanout's framebuffer, the host cursor) must sit outside the range a
    // handle can ever reach, or one of *them* is what a command stream names.
    r.check(
        "and no handle is ever a driver-owned resource id",
        handle < DRIVER_RESOURCE_BASE,
    );

    // ---- CONTEXT_INIT: late, and that is the measurement -------------------
    // The create above made the context implicitly, because Linux's
    // `virtio_gpu_resource_create_ioctl` calls `virtio_gpu_create_context` as
    // soon as the card has 3D - *before* it looks at `target`, so a 2D create
    // would have done it too. An explicit request therefore arrives second and
    // answers `EEXIST`, which is the most counter-intuitive line in the
    // measurement and the one a "serve the explicit request" implementation
    // gets wrong in a way only a later caller sees.
    let params = [(1u64, 2u64), (2, 1)]; // CAPSET_ID = VIRGL2, NUM_RINGS = 1
    let mut ctx = [0u8; 16];
    put_u32(&mut ctx, 0, 2); // num_params
    ctx[8..16].copy_from_slice(&(params.as_ptr() as u64).to_ne_bytes());
    let got = unsafe {
        ioctl(
            fd,
            ioc_d(NR_CONTEXT_INIT, 16),
            ctx.as_mut_ptr() as *mut c_void,
        )
    };
    check_refused(
        r,
        "CONTEXT_INIT is EEXIST - the first 3D command already made the context",
        got,
        Errno::EXIST,
    );

    // ---- EXECBUFFER: what is refused, and what is served -------------------
    // `struct drm_virtgpu_execbuffer`: flags@0, size@4, command@8, bo_handles@16,
    // num_bo_handles@24, fence_fd@28, ring_idx@32.
    let stream = [0u32; 1];
    let mut handles = [handle];
    let mut eb = [0u8; 64];
    eb[8..16].copy_from_slice(&(stream.as_ptr() as u64).to_ne_bytes());
    eb[16..24].copy_from_slice(&(handles.as_ptr() as u64).to_ne_bytes());
    put_u32(&mut eb, 4, 4); // size: the stream is one u32
    put_u32(&mut eb, 24, 1); // num_bo_handles

    put_u32(&mut eb, 0, 0x01); // VIRTGPU_EXECBUF_FENCE_FD_IN
    let got = unsafe { ioctl(fd, ioc_d(NR_EXECBUFFER, 64), eb.as_mut_ptr() as *mut c_void) };
    check_refused(
        r,
        "a fence fd is EINVAL - there is no sync_file to put in one",
        got,
        Errno::INVAL,
    );

    put_u32(&mut eb, 0, 0);
    put_u32(&mut eb, 32, 1); // ring_idx
    let got = unsafe { ioctl(fd, ioc_d(NR_EXECBUFFER, 64), eb.as_mut_ptr() as *mut c_void) };
    check_refused(
        r,
        "a ring other than the first is EINVAL - one ring is driven",
        got,
        Errno::INVAL,
    );

    // A buffer this file does not hold is ENOENT, which is Linux's own answer
    // from the handle lookup rather than a code chosen here.
    put_u32(&mut eb, 32, 0);
    handles[0] = 0xffff;
    let got = unsafe { ioctl(fd, ioc_d(NR_EXECBUFFER, 64), eb.as_mut_ptr() as *mut c_void) };
    check_refused(
        r,
        "a buffer this file does not hold is ENOENT",
        got,
        Errno::NOENT,
    );

    handles[0] = handle;
    // The pointer in `eb` is to this array, so it is taken again now that the
    // array has changed. The address is the same one; saying so is also what
    // keeps the compiler from reporting the write above as never read - the
    // reader is the kernel, and it reads through the pointer.
    eb[16..24].copy_from_slice(&(handles.as_ptr() as u64).to_ne_bytes());
    let got = unsafe { ioctl(fd, ioc_d(NR_EXECBUFFER, 64), eb.as_mut_ptr() as *mut c_void) };
    r.check(
        "a submit is served - what the device makes of the stream is its answer",
        got == 0 || got == refused(Errno::IO),
    );

    // ---- TRANSFER_FROM_HOST, which the other card refuses ------------------
    let mut fh = [0u8; 44];
    put_u32(&mut fh, 0, handle);
    put_u32(&mut fh, 16, 64); // box.w
    put_u32(&mut fh, 20, 64); // box.h
    put_u32(&mut fh, 24, 1); // box.d
    let got = unsafe {
        ioctl(
            fd,
            ioc_d(NR_TRANSFER_FROM_HOST, 44),
            fh.as_mut_ptr() as *mut c_void,
        )
    };
    r.check(
        "TRANSFER_FROM_HOST is served, where a card without 3D answers ENOSYS",
        got != refused(Errno::NOSYS) && got != refused(Errno::INVAL),
    );

    // And an unknown handle is still ENOENT rather than a transfer of nothing.
    put_u32(&mut fh, 0, 9999);
    let got = unsafe {
        ioctl(
            fd,
            ioc_d(NR_TRANSFER_FROM_HOST, 44),
            fh.as_mut_ptr() as *mut c_void,
        )
    };
    check_refused(
        r,
        "TRANSFER_FROM_HOST on an unknown handle is ENOENT",
        got,
        Errno::NOENT,
    );

    // ---- A stream names a resource, and which file may name it --------------
    //
    // This is the shape a compositor's picture was lost to, and it is a claim
    // about *pixels* because it cannot be one about an errno: QEMU answers every
    // submit `OK_NODATA` and throws away what virglrenderer made of the stream
    // (`virtio_gpu_virgl_process_cmd`'s submit arm calls
    // `virgl_renderer_submit_cmd` and never reads its result, which is why the
    // return value is checked for "served" and nothing more). So the witness is
    // the transfer itself: `TRANSFER3D` from the host copies the host's pixels
    // into the pages the device was given as the resource's backing - the very
    // pages this program maps - so a marker written first and gone afterwards is
    // a command that really ran, and a marker still there is a batch that was
    // dropped.
    //
    // A resource is nameable **per context**, and the contexts here are per open
    // file description: `vrend`'s `vrend_renderer_ctx_res_lookup(ctx, handle)`
    // is what every command in a stream runs through, and `vrend_context`'s own
    // comment calls its table "resource bounds to this context". Linux keeps a
    // file's context told about a resource through
    // `virtio_gpu_cmd_context_attach_resource`, sent from
    // `virtio_gpu_gem_object_open` - the hook DRM calls when a *different* file
    // takes a handle to an object - and `..._detach_resource` from its close.
    // QEMU forwards exactly that pair and nothing else: its
    // `virgl_renderer_ctx_attach_resource` has one call site, the
    // `CTX_ATTACH_RESOURCE` handler, and the create handlers attach nothing.
    //
    // The two claims below are one question asked twice, and their *pair* is the
    // measurement: the first submits the stream from the file that made the
    // resource (the control - the same statement a stream that never leaves its
    // own file makes), the second from a second file that imported the
    // descriptor. If only the second fails, a resource is visible to the file
    // that made it and not to the file that took a handle to it; if the first
    // fails too, the create itself is the problem and nothing below it can be
    // trusted.
    let mut pin = [0u8; 56];
    put_u32(&mut pin, 0, 2); // PIPE_TEXTURE_2D: the target a GBM buffer arrives with
    put_u32(&mut pin, 4, 1); // VIRTIO_GPU_FORMAT_B8G8R8A8_UNORM
    put_u32(&mut pin, 8, 2); // bind: scanout
    put_u32(&mut pin, 12, 64);
    put_u32(&mut pin, 16, 64);
    put_u32(&mut pin, 20, 1);
    put_u32(&mut pin, 24, 1);
    put_u32(&mut pin, 28, 1);
    put_u32(&mut pin, 32, 1);
    put_u32(&mut pin, 48, PIN_BYTES as u32);
    let got = unsafe {
        ioctl(
            fd,
            ioc_d(NR_RESOURCE_CREATE, 56),
            pin.as_mut_ptr() as *mut c_void,
        )
    };
    check_ok(r, "the resource a command stream will name is created", got);
    let pin_handle = u32_at(&pin, 40);
    let pin_res = u32_at(&pin, 44);
    r.check(
        "and the stream can spell it - its id is the handle it answered",
        pin_handle != 0 && pin_res == pin_handle,
    );

    // Its pages, through the same `MAP` and `mmap` a dumb buffer goes through:
    // they are the pages the device reads and writes for this resource.
    let mut map = [0u8; 16];
    put_u32(&mut map, 8, pin_handle);
    let got = unsafe { ioctl(fd, ioc_d(NR_MAP, 16), map.as_mut_ptr() as *mut c_void) };
    check_ok(r, "and its pages map", got);
    let offset = u64_at(&map, 0);
    let mapped = unsafe {
        rustix::mm::mmap(
            core::ptr::null_mut(),
            PIN_BYTES,
            ProtFlags::READ | ProtFlags::WRITE,
            MapFlags::SHARED,
            &card,
            offset,
        )
    };
    check_ok(
        r,
        "into this program's own address space",
        mapped.is_ok() as isize,
    );
    let pages = mapped
        .map(|p| p as *mut u8)
        .unwrap_or(core::ptr::null_mut());
    if !pages.is_null() {
        fill_marker(pages, PIN_BYTES, 0xa5);
        r.check(
            "and hold what is written into them",
            marker_intact(pages, PIN_BYTES, 0xa5),
        );
    }

    let stream = transfer3d_from_host(pin_res, 64, 64);

    // The control: the file that made the resource submits the stream. This is
    // the same file, so whatever the create told the device it told about *this*
    // context - if even this is dropped, the resource does not exist on the host
    // at all and the failure below says nothing about which file may name it.
    if !pages.is_null() {
        fill_marker(pages, PIN_BYTES, 0xa5);
        let got = submit(fd, &stream, pin_handle);
        r.check(
            "a submit from the file that made the resource is served",
            got >= 0,
        );
        r.check(
            "and its transfer lands - the host's pixels reach the guest pages",
            !marker_intact(pages, PIN_BYTES, 0xa5),
        );
    }

    // The second file: the descriptor out of this card, and the render node in.
    // This is what a compositor does with a buffer it will draw through - it
    // hands the descriptor to the node it renders on, and that node *adopts the
    // resource id* rather than making a resource of its own: Mesa's
    // `virgl_drm_winsys` asks `RESOURCE_INFO` for the id of any handle it
    // imported, and spells that id in the streams it submits.
    let (got, descriptor) = prime_handle_to_fd(fd, pin_handle);
    check_ok(r, "the resource's descriptor comes out", got);
    match openat(CWD, "/dev/dri/renderD128", OFlags::RDWR, Mode::empty()) {
        Ok(render) => {
            r.check("/dev/dri/renderD128 opens", true);
            let rfd = render.as_raw_fd();
            let (got, imported) = prime_fd_to_handle(rfd, descriptor);
            check_ok(r, "and it goes back in on the render node", got);
            let (got, their_res) = resource_info(rfd, imported);
            r.check(
                "and the render node names the same device resource for it",
                got >= 0 && their_res == pin_res,
            );

            if !pages.is_null() {
                fill_marker(pages, PIN_BYTES, 0xa5);
                let got = submit(rfd, &stream, imported);
                r.check(
                    "a submit from the file that imported it is served",
                    got >= 0,
                );
                // The pin. A resource a second file holds a handle to is one that
                // file's context has to have been told about - Linux tells it from
                // `gem_object_open`, and a card that never does answers the batch
                // with `Illegal resource <id>` and drops it, leaving the marker
                // exactly as it was written.
                r.check(
                    "and that file's transfer lands too - a resource is nameable by the file holding it",
                    !marker_intact(pages, PIN_BYTES, 0xa5),
                );
            }
            drop(render);
        }
        Err(e) => {
            r.check(&format!("/dev/dri/renderD128 opens ({e:?})"), false);
        }
    }
    // The descriptor this program took out, back in: the `OwnedFd` of the card
    // is dropped at the end of `main`, and this one is not owned by anything.
    unsafe { rustix::io::close(descriptor) };
    let mut close = [0u8; 8];
    put_u32(&mut close, 0, pin_handle);
    let got = unsafe {
        ioctl(
            fd,
            ioc_d(NR_GEM_CLOSE, 8),
            close.as_mut_ptr() as *mut c_void,
        )
    };
    check_ok(r, "and the handle releases like any other", got);

    // ---- And the id a released handle frees is nameable by whatever takes it -
    //
    // A resource id here *is* a handle slot, and the next create claims the lowest
    // free one - so the resource made just below lands on the number just released,
    // and the file that names it is the same one whose context was told about the
    // *old* resource under that number. A card that kept "this context has been
    // told about id N" past the release would send no attach for the new one, and
    // the batch naming it would be dropped exactly as before, one reuse later. That
    // is why the release clears the record and tells the context to let go of the
    // id (Linux's `gem_object_close`), and this is the claim that goes red without
    // it.
    if !pages.is_null() {
        let _ = unsafe { rustix::mm::munmap(pages as *mut c_void, PIN_BYTES) };
    }
    let mut again = [0u8; 56];
    put_u32(&mut again, 0, 2);
    put_u32(&mut again, 4, 1);
    put_u32(&mut again, 8, 2);
    put_u32(&mut again, 12, 64);
    put_u32(&mut again, 16, 64);
    put_u32(&mut again, 20, 1);
    put_u32(&mut again, 24, 1);
    put_u32(&mut again, 28, 1);
    put_u32(&mut again, 32, 1);
    put_u32(&mut again, 48, PIN_BYTES as u32);
    let got = unsafe {
        ioctl(
            fd,
            ioc_d(NR_RESOURCE_CREATE, 56),
            again.as_mut_ptr() as *mut c_void,
        )
    };
    check_ok(r, "a resource made after a release is created", got);
    let again_handle = u32_at(&again, 40);
    let again_res = u32_at(&again, 44);
    r.check(
        "and it lands on the released handle's own id",
        again_res == pin_res && again_handle == pin_handle,
    );

    let mut remap = [0u8; 16];
    put_u32(&mut remap, 8, again_handle);
    let got = unsafe { ioctl(fd, ioc_d(NR_MAP, 16), remap.as_mut_ptr() as *mut c_void) };
    check_ok(r, "and its pages map", got);
    match unsafe {
        rustix::mm::mmap(
            core::ptr::null_mut(),
            PIN_BYTES,
            ProtFlags::READ | ProtFlags::WRITE,
            MapFlags::SHARED,
            &card,
            u64_at(&remap, 0),
        )
    } {
        Ok(mapped) => {
            let pages = mapped as *mut u8;
            fill_marker(pages, PIN_BYTES, 0xa5);
            let stream = transfer3d_from_host(again_res, 64, 64);
            let got = submit(fd, &stream, again_handle);
            r.check(
                "and the file that was told about the old one may name the new one",
                got >= 0 && !marker_intact(pages, PIN_BYTES, 0xa5),
            );
            let _ = unsafe { rustix::mm::munmap(pages as *mut c_void, PIN_BYTES) };
        }
        Err(_) => r.check("and its pages map into this program", false),
    }
    let mut close = [0u8; 8];
    put_u32(&mut close, 0, again_handle);
    let got = unsafe {
        ioctl(
            fd,
            ioc_d(NR_GEM_CLOSE, 8),
            close.as_mut_ptr() as *mut c_void,
        )
    };
    check_ok(r, "and that handle releases too", got);

    // ---- blobs stay refused, on this card too ------------------------------
    // The GL device this was measured against offers no blob feature, so Linux
    // answers `EINVAL` there as well - this is the measured answer rather than a
    // gap left open.
    let mut blob = [0u8; 56];
    let got = unsafe {
        ioctl(
            fd,
            ioc_d(NR_RESOURCE_CREATE_BLOB, 56),
            blob.as_mut_ptr() as *mut c_void,
        )
    };
    check_refused(
        r,
        "RESOURCE_CREATE_BLOB is EINVAL - this device has no blob feature",
        got,
        Errno::INVAL,
    );

    // ---- and the 3D handle releases like any other -------------------------
    let mut close = [0u8; 8];
    put_u32(&mut close, 0, handle);
    let got = unsafe {
        ioctl(
            fd,
            ioc_d(NR_GEM_CLOSE, 8),
            close.as_mut_ptr() as *mut c_void,
        )
    };
    check_ok(r, "GEM_CLOSE releases the 3D resource", got);
}
