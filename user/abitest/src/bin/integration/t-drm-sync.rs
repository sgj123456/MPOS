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
//! request that takes them back. `PLAN.md` §7.4 records the measurement this was
//! written from, and `src/gpu/sync.rs` holds the rules with a host test each.
//!
//! The waits are the interesting half: `WAIT_FOR_SUBMIT` is the only form that
//! blocks, and the wake it parks on comes from *another task* - so the one check
//! here that forks exists to prove a sleeper is really woken rather than merely
//! told it would be.
//!
//! Everything runs on `/dev/dri/renderD128` rather than `card0`: none of these
//! requests present anything, the render node is the same card state, and using
//! it keeps the test out of the console-handover business `t-drm-lease` is about.

#![no_std]
#![no_main]

#[path = "../../../lib/lin.rs"]
mod lin;

use core::panic::PanicInfo;
use core::sync::atomic::{AtomicUsize, Ordering};

/// `_IOWR('d', nr, size)` as libdrm builds it: the family byte at bits 8..15.
const fn ioc_d(nr: usize, size: usize) -> usize {
    (3 << 30) | (size << 16) | ((b'd' as usize) << 8) | nr
}

const NR_GET_CAP: usize = 0x0c;
const NR_GETPROPERTY: usize = 0xaa;
const NR_OBJ_GETPROPERTIES: usize = 0xb9;
const NR_OBJ_SETPROPERTY: usize = 0xba;
const NR_MODE_ATOMIC: usize = 0xbc;
const NR_SYNCOBJ_CREATE: usize = 0xbf;
const NR_SYNCOBJ_DESTROY: usize = 0xc0;
const NR_SYNCOBJ_HANDLE_TO_FD: usize = 0xc1;
const NR_SYNCOBJ_FD_TO_HANDLE: usize = 0xc2;
const NR_SYNCOBJ_WAIT: usize = 0xc3;
const NR_SYNCOBJ_RESET: usize = 0xc4;
const NR_SYNCOBJ_SIGNAL: usize = 0xc5;
const NR_SYNCOBJ_TIMELINE_WAIT: usize = 0xca;
const NR_SYNCOBJ_QUERY: usize = 0xcb;
const NR_SYNCOBJ_TRANSFER: usize = 0xcc;
const NR_SYNCOBJ_TIMELINE_SIGNAL: usize = 0xcd;
const NR_SYNCOBJ_EVENTFD: usize = 0xcf;

const PLANE_ID: u32 = 36;
const OBJECT_PLANE: u32 = 0xeeee_eeee;

const DRM_CAP_SYNCOBJ: u64 = 0x13;
const DRM_CAP_SYNCOBJ_TIMELINE: u64 = 0x14;

const CREATE_SIGNALED: u32 = 1 << 0;
const H2F_EXPORT_SYNC_FILE: u32 = 1 << 0;
const F2H_IMPORT_SYNC_FILE: u32 = 1 << 0;
const WAIT_ALL: u32 = 1 << 0;
const WAIT_FOR_SUBMIT: u32 = 1 << 1;
const WAIT_AVAILABLE: u32 = 1 << 2;

const EINVAL: isize = -22;
const ENOENT: isize = -2;
const EBADF: isize = -9;
const EFAULT: isize = -14;
const ETIME: isize = -62;
const EAGAIN: isize = -11;
const O_RDWR: usize = 2;
const POLLIN: i16 = 0x1;
const EFD_NONBLOCK: usize = 0o4000;

fn put32(buf: &mut [u8], off: usize, v: u32) {
    buf[off..off + 4].copy_from_slice(&v.to_ne_bytes());
}
fn put64(buf: &mut [u8], off: usize, v: u64) {
    buf[off..off + 8].copy_from_slice(&v.to_ne_bytes());
}
fn get32(buf: &[u8], off: usize) -> u32 {
    u32::from_ne_bytes([buf[off], buf[off + 1], buf[off + 2], buf[off + 3]])
}
fn get64(buf: &[u8], off: usize) -> u64 {
    let mut b = [0u8; 8];
    b.copy_from_slice(&buf[off..off + 8]);
    u64::from_ne_bytes(b)
}

/// `SYNCOBJ_CREATE`, answering the handle it gave.
fn create(fd: i32, flags: u32) -> (isize, u32) {
    let mut buf = [0u8; 8];
    put32(&mut buf, 4, flags);
    let r = lin::ioctl(fd, ioc_d(NR_SYNCOBJ_CREATE, 8), buf.as_mut_ptr() as usize);
    (r, get32(&buf, 0))
}

/// `SYNCOBJ_WAIT` on one handle. `timeout` is the header's absolute
/// `CLOCK_MONOTONIC` deadline in nanoseconds; -1 is "for ever" and 0 is "do not
/// wait at all".
fn wait_one(fd: i32, handle: u32, timeout: i64, flags: u32) -> (isize, u32) {
    let mut h = [handle];
    let mut buf = [0u8; 40];
    put64(&mut buf, 0, h.as_mut_ptr() as u64);
    put64(&mut buf, 8, timeout as u64);
    put32(&mut buf, 16, 1);
    put32(&mut buf, 20, flags);
    let r = lin::ioctl(fd, ioc_d(NR_SYNCOBJ_WAIT, 40), buf.as_mut_ptr() as usize);
    (r, get32(&buf, 24))
}

