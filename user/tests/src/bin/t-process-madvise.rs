//! t-process-madvise - `process_madvise(2)`, `madvise` addressed at the
//! address space a pidfd names, and the answers a current Linux gives it.
//!
//! The call is the pidfd family's fourth door and the last one that was
//! `ENOSYS` here. Its shape was measured on the host before the kernel's side
//! was written (`.workbuddy/probe_process_madvise.c`, `probe3.c` through
//! `probe8.c`, Linux 7.2.11), and the claims below are those measurements -
//! not the man page's summary of them:
//!
//! * **the answer is a byte count, not `0`.** One 4096-byte iovec answers
//!   `4096`; two answer `8192`; a 2000-byte iovec answers `2000` (the range is
//!   not rounded to pages); an iovec longer than `MAX_RW_COUNT` is clamped and
//!   answers the clamp. A caller sizes its next call from that number.
//! * **the refusal order is not the argument order.** `flags` first (a
//!   nonzero word is `EINVAL` even beside an unreadable iovec, a bad advice and
//!   `pidfd = -1`), then `vlen` above `IOV_MAX` (1024), then the *whole* iovec
//!   array (unreadable is `EFAULT`, so the descriptor is never reached), then
//!   the pidfd (`EBADF`), then the task (`ESRCH` once it exited), then the
//!   authority question (`EPERM`), then the advice vocabulary (`EINVAL`).
//! * **the import has three refusals**: a length above `SSIZE_MAX` is
//!   `EINVAL` (it is a negative `ssize_t`), a length above `MAX_RW_COUNT`
//!   (`0x7fff_f000`) is *clamped* rather than refused, and an address whose
//!   clamped range leaves user space is `EFAULT` - user space ending one page
//!   under `TASK_SIZE_MAX`, which is why `0x7fff_ffff_f000` is refused and
//!   `0x7fff_ffff_e000` is served.
//! * **only four advices cross a process boundary** - `WILLNEED` (3),
//!   `COLD` (20), `PAGEOUT` (21), `COLLAPSE` (25) - and the vocabulary gate
//!   runs before the `CAP_SYS_NICE` gate: a same-uid child answers `EPERM` for
//!   those four and `EINVAL` for every other advice, `MADV_DONTNEED` included.
//! * **a hole is `ENOMEM`, and what the walk already did stands.** A range
//!   with an unmapped page answers `ENOMEM`, but a `MADV_DONTNEED` over the
//!   mapped part before the hole still dropped it; a completed iovec followed
//!   by a failing one answers the bytes it served, not the error. A page
//!   belongs to the range when its *start* is before the range's end.
//!
//! # Why raw doors
//!
//! rustix has no `process_madvise` binding at all, and the interesting
//! arguments are its raw words: the `vlen` above `IOV_MAX`, the advice numbers
//! Linux refuses across a process boundary, an `iov_len` of `usize::MAX`, and an
//! address one page past user space. libc's own table names the call (`440` on
//! both machines), so every call here is `libc::syscall`.
//!
//! The privilege claim needs a child that has *dropped* one capability, which
//! `libc` does not wrap on this target either: `capset` is spelled in the v3
//! wire shape below (two records), the same encoding `t-caps` pins.
//!
//! # What is deliberately not here
//!
//! Two of this kernel's answers are *its own*, and they are named rather than
//! pinned as Linux's: a cross-process hint is answered from the argument shape
//! alone (this kernel cannot ask another address space whether its mapping
//! covers the range - every iovec here points at a range the target really has
//! mapped, so Linux's answer and this kernel's agree on the served case), and
//! `MADV_COLLAPSE` across a process boundary is `EOPNOTSUPP` because there is
//! no huge-page pool to collapse anything into. Both are in `docs/notes/`
//! §6.10 with their measurements.
//!
//! Advice `100` (`MADV_HWPOISON`) on the caller's *own* range is pinned as
//! this kernel's `EINVAL`; Linux answers `EPERM` there because the advice is
//! recognised and the privilege is missing. That difference is `madvise`'s,
//! not this call's - the vocabulary gate it inherits has never accepted
//! `100`/`101` (`t-madvise` names the same divergence) - and it is recorded in
//! `docs/notes/` §6.10 rather than repaired here.

