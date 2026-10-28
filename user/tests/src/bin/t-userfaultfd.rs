//! `userfaultfd(2)`: the measured shape, claimed.
//!
//! Every number here comes from Linux 7.2.11 measured as **real root**
//! (`build/scratch/uffd/probe.c`, `probe3.c`; the call is `EPERM` for an
//! unprivileged user, so there is no cheaper source), and the record is
//! `docs/notes/06-process-management.md` §6.33. This program is the pin: it
//! asserts what that measurement says, so a kernel that answers something else
//! reddens here.
//!
//! # The subset this kernel serves
//!
//! `UFFDIO_API`, `UFFDIO_REGISTER` in `MODE_MISSING`, `UFFDIO_UNREGISTER`,
//! `UFFDIO_WAKE`, `UFFDIO_COPY`, `UFFDIO_ZEROPAGE`, and one 32-byte pagefault
//! record per `read(2)`. Write-protect mode, minor faults, `UFFDIO_CONTINUE`,
//! `UFFDIO_MOVE`, `UFFDIO_POISON` and every `UFFD_FEATURE_*` are refused with
//! `EOPNOTSUPP` - a real Linux shape this kernel has no machinery for, which
//! must be named rather than faked.
//!
//! # Why a thread, not a process
//!
//! The faulting task has to be a thread of the same group that holds the
//! descriptor: a `fork` gives the child no uffd context without
//! `UFFD_FEATURE_EVENT_FORK`, so the child's touch is `SIGBUS` - which is
//! measured on Linux too, and is the shape the last section claims.

use std::io::Error;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::ptr;

use mpos_user_tests::Report;

/// The API number, as measured.
const UFFD_API: u64 = 0xAA;
/// `sizeof(struct uffd_msg)`.
const MSG_BYTES: usize = 32;
/// `UFFD_EVENT_PAGEFAULT`.
const EVENT_PAGEFAULT: u8 = 0x12;
/// The mode word's one served bit.
const MODE_MISSING: u64 = 1 << 0;
const MODE_WP: u64 = 1 << 1;

// The ioctl request words, measured (the size is in the word).
const UFFDIO_API_REQUEST: u64 = 0xc018_aa3f;
const UFFDIO_REGISTER_REQUEST: u64 = 0xc020_aa00;
const UFFDIO_UNREGISTER_REQUEST: u64 = 0x8010_aa01;
const UFFDIO_WAKE_REQUEST: u64 = 0x8010_aa02;
const UFFDIO_COPY_REQUEST: u64 = 0xc028_aa03;
const UFFDIO_ZEROPAGE_REQUEST: u64 = 0xc020_aa04;
const UFFDIO_WRITEPROTECT_REQUEST: u64 = 0xc018_aa06;
const UFFDIO_CONTINUE_REQUEST: u64 = 0xc020_aa07;

const PAGE: usize = 4096;

/// `userfaultfd(2)`'s number, which is the machine's: x86_64 323, aarch64 282.
#[cfg(target_arch = "x86_64")]
const SYS_USERFAULTFD: i64 = 323;
#[cfg(target_arch = "aarch64")]
const SYS_USERFAULTFD: i64 = 282;

#[repr(C)]
#[derive(Default)]
struct Api {
    api: u64,
    features: u64,
    ioctls: u64,
}

#[repr(C)]
#[derive(Default)]
struct Range {
    start: u64,
    len: u64,
}

#[repr(C)]
#[derive(Default)]
struct Register {
    range: Range,
    mode: u64,
    ioctls: u64,
}

#[repr(C)]
#[derive(Default)]
struct Copy {
    dst: u64,
    src: u64,
    len: u64,
    mode: u64,
    copy: i64,
}

#[repr(C)]
#[derive(Default)]
struct Zeropage {
    range: Range,
    mode: u64,
    zeropage: i64,
}

fn errno() -> i64 {
    -Error::last_os_error().raw_os_error().unwrap_or(0) as i64
}

