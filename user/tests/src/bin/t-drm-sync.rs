//! t-drm-sync - the sync object family, and the two fences a commit carries.
//!
//! DRM's `SYNCOBJ_*` requests are how a client tells the kernel - and, through
//! `SYNCOBJ_HANDLE_TO_FD`, another process - that a buffer is ready to be read.
//! A compositor will not expose `wp_linux_drm_syncobj_v1` without them, and
//! `SYNCOBJ_EVENTFD` is what it probes first, so the family is on the critical
//! path of a client that renders on one thread and commits on another.
//!
//! What this test pins is the shape the *card* gives, which is not the shape the
//! upstream sources describe: an object with no fence behind it is **refused**
//! rather than treated as already signaled, a timeline is something an object
//! *becomes*, and the two descriptors the family hands out are told apart by the
//! request that takes them back. `NOTES.md` §7.4 records the measurement this was
//! written from, and `crates/gpu/src/sync.rs` holds the rules with a host test
//! each.
//!
//! The waits are the interesting half: `WAIT_FOR_SUBMIT` is the only form that
//! blocks, and the wake it parks on comes from *another task* - so the one check
//! here that forks exists to prove a sleeper is really woken rather than merely
//! told it would be.
//!
//! Everything runs on `/dev/dri/renderD128` rather than `card0`: none of these
//! requests present anything, the render node is the same card state, and using
//! it keeps the test out of the console-handover business `t-drm-lease` is about.
//!
//! # What the move to the std side changed
//!
//! The door, and nothing about a claim.
//!
//! The ioctl door is `libc::syscall(SYS_ioctl, ...)` through this program's own
//! `ioctl`, and the render node is a `rustix` `OwnedFd`. The parts that are not
//! ioctls go through the crate too: `rustix::event::eventfd` for the counter,
//! `rustix::event::poll` for "is this fence through", `rustix::io::read` for the
//! increment, `rustix::process::waitpid` for the two reaps, and `libc::fork` for
//! batch 18's reason (`rustix` binds no `fork`) with `libc::_exit` in the child so
//! it cannot run the parent's remaining claims from a child's body.
//!
//! One door is raw and says so: `setitimer(ITIMER_REAL)` has no `libc` binding on
//! a Linux target, so it is `libc::syscall(libc::SYS_setitimer, ...)`, whose
//! number is per-machine and comes from `libc`. `SIGALRM` is `libc::sigaction`
//! with a real `struct sigaction` - and *without* `SA_RESTORER`, which is the one
//! thing the no-std version had to spell out by hand (musl's `__restore_rt`, for a
//! kernel with no libc) and which a libc sets for its caller.
//!
//! Every record is spelled as `drm.h` lays it out, with the sizes and offsets as
//! compile-time asserts off a `cc` probe. That is how the previous batch found a
//! field the kernel read four bytes early; here the probe and the kernel agree
//! everywhere, which is the answer worth writing down too. Two things the probe
//! settled that a hand count would not: `drm_syncobj_wait` and
//! `drm_syncobj_timeline_wait` each carry a trailing `deadline_nsec` (40 and 48
//! bytes, not 32 and 40), and `drm_syncobj_transfer`'s `dst_point` is a `__u64`
//! rather than the `__u32` its name beside `src_point` suggests.
//!
//! Not made on a host, for `t-drm`'s reason: every claim below is about *this*
//! kernel's sync objects - the refusal of an unfenced object, the point rules,
//! what an import lands on - and a host's own render node answers its own.

use std::ffi::{c_int, c_void};
use std::mem::{offset_of, size_of};
use std::os::fd::{AsRawFd, OwnedFd};
use std::sync::atomic::{AtomicUsize, Ordering};

use mpos_user_tests::{Report, host_run};
use rustix::event::{EventfdFlags, PollFd, PollFlags, poll};
use rustix::fs::{Mode, OFlags};
use rustix::io::{Errno, read};
use rustix::process::{Pid, WaitOptions, waitpid};
use rustix::time::Timespec;

/// `_IOWR('d', nr, size)` as libdrm builds it: the direction pair at bits 30..31,
/// the argument's size at 16..31, the family byte at 8..15, the request number
/// below that.
const fn ioc_d(nr: u32, size: u32) -> u64 {
    ((3u64) << 30) | ((size as u64) << 16) | ((b'd' as u64) << 8) | (nr as u64)
}

const NR_GET_CAP: u32 = 0x0c;
const NR_SET_CLIENT_CAP: u32 = 0x0d;
/// `DRM_CLIENT_CAP_ATOMIC`: the cap that decides which properties a client is
/// *shown*. `OUT_FENCE_PTR` and `IN_FENCE_FD`, which this file reads below, are
/// both flagged atomic, so a client that has not set it does not find them in
/// the plane's list at all.
const CLIENT_CAP_ATOMIC: u64 = 3;
const NR_GETPROPERTY: u32 = 0xaa;
const NR_OBJ_GETPROPERTIES: u32 = 0xb9;
const NR_OBJ_SETPROPERTY: u32 = 0xba;
const NR_MODE_ATOMIC: u32 = 0xbc;
const NR_SYNCOBJ_CREATE: u32 = 0xbf;
const NR_SYNCOBJ_DESTROY: u32 = 0xc0;
const NR_SYNCOBJ_HANDLE_TO_FD: u32 = 0xc1;
const NR_SYNCOBJ_FD_TO_HANDLE: u32 = 0xc2;
const NR_SYNCOBJ_WAIT: u32 = 0xc3;
const NR_SYNCOBJ_RESET: u32 = 0xc4;
const NR_SYNCOBJ_SIGNAL: u32 = 0xc5;
const NR_SYNCOBJ_TIMELINE_WAIT: u32 = 0xca;
const NR_SYNCOBJ_QUERY: u32 = 0xcb;
const NR_SYNCOBJ_TRANSFER: u32 = 0xcc;
const NR_SYNCOBJ_TIMELINE_SIGNAL: u32 = 0xcd;
const NR_SYNCOBJ_EVENTFD: u32 = 0xcf;

const PLANE_ID: u32 = 36;
const OBJECT_PLANE: u32 = 0xeeee_eeee;

const DRM_CAP_SYNCOBJ: u64 = 0x13;
const DRM_CAP_SYNCOBJ_TIMELINE: u64 = 0x14;

const CREATE_SIGNALED: u32 = 1 << 0;
const H2F_EXPORT_SYNC_FILE: u32 = 1 << 0;
const F2H_IMPORT_SYNC_FILE: u32 = 1 << 0;
/// `DRM_SYNCOBJ_*_FLAGS_TIMELINE`: what makes the request's `point` a *timeline*
/// point rather than a number the operation may ignore. Both directions spell it
/// `1 << 1`, which is the header's own number - the flags are pinned below
/// against their behaviour rather than against a copy of this file's constant.
const H2F_TIMELINE: u32 = 1 << 1;
const F2H_TIMELINE: u32 = 1 << 1;
const WAIT_ALL: u32 = 1 << 0;
const WAIT_FOR_SUBMIT: u32 = 1 << 1;
const WAIT_AVAILABLE: u32 = 1 << 2;

/// `ITIMER_REAL`, which `libc` does not define on a Linux target.
const ITIMER_REAL: c_int = 0;

// --- the records -----------------------------------------------------------------
//
// Spelled as `drm.h` lays them out, sizes as `size_of`, offsets as compile-time
// asserts off `.workbuddy/syncobjsize.c`. `libc` binds none of these.

/// `struct drm_syncobj_create`, `sizeof` 8.
#[repr(C)]
#[derive(Default)]
struct SyncobjCreate {
    handle: u32,
    flags: u32,
}

/// `struct drm_syncobj_destroy`, `sizeof` 8.
#[repr(C)]
#[derive(Default)]
struct SyncobjDestroy {
    handle: u32,
    pad: u32,
}

/// `struct drm_syncobj_handle` - the one record both directions of the
/// handle/fd pair use, `sizeof` 24.
#[repr(C)]
#[derive(Default)]
struct SyncobjHandle {
    handle: u32,
    flags: u32,
    fd: i32,
    pad: u32,
    point: u64,
}

/// `struct drm_syncobj_wait`, `sizeof` 40 - it carries a trailing
/// `deadline_nsec`, so it is 40 and not the 32 the six fields above it fill.
#[repr(C)]
#[derive(Default)]
struct SyncobjWait {
    handles: u64,
    timeout_nsec: i64,
    count_handles: u32,
    flags: u32,
    first_signaled: u32,
    pad: u32,
    deadline_nsec: u64,
}