use mpos_user_tests::{Report, host_run};
use std::mem::size_of;

const PAGE: usize = 4096;
const PROT_RW: libc::c_int = libc::PROT_READ | libc::PROT_WRITE;
const ANON_PRIVATE: libc::c_int = libc::MAP_PRIVATE | libc::MAP_ANONYMOUS;
/// Not zero: "reads back zero" is the DONTNEED claim, so the value written has
/// to be distinguishable from an untouched page.
const PATTERN: u64 = 0x1234_5678_9abc_def0;
/// `MADV_COLLAPSE` (25), which this libc's `MADV_*` block does not name - the
/// number is libc's own ABI table's, spelled here because the claim is about
/// the advice the kernel refuses across a process boundary (the crate's rule
/// (c): a shape the binding cannot express).
const MADV_COLLAPSE: i32 = 25;

static mut ARENA_BASE: *mut libc::c_void = core::ptr::null_mut();
const ARENA_LEN: usize = 4 * 1024 * 1024;

/// `process_madvise(pidfd, iovec, vlen, advice, flags)` - the byte count served
/// or the errno.
fn pm_raw(
    pidfd: i32,
    vec: *const libc::iovec,
    vlen: usize,
    advice: i32,
    flags: u32,
) -> Result<isize, i32> {
    let r = unsafe {
        libc::syscall(
            libc::SYS_process_madvise,
            pidfd as libc::c_long,
            vec as libc::c_long,
            vlen as libc::c_long,
            advice as libc::c_long,
            flags as libc::c_long,
        )
    };
    if r < 0 {
        Err(std::io::Error::last_os_error().raw_os_error().unwrap_or(0))
    } else {
        Ok(r as isize)
    }
}

/// The same call over a slice of iovecs.
fn pm(pidfd: i32, vec: &[libc::iovec], advice: i32, flags: u32) -> Result<isize, i32> {
    pm_raw(pidfd, vec.as_ptr(), vec.len(), advice, flags)
}

/// One range, as the array of one this call takes.
fn one(base: usize, len: usize) -> [libc::iovec; 1] {
    [libc::iovec {
        iov_base: base as *mut libc::c_void,
        iov_len: len,
    }]
}

/// `pidfd_open(pid, flags)`, answered as the descriptor or the errno.
fn pidfd_open(pid: libc::pid_t, flags: u32) -> Result<i32, i32> {
    let r = unsafe {
        libc::syscall(
            libc::SYS_pidfd_open,
            pid as libc::c_long,
            flags as libc::c_long,
        )
    };
    if r < 0 {
        Err(std::io::Error::last_os_error().raw_os_error().unwrap_or(0))
    } else {
        Ok(r as i32)
    }
}

fn refused(ret: Result<isize, i32>, want: i32) -> bool {
    ret == Err(want)
}

/// A refused `pidfd_open`, for the claim that a reaped child cannot be named.
fn pidfd_refused(pid: libc::pid_t, want: i32) -> bool {
    pidfd_open(pid, 0) == Err(want)
}

fn mm(off: usize, len: usize, prot: libc::c_int, flags: libc::c_int) -> isize {
    let addr = if off == usize::MAX {
        core::ptr::null_mut()
    } else {
        unsafe { ARENA_BASE.cast::<u8>().add(off) as *mut libc::c_void }
    };
    unsafe { libc::mmap(addr, len, prot, flags, -1, 0) as isize }
}

/// Leave through `exit_group`, so a forked child never flushes the copy of the
/// parent's buffered stdout it inherited.
fn leave(code: i32) -> ! {
    unsafe {
        libc::syscall(libc::SYS_exit_group, code as libc::c_long);
        libc::_exit(code)
    }
}

fn sleep_ms(ms: i64) {
    let req = libc::timespec {
        tv_sec: ms / 1000,
        tv_nsec: (ms % 1000) * 1_000_000,
    };
    unsafe {
        libc::nanosleep(&req, core::ptr::null_mut());
    }
}