/// `SYNCOBJ_TIMELINE_WAIT` on one handle and one point.
fn timeline_wait_one(fd: i32, handle: u32, point: u64, timeout: i64, flags: u32) -> isize {
    let mut h = [handle];
    let mut p = [point];
    let mut buf = [0u8; 48];
    put64(&mut buf, 0, h.as_mut_ptr() as u64);
    put64(&mut buf, 8, p.as_mut_ptr() as u64);
    put64(&mut buf, 16, timeout as u64);
    put32(&mut buf, 24, 1);
    put32(&mut buf, 28, flags);
    lin::ioctl(
        fd,
        ioc_d(NR_SYNCOBJ_TIMELINE_WAIT, 48),
        buf.as_mut_ptr() as usize,
    )
}

/// `SYNCOBJ_SIGNAL` / `SYNCOBJ_RESET`: one handle, no point.
fn array_call(fd: i32, nr: usize, handle: u32) -> isize {
    let mut h = [handle];
    let mut buf = [0u8; 16];
    put64(&mut buf, 0, h.as_mut_ptr() as u64);
    put32(&mut buf, 8, 1);
    lin::ioctl(fd, ioc_d(nr, 16), buf.as_mut_ptr() as usize)
}

/// `SYNCOBJ_TIMELINE_SIGNAL`: one handle at one point.
fn timeline_signal(fd: i32, handle: u32, point: u64) -> isize {
    let mut h = [handle];
    let mut p = [point];
    let mut buf = [0u8; 24];
    put64(&mut buf, 0, h.as_mut_ptr() as u64);
    put64(&mut buf, 8, p.as_mut_ptr() as u64);
    put32(&mut buf, 16, 1);
    lin::ioctl(
        fd,
        ioc_d(NR_SYNCOBJ_TIMELINE_SIGNAL, 24),
        buf.as_mut_ptr() as usize,
    )
}

/// `SYNCOBJ_QUERY`: the point one object has reached, or a negative errno.
fn query(fd: i32, handle: u32) -> (isize, u64) {
    let mut h = [handle];
    let mut out = [0u64; 1];
    let mut buf = [0u8; 24];
    put64(&mut buf, 0, h.as_mut_ptr() as u64);
    put64(&mut buf, 8, out.as_mut_ptr() as u64);
    put32(&mut buf, 16, 1);
    let r = lin::ioctl(fd, ioc_d(NR_SYNCOBJ_QUERY, 24), buf.as_mut_ptr() as usize);
    (r, out[0])
}

/// `SYNCOBJ_HANDLE_TO_FD`, answering the descriptor or the errno.
fn handle_to_fd(fd: i32, handle: u32, flags: u32, point: u64) -> (isize, i32) {
    let mut buf = [0u8; 24];
    put32(&mut buf, 0, handle);
    put32(&mut buf, 4, flags);
    put32(&mut buf, 8, 0xffff_ffffu32); // fd = -1
    put64(&mut buf, 16, point);
    let r = lin::ioctl(
        fd,
        ioc_d(NR_SYNCOBJ_HANDLE_TO_FD, 24),
        buf.as_mut_ptr() as usize,
    );
    (r, get32(&buf, 8) as i32)
}

/// `SYNCOBJ_FD_TO_HANDLE`, answering the handle it made.
fn fd_to_handle(fd: i32, target: u32, flags: u32, dfd: i32) -> (isize, u32) {
    let mut buf = [0u8; 24];
    put32(&mut buf, 0, target);
    put32(&mut buf, 4, flags);
    put32(&mut buf, 8, dfd as u32);
    let r = lin::ioctl(
        fd,
        ioc_d(NR_SYNCOBJ_FD_TO_HANDLE, 24),
        buf.as_mut_ptr() as usize,
    );
    (r, get32(&buf, 0))
}

/// Whether a descriptor polls readable, which is what a `sync_file` says.
fn polls_readable(fd: i32) -> bool {
    let mut fds = [lin::PollFd {
        fd,
        events: POLLIN,
        revents: 0,
    }];
    let n = lin::poll(&mut fds, 0);
    n > 0 && fds[0].revents & POLLIN != 0
}

/// `GET_CAP`'s answer for one capability.
fn cap(fd: i32, which: u64) -> (isize, u64) {
    let mut buf = [0u8; 16];
    put64(&mut buf, 0, which);
    let r = lin::ioctl(fd, ioc_d(NR_GET_CAP, 16), buf.as_mut_ptr() as usize);
    (r, get64(&buf, 8))
}