/// `struct drm_syncobj_timeline_wait`, `sizeof` 48 - same trailing field.
#[repr(C)]
#[derive(Default)]
struct SyncobjTimelineWait {
    handles: u64,
    points: u64,
    timeout_nsec: i64,
    count_handles: u32,
    flags: u32,
    first_signaled: u32,
    pad: u32,
    deadline_nsec: u64,
}

/// `struct drm_syncobj_array`, `sizeof` 16.
#[repr(C)]
#[derive(Default)]
struct SyncobjArray {
    handles: u64,
    count_handles: u32,
    pad: u32,
}

/// `struct drm_syncobj_timeline_array`, `sizeof` 24 - `QUERY` and
/// `TIMELINE_SIGNAL` both spell their argument with it.
#[repr(C)]
#[derive(Default)]
struct SyncobjTimelineArray {
    handles: u64,
    points: u64,
    count_handles: u32,
    flags: u32,
}

/// `struct drm_syncobj_transfer`, `sizeof` 32. `dst_point` is a `__u64`, not the
/// `__u32` its place beside `src_point` suggests.
#[repr(C)]
#[derive(Default)]
struct SyncobjTransfer {
    src_handle: u32,
    dst_handle: u32,
    src_point: u64,
    dst_point: u64,
    flags: u32,
    pad: u32,
}

/// `struct drm_syncobj_eventfd`, `sizeof` 24.
#[repr(C)]
#[derive(Default)]
struct SyncobjEventfd {
    handle: u32,
    flags: u32,
    point: u64,
    fd: i32,
    pad: u32,
}

/// `struct drm_mode_atomic`, `sizeof` 56 - four pointers plus the two the
/// caller never sets (`reserved`, `user_data`).
#[repr(C)]
#[derive(Default)]
struct ModeAtomic {
    flags: u32,
    count_objs: u32,
    objs_ptr: u64,
    count_props_ptr: u64,
    props_ptr: u64,
    prop_values_ptr: u64,
    reserved: u64,
    user_data: u64,
}

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

/// `struct drm_mode_obj_get_properties`, `sizeof` 32.
#[repr(C)]
#[derive(Default)]
struct ObjGetProperties {
    props_ptr: u64,
    prop_values_ptr: u64,
    count_props: u32,
    obj_id: u32,
    obj_type: u32,
}

/// `struct drm_get_cap` / `struct drm_set_client_cap`, both `sizeof` 16.
#[repr(C)]
#[derive(Default)]
struct Cap {
    capability: u64,
    value: u64,
}

const _: () = assert!(size_of::<SyncobjCreate>() == 8);
const _: () = assert!(offset_of!(SyncobjCreate, handle) == 0);
const _: () = assert!(offset_of!(SyncobjCreate, flags) == 4);

const _: () = assert!(size_of::<SyncobjDestroy>() == 8);
const _: () = assert!(offset_of!(SyncobjDestroy, handle) == 0);

const _: () = assert!(size_of::<SyncobjHandle>() == 24);
const _: () = assert!(offset_of!(SyncobjHandle, handle) == 0);
const _: () = assert!(offset_of!(SyncobjHandle, flags) == 4);
const _: () = assert!(offset_of!(SyncobjHandle, fd) == 8);
const _: () = assert!(offset_of!(SyncobjHandle, point) == 16);

const _: () = assert!(size_of::<SyncobjWait>() == 40);
const _: () = assert!(offset_of!(SyncobjWait, handles) == 0);
const _: () = assert!(offset_of!(SyncobjWait, timeout_nsec) == 8);
const _: () = assert!(offset_of!(SyncobjWait, count_handles) == 16);
const _: () = assert!(offset_of!(SyncobjWait, flags) == 20);
const _: () = assert!(offset_of!(SyncobjWait, first_signaled) == 24);

const _: () = assert!(size_of::<SyncobjTimelineWait>() == 48);
const _: () = assert!(offset_of!(SyncobjTimelineWait, handles) == 0);
const _: () = assert!(offset_of!(SyncobjTimelineWait, points) == 8);
const _: () = assert!(offset_of!(SyncobjTimelineWait, timeout_nsec) == 16);
const _: () = assert!(offset_of!(SyncobjTimelineWait, count_handles) == 24);
const _: () = assert!(offset_of!(SyncobjTimelineWait, flags) == 28);

const _: () = assert!(size_of::<SyncobjArray>() == 16);
const _: () = assert!(offset_of!(SyncobjArray, handles) == 0);
const _: () = assert!(offset_of!(SyncobjArray, count_handles) == 8);

const _: () = assert!(size_of::<SyncobjTimelineArray>() == 24);
const _: () = assert!(offset_of!(SyncobjTimelineArray, handles) == 0);
const _: () = assert!(offset_of!(SyncobjTimelineArray, points) == 8);
const _: () = assert!(offset_of!(SyncobjTimelineArray, count_handles) == 16);

const _: () = assert!(size_of::<SyncobjTransfer>() == 32);
const _: () = assert!(offset_of!(SyncobjTransfer, src_handle) == 0);
const _: () = assert!(offset_of!(SyncobjTransfer, dst_handle) == 4);
const _: () = assert!(offset_of!(SyncobjTransfer, src_point) == 8);
const _: () = assert!(offset_of!(SyncobjTransfer, dst_point) == 16);
const _: () = assert!(offset_of!(SyncobjTransfer, flags) == 24);

const _: () = assert!(size_of::<SyncobjEventfd>() == 24);
const _: () = assert!(offset_of!(SyncobjEventfd, handle) == 0);
const _: () = assert!(offset_of!(SyncobjEventfd, flags) == 4);
const _: () = assert!(offset_of!(SyncobjEventfd, point) == 8);
const _: () = assert!(offset_of!(SyncobjEventfd, fd) == 16);

const _: () = assert!(size_of::<ModeAtomic>() == 56);
const _: () = assert!(offset_of!(ModeAtomic, flags) == 0);
const _: () = assert!(offset_of!(ModeAtomic, count_objs) == 4);
const _: () = assert!(offset_of!(ModeAtomic, objs_ptr) == 8);
const _: () = assert!(offset_of!(ModeAtomic, count_props_ptr) == 16);
const _: () = assert!(offset_of!(ModeAtomic, props_ptr) == 24);
const _: () = assert!(offset_of!(ModeAtomic, prop_values_ptr) == 32);

const _: () = assert!(size_of::<GetProperty>() == 64);
const _: () = assert!(offset_of!(GetProperty, prop_id) == 16);
const _: () = assert!(offset_of!(GetProperty, name) == 24);

const _: () = assert!(size_of::<ObjGetProperties>() == 32);
const _: () = assert!(offset_of!(ObjGetProperties, count_props) == 16);
const _: () = assert!(offset_of!(ObjGetProperties, obj_id) == 20);
const _: () = assert!(offset_of!(ObjGetProperties, obj_type) == 24);

const _: () = assert!(size_of::<Cap>() == 16);

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

// --- the sync-object requests -------------------------------------------------------

/// `SYNCOBJ_CREATE`, answering the handle it gave.
fn create(fd: i32, flags: u32) -> (isize, u32) {
    let mut arg = SyncobjCreate {
        flags,
        ..SyncobjCreate::default()
    };
    let r = unsafe {
        ioctl(
            fd,
            ioc_d(NR_SYNCOBJ_CREATE, size_of::<SyncobjCreate>() as u32),
            (&raw mut arg) as *mut c_void,
        )
    };
    (r, arg.handle)
}

/// `SYNCOBJ_WAIT` on one handle. `timeout` is the header's absolute
/// `CLOCK_MONOTONIC` deadline in nanoseconds; -1 is "for ever" and 0 is "do not
/// wait at all".
fn wait_one(fd: i32, handle: u32, timeout: i64, flags: u32) -> (isize, u32) {
    let mut h = [handle];
    let mut arg = SyncobjWait {
        timeout_nsec: timeout,
        count_handles: 1,
        flags,
        ..SyncobjWait::default()
    };
    arg.handles = h.as_mut_ptr() as u64;
    let r = unsafe {
        ioctl(
            fd,
            ioc_d(NR_SYNCOBJ_WAIT, size_of::<SyncobjWait>() as u32),
            (&raw mut arg) as *mut c_void,
        )
    };
    (r, arg.first_signaled)
}

/// `SYNCOBJ_TIMELINE_WAIT` on one handle and one point.
fn timeline_wait_one(fd: i32, handle: u32, point: u64, timeout: i64, flags: u32) -> isize {
    let mut h = [handle];
    let mut p = [point];
    let mut arg = SyncobjTimelineWait {
        timeout_nsec: timeout,
        count_handles: 1,
        flags,
        ..SyncobjTimelineWait::default()
    };
    arg.handles = h.as_mut_ptr() as u64;
    arg.points = p.as_mut_ptr() as u64;
    unsafe {
        ioctl(
            fd,
            ioc_d(
                NR_SYNCOBJ_TIMELINE_WAIT,
                size_of::<SyncobjTimelineWait>() as u32,
            ),
            (&raw mut arg) as *mut c_void,
        )
    }
}