/// `waitid(P_PID, ...)` through the raw door - the look that says "exited but
/// not collected" without collecting it.
fn waitid_pid(pid: libc::pid_t, info: &mut libc::siginfo_t, options: i32) -> Result<(), i32> {
    let r = unsafe {
        libc::syscall(
            libc::SYS_waitid,
            libc::P_PID as libc::c_long,
            pid as libc::c_long,
            info as *mut libc::siginfo_t as libc::c_long,
            options as libc::c_long,
            0 as libc::c_long,
        )
    };
    if r < 0 {
        Err(std::io::Error::last_os_error().raw_os_error().unwrap_or(0))
    } else {
        Ok(())
    }
}

/// Wait until `kid` is a zombie, *without* collecting it: `WNOWAIT` is how a
/// parent tells "exited, still in the table" from "still running", and the
/// difference is exactly what this program's `ESRCH` claim turns on.
fn wait_until_zombie(kid: libc::pid_t) -> bool {
    for _ in 0..400 {
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        if waitid_pid(
            kid,
            &mut info,
            libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
        )
        .is_ok()
            && unsafe { info.si_pid() } == kid
        {
            return true;
        }
        sleep_ms(5);
    }
    false
}

/// The uapi's capability record and header, in the v3 wire shape (two records,
/// 32 + 32 bits).
#[repr(C)]
struct CapRecord {
    effective: u32,
    permitted: u32,
    inheritable: u32,
}

#[repr(C)]
struct CapHeader {
    version: u32,
    pid: libc::c_int,
}

const CAP_V3: u32 = 0x2008_0522;
/// Linux's `CAP_SYS_NICE`, the one `process_madvise` asks for when the target
/// is another process (`crates/cred`'s own `CAP_SYS_NICE`).
const CAP_SYS_NICE: u32 = 23;
/// The full set, as the initial credentials hold it.
const CAP_FULL_LO: u32 = 0xffff_ffff;
const CAP_FULL_HI: u32 = (1u32 << (40 - 31)) - 1;

/// Drop `CAP_SYS_NICE` from this (root) process's effective and permitted sets,
/// through `capset`'s v3 wire. Returns `0` or the negated errno.
fn drop_sys_nice() -> isize {
    let low = CAP_FULL_LO & !(1u32 << CAP_SYS_NICE);
    let mut header = CapHeader {
        version: CAP_V3,
        pid: 0,
    };
    let mut data = [
        CapRecord {
            effective: low,
            permitted: low,
            inheritable: 0,
        },
        CapRecord {
            effective: 0,
            permitted: CAP_FULL_HI,
            inheritable: 0,
        },
    ];
    let r = unsafe {
        libc::syscall(
            libc::SYS_capset,
            &mut header as *mut CapHeader as libc::c_long,
            data.as_mut_ptr() as libc::c_long,
        )
    };
    if r < 0 {
        -(std::io::Error::last_os_error().raw_os_error().unwrap_or(0) as isize)
    } else {
        0
    }
}