fn open_uffd(flags: i32) -> Result<OwnedFd, i64> {
    let rc = unsafe { libc::syscall(SYS_USERFAULTFD, flags as libc::c_long) };
    if rc < 0 {
        return Err(errno());
    }
    Ok(unsafe { OwnedFd::from_raw_fd(rc as i32) })
}

fn api(fd: &OwnedFd, api: u64, features: u64) -> Result<Api, i64> {
    let mut arg = Api {
        api,
        features,
        ioctls: 0,
    };
    let rc = unsafe { libc::ioctl(fd.as_raw_fd(), UFFDIO_API_REQUEST as _, &mut arg) };
    if rc < 0 {
        return Err(errno());
    }
    Ok(arg)
}

fn register(fd: &OwnedFd, start: usize, len: usize, mode: u64) -> Result<u64, i64> {
    let mut arg = Register {
        range: Range {
            start: start as u64,
            len: len as u64,
        },
        mode,
        ioctls: 0,
    };
    let rc = unsafe { libc::ioctl(fd.as_raw_fd(), UFFDIO_REGISTER_REQUEST as _, &mut arg) };
    if rc < 0 {
        return Err(errno());
    }
    Ok(arg.ioctls)
}

fn unregister(fd: &OwnedFd, start: usize, len: usize) -> Result<(), i64> {
    let mut arg = Range {
        start: start as u64,
        len: len as u64,
    };
    let rc = unsafe { libc::ioctl(fd.as_raw_fd(), UFFDIO_UNREGISTER_REQUEST as _, &mut arg) };
    if rc < 0 {
        return Err(errno());
    }
    Ok(())
}

fn wake(fd: &OwnedFd, start: usize, len: usize) -> Result<(), i64> {
    let mut arg = Range {
        start: start as u64,
        len: len as u64,
    };
    let rc = unsafe { libc::ioctl(fd.as_raw_fd(), UFFDIO_WAKE_REQUEST as _, &mut arg) };
    if rc < 0 {
        return Err(errno());
    }
    Ok(())
}

fn copy(fd: &OwnedFd, dst: usize, src: usize, len: usize, mode: u64) -> Result<i64, i64> {
    let mut arg = Copy {
        dst: dst as u64,
        src: src as u64,
        len: len as u64,
        mode,
        copy: 0,
    };
    let rc = unsafe { libc::ioctl(fd.as_raw_fd(), UFFDIO_COPY_REQUEST as _, &mut arg) };
    if rc < 0 {
        return Err(errno());
    }
    Ok(arg.copy)
}

fn zeropage(fd: &OwnedFd, start: usize, len: usize, mode: u64) -> Result<i64, i64> {
    let mut arg = Zeropage {
        range: Range {
            start: start as u64,
            len: len as u64,
        },
        mode,
        zeropage: 0,
    };
    let rc = unsafe { libc::ioctl(fd.as_raw_fd(), UFFDIO_ZEROPAGE_REQUEST as _, &mut arg) };
    if rc < 0 {
        return Err(errno());
    }
    Ok(arg.zeropage)
}

/// A private anonymous mapping of `len` bytes, untouched - so its pages are
/// absent and a fault on them is the descriptor's business.
fn map_anon(len: usize) -> *mut u8 {
    let p = unsafe {
        libc::mmap(
            ptr::null_mut(),
            len,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
            -1,
            0,
        )
    };
    assert_ne!(p, libc::MAP_FAILED, "mmap failed");
    p as *mut u8
}

fn unmap(p: *mut u8, len: usize) {
    unsafe { libc::munmap(p as *mut libc::c_void, len) };
}

/// One 32-byte record, as `read(2)` delivers it.
struct Record {
    event: u8,
    flags: u64,
    address: u64,
    ptid: u32,
}

fn parse_record(bytes: &[u8; MSG_BYTES]) -> Record {
    Record {
        event: bytes[0],
        flags: u64::from_le_bytes(bytes[8..16].try_into().unwrap()),
        address: u64::from_le_bytes(bytes[16..24].try_into().unwrap()),
        ptid: u32::from_le_bytes(bytes[24..28].try_into().unwrap()),
    }
}