/// `SYNCOBJ_SIGNAL` / `SYNCOBJ_RESET`: one handle, no point.
fn array_call(fd: i32, nr: u32, handle: u32) -> isize {
    let mut h = [handle];
    let mut arg = SyncobjArray {
        count_handles: 1,
        ..SyncobjArray::default()
    };
    arg.handles = h.as_mut_ptr() as u64;
    unsafe {
        ioctl(
            fd,
            ioc_d(nr, size_of::<SyncobjArray>() as u32),
            (&raw mut arg) as *mut c_void,
        )
    }
}

/// `SYNCOBJ_TIMELINE_SIGNAL`: one handle at one point.
fn timeline_signal(fd: i32, handle: u32, point: u64) -> isize {
    let mut h = [handle];
    let mut p = [point];
    let mut arg = SyncobjTimelineArray {
        count_handles: 1,
        ..SyncobjTimelineArray::default()
    };
    arg.handles = h.as_mut_ptr() as u64;
    arg.points = p.as_mut_ptr() as u64;
    unsafe {
        ioctl(
            fd,
            ioc_d(
                NR_SYNCOBJ_TIMELINE_SIGNAL,
                size_of::<SyncobjTimelineArray>() as u32,
            ),
            (&raw mut arg) as *mut c_void,
        )
    }
}

/// `SYNCOBJ_QUERY`: the point one object has reached, or a negative errno.
fn query(fd: i32, handle: u32) -> (isize, u64) {
    let mut h = [handle];
    let mut out = [0u64; 1];
    let mut arg = SyncobjTimelineArray {
        count_handles: 1,
        ..SyncobjTimelineArray::default()
    };
    arg.handles = h.as_mut_ptr() as u64;
    arg.points = out.as_mut_ptr() as u64;
    let r = unsafe {
        ioctl(
            fd,
            ioc_d(NR_SYNCOBJ_QUERY, size_of::<SyncobjTimelineArray>() as u32),
            (&raw mut arg) as *mut c_void,
        )
    };
    (r, out[0])
}

/// `SYNCOBJ_HANDLE_TO_FD`, answering the descriptor or the errno.
fn handle_to_fd(fd: i32, handle: u32, flags: u32, point: u64) -> (isize, i32) {
    let mut arg = SyncobjHandle {
        handle,
        flags,
        fd: -1,
        point,
        ..SyncobjHandle::default()
    };
    let r = unsafe {
        ioctl(
            fd,
            ioc_d(NR_SYNCOBJ_HANDLE_TO_FD, size_of::<SyncobjHandle>() as u32),
            (&raw mut arg) as *mut c_void,
        )
    };
    (r, arg.fd)
}

/// `SYNCOBJ_FD_TO_HANDLE`, answering the handle it made. The `point` field is
/// left as [`fd_to_handle_at`]'s zero, which is every call but the ones about
/// the field itself.
fn fd_to_handle(fd: i32, target: u32, flags: u32, dfd: i32) -> (isize, u32) {
    fd_to_handle_at(fd, target, flags, dfd, 0)
}

/// The same request with the `point` field named - the field the two
/// `*_FLAGS_TIMELINE` flags are about.
fn fd_to_handle_at(fd: i32, target: u32, flags: u32, dfd: i32, point: u64) -> (isize, u32) {
    let mut arg = SyncobjHandle {
        handle: target,
        flags,
        fd: dfd,
        point,
        ..SyncobjHandle::default()
    };
    let r = unsafe {
        ioctl(
            fd,
            ioc_d(NR_SYNCOBJ_FD_TO_HANDLE, size_of::<SyncobjHandle>() as u32),
            (&raw mut arg) as *mut c_void,
        )
    };
    (r, arg.handle)
}

/// `SYNCOBJ_DESTROY`: one handle.
fn destroy(fd: i32, handle: u32) -> isize {
    let mut arg = SyncobjDestroy {
        handle,
        ..SyncobjDestroy::default()
    };
    unsafe {
        ioctl(
            fd,
            ioc_d(NR_SYNCOBJ_DESTROY, size_of::<SyncobjDestroy>() as u32),
            (&raw mut arg) as *mut c_void,
        )
    }
}

/// A `poll` timeout of nothing at all: the wait that asks and never parks.
const NOW: Timespec = Timespec {
    tv_sec: 0,
    tv_nsec: 0,
};

/// The negative errno a failed `read` left, so a claim that names `EAGAIN` says
/// what actually arrived.
fn errno_is(e: Errno) -> isize {
    -(e.raw_os_error() as isize)
}

/// Whether a descriptor polls readable, which is what a `sync_file` says.
fn polls_readable(fd: i32) -> bool {
    let raw = unsafe { std::os::fd::BorrowedFd::borrow_raw(fd) };
    let mut fds = [PollFd::from_borrowed_fd(raw, PollFlags::IN)];
    match poll(&mut fds, Some(&NOW)) {
        Ok(n) => n > 0 && fds[0].revents().contains(PollFlags::IN),
        Err(_) => false,
    }
}

/// `GET_CAP`'s answer for one capability.
fn cap(fd: i32, which: u64) -> (isize, u64) {
    let mut arg = Cap {
        capability: which,
        ..Cap::default()
    };
    let r = unsafe {
        ioctl(
            fd,
            ioc_d(NR_GET_CAP, size_of::<Cap>() as u32),
            (&raw mut arg) as *mut c_void,
        )
    };
    (r, arg.value)
}

/// Set `DRM_CLIENT_CAP_ATOMIC` on a descriptor. See the constant: the two fence
/// properties this file reads below are atomic ones, and the plane's list does
/// not carry them for a client that has not asked.
fn set_atomic(fd: i32) -> isize {
    let mut arg = Cap {
        capability: CLIENT_CAP_ATOMIC,
        value: 1,
    };
    unsafe {
        ioctl(
            fd,
            ioc_d(NR_SET_CLIENT_CAP, size_of::<Cap>() as u32),
            (&raw mut arg) as *mut c_void,
        )
    }
}

/// The id of a property, by name, out of one object's list.
///
/// The name is the ABI and the id is this card's numbering, so a test that
/// hard-coded the id would be checking its own copy of the driver's constants
/// rather than what a client is told.
fn property_id(fd: i32, obj: u32, obj_type: u32, want: &[u8]) -> u32 {
    let mut props = [0u32; 32];
    let mut values = [0u64; 32];
    let mut arg = ObjGetProperties {
        count_props: props.len() as u32,
        obj_id: obj,
        obj_type,
        ..ObjGetProperties::default()
    };
    arg.props_ptr = props.as_mut_ptr() as u64;
    arg.prop_values_ptr = values.as_mut_ptr() as u64;
    let got = unsafe {
        ioctl(
            fd,
            ioc_d(NR_OBJ_GETPROPERTIES, size_of::<ObjGetProperties>() as u32),
            (&raw mut arg) as *mut c_void,
        )
    };
    if got != 0 {
        return 0;
    }
    let count = arg.count_props as usize;
    for &id in props.iter().take(count.min(props.len())) {
        let mut p = GetProperty {
            prop_id: id,
            ..GetProperty::default()
        };
        if unsafe {
            ioctl(
                fd,
                ioc_d(NR_GETPROPERTY, size_of::<GetProperty>() as u32),
                (&raw mut p) as *mut c_void,
            )
        } != 0
        {
            continue;
        }
        let end = p.name.iter().position(|&c| c == 0).unwrap_or(p.name.len());
        if &p.name[..end] == want {
            return id;
        }
    }
    0
}

/// `MODE_ATOMIC` over one property of the plane, with the commit's own
/// `user_data` unused and no flip event asked for.
fn atomic_one(fd: i32, prop: u32, value: u64, flags: u32) -> isize {
    let mut objs = [PLANE_ID];
    let mut counts = [1u32];
    let mut props = [prop];
    let mut values = [value];
    let mut arg = ModeAtomic {
        flags,
        count_objs: 1,
        ..ModeAtomic::default()
    };
    arg.objs_ptr = objs.as_mut_ptr() as u64;
    arg.count_props_ptr = counts.as_mut_ptr() as u64;
    arg.props_ptr = props.as_mut_ptr() as u64;
    arg.prop_values_ptr = values.as_mut_ptr() as u64;
    unsafe {
        ioctl(
            fd,
            ioc_d(NR_MODE_ATOMIC, size_of::<ModeAtomic>() as u32),
            (&raw mut arg) as *mut c_void,
        )
    }
}