fn main() {
    println!("t-process-madvise");
    let mut r = Report::new();

    let arena = unsafe {
        libc::mmap(
            core::ptr::null_mut(),
            ARENA_LEN,
            libc::PROT_NONE,
            ANON_PRIVATE,
            -1,
            0,
        )
    };
    if arena as isize == -1 || arena == libc::MAP_FAILED {
        r.check("the fixture arena maps", false);
        r.finish();
    }
    unsafe { ARENA_BASE = arena };
    r.check("the fixture arena maps", true);
    let base = arena as usize;

    // The one self pidfd every self-target claim is made through.
    let self_fd = match pidfd_open(unsafe { libc::getpid() }, 0) {
        Ok(fd) => fd,
        Err(e) => {
            r.check("pidfd_open(self, 0) succeeds", false);
            println!("note: pidfd_open(self) answered errno {e}");
            r.finish();
        }
    };

    // ---- the ranges the self claims run over ---------------------------
    let span = mm(0x1_0000, 2 * PAGE, PROT_RW, ANON_PRIVATE | libc::MAP_FIXED);
    r.check(
        "the two-page self range maps fixed",
        span == (base + 0x1_0000) as isize,
    );
    let span = span as usize;

    // page then a genuine hole: the second page is unmapped again, so the
    // address exists in the arena reservation but no mapping covers it.
    let holed = mm(0x2_0000, 2 * PAGE, PROT_RW, ANON_PRIVATE | libc::MAP_FIXED);
    r.check(
        "the holed range maps fixed",
        holed == (base + 0x2_0000) as isize,
    );
    let holed = holed as usize;
    let hole = holed + PAGE;
    r.check(
        "the holed range's second page is unmapped again",
        unsafe { libc::munmap(hole as *mut libc::c_void, PAGE) } == 0,
    );

    // A page that stays mapped after the hole, for the "error stops the walk"
    // claim.
    let last = mm(0x3_0000, PAGE, PROT_RW, ANON_PRIVATE | libc::MAP_FIXED);
    r.check(
        "the after-the-hole page maps fixed",
        last == (base + 0x3_0000) as isize,
    );
    let last = last as usize;
    unsafe { (last as *mut u64).write_volatile(PATTERN) };

    // ---- the refusal order ---------------------------------------------
    r.check(
        "a nonzero flags word is EINVAL even beside an unreadable iovec, a bad advice and pidfd -1",
        pm_raw(-1, 1 as *const libc::iovec, 1, 5, 1) == Err(libc::EINVAL),
    );
    r.check(
        "vlen above IOV_MAX (1024) is EINVAL before the array is read",
        pm_raw(self_fd, core::ptr::null(), 1025, 3, 0) == Err(libc::EINVAL),
    );
    r.check(
        "an unreadable iovec array is EFAULT before the descriptor is looked at",
        pm_raw(self_fd, 1 as *const libc::iovec, 1, 3, 0) == Err(libc::EFAULT),
    );
    r.check(
        "pidfd -1 is EBADF",
        pm_raw(-1, one(span, PAGE).as_ptr(), 1, 3, 0) == Err(libc::EBADF),
    );
    // The descriptor is asked about *before* the advice vocabulary: -1 beside
    // advice 5 answers EBADF, while 5 on a real pidfd answers EINVAL.
    r.check(
        "pidfd -1 beside the invalid advice 5 is EBADF - the descriptor comes first",
        pm_raw(-1, one(span, PAGE).as_ptr(), 1, 5, 0) == Err(libc::EBADF),
    );
    match std::ffi::CString::new("/dev/null") {
        Ok(path) => {
            let devnull = unsafe { libc::open(path.as_ptr(), libc::O_RDONLY) };
            let answered = pm(devnull, &one(span, PAGE), 3, 0);
            r.check(
                "a descriptor that is not a pidfd is EBADF",
                answered == Err(libc::EBADF),
            );
            if devnull >= 0 {
                unsafe { libc::close(devnull) };
            }
        }
        Err(_) => r.check("a descriptor that is not a pidfd is EBADF", false),
    }
    r.check(
        "a closed descriptor number is EBADF",
        pm_raw(999, one(span, PAGE).as_ptr(), 1, 3, 0) == Err(libc::EBADF),
    );

    // The import's three refusals, in the order they are decided.
    r.check(
        "an iov_len above SSIZE_MAX is EINVAL (a negative ssize_t)",
        pm(self_fd, &one(span, usize::MAX), 3, 0) == Err(libc::EINVAL),
    );
    r.check(
        "a range that leaves user space is EFAULT",
        pm(self_fd, &one(0x1_0000_0000_0000, PAGE), 3, 0) == Err(libc::EFAULT),
    );

    // ---- the self-target answers ---------------------------------------
    r.check(
        "WILLNEED over one page answers the bytes served, not 0",
        pm(self_fd, &one(span, PAGE), 3, 0) == Ok(PAGE as isize),
    );
    r.check(
        "a 2000-byte range answers 2000 - the length is not rounded to pages",
        pm(self_fd, &one(span, 2000), 3, 0) == Ok(2000),
    );
    let two = [
        libc::iovec {
            iov_base: span as *mut libc::c_void,
            iov_len: PAGE,
        },
        libc::iovec {
            iov_base: (span + PAGE) as *mut libc::c_void,
            iov_len: PAGE,
        },
    ];
    r.check(
        "two iovecs answer the sum of their lengths",
        pm(self_fd, &two, 3, 0) == Ok(8192),
    );
    r.check(
        "a 0x1800-byte range over two mapped pages answers 6144",
        pm(self_fd, &one(span, 0x1800), 3, 0) == Ok(0x1800),
    );
    r.check(
        "a zero-length iovec is skipped entirely - not even its base's alignment is looked at",
        pm(self_fd, &one(span + 1, 0), 3, 0) == Ok(0),
    );
    r.check(
        "an unaligned base with a length is EINVAL",
        pm(self_fd, &one(span + 1, PAGE), 3, 0) == Err(libc::EINVAL),
    );
    r.check(
        "the retired advice 5 is EINVAL on a real range",
        pm(self_fd, &one(span, PAGE), 5, 0) == Err(libc::EINVAL),
    );
    r.check(
        "advice 5 is EINVAL even with no range at all - the vocabulary gate runs first",
        pm_raw(self_fd, core::ptr::null(), 0, 5, 0) == Err(libc::EINVAL),
    );
    r.check(
        "no iovecs over a recognised advice answers 0",
        pm_raw(self_fd, core::ptr::null(), 0, 23, 0) == Ok(0),
    );
    r.check(
        "MADV_COLLAPSE on the caller's own range is EINVAL - it is not a hint",
        pm(self_fd, &one(span, PAGE), MADV_COLLAPSE, 0) == Err(libc::EINVAL),
    );
    // This kernel's own answer: `100` is outside the vocabulary it inherits
    // from `madvise`, where Linux recognises it and asks `EPERM` for the
    // privilege. The difference is the inherited one, named in the header.
    let advice100 = pm(self_fd, &one(span, PAGE), 100, 0);
    r.check(
        "MADV_HWPOISON answers this kernel's EINVAL (Linux: EPERM - the inherited madvise divergence)",
        refused(advice100, libc::EINVAL)
            || host_run() && refused(advice100, libc::EPERM),
    );

    // ---- DONTNEED's page promise, and the hole rule ---------------------
    unsafe { (span as *mut u64).write_volatile(PATTERN) };
    r.check(
        "DONTNEED on the caller's own dirty page answers the bytes served",
        pm(self_fd, &one(span, PAGE), libc::MADV_DONTNEED, 0) == Ok(PAGE as isize),
    );
    r.check(
        "after DONTNEED the private page reads back zero",
        unsafe { (span as *const u64).read_volatile() } == 0,
    );

    // page then a hole: ENOMEM, but the DONTNEED of the mapped page before the
    // hole already happened (measured: the dirtied page reads back zero after
    // the call that answered ENOMEM).
    unsafe { (holed as *mut u64).write_volatile(PATTERN) };
    r.check(
        "a hole anywhere inside one iovec is ENOMEM",
        pm(self_fd, &one(holed, 2 * PAGE), 3, 0) == Err(libc::ENOMEM),
    );
    r.check(
        "DONTNEED over a range whose second page is a hole is ENOMEM",
        pm(self_fd, &one(holed, 2 * PAGE), libc::MADV_DONTNEED, 0) == Err(libc::ENOMEM),
    );
    r.check(
        "the mapped page before the hole was dropped all the same",
        unsafe { (holed as *const u64).read_volatile() } == 0,
    );
    // A completed iovec followed by a failing one answers the bytes served.
    let ok_then_hole = [
        libc::iovec {
            iov_base: last as *mut libc::c_void,
            iov_len: PAGE,
        },
        libc::iovec {
            iov_base: hole as *mut libc::c_void,
            iov_len: PAGE,
        },
    ];
    r.check(
        "one good iovec then a hole answers the bytes served, not the error",
        pm(self_fd, &ok_then_hole, 3, 0) == Ok(PAGE as isize),
    );
    // The reverse stop the walk: a failing iovec leaves the later ones alone.
    let hole_then_ok = [
        libc::iovec {
            iov_base: hole as *mut libc::c_void,
            iov_len: PAGE,
        },
        libc::iovec {
            iov_base: last as *mut libc::c_void,
            iov_len: PAGE,
        },
    ];
    let hole_first = pm(self_fd, &hole_then_ok, libc::MADV_DONTNEED, 0);
    r.check(
        "a hole in the first iovec answers ENOMEM",
        hole_first == Err(libc::ENOMEM),
    );
    r.check(
        "and the iovec after it was not touched",
        unsafe { (last as *const u64).read_volatile() } == PATTERN,
    );

    // ---- another process, through its pidfd -----------------------------
    //
    // One child announces a page it has mapped and then parks, so every
    // cross-process claim is made against a live, same-uid target. Its report
    // pipe carries the address (a 64-bit write), the control pipe releases it.
    let mut addr_pipe = [0i32; 2];
    let mut ctl_pipe = [0i32; 2];
    let ready = unsafe { libc::pipe(addr_pipe.as_mut_ptr()) } == 0
        && unsafe { libc::pipe(ctl_pipe.as_mut_ptr()) } == 0;
    let kid = if ready { unsafe { libc::fork() } } else { -1 };
    if kid == 0 {
        // The child: map a page of its own, say where, wait to be released.
        unsafe {
            libc::close(addr_pipe[0]);
            libc::close(ctl_pipe[1]);
        }
        let page = mm(0x4_0000, PAGE, PROT_RW, ANON_PRIVATE | libc::MAP_FIXED);
        let page = if page > 0 { page as u64 } else { 0 };
        unsafe {
            libc::write(
                addr_pipe[1],
                (&raw const page).cast::<libc::c_void>(),
                size_of::<u64>(),
            );
        }
        let mut byte = [0u8; 1];
        unsafe {
            libc::read(ctl_pipe[0], byte.as_mut_ptr().cast::<libc::c_void>(), 1);
        }
        leave(0);
    }
    if ready {
        unsafe {
            libc::close(addr_pipe[1]);
            libc::close(ctl_pipe[0]);
        }
    }
    let mut kid_page = 0u64;
    let got = ready
        && kid > 0
        && unsafe {
            libc::read(
                addr_pipe[0],
                (&raw mut kid_page).cast::<libc::c_void>(),
                size_of::<u64>(),
            )
        } == size_of::<u64>() as isize;
    let target = if got { pidfd_open(kid, 0).ok() } else { None };
    r.check(
        "the cross fixture's child announced a mapped page and a pidfd",
        target.is_some(),
    );
    match target {
        Some(fd) => {
            let range = one(kid_page as usize, PAGE);
            // DONTNEED is measured EINVAL across a process boundary, and the
            // vocabulary gate answers before the authority question.
            r.check(
                "DONTNEED for another process is EINVAL - it never crosses, even for root",
                pm(fd, &range, libc::MADV_DONTNEED, 0) == Err(libc::EINVAL),
            );
            r.check(
                "an advice outside the four-advice cross set is EINVAL",
                pm(fd, &range, libc::MADV_NORMAL, 0) == Err(libc::EINVAL),
            );
            // Root holds CAP_SYS_NICE, so the whitelisted hint is served; the
            // bytes are the promise. A host run is a non-root run (the probes
            // ran with an empty effective set), so that claim is named there.
            let served = pm(fd, &range, libc::MADV_WILLNEED, 0);
            r.check(
                "WILLNEED for another process is served as its byte count when CAP_SYS_NICE is held",
                served == Ok(PAGE as isize) || host_run(),
            );
            // This kernel's own refusal: no huge-page pool exists to collapse
            // a range into, so the promise cannot be kept.
            let collapse = pm(fd, &range, MADV_COLLAPSE, 0);
            r.check(
                "MADV_COLLAPSE across a process boundary answers this kernel's EOPNOTSUPP",
                refused(collapse, libc::EOPNOTSUPP) || host_run(),
            );
            unsafe { libc::close(fd) };
        }
        None => {
            r.check(
                "DONTNEED for another process is EINVAL - it never crosses, even for root",
                false,
            );
        }
    }
    if ready {
        unsafe {
            let byte = [b'x'];
            libc::write(ctl_pipe[1], byte.as_ptr().cast::<libc::c_void>(), 1);
            libc::close(ctl_pipe[1]);
            libc::waitpid(kid, core::ptr::null_mut(), 0);
        }
    }

    // ---- the authority gate --------------------------------------------
    //
    // A child that drops `CAP_SYS_NICE` (root holds `CAP_SETPCAP`, so the
    // lowering is accepted) calls the same call on its *parent*: the advice is
    // whitelisted and the descriptor good, so the answer is the authority
    // answer, EPERM.
    let parent_page = mm(0x5_0000, PAGE, PROT_RW, ANON_PRIVATE | libc::MAP_FIXED);
    r.check(
        "the parent's page for the child to advise maps fixed",
        parent_page == (base + 0x5_0000) as isize,
    );
    let mut report = [0i32; 2];
    let made = unsafe { libc::pipe(report.as_mut_ptr()) } == 0;
    let waiter = if made { unsafe { libc::fork() } } else { -1 };
    if waiter == 0 {
        unsafe { libc::close(report[0]) };
        let dropped = drop_sys_nice();
        let mut outcome = i64::MIN;
        let parent = pidfd_open(unsafe { libc::getppid() }, 0);
        if let Ok(fd) = parent {
            let answered = pm(fd, &one(parent_page as usize, PAGE), libc::MADV_WILLNEED, 0);
            outcome = match answered {
                Ok(bytes) => bytes as i64,
                Err(errno) => -(errno as i64),
            };
        }
        // Two facts, two words: the lowering's own answer and the call's.
        let words = [dropped as i64, outcome];
        unsafe {
            libc::write(
                report[1],
                words.as_ptr().cast::<libc::c_void>(),
                size_of::<[i64; 2]>(),
            );
        }
        leave(0);
    }
    if made {
        unsafe { libc::close(report[1]) };
    }
    let mut words = [i64::MIN; 2];
    let read = made
        && waiter > 0
        && unsafe {
            libc::read(
                report[0],
                words.as_mut_ptr().cast::<libc::c_void>(),
                size_of::<[i64; 2]>(),
            )
        } == size_of::<[i64; 2]>() as isize;
    r.check(
        "the child's capset dropped CAP_SYS_NICE (it held CAP_SETPCAP to do it)",
        read && words[0] == 0 || host_run(),
    );
    r.check(
        "a caller without CAP_SYS_NICE gets EPERM for a whitelisted advice",
        read && words[1] == -(libc::EPERM as i64),
    );
    if made && waiter > 0 {
        unsafe { libc::waitpid(waiter, core::ptr::null_mut(), 0) };
    }

    // ---- a dead-but-unreaped target -------------------------------------
    //
    // Open the pidfd while the child is alive (Linux refuses `pidfd_open` for
    // one already reaped), then stop it and ask: a task whose address space is
    // gone is ESRCH even though the descriptor still names it.
    let mut stop = [0i32; 2];
    let stoppable = unsafe { libc::pipe(stop.as_mut_ptr()) } == 0;
    let zombie = if stoppable {
        unsafe { libc::fork() }
    } else {
        -1
    };
    if zombie == 0 {
        unsafe {
            libc::close(stop[1]);
            let mut byte = [0u8; 1];
            libc::read(stop[0], byte.as_mut_ptr().cast::<libc::c_void>(), 1);
        }
        leave(0);
    }
    if stoppable {
        unsafe { libc::close(stop[0]) };
    }
    let zombie_fd = if stoppable && zombie > 0 {
        pidfd_open(zombie, 0).ok()
    } else {
        None
    };
    r.check("the pidfd of a live child opens", zombie_fd.is_some());
    if let Some(fd) = zombie_fd {
        if stoppable {
            unsafe {
                let byte = [b'x'];
                libc::write(stop[1], byte.as_ptr().cast::<libc::c_void>(), 1);
                libc::close(stop[1]);
            }
        }
        r.check("the child became a zombie", wait_until_zombie(zombie));
        r.check(
            "process_madvise through a zombie's pidfd is ESRCH - its address space is gone",
            pm(fd, &one(span, PAGE), libc::MADV_WILLNEED, 0) == Err(libc::ESRCH),
        );
        unsafe {
            libc::waitpid(zombie, core::ptr::null_mut(), 0);
            libc::close(fd);
        }
    }

    // A reaped child cannot be named at all - the door before the descriptor.
    let reaped = unsafe { libc::fork() };
    if reaped == 0 {
        leave(0);
    }
    if reaped > 0 {
        unsafe { libc::waitpid(reaped, core::ptr::null_mut(), 0) };
        r.check(
            "pidfd_open of a reaped child is ESRCH",
            pidfd_refused(reaped, libc::ESRCH),
        );
    }

    let _ = unsafe { libc::munmap(arena, ARENA_LEN) };
    unsafe { libc::close(self_fd) };

    r.finish()
}