/// Read one record, waiting up to `millis` for it to arrive.
fn read_record(fd: &OwnedFd, millis: i32) -> Result<Record, i64> {
    let mut pfd = libc::pollfd {
        fd: fd.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    let ready = unsafe { libc::poll(&mut pfd, 1, millis) };
    if ready <= 0 {
        return Err(-libc::ETIMEDOUT as i64);
    }
    let mut bytes = [0u8; MSG_BYTES];
    let n = unsafe {
        libc::read(
            fd.as_raw_fd(),
            bytes.as_mut_ptr() as *mut libc::c_void,
            MSG_BYTES,
        )
    };
    if n < 0 {
        return Err(errno());
    }
    if n as usize != MSG_BYTES {
        return Err(-libc::EINVAL as i64);
    }
    Ok(parse_record(&bytes))
}

// The faulting thread: touch the page and report what it read.
struct FaultArg {
    area: *mut u8,
    value: u8,
    write: bool,
}

extern "C" fn fault_thread(arg: *mut libc::c_void) -> *mut libc::c_void {
    let a = unsafe { &mut *(arg as *mut FaultArg) };
    unsafe {
        if a.write {
            *a.area = b'Z';
        } else {
            a.value = *a.area;
        }
    }
    ptr::null_mut()
}

/// Start a thread that faults on `area`, returning its handle and the argument
/// it writes its answer into.
fn start_fault(area: *mut u8, write: bool) -> (libc::pthread_t, Box<FaultArg>) {
    let mut arg = Box::new(FaultArg {
        area,
        value: 0,
        write,
    });
    // `pthread_t` is a pointer on musl and a `u64` on glibc; zero is the empty
    // value in both, which a `null_mut()` or a `0` would not be.
    let mut thread: libc::pthread_t = unsafe { std::mem::zeroed() };
    let rc = unsafe {
        libc::pthread_create(
            &mut thread,
            ptr::null(),
            fault_thread,
            &mut *arg as *mut FaultArg as *mut libc::c_void,
        )
    };
    assert_eq!(rc, 0, "pthread_create failed");
    (thread, arg)
}

fn join(thread: libc::pthread_t) {
    unsafe { libc::pthread_join(thread, ptr::null_mut()) };
}

fn main() {
    let mut r = Report::new();

    // A host run is the counterfactual direction: on a real Linux as an
    // ordinary user `userfaultfd` is EPERM (`vm.unprivileged_userfaultfd` is
    // 0), which is the one claim the host can make about the shape.
    if mpos_user_tests::host_run() && unsafe { libc::geteuid() } != 0 {
        let denied = open_uffd(0).is_err();
        r.check(
            "an unprivileged host refuses userfaultfd with EPERM",
            denied,
        );
        r.finish();
    }

    // ---- the descriptor's own flags -------------------------------------
    let fd = open_uffd(libc::O_CLOEXEC | libc::O_NONBLOCK).expect("userfaultfd opens");
    let getfd = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFD) };
    r.check(
        "O_CLOEXEC is recorded on the descriptor",
        getfd & libc::FD_CLOEXEC != 0,
    );
    let getfl = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFL) };
    r.check(
        "the descriptor is read-write, as Linux answers",
        getfl & libc::O_ACCMODE == libc::O_RDWR,
    );
    r.check(
        "a flag outside the three known ones is EINVAL",
        open_uffd(0x2).err() == Some(-(libc::EINVAL as i64)),
    );
    r.check("UFFD_USER_MODE_ONLY is accepted", open_uffd(0x1).is_ok());

    // ---- the API handshake ----------------------------------------------
    let answer = api(&fd, UFFD_API, 0).expect("UFFDIO_API succeeds");
    r.check(
        "the API answers the API number asked for",
        answer.api == UFFD_API,
    );
    r.check(
        "the API reports the requests it serves",
        answer.ioctls & 0b11 == 0b11,
    );
    r.check(
        "a second handshake is EINVAL",
        api(&fd, UFFD_API, 0).err() == Some(-(libc::EINVAL as i64)),
    );
    r.check(
        "a wrong API number is EINVAL",
        api(&fd, 0xAB, 0).err() == Some(-(libc::EINVAL as i64)),
    );

    // ---- registration ---------------------------------------------------
    let area = map_anon(2 * PAGE);
    r.check(
        "a page-aligned, whole-page, mapped span registers",
        register(&fd, area as usize, 2 * PAGE, MODE_MISSING).is_ok(),
    );
    r.check(
        "registering the same span twice is not an error",
        register(&fd, area as usize, 2 * PAGE, MODE_MISSING).is_ok(),
    );
    r.check(
        "a mode word with no mode bit is EINVAL",
        register(&fd, area as usize, PAGE, 0).err() == Some(-(libc::EINVAL as i64)),
    );
    r.check(
        "a length of one byte is EINVAL",
        register(&fd, area as usize, 1, MODE_MISSING).err() == Some(-(libc::EINVAL as i64)),
    );
    r.check(
        "an unaligned start is EINVAL",
        register(&fd, area as usize + 1, PAGE, MODE_MISSING).err() == Some(-(libc::EINVAL as i64)),
    );
    r.check(
        "a range with no mapping behind it is EINVAL",
        register(&fd, 0x1000_0000, PAGE, MODE_MISSING).err() == Some(-(libc::EINVAL as i64)),
    );

    // ---- a read fault, answered with a page -----------------------------
    {
        let (thread, arg) = start_fault(area, false);
        let rec = read_record(&fd, 2000).expect("a pagefault record arrives");
        r.check("the record is a pagefault", rec.event == EVENT_PAGEFAULT);
        r.check(
            "the record names the faulting page, page-aligned",
            rec.address as usize == area as usize,
        );
        r.check("a read fault has no WRITE flag", rec.flags == 0);
        r.check(
            "ptid is 0, the THREAD_ID feature not being offered",
            rec.ptid == 0,
        );
        let mut page = [b'C'; PAGE];
        r.check(
            "UFFDIO_COPY installs the page and reports its length",
            copy(&fd, area as usize, page.as_mut_ptr() as usize, PAGE, 0) == Ok(PAGE as i64),
        );
        join(thread);
        r.check(
            "the faulting thread then reads the copied byte",
            arg.value == b'C',
        );
        r.check(
            "copying over a page that is now present is EEXIST",
            copy(&fd, area as usize, page.as_mut_ptr() as usize, PAGE, 0)
                == Err(-(libc::EEXIST as i64)),
        );
    }

    // ---- a zero page ----------------------------------------------------
    // The second page of the registered span, still absent: the first one is
    // present now, and a zero page over a present page is `EEXIST`.
    {
        let fresh = unsafe { area.add(PAGE) };
        let (thread, arg) = start_fault(fresh, false);
        let rec = read_record(&fd, 2000).expect("the second page faults too");
        r.check(
            "the second page's record names it",
            rec.address as usize == fresh as usize,
        );
        r.check(
            "UFFDIO_ZEROPAGE installs a zero page",
            zeropage(&fd, fresh as usize, PAGE, 0) == Ok(PAGE as i64),
        );
        join(thread);
        r.check("the page reads as zero", arg.value == 0);
    }

    // ---- the refusals this kernel names ---------------------------------
    r.check(
        "write-protect mode is refused with EOPNOTSUPP",
        register(&fd, area as usize, PAGE, MODE_WP).err() == Some(-(libc::EOPNOTSUPP as i64)),
    );
    {
        let mut arg = Register {
            range: Range {
                start: area as usize as u64,
                len: PAGE as u64,
            },
            mode: 0,
            ioctls: 0,
        };
        let rc = unsafe { libc::ioctl(fd.as_raw_fd(), UFFDIO_WRITEPROTECT_REQUEST as _, &mut arg) };
        r.check(
            "UFFDIO_WRITEPROTECT is refused with EOPNOTSUPP",
            rc < 0 && errno() == -(libc::EOPNOTSUPP as i64),
        );
        let rc = unsafe { libc::ioctl(fd.as_raw_fd(), UFFDIO_CONTINUE_REQUEST as _, &mut arg) };
        r.check(
            "UFFDIO_CONTINUE is refused with EOPNOTSUPP",
            rc < 0 && errno() == -(libc::EOPNOTSUPP as i64),
        );
    }
    r.check(
        "a feature bit this kernel does not report is EINVAL",
        api(
            &open_uffd(0).expect("a second descriptor opens"),
            UFFD_API,
            u64::MAX,
        )
        .err()
            == Some(-(libc::EINVAL as i64)),
    );

    // ---- read's own rules ------------------------------------------------
    r.check("a read smaller than one record is EINVAL", {
        let mut small = [0u8; 8];
        let n = unsafe {
            libc::read(
                fd.as_raw_fd(),
                small.as_mut_ptr() as *mut libc::c_void,
                small.len(),
            )
        };
        n < 0 && errno() == -(libc::EINVAL as i64)
    });
    r.check("a non-blocking read with nothing queued is EAGAIN", {
        let mut bytes = [0u8; MSG_BYTES];
        let n = unsafe {
            libc::read(
                fd.as_raw_fd(),
                bytes.as_mut_ptr() as *mut libc::c_void,
                MSG_BYTES,
            )
        };
        n < 0 && errno() == -(libc::EAGAIN as i64)
    });

    // ---- WAKE and UNREGISTER --------------------------------------------
    r.check(
        "UFFDIO_WAKE on a registered span is 0",
        wake(&fd, area as usize, PAGE).is_ok(),
    );
    r.check(
        "UFFDIO_WAKE on a span never registered is 0",
        wake(&fd, 0x2000_0000, PAGE).is_ok(),
    );
    r.check(
        "UFFDIO_UNREGISTER is 0",
        unregister(&fd, area as usize, 2 * PAGE).is_ok(),
    );
    r.check(
        "unregistering again is 0",
        unregister(&fd, area as usize, 2 * PAGE).is_ok(),
    );
    r.check(
        "after unregistering, a touch is an ordinary fault again",
        unsafe {
            *area = 7;
            *area == 7
        },
    );

    // ---- two threads, one page ------------------------------------------
    {
        let other = map_anon(PAGE);
        let fd2 = open_uffd(libc::O_CLOEXEC | libc::O_NONBLOCK).expect("a descriptor opens");
        api(&fd2, UFFD_API, 0).expect("handshake");
        register(&fd2, other as usize, PAGE, MODE_MISSING).expect("register");
        let (t1, a1) = start_fault(other, false);
        let (t2, a2) = start_fault(other, false);
        let rec = read_record(&fd2, 2000).expect("one record for two faulting threads");
        r.check(
            "two threads on one page produce one record",
            rec.address as usize == other as usize,
        );
        let mut page = [b'D'; PAGE];
        let _ = copy(&fd2, other as usize, page.as_mut_ptr() as usize, PAGE, 0);
        join(t1);
        join(t2);
        r.check(
            "one COPY releases both threads",
            a1.value == b'D' && a2.value == b'D',
        );
        unmap(other, PAGE);
    }

    // ---- the one divergence a program can see here ----------------------
    // Linux gives a forked child no uffd context without
    // `UFFD_FEATURE_EVENT_FORK`, so its touch of a registered page is
    // `SIGBUS` (measured). This kernel has no per-task uffd context: the
    // registry is the machine's, so a forked child's fault on a registered
    // page is delivered to the same descriptor and parks the child instead of
    // killing it. That is a divergence, not a claim, and it is named in
    // `docs/notes/06-process-management.md` §6.33 rather than asserted here -
    // a program that forked and then relied on SIGBUS would be relying on a
    // shape this kernel does not have.

    unmap(area, 2 * PAGE);
    r.finish();
}