// ---- interrupting a blocked wait: the `SA_RESTART` rewind pin --------------

/// How many times the alarm handler ran. Each is a wait a signal reached - the
/// subject of the rewind check at the end of the file.
static RAN: AtomicUsize = AtomicUsize::new(0);

extern "C" fn on_alarm(_sig: c_int, _info: *mut libc::siginfo_t, _ctx: *mut c_void) {
    RAN.fetch_add(1, Ordering::Relaxed);
}

/// Install a *caught* `SIGALRM` handler, with or without `SA_RESTART`. Caught, so
/// the alarm ends the wait rather than killing the process; the flag decides
/// whether the kernel then rewinds the interrupted `ioctl` or lets it answer
/// `EINTR`.
///
/// `SA_RESTORER` is deliberately *not* set: it is the one thing the no-std
/// version had to spell out by hand, because a kernel with no libc has nothing
/// to return through. A libc sets it for its caller.
fn install_alarm(restart: bool) {
    let mut sa: libc::sigaction = unsafe { std::mem::zeroed() };
    // `sigaction`'s handler field is a `sighandler_t`, which is an integer here
    // rather than a pointer, so the function goes through a pointer first.
    sa.sa_sigaction = on_alarm as *const () as usize;
    sa.sa_flags = libc::SA_SIGINFO | if restart { libc::SA_RESTART } else { 0 };
    unsafe {
        libc::sigaction(libc::SIGALRM, &sa, std::ptr::null_mut());
    }
}

/// Arm `ITIMER_REAL` to fire once, `ms` from now, and disarm any earlier one.
///
/// `libc` binds no `setitimer` on a Linux target, so this is the raw syscall -
/// with the number from `libc`, which is what keeps it right on both machines.
fn alarm_once(ms: u64) {
    let next = libc::itimerval {
        it_interval: libc::timeval {
            tv_sec: 0,
            tv_usec: 0,
        },
        it_value: libc::timeval {
            tv_sec: (ms / 1000) as i64,
            tv_usec: ((ms % 1000) as i64) * 1000,
        },
    };
    let mut spent: libc::itimerval = unsafe { std::mem::zeroed() };
    unsafe {
        libc::syscall(
            libc::SYS_setitimer,
            ITIMER_REAL as libc::c_long,
            &next,
            &mut spent,
        );
    }
}

/// A `SYNCOBJ_WAIT` deadline an absolute `ms` after the monotonic clock, which
/// the header wants in nanoseconds since boot. A fixed number would be a
/// deadline that the sweep's own elapsed boot-time has already passed.
fn deadline_ms_ahead(ms: i64) -> i64 {
    let mut ts: libc::timespec = unsafe { std::mem::zeroed() };
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    ts.tv_sec as i64 * 1_000_000_000 + ts.tv_nsec as i64 + ms * 1_000_000
}

/// Sleep in a forked child, where `std`'s own timers would be reached through
/// locks the parent may hold.
fn child_nap(ms: u64) {
    let req = libc::timespec {
        tv_sec: (ms / 1000) as i64,
        tv_nsec: ((ms % 1000) as i64) * 1_000_000,
    };
    let mut rem: libc::timespec = unsafe { std::mem::zeroed() };
    unsafe {
        libc::nanosleep(&req, &mut rem);
    }
}

/// Reap one child and answer its exit status, or `None` if it did not exit.
fn reap(child: i32) -> Option<i32> {
    waitpid(Pid::from_raw(child), WaitOptions::empty())
        .ok()
        .flatten()
        .and_then(|(_, status)| status.exit_status())
}