/// The id of a property, by name, out of one object's list.
///
/// The name is the ABI and the id is this card's numbering, so a test that
/// hard-coded the id would be checking its own copy of the driver's constants
/// rather than what a client is told.
fn property_id(fd: i32, obj: u32, obj_type: u32, want: &[u8]) -> u32 {
    let mut props = [0u32; 32];
    let mut values = [0u64; 32];
    let mut buf = [0u8; 32];
    put64(&mut buf, 0, props.as_mut_ptr() as u64);
    put64(&mut buf, 8, values.as_mut_ptr() as u64);
    put32(&mut buf, 16, 32);
    put32(&mut buf, 20, obj);
    put32(&mut buf, 24, obj_type);
    if lin::ioctl(
        fd,
        ioc_d(NR_OBJ_GETPROPERTIES, 32),
        buf.as_mut_ptr() as usize,
    ) != 0
    {
        return 0;
    }
    let count = get32(&buf, 16) as usize;
    for &id in props.iter().take(count.min(32)) {
        let mut p = [0u8; 64];
        put32(&mut p, 16, id);
        if lin::ioctl(fd, ioc_d(NR_GETPROPERTY, 64), p.as_mut_ptr() as usize) != 0 {
            continue;
        }
        let name = &p[24..56];
        let end = name.iter().position(|&c| c == 0).unwrap_or(name.len());
        if &name[..end] == want {
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
    let mut buf = [0u8; 56];
    put32(&mut buf, 0, flags);
    put32(&mut buf, 4, 1);
    put64(&mut buf, 8, objs.as_mut_ptr() as u64);
    put64(&mut buf, 16, counts.as_mut_ptr() as u64);
    put64(&mut buf, 24, props.as_mut_ptr() as u64);
    put64(&mut buf, 32, values.as_mut_ptr() as u64);
    lin::ioctl(fd, ioc_d(NR_MODE_ATOMIC, 56), buf.as_mut_ptr() as usize)
}

// ---- interrupting a blocked wait: the `SA_RESTART` rewind pin --------------

/// How many times the alarm handler ran. Each is a wait a signal reached - the
/// subject of the rewind check at the end of the file.
static RAN: AtomicUsize = AtomicUsize::new(0);

extern "C" fn on_alarm(_sig: i32, _info: *mut lin::SigInfo, _ctx: *mut u8) {
    RAN.fetch_add(1, Ordering::Relaxed);
}

/// musl's `__restore_rt`, for a kernel with no libc.
extern "C" fn restorer() {
    let _ = lin::rt_sigreturn();
}

/// Install a *caught* `SIGALRM` handler, with or without `SA_RESTART`. Caught, so
/// the alarm ends the wait rather than killing the process; the flag decides
/// whether the kernel then rewinds the interrupted `ioctl` or lets it answer
/// `EINTR`.
fn install_alarm(restart: bool) {
    let mut flags = lin::SA_SIGINFO | lin::SA_RESTORER;
    if restart {
        flags |= lin::SA_RESTART;
    }
    let act = lin::SigAction {
        handler: on_alarm as extern "C" fn(i32, *mut lin::SigInfo, *mut u8) as usize,
        flags,
        restorer: restorer as extern "C" fn() as usize,
        mask: 0,
    };
    lin::rt_sigaction(lin::SIGALRM, &act, core::ptr::null_mut());
}

/// Arm `ITIMER_REAL` to fire once, `ms` from now, and disarm any earlier one.
fn alarm_once(ms: u64) {
    let mut spent = lin::Itimerval {
        it_interval: lin::TimeVal {
            tv_sec: 0,
            tv_usec: 0,
        },
        it_value: lin::TimeVal {
            tv_sec: 0,
            tv_usec: 0,
        },
    };
    let next = lin::Itimerval {
        it_interval: lin::TimeVal {
            tv_sec: 0,
            tv_usec: 0,
        },
        it_value: lin::TimeVal {
            tv_sec: (ms / 1000) as i64,
            tv_usec: ((ms % 1000) as i64) * 1000,
        },
    };
    lin::setitimer(lin::ITIMER_REAL, Some(&next), &mut spent);
}

/// A `SYNCOBJ_WAIT` deadline an absolute `ms` after the monotonic clock, which
/// the header wants in nanoseconds since boot. A fixed number would be a
/// deadline that the sweep's own elapsed boot-time has already passed.
fn deadline_ms_ahead(ms: i64) -> i64 {
    let mut ts = lin::TimeSpec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    lin::clock_gettime(lin::CLOCK_MONOTONIC, &mut ts);
    ts.tv_sec as i64 * 1_000_000_000 + ts.tv_nsec as i64 + ms * 1_000_000
}

#[unsafe(no_mangle)]
extern "C" fn mpos_user_main(_argc: usize, _argv: *const *const u8) -> ! {
    lin::puts(b"t-drm-sync\n");
    let mut failed = false;

    let fd = lin::openat(lin::AT_FDCWD, b"/dev/dri/renderD128\0", O_RDWR, 0) as i32;
    lin::check_true(&mut failed, b"/dev/dri/renderD128 opens", fd >= 0);
    if fd < 0 {
        lin::done(failed);
    }

    // The two capabilities Smithay asks before it will expose the explicit-sync
    // protocol at all. Answering 1 is a promise the rest of this file is the
    // evidence for.
    let (r, v) = cap(fd, DRM_CAP_SYNCOBJ);
    lin::check_eq(&mut failed, b"GET_CAP(SYNCOBJ) succeeds", r, 0);
    lin::check_eq(&mut failed, b"and the card reports it", v as isize, 1);
    let (r, v) = cap(fd, DRM_CAP_SYNCOBJ_TIMELINE);
    lin::check_eq(&mut failed, b"GET_CAP(SYNCOBJ_TIMELINE) succeeds", r, 0);
    lin::check_eq(&mut failed, b"and the card reports it too", v as isize, 1);

    // --- creation, and the state every refusal is about ---
    let (r, plain) = create(fd, 0);
    lin::check_eq(&mut failed, b"CREATE a plain object succeeds", r, 0);
    lin::check_true(&mut failed, b"and it is given a handle", plain != 0);
    let (r, signaled) = create(fd, CREATE_SIGNALED);
    lin::check_eq(&mut failed, b"CREATE SIGNALED succeeds", r, 0);
    lin::check_true(
        &mut failed,
        b"and a second object gets its own handle",
        signaled != plain,
    );
    let (r, _) = create(fd, 0x8000);
    lin::check_eq(&mut failed, b"an unknown CREATE flag is refused", r, EINVAL);

    // An object with nothing behind it is *refused*, not treated as ready: this
    // is the rule the measured card has and the upstream sources do not.
    let (r, _) = wait_one(fd, plain, 0, 0);
    lin::check_eq(
        &mut failed,
        b"WAIT an object with no fence is refused, not answered",
        r,
        EINVAL,
    );
    let (r, _) = wait_one(fd, plain, -1, 0);
    lin::check_eq(
        &mut failed,
        b"and no timeout makes it a wait that could end",
        r,
        EINVAL,
    );
    let (r, _) = wait_one(fd, signaled, 0, 0);
    lin::check_eq(&mut failed, b"WAIT the SIGNALED one answers at once", r, 0);
    let (_, first) = wait_one(fd, signaled, 0, 0);
    lin::check_eq(
        &mut failed,
        b"naming the handle that satisfied it",
        first as isize,
        0,
    );
    // `WAIT_ALL` is the "every handle" form, and on this card the *empty* object
    // poisons the whole array rather than being skipped as already-signaled.
    let mixed = [signaled, plain];
    let mut both = [0u8; 40];
    put64(&mut both, 0, mixed.as_ptr() as u64);
    put32(&mut both, 16, 2);
    put32(&mut both, 20, WAIT_ALL);
    let r = lin::ioctl(fd, ioc_d(NR_SYNCOBJ_WAIT, 40), both.as_mut_ptr() as usize);
    lin::check_eq(
        &mut failed,
        b"WAIT_ALL over a signaled and an empty object is refused",
        r,
        EINVAL,
    );
    let (r, other) = create(fd, CREATE_SIGNALED);
    lin::check_eq(&mut failed, b"a second signaled object", r, 0);
    let full = [signaled, other];
    let mut all_buf = [0u8; 40];
    put64(&mut all_buf, 0, full.as_ptr() as u64);
    put32(&mut all_buf, 16, 2);
    put32(&mut all_buf, 20, WAIT_ALL);
    let r = lin::ioctl(
        fd,
        ioc_d(NR_SYNCOBJ_WAIT, 40),
        all_buf.as_mut_ptr() as usize,
    );
    lin::check_eq(&mut failed, b"WAIT_ALL over two signaled objects", r, 0);

    // `WAIT_FOR_SUBMIT` is the only form that blocks, and with nothing coming
    // it is the clock that ends the wait.
    let (r, _) = wait_one(fd, plain, 0, WAIT_FOR_SUBMIT);
    lin::check_eq(
        &mut failed,
        b"WAIT_FOR_SUBMIT with nothing coming times out",
        r,
        ETIME,
    );
    let (r, _) = wait_one(fd, 0xdead_beef, 0, 0);
    lin::check_eq(&mut failed, b"WAIT a handle that does not exist", r, ENOENT);
    let (r, _) = wait_one(fd, plain, 0, 0x8000);
    lin::check_eq(&mut failed, b"an unknown WAIT flag is refused", r, EINVAL);
    let mut empty = [0u8; 40];
    put32(&mut empty, 16, 0);
    let r = lin::ioctl(fd, ioc_d(NR_SYNCOBJ_WAIT, 40), empty.as_mut_ptr() as usize);
    lin::check_eq(&mut failed, b"WAIT of no handles is a no-op", r, 0);
    let mut null = [0u8; 40];
    put32(&mut null, 16, 1);
    let r = lin::ioctl(fd, ioc_d(NR_SYNCOBJ_WAIT, 40), null.as_mut_ptr() as usize);
    lin::check_eq(
        &mut failed,
        b"and a null handle array is the caller's own memory failing",
        r,
        EFAULT,
    );

    // --- SIGNAL and RESET: the two writes that make and unmake a fence ---
    lin::check_eq(
        &mut failed,
        b"SIGNAL succeeds",
        array_call(fd, NR_SYNCOBJ_SIGNAL, plain),
        0,
    );
    let (r, _) = wait_one(fd, plain, 0, 0);
    lin::check_eq(&mut failed, b"and the object is waitable now", r, 0);
    lin::check_eq(
        &mut failed,
        b"RESET succeeds",
        array_call(fd, NR_SYNCOBJ_RESET, plain),
        0,
    );
    let (r, _) = wait_one(fd, plain, 0, 0);
    lin::check_eq(&mut failed, b"and takes the fence away again", r, EINVAL);
    lin::check_eq(
        &mut failed,
        b"RESET of a handle that does not exist",
        array_call(fd, NR_SYNCOBJ_RESET, 0xdead_beef),
        ENOENT,
    );

    // --- a sleeper really woken by another task ---
    lin::check_eq(
        &mut failed,
        b"SIGNAL the object the waiter will park on",
        array_call(fd, NR_SYNCOBJ_SIGNAL, plain),
        0,
    );
    lin::check_eq(
        &mut failed,
        b"RESET it again so the wait has something to wait for",
        array_call(fd, NR_SYNCOBJ_RESET, plain),
        0,
    );
    let child = lin::fork();
    if child == 0 {
        // The child holds the same card description, so it names the same
        // handle: this is the shape a render thread waking a commit thread has.
        let nap = lin::TimeSpec {
            tv_sec: 0,
            tv_nsec: 50_000_000,
        };
        let mut rem = lin::TimeSpec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        lin::nanosleep(&nap, &mut rem);
        let _ = array_call(fd, NR_SYNCOBJ_SIGNAL, plain);
        lin::exit(0);
    }
    // Two seconds of budget against a 50 ms nap: the wait ends when the fence
    // arrives, and if the wake were lost this would be the clock instead.
    let (r, _) = wait_one(fd, plain, 2_000_000_000, WAIT_FOR_SUBMIT);
    lin::check_eq(
        &mut failed,
        b"a WAIT_FOR_SUBMIT is woken by another task's SIGNAL",
        r,
        0,
    );
    let mut status = 0i32;
    lin::wait4(child as usize, &mut status, 0);
    lin::check_eq(
        &mut failed,
        b"and the signaller exited cleanly",
        lin::wexitstatus(status) as isize,
        0,
    );

    // --- timelines: a point submitted is what makes an object one ---
    lin::check_eq(
        &mut failed,
        b"TIMELINE_WAIT an object that never had a point is refused",
        timeline_wait_one(fd, signaled, 0, 0, 0),
        EINVAL,
    );
    lin::check_eq(
        &mut failed,
        b"TIMELINE_SIGNAL point 4 succeeds",
        timeline_signal(fd, signaled, 4),
        0,
    );
    let (r, point) = query(fd, signaled);
    lin::check_eq(&mut failed, b"QUERY succeeds", r, 0);
    lin::check_eq(
        &mut failed,
        b"and reports the point submitted",
        point as isize,
        4,
    );
    lin::check_eq(
        &mut failed,
        b"TIMELINE_WAIT at that point answers",
        timeline_wait_one(fd, signaled, 4, 0, 0),
        0,
    );
    lin::check_eq(
        &mut failed,
        b"and at a point below it too",
        timeline_wait_one(fd, signaled, 1, 0, 0),
        0,
    );
    lin::check_eq(
        &mut failed,
        b"but not at a point nothing has reached",
        timeline_wait_one(fd, signaled, 5, 0, 0),
        EINVAL,
    );
    lin::check_eq(
        &mut failed,
        b"which WAIT_FOR_SUBMIT answers on the fence alone",
        timeline_wait_one(fd, signaled, 9, 0, WAIT_FOR_SUBMIT),
        0,
    );
    // A point only moves forward: a backwards signal is accepted and changes
    // nothing, which is what stops a wait on a point already passed from failing
    // again.
    lin::check_eq(
        &mut failed,
        b"a backwards TIMELINE_SIGNAL is accepted",
        timeline_signal(fd, signaled, 2),
        0,
    );
    let (_, point) = query(fd, signaled);
    lin::check_eq(&mut failed, b"and moves nothing", point as isize, 4);
    // `SIGNAL` is accepted on a timeline and leaves the point alone.
    lin::check_eq(
        &mut failed,
        b"SIGNAL on a timeline is accepted",
        array_call(fd, NR_SYNCOBJ_SIGNAL, signaled),
        0,
    );
    let (_, point) = query(fd, signaled);
    lin::check_eq(
        &mut failed,
        b"and does not move the point",
        point as isize,
        4,
    );
    lin::check_eq(
        &mut failed,
        b"RESET on a timeline succeeds",
        array_call(fd, NR_SYNCOBJ_RESET, signaled),
        0,
    );
    let (_, point) = query(fd, signaled);
    lin::check_eq(
        &mut failed,
        b"and takes the point back to zero",
        point as isize,
        0,
    );
    let (r, _) = wait_one(fd, signaled, 0, 0);
    lin::check_eq(&mut failed, b"with the fence gone as well", r, EINVAL);
    lin::check_eq(
        &mut failed,
        b"TIMELINE_SIGNAL with an unknown flag",
        {
            let mut h = [signaled];
            let mut p = [3u64];
            let mut buf = [0u8; 24];
            put64(&mut buf, 0, h.as_mut_ptr() as u64);
            put64(&mut buf, 8, p.as_mut_ptr() as u64);
            put32(&mut buf, 16, 1);
            put32(&mut buf, 20, 0x8000);
            lin::ioctl(
                fd,
                ioc_d(NR_SYNCOBJ_TIMELINE_SIGNAL, 24),
                buf.as_mut_ptr() as usize,
            )
        },
        EINVAL,
    );

    // --- the two descriptors, and the requests that tell them apart ---
    // The fence state is *put* where this section wants it rather than
    // remembered from the sections above. The waker check left a fence on
    // `plain` from the child's SIGNAL, and an assertion about "an object with no
    // fence" that relies on remembering that is an assertion about the test.
    lin::check_eq(
        &mut failed,
        b"RESET the plain object, which the waker left fenced",
        array_call(fd, NR_SYNCOBJ_RESET, plain),
        0,
    );
    let (r, fenced) = create(fd, CREATE_SIGNALED);
    lin::check_eq(
        &mut failed,
        b"and make a fenced object for the export",
        r,
        0,
    );
    let (r, objfd) = handle_to_fd(fd, fenced, 0, 0);
    lin::check_eq(&mut failed, b"HANDLE_TO_FD flags 0 succeeds", r, 0);
    lin::check_true(&mut failed, b"and answers a descriptor", objfd >= 0);
    let (r, again) = fd_to_handle(fd, 0, 0, objfd);
    lin::check_eq(&mut failed, b"FD_TO_HANDLE takes it back", r, 0);
    lin::check_true(&mut failed, b"as another handle", again != 0);
    // Both handles name one object, which is the whole reason the descriptor
    // exists: this is how one process's timeline becomes another's.
    let (r, p1) = query(fd, again);
    let (_, p2) = query(fd, fenced);
    lin::check_eq(
        &mut failed,
        b"the imported handle reads the same object",
        r,
        0,
    );
    lin::check_eq(
        &mut failed,
        b"with the same point",
        p1 as isize,
        p2 as isize,
    );
    // A point submitted through one handle is visible through the other, which is
    // the sharing made concrete rather than two ids merely agreeing on zero.
    lin::check_eq(
        &mut failed,
        b"a point submitted through one handle",
        timeline_signal(fd, again, 6),
        0,
    );
    let (_, p2) = query(fd, fenced);
    lin::check_eq(&mut failed, b"reads back through the other", p2 as isize, 6);
    let (r, _) = fd_to_handle(fd, again, 0, objfd);
    lin::check_eq(
        &mut failed,
        b"FD_TO_HANDLE with a handle set is the import direction's shape",
        r,
        EINVAL,
    );
    let (r, _) = fd_to_handle(fd, 0, 0, -1);
    lin::check_eq(&mut failed, b"FD_TO_HANDLE from a closed fd", r, EBADF);

    // `EXPORT_SYNC_FILE` refuses an object with nothing to export, which the
    // measured card does and the sources do not.
    let (r, _) = handle_to_fd(fd, plain, H2F_EXPORT_SYNC_FILE, 0);
    lin::check_eq(
        &mut failed,
        b"EXPORT_SYNC_FILE of an object with no fence is refused",
        r,
        EINVAL,
    );
    let (r, syncfile) = handle_to_fd(fd, fenced, H2F_EXPORT_SYNC_FILE, 0);
    lin::check_eq(
        &mut failed,
        b"EXPORT_SYNC_FILE of a signaled one succeeds",
        r,
        0,
    );
    lin::check_true(
        &mut failed,
        b"and the descriptor polls readable, as a retired fence does",
        polls_readable(syncfile),
    );
    let (r, _) = fd_to_handle(fd, 0, F2H_IMPORT_SYNC_FILE, syncfile);
    lin::check_eq(
        &mut failed,
        b"IMPORT_SYNC_FILE with no handle to replace is ENOENT",
        r,
        ENOENT,
    );
    let (r, imported) = fd_to_handle(fd, fenced, F2H_IMPORT_SYNC_FILE, syncfile);
    lin::check_eq(
        &mut failed,
        b"IMPORT_SYNC_FILE into an existing object succeeds",
        r,
        0,
    );
    lin::check_eq(
        &mut failed,
        b"and echoes the handle it replaced",
        imported as isize,
        fenced as isize,
    );
    let (r, _) = handle_to_fd(fd, fenced, 0x8000, 0);
    lin::check_eq(&mut failed, b"HANDLE_TO_FD with an unknown flag", r, EINVAL);

    // --- TRANSFER needs one live fence between its two ends ---
    let (r, dst) = create(fd, 0);
    lin::check_eq(&mut failed, b"a destination object is made", r, 0);
    let mut xfer = [0u8; 32];
    put32(&mut xfer, 0, 0xdead_beef);
    put32(&mut xfer, 4, dst);
    let r = lin::ioctl(
        fd,
        ioc_d(NR_SYNCOBJ_TRANSFER, 32),
        xfer.as_mut_ptr() as usize,
    );
    lin::check_eq(&mut failed, b"TRANSFER from nowhere", r, ENOENT);
    put32(&mut xfer, 0, dst);
    put32(&mut xfer, 4, 0xdead_beef);
    let r = lin::ioctl(
        fd,
        ioc_d(NR_SYNCOBJ_TRANSFER, 32),
        xfer.as_mut_ptr() as usize,
    );
    lin::check_eq(&mut failed, b"TRANSFER to nowhere", r, ENOENT);
    // Two empty objects: nothing to carry and nothing to carry it to, which the
    // measured card refuses and which an `ok` here would make a silent no-op.
    // Both ends are *put* empty first: `plain` was given a fence again by the
    // export section's import, so the state is set rather than assumed.
    lin::check_eq(
        &mut failed,
        b"RESET the source so neither end holds a fence",
        array_call(fd, NR_SYNCOBJ_RESET, plain),
        0,
    );
    put32(&mut xfer, 0, plain);
    put32(&mut xfer, 4, dst);
    let r = lin::ioctl(
        fd,
        ioc_d(NR_SYNCOBJ_TRANSFER, 32),
        xfer.as_mut_ptr() as usize,
    );
    lin::check_eq(
        &mut failed,
        b"TRANSFER with no fence at either end is refused",
        r,
        EINVAL,
    );
    put32(&mut xfer, 0, fenced);
    put32(&mut xfer, 4, dst);
    put64(&mut xfer, 16, 7);
    let r = lin::ioctl(
        fd,
        ioc_d(NR_SYNCOBJ_TRANSFER, 32),
        xfer.as_mut_ptr() as usize,
    );
    lin::check_eq(&mut failed, b"TRANSFER a signaled object succeeds", r, 0);
    let (r, _) = wait_one(fd, dst, 0, 0);
    lin::check_eq(&mut failed, b"and the destination holds the fence", r, 0);
    let (_, point) = query(fd, dst);
    lin::check_eq(
        &mut failed,
        b"and is a timeline at the point it was told",
        point as isize,
        7,
    );

    // --- EVENTFD: the same arrival, as a counter an event loop can read ---
    let ev = lin::eventfd(0, EFD_NONBLOCK) as i32;
    lin::check_true(&mut failed, b"an eventfd is made", ev >= 0);
    let (r, fresh) = create(fd, 0);
    lin::check_eq(&mut failed, b"a fresh object for the counter", r, 0);
    let mut ev_buf = [0u8; 24];
    put32(&mut ev_buf, 0, fresh);
    put64(&mut ev_buf, 8, 3);
    put32(&mut ev_buf, 16, ev as u32);
    let r = lin::ioctl(fd, ioc_c(24), ev_buf.as_mut_ptr() as usize);
    lin::check_eq(&mut failed, b"EVENTFD registers a point", r, 0);
    let mut counter = [0u8; 8];
    lin::check_eq(
        &mut failed,
        b"and an unreached point fires nothing",
        lin::read(ev, &mut counter),
        EAGAIN,
    );
    lin::check_eq(
        &mut failed,
        b"TIMELINE_SIGNAL past that point",
        timeline_signal(fd, fresh, 3),
        0,
    );
    lin::check_eq(
        &mut failed,
        b"hands the counter its one increment",
        lin::read(ev, &mut counter),
        8,
    );
    lin::check_eq(
        &mut failed,
        b"and the increment is one",
        get64(&counter, 0) as isize,
        1,
    );
    lin::check_eq(
        &mut failed,
        b"a second signal owes nothing more",
        {
            let _ = timeline_signal(fd, fresh, 4);
            lin::read(ev, &mut counter)
        },
        EAGAIN,
    );
    put32(&mut ev_buf, 0, fresh);
    put32(&mut ev_buf, 4, WAIT_AVAILABLE);
    put32(&mut ev_buf, 16, ev as u32);
    let r = lin::ioctl(fd, ioc_c(24), ev_buf.as_mut_ptr() as usize);
    lin::check_eq(
        &mut failed,
        b"EVENTFD with WAIT_AVAILABLE is accepted",
        r,
        0,
    );
    // The object already holds a fence, so this one is owed its increment at
    // once rather than at the next signal.
    lin::check_eq(
        &mut failed,
        b"and fires at once for a fence that is already there",
        lin::read(ev, &mut counter),
        8,
    );
    put32(&mut ev_buf, 0, fresh);
    put32(&mut ev_buf, 4, 0x8);
    put32(&mut ev_buf, 16, ev as u32);
    let r = lin::ioctl(fd, ioc_c(24), ev_buf.as_mut_ptr() as usize);
    lin::check_eq(&mut failed, b"EVENTFD with an unknown flag", r, EINVAL);
    put32(&mut ev_buf, 4, 0);
    put32(&mut ev_buf, 16, -1i32 as u32);
    let r = lin::ioctl(fd, ioc_c(24), ev_buf.as_mut_ptr() as usize);
    lin::check_eq(&mut failed, b"EVENTFD from a closed fd", r, EBADF);

    // --- the two fences a commit carries ---
    //
    // The plane's property ids are read out of the card's own list rather than
    // written down here: the name is the ABI, the id is this card's numbering,
    // and a test that hard-coded one would be checking its own copy of the
    // driver's constants.
    let out_ptr = property_id(fd, PLANE_ID, OBJECT_PLANE, b"OUT_FENCE_PTR");
    lin::check_true(
        &mut failed,
        b"the plane publishes OUT_FENCE_PTR",
        out_ptr != 0,
    );
    let in_fd = property_id(fd, PLANE_ID, OBJECT_PLANE, b"IN_FENCE_FD");
    lin::check_true(&mut failed, b"and IN_FENCE_FD", in_fd != 0);

    if out_ptr != 0 {
        // A commit writes the descriptor it made into the caller's own memory,
        // so the value is the address of a local: this is the half of the pair
        // that makes a flip waitable from the other side.
        let mut out_fence_fd = -1i32;
        let r = atomic_one(fd, out_ptr, &mut out_fence_fd as *mut i32 as u64, 0);
        lin::check_eq(
            &mut failed,
            b"a commit carrying OUT_FENCE_PTR succeeds",
            r,
            0,
        );
        lin::check_true(
            &mut failed,
            b"and writes back a fence descriptor",
            out_fence_fd >= 0,
        );
        lin::check_true(
            &mut failed,
            b"which is already through, because the frame has gone",
            out_fence_fd >= 0 && polls_readable(out_fence_fd),
        );
        if out_fence_fd >= 0 {
            lin::close(out_fence_fd);
        }
        // A pointer that is not the caller's memory is refused before anything
        // is applied - the write at the end would otherwise fault in the kernel.
        let r = atomic_one(fd, out_ptr, 0x1000, 0);
        lin::check_eq(
            &mut failed,
            b"an OUT_FENCE_PTR the caller does not own is refused",
            r,
            EFAULT,
        );
        // The single-property door cannot honour a pointer that describes a
        // frame, which is what the property's `atomic` nature means on a real
        // card too. `struct drm_mode_obj_set_property`: value@0, prop_id@8,
        // obj_id@12, obj_type@16.
        let mut set = [0u8; 24];
        put64(&mut set, 0, 0);
        put32(&mut set, 8, out_ptr);
        put32(&mut set, 12, PLANE_ID);
        put32(&mut set, 16, OBJECT_PLANE);
        let r = lin::ioctl(fd, ioc_d(NR_OBJ_SETPROPERTY, 24), set.as_mut_ptr() as usize);
        lin::check_eq(
            &mut failed,
            b"and OBJ_SETPROPERTY refuses it as an atomic-only property",
            r,
            EINVAL,
        );
    }
    if in_fd != 0 {
        // -1 is the property's own spelling of "no fence", which is what every
        // commit that is not part of an explicit-sync chain carries.
        let r = atomic_one(fd, in_fd, -1i64 as u64, 0);
        lin::check_eq(
            &mut failed,
            b"a commit carrying IN_FENCE_FD = -1 succeeds",
            r,
            0,
        );
        let r = atomic_one(fd, in_fd, -1i32 as u64 & 0xffff_ffff, 0);
        lin::check_eq(
            &mut failed,
            b"and so does the unsigned spelling of the same value",
            r,
            0,
        );
        let r = atomic_one(fd, in_fd, 0x7fff, 0);
        lin::check_eq(
            &mut failed,
            b"a commit handed a descriptor that names nothing",
            r,
            EBADF,
        );
        // A real acquire fence: exported from a sync object, so the commit is
        // ordered behind it. It is retired, so the wait ends at once - which is
        // the honest shape here, where nothing can leave one in flight.
        let (r, _fence_obj) = create(fd, CREATE_SIGNALED);
        lin::check_eq(&mut failed, b"an object with a fence to export", r, 0);
        let (r, sf) = handle_to_fd(fd, _fence_obj, H2F_EXPORT_SYNC_FILE, 0);
        lin::check_eq(&mut failed, b"exported as a sync_file", r, 0);
        if r == 0 {
            let r = atomic_one(fd, in_fd, sf as u32 as u64, 0);
            lin::check_eq(
                &mut failed,
                b"a commit handed a real acquire fence is ordered behind it",
                r,
                0,
            );
            // And a descriptor that is not a fence at all is refused, the same
            // answer Linux's `sync_file_get_fence` gives.
            let r = atomic_one(fd, in_fd, fd as u32 as u64, 0);
            lin::check_eq(
                &mut failed,
                b"and a commit handed the card itself as a fence",
                r,
                EINVAL,
            );
            lin::close(sf);
        }
    }

    // --- destruction ---
    lin::check_eq(
        &mut failed,
        b"DESTROY a handle that does not exist is an argument error",
        {
            let mut buf = [0u8; 8];
            put32(&mut buf, 0, 0xdead_beef);
            lin::ioctl(fd, ioc_d(NR_SYNCOBJ_DESTROY, 8), buf.as_mut_ptr() as usize)
        },
        EINVAL,
    );
    lin::check_eq(
        &mut failed,
        b"DESTROY a handle the card gave out",
        {
            let mut buf = [0u8; 8];
            put32(&mut buf, 0, dst);
            lin::ioctl(fd, ioc_d(NR_SYNCOBJ_DESTROY, 8), buf.as_mut_ptr() as usize)
        },
        0,
    );
    lin::check_eq(
        &mut failed,
        b"and it names nothing afterwards",
        {
            let mut buf = [0u8; 8];
            put32(&mut buf, 0, dst);
            lin::ioctl(fd, ioc_d(NR_SYNCOBJ_DESTROY, 8), buf.as_mut_ptr() as usize)
        },
        EINVAL,
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
        let (r, esig) = create(fd, 0);
        lin::check_eq(&mut failed, b"an object to interrupt a wait on", r, 0);

        // Branch A - no `SA_RESTART`: the signal ends the wait.
        install_alarm(false);
        let kid = lin::fork();
        if kid == 0 {
            lin::sleep_ms(300);
            let _ = array_call(fd, NR_SYNCOBJ_SIGNAL, esig);
            lin::exit(0);
        }
        alarm_once(150);
        let (r, _) = wait_one(fd, esig, deadline_ms_ahead(2000), WAIT_FOR_SUBMIT);
        lin::check_eq(
            &mut failed,
            b"a SIGALRM ends a WAIT_FOR_SUBMIT with EINTR when the handler lacks SA_RESTART",
            r,
            lin::EINTR,
        );
        lin::check_true(
            &mut failed,
            b"and the handler ran first",
            RAN.load(Ordering::Relaxed) >= 1,
        );
        // Collect the child (it signals at 300 ms), then clear the fence it left.
        let mut status = 0i32;
        let _ = lin::wait4(kid as usize, &mut status, 0);
        lin::check_eq(
            &mut failed,
            b"and the object can be reset for the second branch",
            array_call(fd, NR_SYNCOBJ_RESET, esig),
            0,
        );

        // Branch B - with `SA_RESTART`: the rewind rides past the signal to the
        // fence the child submits, so the same call answers 0, not EINTR.
        install_alarm(true);
        let kid2 = lin::fork();
        if kid2 == 0 {
            lin::sleep_ms(300);
            let _ = array_call(fd, NR_SYNCOBJ_SIGNAL, esig);
            lin::exit(0);
        }
        alarm_once(150);
        let (r, _) = wait_one(fd, esig, deadline_ms_ahead(2000), WAIT_FOR_SUBMIT);
        lin::check_eq(
            &mut failed,
            b"SA_RESTART rewinds the same WAIT_FOR_SUBMIT, so it ends on the fence",
            r,
            0,
        );
        let _ = lin::wait4(kid2 as usize, &mut status, 0);

        // Restore the default disposition so nothing downstream inherits it.
        let back = lin::SigAction {
            handler: 0,
            flags: 0,
            restorer: 0,
            mask: 0,
        };
        lin::rt_sigaction(lin::SIGALRM, &back, core::ptr::null_mut());
    }

    lin::done(failed);
}

/// `SYNCOBJ_EVENTFD`'s request word, which one local helper builds twice.
const fn ioc_c(size: usize) -> usize {
    ioc_d(NR_SYNCOBJ_EVENTFD, size)
}

#[panic_handler]
fn panic(_info: &PanicInfo) -> ! {
    lin::exit(101)
}