fn main() {
    println!("t-drm-sync");
    let mut r = Report::new();

    // See the header: every claim below is about this kernel's sync objects -
    // the refusal of an unfenced object, the point rules, what an import lands
    // on - so a hosted run makes no claim at all.
    if host_run() {
        println!(
            "note: not made on a host - every claim below is about this kernel's sync \
             objects, and a host's own render node answers its own"
        );
        r.finish();
    }

    let node: OwnedFd = match rustix::fs::open("/dev/dri/renderD128", OFlags::RDWR, Mode::empty()) {
        Ok(fd) => {
            r.check("/dev/dri/renderD128 opens", true);
            fd
        }
        Err(e) => {
            r.check(&format!("/dev/dri/renderD128 opens ({e:?})"), false);
            println!("FAILED");
            std::process::exit(1);
        }
    };
    let fd = node.as_raw_fd();

    // The cap a compositor sets before it reads a property. Set here and not
    // only asserted, because the plane's property list lower down is read the way
    // a compositor reads it - and `OUT_FENCE_PTR`/`IN_FENCE_FD` are atomic
    // properties, which a client that has not asked for them is not shown.
    check_is(
        &mut r,
        "DRM_CLIENT_CAP_ATOMIC is accepted on the render node",
        set_atomic(fd),
        0,
    );

    // The two capabilities Smithay asks before it will expose the explicit-sync
    // protocol at all. Answering 1 is a promise the rest of this file is the
    // evidence for.
    let (got, v) = cap(fd, DRM_CAP_SYNCOBJ);
    check_is(&mut r, "GET_CAP(SYNCOBJ) succeeds", got, 0);
    check_is(&mut r, "and the card reports it", v as isize, 1);
    let (got, v) = cap(fd, DRM_CAP_SYNCOBJ_TIMELINE);
    check_is(&mut r, "GET_CAP(SYNCOBJ_TIMELINE) succeeds", got, 0);
    check_is(&mut r, "and the card reports it too", v as isize, 1);

    // --- creation, and the state every refusal is about ---
    let (got, plain) = create(fd, 0);
    check_is(&mut r, "CREATE a plain object succeeds", got, 0);
    check_true(&mut r, "and it is given a handle", plain != 0);
    let (got, signaled) = create(fd, CREATE_SIGNALED);
    check_is(&mut r, "CREATE SIGNALED succeeds", got, 0);
    check_true(
        &mut r,
        "and a second object gets its own handle",
        signaled != plain,
    );
    let (got, _) = create(fd, 0x8000);
    check_refused(
        &mut r,
        "an unknown CREATE flag is refused",
        got,
        Errno::INVAL,
    );

    // An object with nothing behind it is *refused*, not treated as ready: this
    // is the rule the measured card has and the upstream sources do not.
    let (got, _) = wait_one(fd, plain, 0, 0);
    check_refused(
        &mut r,
        "WAIT an object with no fence is refused, not answered",
        got,
        Errno::INVAL,
    );
    let (got, _) = wait_one(fd, plain, -1, 0);
    check_refused(
        &mut r,
        "and no timeout makes it a wait that could end",
        got,
        Errno::INVAL,
    );
    let (got, _) = wait_one(fd, signaled, 0, 0);
    check_is(&mut r, "WAIT the SIGNALED one answers at once", got, 0);
    let (_, first) = wait_one(fd, signaled, 0, 0);
    check_is(
        &mut r,
        "naming the handle that satisfied it",
        first as isize,
        0,
    );
    // `WAIT_ALL` is the "every handle" form, and on this card the *empty* object
    // poisons the whole array rather than being skipped as already-signaled.
    let mut mixed = [signaled, plain];
    let mut arg = SyncobjWait {
        count_handles: 2,
        flags: WAIT_ALL,
        ..SyncobjWait::default()
    };
    arg.handles = mixed.as_mut_ptr() as u64;
    let got = unsafe {
        ioctl(
            fd,
            ioc_d(NR_SYNCOBJ_WAIT, size_of::<SyncobjWait>() as u32),
            (&raw mut arg) as *mut c_void,
        )
    };
    check_refused(
        &mut r,
        "WAIT_ALL over a signaled and an empty object is refused",
        got,
        Errno::INVAL,
    );
    let (got, other) = create(fd, CREATE_SIGNALED);
    check_is(&mut r, "a second signaled object", got, 0);
    let mut full = [signaled, other];
    let mut arg = SyncobjWait {
        count_handles: 2,
        flags: WAIT_ALL,
        ..SyncobjWait::default()
    };
    arg.handles = full.as_mut_ptr() as u64;
    let got = unsafe {
        ioctl(
            fd,
            ioc_d(NR_SYNCOBJ_WAIT, size_of::<SyncobjWait>() as u32),
            (&raw mut arg) as *mut c_void,
        )
    };
    check_is(&mut r, "WAIT_ALL over two signaled objects", got, 0);

    // `WAIT_FOR_SUBMIT` is the only form that blocks, and with nothing coming
    // it is the clock that ends the wait.
    let (got, _) = wait_one(fd, plain, 0, WAIT_FOR_SUBMIT);
    check_refused(
        &mut r,
        "WAIT_FOR_SUBMIT with nothing coming times out",
        got,
        Errno::TIME,
    );
    let (got, _) = wait_one(fd, 0xdead_beef, 0, 0);
    check_refused(
        &mut r,
        "WAIT a handle that does not exist",
        got,
        Errno::NOENT,
    );
    let (got, _) = wait_one(fd, plain, 0, 0x8000);
    check_refused(&mut r, "an unknown WAIT flag is refused", got, Errno::INVAL);
    let mut arg = SyncobjWait::default();
    let got = unsafe {
        ioctl(
            fd,
            ioc_d(NR_SYNCOBJ_WAIT, size_of::<SyncobjWait>() as u32),
            (&raw mut arg) as *mut c_void,
        )
    };
    check_is(&mut r, "WAIT of no handles is a no-op", got, 0);
    let mut arg = SyncobjWait {
        count_handles: 1,
        ..SyncobjWait::default()
    };
    let got = unsafe {
        ioctl(
            fd,
            ioc_d(NR_SYNCOBJ_WAIT, size_of::<SyncobjWait>() as u32),
            (&raw mut arg) as *mut c_void,
        )
    };
    check_refused(
        &mut r,
        "and a null handle array is the caller's own memory failing",
        got,
        Errno::FAULT,
    );

    // --- SIGNAL and RESET: the two writes that make and unmake a fence ---
    check_is(
        &mut r,
        "SIGNAL succeeds",
        array_call(fd, NR_SYNCOBJ_SIGNAL, plain),
        0,
    );
    let (got, _) = wait_one(fd, plain, 0, 0);
    check_is(&mut r, "and the object is waitable now", got, 0);
    check_is(
        &mut r,
        "RESET succeeds",
        array_call(fd, NR_SYNCOBJ_RESET, plain),
        0,
    );
    let (got, _) = wait_one(fd, plain, 0, 0);
    check_refused(&mut r, "and takes the fence away again", got, Errno::INVAL);
    check_refused(
        &mut r,
        "RESET of a handle that does not exist",
        array_call(fd, NR_SYNCOBJ_RESET, 0xdead_beef),
        Errno::NOENT,
    );

    // --- a sleeper really woken by another task ---
    check_is(
        &mut r,
        "SIGNAL the object the waiter will park on",
        array_call(fd, NR_SYNCOBJ_SIGNAL, plain),
        0,
    );
    check_is(
        &mut r,
        "RESET it again so the wait has something to wait for",
        array_call(fd, NR_SYNCOBJ_RESET, plain),
        0,
    );
    let child = unsafe { libc::fork() };
    if child == 0 {
        // The child holds the same card description, so it names the same
        // handle: this is the shape a render thread waking a commit thread has.
        child_nap(50);
        let _ = array_call(fd, NR_SYNCOBJ_SIGNAL, plain);
        unsafe { libc::_exit(0) };
    }
    // A second of budget against a 50 ms nap: the wait ends when the fence
    // arrives, and if the wake were lost this would be the clock instead. The
    // deadline is *absolute* - the ABI's `timeout_nsec` is a `CLOCK_MONOTONIC`
    // instant, not a duration - so it is "now + 2 s" through the helper the
    // `SA_RESTART` checks below already use. A bare `2_000_000_000` here read as
    // "two seconds after boot", which is in the past by the time this program
    // runs, so the wait answered `ETIME` before the child could ever signal:
    // the check passed only while the sweep reached it inside the machine's
    // first two seconds, and stopped passing once the sweep grew past that.
    let (got, _) = wait_one(fd, plain, deadline_ms_ahead(1_000), WAIT_FOR_SUBMIT);
    check_is(
        &mut r,
        "a WAIT_FOR_SUBMIT is woken by another task's SIGNAL",
        got,
        0,
    );
    check_is(
        &mut r,
        "and the signaller exited cleanly",
        reap(child).unwrap_or(-1) as isize,
        0,
    );

    // --- timelines: a point submitted is what makes an object one ---
    check_refused(
        &mut r,
        "TIMELINE_WAIT an object that never had a point is refused",
        timeline_wait_one(fd, signaled, 0, 0, 0),
        Errno::INVAL,
    );
    check_is(
        &mut r,
        "TIMELINE_SIGNAL point 4 succeeds",
        timeline_signal(fd, signaled, 4),
        0,
    );
    let (got, point) = query(fd, signaled);
    check_is(&mut r, "QUERY succeeds", got, 0);
    check_is(&mut r, "and reports the point submitted", point as isize, 4);
    check_is(
        &mut r,
        "TIMELINE_WAIT at that point answers",
        timeline_wait_one(fd, signaled, 4, 0, 0),
        0,
    );
    check_is(
        &mut r,
        "and at a point below it too",
        timeline_wait_one(fd, signaled, 1, 0, 0),
        0,
    );
    check_refused(
        &mut r,
        "but not at a point nothing has reached",
        timeline_wait_one(fd, signaled, 5, 0, 0),
        Errno::INVAL,
    );
    check_is(
        &mut r,
        "which WAIT_FOR_SUBMIT answers on the fence alone",
        timeline_wait_one(fd, signaled, 9, 0, WAIT_FOR_SUBMIT),
        0,
    );
    // A point only moves forward: a backwards signal is accepted and changes
    // nothing, which is what stops a wait on a point already passed from failing
    // again.
    check_is(
        &mut r,
        "a backwards TIMELINE_SIGNAL is accepted",
        timeline_signal(fd, signaled, 2),
        0,
    );
    let (_, point) = query(fd, signaled);
    check_is(&mut r, "and moves nothing", point as isize, 4);
    // `SIGNAL` is accepted on a timeline and leaves the point alone.
    check_is(
        &mut r,
        "SIGNAL on a timeline is accepted",
        array_call(fd, NR_SYNCOBJ_SIGNAL, signaled),
        0,
    );
    let (_, point) = query(fd, signaled);
    check_is(&mut r, "and does not move the point", point as isize, 4);
    check_is(
        &mut r,
        "RESET on a timeline succeeds",
        array_call(fd, NR_SYNCOBJ_RESET, signaled),
        0,
    );
    let (_, point) = query(fd, signaled);
    check_is(
        &mut r,
        "and takes the point back to zero",
        point as isize,
        0,
    );
    let (got, _) = wait_one(fd, signaled, 0, 0);
    check_refused(&mut r, "with the fence gone as well", got, Errno::INVAL);
    {
        // `TIMELINE_SIGNAL` with a flag the request does not have. Its record is
        // the timeline-array one, whose `flags` sits at 20.
        let mut h = [signaled];
        let mut p = [3u64];
        let mut bad = SyncobjTimelineArray {
            count_handles: 1,
            flags: 0x8000,
            ..SyncobjTimelineArray::default()
        };
        bad.handles = h.as_mut_ptr() as u64;
        bad.points = p.as_mut_ptr() as u64;
        let got = unsafe {
            ioctl(
                fd,
                ioc_d(
                    NR_SYNCOBJ_TIMELINE_SIGNAL,
                    size_of::<SyncobjTimelineArray>() as u32,
                ),
                (&raw mut bad) as *mut c_void,
            )
        };
        check_refused(
            &mut r,
            "TIMELINE_SIGNAL with an unknown flag",
            got,
            Errno::INVAL,
        );
    }

    // --- the two descriptors, and the requests that tell them apart ---
    // The fence state is *put* where this section wants it rather than
    // remembered from the sections above. The waker check left a fence on
    // `plain` from the child's SIGNAL, and an assertion about "an object with no
    // fence" that relies on remembering that is an assertion about the test.
    check_is(
        &mut r,
        "RESET the plain object, which the waker left fenced",
        array_call(fd, NR_SYNCOBJ_RESET, plain),
        0,
    );
    let (got, fenced) = create(fd, CREATE_SIGNALED);
    check_is(&mut r, "and make a fenced object for the export", got, 0);
    let (got, objfd) = handle_to_fd(fd, fenced, 0, 0);
    check_is(&mut r, "HANDLE_TO_FD flags 0 succeeds", got, 0);
    check_true(&mut r, "and answers a descriptor", objfd >= 0);
    let (got, again) = fd_to_handle(fd, 0, 0, objfd);
    check_is(&mut r, "FD_TO_HANDLE takes it back", got, 0);
    check_true(&mut r, "as another handle", again != 0);
    // Both handles name one object, which is the whole reason the descriptor
    // exists: this is how one process's timeline becomes another's.
    let (got, p1) = query(fd, again);
    let (_, p2) = query(fd, fenced);
    check_is(&mut r, "the imported handle reads the same object", got, 0);
    check_is(&mut r, "with the same point", p1 as isize, p2 as isize);
    // A point submitted through one handle is visible through the other, which is
    // the sharing made concrete rather than two ids merely agreeing on zero.
    check_is(
        &mut r,
        "a point submitted through one handle",
        timeline_signal(fd, again, 6),
        0,
    );
    let (_, p2) = query(fd, fenced);
    check_is(&mut r, "reads back through the other", p2 as isize, 6);
    let (got, _) = fd_to_handle(fd, again, 0, objfd);
    check_refused(
        &mut r,
        "FD_TO_HANDLE with a handle set is the import direction's shape",
        got,
        Errno::INVAL,
    );
    let (got, _) = fd_to_handle(fd, 0, 0, -1);
    check_refused(&mut r, "FD_TO_HANDLE from a closed fd", got, Errno::BADF);

    // `EXPORT_SYNC_FILE` refuses an object with nothing to export, which the
    // measured card does and the sources do not.
    let (got, _) = handle_to_fd(fd, plain, H2F_EXPORT_SYNC_FILE, 0);
    check_refused(
        &mut r,
        "EXPORT_SYNC_FILE of an object with no fence is refused",
        got,
        Errno::INVAL,
    );
    let (got, syncfile) = handle_to_fd(fd, fenced, H2F_EXPORT_SYNC_FILE, 0);
    check_is(
        &mut r,
        "EXPORT_SYNC_FILE of a signaled one succeeds",
        got,
        0,
    );
    check_true(
        &mut r,
        "and the descriptor polls readable, as a retired fence does",
        polls_readable(syncfile),
    );
    let (got, _) = fd_to_handle(fd, 0, F2H_IMPORT_SYNC_FILE, syncfile);
    check_refused(
        &mut r,
        "IMPORT_SYNC_FILE with no handle to replace is ENOENT",
        got,
        Errno::NOENT,
    );
    let (got, imported) = fd_to_handle(fd, fenced, F2H_IMPORT_SYNC_FILE, syncfile);
    check_is(
        &mut r,
        "IMPORT_SYNC_FILE into an existing object succeeds",
        got,
        0,
    );
    check_is(
        &mut r,
        "and echoes the handle it replaced",
        imported as isize,
        fenced as isize,
    );
    let (got, _) = handle_to_fd(fd, fenced, 0x8000, 0);
    check_refused(
        &mut r,
        "HANDLE_TO_FD with an unknown flag",
        got,
        Errno::INVAL,
    );

    // --- the `point` field, and the flag that makes it a timeline point --------
    // All of this is measured on the real card (§7.4), because the sources say
    // nothing about `point` at all and the note beside the kernel's constant
    // used to call the flag a no-op. Three rules on this side, each of which
    // this card answered differently before:
    //
    //   * a non-zero point needs `EXPORT_SYNC_FILE`, the only form whose
    //     descriptor can carry one;
    //   * with `TIMELINE` the point is a *timeline* point, so the object must
    //     have reached it;
    //   * without `TIMELINE` the point is not read at all.
    let (got, _) = handle_to_fd(fd, fenced, 0, 5);
    check_refused(
        &mut r,
        "a point with no EXPORT_SYNC_FILE is refused",
        got,
        Errno::INVAL,
    );
    let (got, _) = handle_to_fd(fd, fenced, H2F_TIMELINE, 5);
    check_refused(
        &mut r,
        "and TIMELINE alone does not make it legal",
        got,
        Errno::INVAL,
    );
    // The object for the next pair has to be a *fresh* one: `CREATE` has no
    // timeline flag, so a new object has no point at all - where the object
    // above has been a timeline since a point was submitted to it through the
    // other handle, and would answer this request rather than refusing it.
    let (got, binary) = create(fd, CREATE_SIGNALED);
    check_is(&mut r, "a fresh object for the point rule is made", got, 0);
    let (got, _) = handle_to_fd(fd, binary, H2F_EXPORT_SYNC_FILE | H2F_TIMELINE, 5);
    check_refused(
        &mut r,
        "a timeline point on an object that is not a timeline is refused",
        got,
        Errno::INVAL,
    );
    // The control for the rule above, and the reason it is a control: the *same*
    // request on the *same* object with one flag fewer is taken, which is what
    // says the flag is what makes the number a point rather than the number by
    // itself.
    let (got, _plain_sf) = handle_to_fd(fd, binary, H2F_EXPORT_SYNC_FILE, 5);
    check_is(
        &mut r,
        "while without TIMELINE the same point is not read at all",
        got,
        0,
    );
    // On a timeline, the point has to be one the object has *reached*, and the
    // pair that settles it is 5 against 6: "the object is a timeline" alone
    // would take both.
    let (got, tl) = create(fd, 0);
    check_is(&mut r, "an object for the point rules is made", got, 0);
    check_is(
        &mut r,
        "and given a point of 5",
        timeline_signal(fd, tl, 5),
        0,
    );
    let (got, _at5) = handle_to_fd(fd, tl, H2F_EXPORT_SYNC_FILE | H2F_TIMELINE, 5);
    check_is(
        &mut r,
        "the point the object has reached is answered",
        got,
        0,
    );
    let (got, _below) = handle_to_fd(fd, tl, H2F_EXPORT_SYNC_FILE | H2F_TIMELINE, 3);
    check_is(&mut r, "and so is a point below it", got, 0);
    let (got, _) = handle_to_fd(fd, tl, H2F_EXPORT_SYNC_FILE | H2F_TIMELINE, 6);
    check_refused(&mut r, "but not one above it", got, Errno::INVAL);

    // The import side's half: a non-zero point is the import form's to name, for
    // the same reason it is the export form's on the other side.
    let (got, objfd) = handle_to_fd(fd, fenced, 0, 0);
    check_is(
        &mut r,
        "a plain object exports the descriptor the import rules are about",
        got,
        0,
    );
    if objfd >= 0 {
        let (got, _) = fd_to_handle_at(fd, 0, 0, objfd, 5);
        check_refused(
            &mut r,
            "an import-side point with no IMPORT_SYNC_FILE is refused",
            got,
            Errno::INVAL,
        );
        let (got, _) = fd_to_handle_at(fd, 0, F2H_TIMELINE, objfd, 5);
        check_refused(
            &mut r,
            "and TIMELINE alone does not make it legal",
            got,
            Errno::INVAL,
        );
        let (got, second) = fd_to_handle_at(fd, 0, 0, objfd, 0);
        check_is(&mut r, "while point 0 is taken", got, 0);
        check_true(&mut r, "as another handle", second != 0);
    }

    // --- what an import *lands* on ----------------------------------------------
    // The other half of the flag, and the half this card answered wrongly in both
    // directions before (§7.4). Measured on the real card, and the two shapes are
    // two rules rather than one with a variant:
    //
    //   * with `TIMELINE` the request names the point, and it *raises* like every
    //     other point writer - a destination at 4 asked for 2 stays at 4, and one
    //     asked for 6 goes to 6;
    //   * without it the *fence's* own point lands, exactly - a timeline's
    //     `sync_file` lands its point (9 in the measurement, even when the export
    //     named 5), and a plain object's lands 0, which is a point going
    //     backwards through the one door that may do that.
    //
    // Both shapes leave the destination a timeline, which is why the pairs below
    // ask `TIMELINE_WAIT` and not only for the point: an object that reads 0 is
    // either one that never had a point or one that has just been given point 0,
    // and only the wait tells them apart.
    let (got, plain_obj) = create(fd, CREATE_SIGNALED);
    check_is(&mut r, "a plain object with a fence to export", got, 0);
    let (got, sf_plain) = handle_to_fd(fd, plain_obj, H2F_EXPORT_SYNC_FILE, 0);
    check_is(&mut r, "and its sync_file, which carries no point", got, 0);
    let (got, tl9) = create(fd, 0);
    check_is(&mut r, "a second object for the timeline side", got, 0);
    check_is(&mut r, "at point 9", timeline_signal(fd, tl9, 9), 0);
    let (got, sf9) = handle_to_fd(fd, tl9, H2F_EXPORT_SYNC_FILE, 0);
    check_is(&mut r, "whose sync_file is exported", got, 0);
    // The export *at* a point, which the measurement says does not change what a
    // later import lands on.
    let (got, sf_at5) = handle_to_fd(fd, tl9, H2F_EXPORT_SYNC_FILE | H2F_TIMELINE, 5);
    check_is(
        &mut r,
        "and one asked for at point 5, which is the third descriptor",
        got,
        0,
    );
    if sf_plain >= 0 && sf9 >= 0 && sf_at5 >= 0 {
        // A plain object's fence: no point to carry, so the destination lands on
        // 0 - below where it was, which no other door here allows.
        let (_, dst) = create(fd, 0);
        check_is(
            &mut r,
            "a destination at point 4",
            timeline_signal(fd, dst, 4),
            0,
        );
        let got = fd_to_handle(fd, dst, F2H_IMPORT_SYNC_FILE, sf_plain).0;
        check_is(&mut r, "importing a plain object's sync_file", got, 0);
        let (_, landed) = query(fd, dst);
        check_is(
            &mut r,
            "lands the fence's own point, 0, below the one it had",
            landed as isize,
            0,
        );
        check_is(
            &mut r,
            "and leaves it a timeline from point 0",
            timeline_wait_one(fd, dst, 0, 0, 0),
            0,
        );

        // A timeline's fence: the point it was made at lands, not the 4 that was
        // there and not anything the request said.
        let (_, dst) = create(fd, 0);
        timeline_signal(fd, dst, 4);
        let got = fd_to_handle(fd, dst, F2H_IMPORT_SYNC_FILE, sf9).0;
        check_is(&mut r, "importing a timeline's sync_file", got, 0);
        let (_, landed) = query(fd, dst);
        check_is(&mut r, "lands that timeline's point, 9", landed as isize, 9);

        // The export *named* a point and it does not matter, which is the check
        // that separates "the fence's point" from "the request's".
        let (_, dst) = create(fd, 0);
        timeline_signal(fd, dst, 4);
        let got = fd_to_handle(fd, dst, F2H_IMPORT_SYNC_FILE, sf_at5).0;
        check_is(
            &mut r,
            "importing the sync_file an export asked for at point 5",
            got,
            0,
        );
        let (_, landed) = query(fd, dst);
        check_is(
            &mut r,
            "lands the object's own point, 9, and not the 5",
            landed as isize,
            9,
        );

        // The `TIMELINE` half: the request names the point, and it raises.
        let (_, below) = create(fd, 0);
        timeline_signal(fd, below, 4);
        let got = fd_to_handle_at(fd, below, F2H_IMPORT_SYNC_FILE | F2H_TIMELINE, sf9, 2).0;
        check_is(&mut r, "a TIMELINE import is taken", got, 0);
        let (_, landed) = query(fd, below);
        check_is(
            &mut r,
            "and a point below the destination's leaves it where it was",
            landed as isize,
            4,
        );
        let (_, above) = create(fd, 0);
        timeline_signal(fd, above, 4);
        let got = fd_to_handle_at(fd, above, F2H_IMPORT_SYNC_FILE | F2H_TIMELINE, sf9, 6).0;
        check_is(&mut r, "a second TIMELINE import", got, 0);
        let (_, landed) = query(fd, above);
        check_is(&mut r, "with a point above it moves it", landed as isize, 6);

        // And the shape that says a plain object is *made* a timeline by an
        // import: it refuses `TIMELINE_WAIT` first, and answers it afterwards.
        let (_, fresh) = create(fd, 0);
        check_refused(
            &mut r,
            "a fresh object refuses TIMELINE_WAIT",
            timeline_wait_one(fd, fresh, 0, 0, 0),
            Errno::INVAL,
        );
        let got = fd_to_handle(fd, fresh, F2H_IMPORT_SYNC_FILE, sf_plain).0;
        check_is(&mut r, "an import into it is taken", got, 0);
        check_is(
            &mut r,
            "and leaves it a timeline from point 0",
            timeline_wait_one(fd, fresh, 0, 0, 0),
            0,
        );
    }

    // --- TRANSFER needs one live fence between its two ends ---
    let (got, dst) = create(fd, 0);
    check_is(&mut r, "a destination object is made", got, 0);
    let mut xfer = SyncobjTransfer {
        src_handle: 0xdead_beef,
        dst_handle: dst,
        ..SyncobjTransfer::default()
    };
    let got = unsafe {
        ioctl(
            fd,
            ioc_d(NR_SYNCOBJ_TRANSFER, size_of::<SyncobjTransfer>() as u32),
            (&raw mut xfer) as *mut c_void,
        )
    };
    check_refused(&mut r, "TRANSFER from nowhere", got, Errno::NOENT);
    xfer.src_handle = dst;
    xfer.dst_handle = 0xdead_beef;
    let got = unsafe {
        ioctl(
            fd,
            ioc_d(NR_SYNCOBJ_TRANSFER, size_of::<SyncobjTransfer>() as u32),
            (&raw mut xfer) as *mut c_void,
        )
    };
    check_refused(&mut r, "TRANSFER to nowhere", got, Errno::NOENT);
    // Two empty objects: nothing to carry and nothing to carry it to, which the
    // measured card refuses and which an `ok` here would make a silent no-op.
    // Both ends are *put* empty first: `plain` was given a fence again by the
    // export section's import, so the state is set rather than assumed.
    check_is(
        &mut r,
        "RESET the source so neither end holds a fence",
        array_call(fd, NR_SYNCOBJ_RESET, plain),
        0,
    );
    xfer.src_handle = plain;
    xfer.dst_handle = dst;
    let got = unsafe {
        ioctl(
            fd,
            ioc_d(NR_SYNCOBJ_TRANSFER, size_of::<SyncobjTransfer>() as u32),
            (&raw mut xfer) as *mut c_void,
        )
    };
    check_refused(
        &mut r,
        "TRANSFER with no fence at either end is refused",
        got,
        Errno::INVAL,
    );
    xfer.src_handle = fenced;
    xfer.dst_handle = dst;
    xfer.dst_point = 7;
    let got = unsafe {
        ioctl(
            fd,
            ioc_d(NR_SYNCOBJ_TRANSFER, size_of::<SyncobjTransfer>() as u32),
            (&raw mut xfer) as *mut c_void,
        )
    };
    check_is(&mut r, "TRANSFER a signaled object succeeds", got, 0);
    let (got, _) = wait_one(fd, dst, 0, 0);
    check_is(&mut r, "and the destination holds the fence", got, 0);
    let (_, point) = query(fd, dst);
    check_is(
        &mut r,
        "and is a timeline at the point it was told",
        point as isize,
        7,
    );

    // --- EVENTFD: the same arrival, as a counter an event loop can read ---
    let ev = rustix::event::eventfd(0, EventfdFlags::NONBLOCK).expect("an eventfd");
    check_true(&mut r, "an eventfd is made", true);
    let evfd = ev.as_raw_fd();
    let (got, fresh) = create(fd, 0);
    check_is(&mut r, "a fresh object for the counter", got, 0);
    let mut ev_arg = SyncobjEventfd {
        handle: fresh,
        point: 3,
        fd: evfd,
        ..SyncobjEventfd::default()
    };
    let got = unsafe {
        ioctl(
            fd,
            ioc_d(NR_SYNCOBJ_EVENTFD, size_of::<SyncobjEventfd>() as u32),
            (&raw mut ev_arg) as *mut c_void,
        )
    };
    check_is(&mut r, "EVENTFD registers a point", got, 0);
    let mut counter = [0u8; 8];
    check_refused(
        &mut r,
        "and an unreached point fires nothing",
        read(&ev, &mut counter)
            .map(|n| n as isize)
            .unwrap_or_else(errno_is),
        Errno::AGAIN,
    );
    check_is(
        &mut r,
        "TIMELINE_SIGNAL past that point",
        timeline_signal(fd, fresh, 3),
        0,
    );
    check_is(
        &mut r,
        "hands the counter its one increment",
        read(&ev, &mut counter)
            .map(|n| n as isize)
            .unwrap_or_else(errno_is),
        8,
    );
    check_is(
        &mut r,
        "and the increment is one",
        u64::from_ne_bytes(counter) as isize,
        1,
    );
    let _ = timeline_signal(fd, fresh, 4);
    check_refused(
        &mut r,
        "a second signal owes nothing more",
        read(&ev, &mut counter)
            .map(|n| n as isize)
            .unwrap_or_else(errno_is),
        Errno::AGAIN,
    );
    let mut ev_arg = SyncobjEventfd {
        handle: fresh,
        flags: WAIT_AVAILABLE,
        fd: evfd,
        ..SyncobjEventfd::default()
    };
    let got = unsafe {
        ioctl(
            fd,
            ioc_d(NR_SYNCOBJ_EVENTFD, size_of::<SyncobjEventfd>() as u32),
            (&raw mut ev_arg) as *mut c_void,
        )
    };
    check_is(&mut r, "EVENTFD with WAIT_AVAILABLE is accepted", got, 0);
    // The object already holds a fence, so this one is owed its increment at
    // once rather than at the next signal.
    check_is(
        &mut r,
        "and fires at once for a fence that is already there",
        read(&ev, &mut counter)
            .map(|n| n as isize)
            .unwrap_or_else(errno_is),
        8,
    );
    let mut ev_arg = SyncobjEventfd {
        handle: fresh,
        flags: 0x8,
        fd: evfd,
        ..SyncobjEventfd::default()
    };
    let got = unsafe {
        ioctl(
            fd,
            ioc_d(NR_SYNCOBJ_EVENTFD, size_of::<SyncobjEventfd>() as u32),
            (&raw mut ev_arg) as *mut c_void,
        )
    };
    check_refused(&mut r, "EVENTFD with an unknown flag", got, Errno::INVAL);
    let mut ev_arg = SyncobjEventfd {
        handle: fresh,
        fd: -1,
        ..SyncobjEventfd::default()
    };
    let got = unsafe {
        ioctl(
            fd,
            ioc_d(NR_SYNCOBJ_EVENTFD, size_of::<SyncobjEventfd>() as u32),
            (&raw mut ev_arg) as *mut c_void,
        )
    };
    check_refused(&mut r, "EVENTFD from a closed fd", got, Errno::BADF);

    // --- the two fences a commit carries ---
    //
    // The plane's property ids are read out of the card's own list rather than
    // written down here: the name is the ABI, the id is this card's numbering,
    // and a test that hard-coded one would be checking its own copy of the
    // driver's constants.
    let out_ptr = property_id(fd, PLANE_ID, OBJECT_PLANE, b"OUT_FENCE_PTR");
    check_true(&mut r, "the plane publishes OUT_FENCE_PTR", out_ptr != 0);
    let in_fd = property_id(fd, PLANE_ID, OBJECT_PLANE, b"IN_FENCE_FD");
    check_true(&mut r, "and IN_FENCE_FD", in_fd != 0);

    if out_ptr != 0 {
        // A commit writes the descriptor it made into the caller's own memory,
        // so the value is the address of a local: this is the half of the pair
        // that makes a flip waitable from the other side.
        let mut out_fence_fd: i32 = -1;
        let got = atomic_one(fd, out_ptr, &mut out_fence_fd as *mut i32 as u64, 0);
        check_is(&mut r, "a commit carrying OUT_FENCE_PTR succeeds", got, 0);
        check_true(
            &mut r,
            "and writes back a fence descriptor",
            out_fence_fd >= 0,
        );
        check_true(
            &mut r,
            "which is already through, because the frame has gone",
            out_fence_fd >= 0 && polls_readable(out_fence_fd),
        );
        // A pointer that is not the caller's memory is refused before anything
        // is applied - the write at the end would otherwise fault in the kernel.
        let got = atomic_one(fd, out_ptr, 0x1000, 0);
        check_refused(
            &mut r,
            "an OUT_FENCE_PTR the caller does not own is refused",
            got,
            Errno::FAULT,
        );
        // The single-property door cannot honour a pointer that describes a
        // frame, which is what the property's `atomic` nature means on a real
        // card too. `struct drm_mode_obj_set_property`: value@0, prop_id@8,
        // obj_id@12, obj_type@16.
        let mut set = [0u8; 24];
        set[8..12].copy_from_slice(&out_ptr.to_ne_bytes());
        set[12..16].copy_from_slice(&PLANE_ID.to_ne_bytes());
        set[16..20].copy_from_slice(&OBJECT_PLANE.to_ne_bytes());
        let got = unsafe {
            ioctl(
                fd,
                ioc_d(NR_OBJ_SETPROPERTY, 24),
                set.as_mut_ptr() as *mut c_void,
            )
        };
        check_refused(
            &mut r,
            "and OBJ_SETPROPERTY refuses it as an atomic-only property",
            got,
            Errno::INVAL,
        );
    }
    if in_fd != 0 {
        // -1 is the property's own spelling of "no fence", which is what every
        // commit that is not part of an explicit-sync chain carries.
        let got = atomic_one(fd, in_fd, -1i64 as u64, 0);
        check_is(
            &mut r,
            "a commit carrying IN_FENCE_FD = -1 succeeds",
            got,
            0,
        );
        let got = atomic_one(fd, in_fd, (-1i32 as u32) as u64, 0);
        check_is(
            &mut r,
            "and so does the unsigned spelling of the same value",
            got,
            0,
        );
        let got = atomic_one(fd, in_fd, 0x7fff, 0);
        check_refused(
            &mut r,
            "a commit handed a descriptor that names nothing",
            got,
            Errno::BADF,
        );
        // A real acquire fence: exported from a sync object, so the commit is
        // ordered behind it. It is retired, so the wait ends at once - which is
        // the honest shape here, where nothing can leave one in flight.
        let (got, fence_obj) = create(fd, CREATE_SIGNALED);
        check_is(&mut r, "an object with a fence to export", got, 0);
        let (got, sf) = handle_to_fd(fd, fence_obj, H2F_EXPORT_SYNC_FILE, 0);
        check_is(&mut r, "exported as a sync_file", got, 0);
        let got = atomic_one(fd, in_fd, sf as u32 as u64, 0);
        check_is(
            &mut r,
            "a commit handed a real acquire fence is ordered behind it",
            got,
            0,
        );
        // And a descriptor that is not a fence at all is refused, the same
        // answer Linux's `sync_file_get_fence` gives.
        let got = atomic_one(fd, in_fd, fd as u32 as u64, 0);
        check_refused(
            &mut r,
            "and a commit handed the card itself as a fence",
            got,
            Errno::INVAL,
        );
    }

    // --- destruction ---
    check_refused(
        &mut r,
        "DESTROY a handle that does not exist is an argument error",
        destroy(fd, 0xdead_beef),
        Errno::INVAL,
    );
    check_is(
        &mut r,
        "DESTROY a handle the card gave out",
        destroy(fd, dst),
        0,
    );
    check_refused(
        &mut r,
        "and it names nothing afterwards",
        destroy(fd, dst),
        Errno::INVAL,
    );

    // --- an interrupted wait, and whether `SA_RESTART` rewinds it ------------
    //
    // `ioctl(2)` is restartable per *request*: Linux's `drm_syncobj.c` returns
    // `-ERESTARTSYS` when a signal ends a sync-object wait, so the syscall layer
    // rewinds that request under `SA_RESTART` and answers `EINTR` otherwise -
    // while an ordinary request's `EINTR` is never rewound. This kernel's park
    // already answers `EINTR` for exactly these waits (`syncobj_park`), and the
    // rewind is the per-request verdict (§2.5).
    //
    // One object, driven twice. A `SIGALRM` fires while the parent blocks in a
    // `WAIT_FOR_SUBMIT` on an unfenced object, and a forked child submits a fence
    // later. Without `SA_RESTART` the signal ends the park (`EINTR`) and the
    // fence, arriving after, is beside the point. With it the same signal only
    // rewinds the call - so the park rides on to the child's fence and returns 0.
    // A kernel missing the per-request rewind gives `EINTR` in both, failing the
    // second check.
    {
        let (got, esig) = create(fd, 0);
        check_is(&mut r, "an object to interrupt a wait on", got, 0);

        // Branch A - no `SA_RESTART`: the signal ends the wait.
        install_alarm(false);
        let kid = unsafe { libc::fork() };
        if kid == 0 {
            child_nap(100);
            let _ = array_call(fd, NR_SYNCOBJ_SIGNAL, esig);
            unsafe { libc::_exit(0) };
        }
        alarm_once(50);
        let (got, _) = wait_one(fd, esig, deadline_ms_ahead(2000), WAIT_FOR_SUBMIT);
        check_refused(
            &mut r,
            "a SIGALRM ends a WAIT_FOR_SUBMIT with EINTR when the handler lacks SA_RESTART",
            got,
            Errno::INTR,
        );
        check_true(
            &mut r,
            "and the handler ran first",
            RAN.load(Ordering::Relaxed) >= 1,
        );
        // Collect the child (it signals at 100 ms), then clear the fence it left.
        reap(kid);
        check_is(
            &mut r,
            "and the object can be reset for the second branch",
            array_call(fd, NR_SYNCOBJ_RESET, esig),
            0,
        );

        // Branch B - with `SA_RESTART`: the rewind rides past the signal to the
        // fence the child submits, so the same call answers 0, not EINTR.
        install_alarm(true);
        let kid2 = unsafe { libc::fork() };
        if kid2 == 0 {
            child_nap(100);
            let _ = array_call(fd, NR_SYNCOBJ_SIGNAL, esig);
            unsafe { libc::_exit(0) };
        }
        alarm_once(50);
        let (got, _) = wait_one(fd, esig, deadline_ms_ahead(2000), WAIT_FOR_SUBMIT);
        check_is(
            &mut r,
            "SA_RESTART rewinds the same WAIT_FOR_SUBMIT, so it ends on the fence",
            got,
            0,
        );
        reap(kid2);

        // Restore the default disposition so nothing downstream inherits it.
        let back: libc::sigaction = unsafe { std::mem::zeroed() };
        unsafe {
            libc::sigaction(libc::SIGALRM, &back, std::ptr::null_mut());
        }
    }

    r.finish();
}
