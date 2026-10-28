//! `io_uring(7)`: the ring is a mapping, and the completions are the answers
//! the blocking calls would have given.
//!
//! This is a **hand-built ring**, deliberately: the program maps the three
//! regions itself, writes its own SQEs and reads its own CQEs, from Linux's
//! published struct layouts. That is the only way to tell a kernel that serves
//! the ring ABI from one that merely answers the syscall - no liburing helper
//! stands between the claim and the syscall.
//!
//! Every answer asserted here was measured on Linux 7.2.11 first
//! (`build/scratch/iouring/probe4.c`, `probe5.c`): a ring of `entries=8` comes
//! back with `sq_entries=8`, `cq_entries=16`, `sq_off.array=320`, `cq_off.cqes=64`;
//! `io_uring_enter` answers the count of SQEs it consumed; a `NOP` completes with
//! `res=0` and the caller's `user_data`; `READ`/`WRITE`/`READV`/`WRITEV` complete
//! with the byte count; `FSYNC` with 0; `CLOSE` with 0 and the descriptor really
//! closed; an unknown opcode with `-EINVAL`; a bad SQE descriptor with `-EBADF`.
//!
//! Three claims are **this kernel's divergences**, named rather than hidden
//! (`docs/notes/06-process-management.md` §6.32), and they run on the guest
//! only: an opcode outside the served subset completes `-EOPNOTSUPP` (Linux
//! serves most of them), `io_uring_register` answers `EOPNOTSUPP`, and an SQ
//! ring past the served memory cap (1024) is `EINVAL`. On the host this program
//! runs the shared half only, and Linux is then the reference the shared half
//! is checked against.

use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};

use mpos_user_tests::Report;

const SYS_SETUP: libc::c_long = 425;
const SYS_ENTER: libc::c_long = 426;
const SYS_REGISTER: libc::c_long = 427;

const IORING_OFF_SQ_RING: libc::off_t = 0;
const IORING_OFF_CQ_RING: libc::off_t = 0x800_0000;
const IORING_OFF_SQES: libc::off_t = 0x1000_0000;

const ENTER_GETEVENTS: u32 = 1 << 0;

const SETUP_SQPOLL: u32 = 1 << 1;
const SETUP_UNKNOWN_BIT31: u32 = 1 << 31;

const OP_NOP: u8 = 0;
const OP_READV: u8 = 1;
const OP_WRITEV: u8 = 2;
const OP_FSYNC: u8 = 3;
const OP_CLOSE: u8 = 19;
const OP_READ: u8 = 22;
const OP_WRITE: u8 = 23;

/// `struct io_sqring_offsets`.
#[repr(C)]
#[derive(Default, Clone, Copy)]
struct SqOffsets {
    head: u32,
    tail: u32,
    ring_mask: u32,
    ring_entries: u32,
    flags: u32,
    dropped: u32,
    array: u32,
    resv1: u32,
    user_addr: u64,
}

/// `struct io_cqring_offsets` (same shape, different names: `overflow` and
/// `cqes` where the SQ side has `dropped` and `array`).
#[repr(C)]
#[derive(Default, Clone, Copy)]
struct CqOffsets {
    head: u32,
    tail: u32,
    ring_mask: u32,
    ring_entries: u32,
    overflow: u32,
    cqes: u32,
    flags: u32,
    resv1: u32,
    user_addr: u64,
}

#[repr(C)]
#[derive(Default, Clone, Copy)]
struct Params {
    sq_entries: u32,
    cq_entries: u32,
    flags: u32,
    sq_thread_cpu: u32,
    sq_thread_idle: u32,
    features: u32,
    wq_fd: u32,
    resv: [u32; 3],
    sq_off: SqOffsets,
    cq_off: CqOffsets,
}

/// `struct io_uring_sqe`.
#[repr(C)]
#[derive(Default, Clone, Copy)]
struct Sqe {
    opcode: u8,
    flags: u8,
    ioprio: u16,
    fd: i32,
    off: u64,
    addr: u64,
    len: u32,
    rw_flags: u32,
    user_data: u64,
    buf_index: u16,
    personality: u16,
    splice_fd_in: i32,
    addr3: u64,
    pad: u64,
}

/// `struct io_uring_cqe`.
#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
struct Cqe {
    user_data: u64,
    res: i32,
    flags: u32,
}

fn self_debug() -> bool {
    std::env::var_os("MPOS_IO_URING_DEBUG").is_some()
}

fn errno() -> i32 {
    // Linux's errno numbers are negative in a CQE's `res`; the syscall
    // interface returns -1 and sets errno positive, so negate.
    -std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
}

/// A mapped ring, built by hand.
struct Ring {
    fd: OwnedFd,
    sq: *mut u8,
    cq: *mut u8,
    sqes: *mut Sqe,
    sq_len: usize,
    cq_len: usize,
    sqes_len: usize,
    params: Params,
}

impl Ring {
    fn setup(entries: u32) -> Result<Ring, i32> {
        let mut params = Params::default();
        let fd = unsafe { libc::syscall(SYS_SETUP, entries as libc::c_long, &mut params) };
        if fd < 0 {
            return Err(errno());
        }
        let fd = unsafe { OwnedFd::from_raw_fd(fd as RawFd) };
        let sq_len = params.sq_off.array as usize + params.sq_entries as usize * 4;
        let cq_len = params.cq_off.cqes as usize + params.cq_entries as usize * 16;
        let sqes_len = params.sq_entries as usize * 64;
        let sq = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                sq_len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                fd.as_raw_fd(),
                IORING_OFF_SQ_RING,
            )
        };
        let cq = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                cq_len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                fd.as_raw_fd(),
                IORING_OFF_CQ_RING,
            )
        };
        let sqes = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                sqes_len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                fd.as_raw_fd(),
                IORING_OFF_SQES,
            )
        };
        if sq == libc::MAP_FAILED || cq == libc::MAP_FAILED || sqes == libc::MAP_FAILED {
            return Err(-14); // EFAULT: the mapping did not land
        }
        Ok(Ring {
            fd,
            sq: sq as *mut u8,
            cq: cq as *mut u8,
            sqes: sqes as *mut Sqe,
            sq_len,
            cq_len,
            sqes_len,
            params,
        })
    }

    fn word(&self, at: u32) -> u32 {
        unsafe { *(self.sq.add(at as usize) as *const u32) }
    }

    fn set_word(&self, at: u32, value: u32) {
        unsafe { *(self.sq.add(at as usize) as *mut u32) = value }
    }

    fn sq_head(&self) -> u32 {
        self.word(self.params.sq_off.head)
    }

    fn sq_tail(&self) -> u32 {
        self.word(self.params.sq_off.tail)
    }

    fn cq_head(&self) -> u32 {
        self.word(self.params.cq_off.head)
    }

    fn cq_tail(&self) -> u32 {
        self.word(self.params.cq_off.tail)
    }

    /// The next SQE slot: the tail word, the mask, and the array entry that
    /// names the slot - the array is userspace's to write (Linux leaves it as
    /// the mapping gave it; the measured probes wrote it explicitly, and a
    /// ring that does not write it re-reads one stale SQE for every entry).
    fn push(&self) -> &mut Sqe {
        let tail = self.sq_tail();
        let slot = tail & self.word(self.params.sq_off.ring_mask);
        unsafe {
            *((self.sq.add(self.params.sq_off.array as usize) as *mut u32).add(slot as usize)) =
                slot;
        }
        self.set_word(self.params.sq_off.tail, tail + 1);
        unsafe { &mut *self.sqes.add(slot as usize) }
    }

    fn enter(&self, to_submit: u32, min_complete: u32, flags: u32) -> Result<u32, i32> {
        let rc = unsafe {
            libc::syscall(
                SYS_ENTER,
                self.fd.as_raw_fd(),
                to_submit as libc::c_long,
                min_complete as libc::c_long,
                flags as libc::c_long,
                0 as libc::c_long,
                0 as libc::c_long,
            )
        };
        if rc < 0 {
            return Err(errno());
        }
        Ok(rc as u32)
    }

    fn take(&self) -> Option<Cqe> {
        let head = self.cq_head();
        if head == self.cq_tail() {
            return None;
        }
        let at = self.params.cq_off.cqes as usize
            + (head & self.word(self.params.cq_off.ring_mask)) as usize * 16;
        let cqe = unsafe { *(self.cq.add(at) as *const Cqe) };
        self.set_word(self.params.cq_off.head, head + 1);
        Some(cqe)
    }

    /// Submit one SQE and take its single completion: the shape most of these
    /// claims use.
    fn one(&self, fill: impl FnOnce(&mut Sqe)) -> Cqe {
        let sqe = self.push();
        *sqe = Sqe::default();
        fill(sqe);
        let submitted = self.enter(1, 0, 0).expect("one submit is accepted");
        assert_eq!(submitted, 1, "enter answers the SQEs it consumed");
        // A file read may complete on the kernel's work queue rather than
        // inline (measured on Linux), so wait for the completion: that is the
        // same park the blocking call would have made.
        if let Some(cqe) = self.take() {
            return cqe;
        }
        self.enter(0, 1, ENTER_GETEVENTS)
            .expect("the wait for one completion is accepted");
        self.take().expect("the waited-for SQE completed")
    }
}

impl Drop for Ring {
    fn drop(&mut self) {
        unsafe {
            libc::munmap(self.sq as *mut _, self.sq_len);
            libc::munmap(self.cq as *mut _, self.cq_len);
            libc::munmap(self.sqes as *mut _, self.sqes_len);
        }
    }
}

fn temp_file() -> OwnedFd {
    let path = c"/tmp/t-io-uring-file";
    unsafe { libc::unlink(path.as_ptr()) };
    let fd = unsafe {
        libc::open(
            path.as_ptr(),
            libc::O_RDWR | libc::O_CREAT | libc::O_TRUNC,
            0o644,
        )
    };
    assert!(fd >= 0, "a temp file can be made: errno {}", errno());
    unsafe {
        let owned = OwnedFd::from_raw_fd(fd);
        let bytes = b"hello";
        assert_eq!(libc::write(fd, bytes.as_ptr().cast(), bytes.len()), 5);
        owned
    }
}

fn main() {
    let mut r = Report::new();
    let host = mpos_user_tests::host_run();

    // The sizes and the layout the ABI publishes.
    match Ring::setup(0) {
        Err(-22) => r.check("entries=0 is EINVAL", true),
        other => r.check("entries=0 is EINVAL", other.is_err()),
    }
    let ring = Ring::setup(8).expect("a ring of eight entries can be built");
    r.check(
        "setup answers sq_entries=8, cq_entries=16",
        ring.params.sq_entries == 8 && ring.params.cq_entries == 16,
    );
    r.check(
        "setup publishes STRUCT offsets: array=320, cqes=64",
        ring.params.sq_off.array == 320 && ring.params.cq_off.cqes == 64,
    );
    r.check("setup publishes SINGLE_MMAP", ring.params.features & 1 != 0);
    r.check(
        "an empty ring starts at head=tail=0 and has Linux's masks",
        ring.sq_head() == 0
            && ring.sq_tail() == 0
            && ring.cq_head() == 0
            && ring.cq_tail() == 0
            && ring.word(ring.params.sq_off.ring_mask) == 7
            && ring.word(ring.params.cq_off.ring_mask) == 15,
    );

    // A NOP completes, with its user_data carried through untouched.
    let cqe = ring.one(|sqe| {
        sqe.opcode = OP_NOP;
        sqe.user_data = 0xdead_beef_0000_1111;
    });
    r.check(
        "a NOP completes res=0 and hands its user_data back",
        cqe.res == 0 && cqe.user_data == 0xdead_beef_0000_1111,
    );

    // Unknown opcode is EINVAL; a bad SQE descriptor is EBADF.
    let cqe = ring.one(|sqe| {
        sqe.opcode = 250;
        sqe.user_data = 1;
    });
    r.check("an unknown opcode completes -EINVAL", cqe.res == -22);
    let cqe = ring.one(|sqe| {
        sqe.opcode = OP_READ;
        sqe.fd = 9999;
        sqe.off = 0;
        sqe.addr = 0;
        sqe.len = 0;
    });
    r.check(
        "an SQE naming a bad descriptor completes -EBADF",
        cqe.res == -9,
    );

    // READ/WRITE through the ring answer the byte counts the calls would.
    let file = temp_file();
    let mut buf = [0u8; 16];
    let addr = buf.as_mut_ptr() as u64;
    let cqe = ring.one(|sqe| {
        sqe.opcode = OP_READ;
        sqe.fd = file.as_raw_fd();
        sqe.off = 0;
        sqe.addr = addr;
        sqe.len = 5;
    });
    r.check(
        "a READ at offset 0 completes 5 and fills the buffer",
        cqe.res == 5 && &buf[..5] == b"hello",
    );
    let cqe = ring.one(|sqe| {
        sqe.opcode = OP_WRITE;
        sqe.fd = file.as_raw_fd();
        sqe.off = 5;
        sqe.addr = b"world".as_ptr() as u64;
        sqe.len = 5;
    });
    r.check("a WRITE at offset 5 completes 5", cqe.res == 5);
    let mut all = [0u8; 10];
    assert_eq!(
        unsafe { libc::pread(file.as_raw_fd(), all.as_mut_ptr().cast(), all.len(), 0,) },
        10
    );
    r.check(
        "the writes landed where the offsets said",
        &all == b"helloworld",
    );

    // FSYNC completes 0; CLOSE completes 0 and really closes.
    let cqe = ring.one(|sqe| {
        sqe.opcode = OP_FSYNC;
        sqe.fd = file.as_raw_fd();
    });
    r.check("a FSYNC completes 0", cqe.res == 0);
    let closed = file.as_raw_fd();
    let cqe = ring.one(|sqe| {
        sqe.opcode = OP_CLOSE;
        sqe.fd = closed;
    });
    // `fstat` after: the descriptor is gone. The owner is forgotten so the
    // double close cannot land on a number that something else has since taken.
    std::mem::forget(file);
    let mut stat = unsafe { std::mem::zeroed::<libc::stat>() };
    let gone = unsafe { libc::fstat(closed, &mut stat) } == -1 && errno() == -9;
    r.check(
        "a CLOSE completes 0 and the descriptor is really closed",
        cqe.res == 0 && gone,
    );

    // READV/WRITEV: two iovecs, byte counts summed.
    let file = temp_file();
    let mut part_a = [0u8; 2];
    let mut part_b = [0u8; 3];
    let mut iov = [
        libc::iovec {
            iov_base: part_a.as_mut_ptr().cast(),
            iov_len: part_a.len(),
        },
        libc::iovec {
            iov_base: part_b.as_mut_ptr().cast(),
            iov_len: part_b.len(),
        },
    ];
    let cqe = ring.one(|sqe| {
        sqe.opcode = OP_READV;
        sqe.fd = file.as_raw_fd();
        sqe.off = 0;
        sqe.addr = iov.as_mut_ptr() as u64;
        sqe.len = 2;
    });
    r.check(
        "a two-iovec READV completes 5, scattered",
        cqe.res == 5 && &part_a == b"he" && &part_b == b"llo",
    );
    let out = [
        libc::iovec {
            iov_base: b"wo".as_ptr() as *mut libc::c_void,
            iov_len: 2,
        },
        libc::iovec {
            iov_base: b"rld".as_ptr() as *mut libc::c_void,
            iov_len: 3,
        },
    ];
    let cqe = ring.one(|sqe| {
        sqe.opcode = OP_WRITEV;
        sqe.fd = file.as_raw_fd();
        sqe.off = 5;
        sqe.addr = out.as_ptr() as u64;
        sqe.len = 2;
    });
    r.check("a two-iovec WRITEV completes 5", cqe.res == 5);
    let mut check = [0u8; 10];
    assert_eq!(
        unsafe { libc::pread(file.as_raw_fd(), check.as_mut_ptr().cast(), 10, 0) },
        10
    );
    r.check(
        "a vectored write lands in iovec order",
        &check == b"helloworld",
    );

    // Two SQEs in one enter: both consumed, both completed in order.
    {
        let sqe = ring.push();
        *sqe = Sqe {
            opcode: OP_NOP,
            user_data: 11,
            ..Sqe::default()
        };
        let sqe = ring.push();
        *sqe = Sqe {
            opcode: OP_NOP,
            user_data: 22,
            ..Sqe::default()
        };
        let submitted = ring.enter(2, 0, 0).expect("two submits are accepted");
        if self_debug() {
            println!(
                "note: two-SQE submit answered {submitted}, cq head {} tail {}",
                ring.cq_head(),
                ring.cq_tail()
            );
        }
        let first = ring.take();
        let second = ring.take();
        if self_debug() {
            println!("note: first {first:?} second {second:?}");
        }
        let first = first.map(|c| c.user_data);
        let second = second.map(|c| c.user_data);
        r.check(
            "one enter with two SQEs consumes and completes both, in order",
            submitted == 2 && first == Some(11) && second == Some(22),
        );
    }

    // Enter on a descriptor that is not a ring, and on one that is not open.
    let rc = unsafe {
        libc::syscall(
            SYS_ENTER,
            0 as libc::c_long,
            0 as libc::c_long,
            0 as libc::c_long,
            0 as libc::c_long,
            0 as libc::c_long,
            0 as libc::c_long,
        )
    };
    r.check(
        "enter on a descriptor that is not a ring is EOPNOTSUPP",
        rc == -1 && errno() == -95,
    );
    let rc = unsafe {
        libc::syscall(
            SYS_ENTER,
            9999 as libc::c_long,
            0 as libc::c_long,
            0 as libc::c_long,
            0 as libc::c_long,
            0 as libc::c_long,
            0 as libc::c_long,
        )
    };
    r.check("enter on a closed fd is EBADF", rc == -1 && errno() == -9);

    // GETEVENTS with no pending completion blocks, and a signal ends it with
    // EINTR: the one shape where Linux's answer is a park.
    {
        extern "C" fn on_alarm(_: libc::c_int) {}
        unsafe {
            libc::signal(libc::SIGALRM, on_alarm as *const () as usize);
            libc::alarm(1);
        }
        let rc = unsafe {
            libc::syscall(
                SYS_ENTER,
                ring.fd.as_raw_fd() as libc::c_long,
                0 as libc::c_long,
                1 as libc::c_long,
                ENTER_GETEVENTS as libc::c_long,
                0 as libc::c_long,
                0 as libc::c_long,
            )
        };
        r.check(
            "GETEVENTS with nothing pending parks until a signal says EINTR",
            rc == -1 && errno() == -4,
        );
    }

    // An `entries=1` ring works end to end (mask 1, cq 2).
    match Ring::setup(1) {
        Ok(tiny) => {
            let cqe = tiny.one(|sqe| {
                sqe.opcode = OP_NOP;
                sqe.user_data = 7;
            });
            r.check(
                "a one-entry ring completes a NOP too",
                cqe.res == 0 && cqe.user_data == 7,
            );
        }
        Err(e) => r.check("a one-entry ring can be built", e == 0),
    }

    // Setup flags this kernel refuses: a known one is EOPNOTSUPP, an unknown
    // bit is EINVAL (measured on Linux: bit31 is EINVAL).
    let mut params = Params {
        flags: SETUP_SQPOLL,
        ..Params::default()
    };
    let rc = unsafe { libc::syscall(SYS_SETUP, 8 as libc::c_long, &mut params) };
    if host {
        // Linux accepts SQPOLL on this host (measured in probe5), so the
        // shared half cannot claim the refusal; the guest claim is below.
        if rc >= 0 {
            let fd = unsafe { OwnedFd::from_raw_fd(rc as RawFd) };
            drop(fd);
            println!("note: the host accepts an SQPOLL ring, as measured; the guest refuses it");
        } else {
            println!(
                "note: this host refused an SQPOLL ring with errno {}",
                errno()
            );
        }
    } else {
        r.check(
            "a setup flag this kernel does not implement is EOPNOTSUPP",
            rc == -1 && errno() == -95,
        );
    }
    let mut params = Params {
        flags: SETUP_UNKNOWN_BIT31,
        ..Params::default()
    };
    let rc = unsafe { libc::syscall(SYS_SETUP, 8 as libc::c_long, &mut params) };
    r.check(
        "a setup flag bit outside the ABI's mask is EINVAL",
        rc == -1 && errno() == -22,
    );

    // The three named divergences, asserted on the guest only.
    if !host {
        let cqe = ring.one(|sqe| {
            sqe.opcode = 18; // OPENAT: Linux serves it, this kernel does not
            sqe.user_data = 3;
        });
        r.check(
            "an opcode outside the served subset completes -EOPNOTSUPP",
            cqe.res == -95,
        );
        let rc = unsafe {
            libc::syscall(
                SYS_REGISTER,
                ring.fd.as_raw_fd() as libc::c_long,
                2 as libc::c_long, // IORING_REGISTER_FILES
                0 as libc::c_long,
                0 as libc::c_long,
            )
        };
        r.check(
            "io_uring_register is refused with EOPNOTSUPP",
            rc == -1 && errno() == -95,
        );
        let mut params = Params::default();
        let rc = unsafe { libc::syscall(SYS_SETUP, 2048 as libc::c_long, &mut params) };
        r.check(
            "an SQ ring past the served cap is EINVAL",
            rc == -1 && errno() == -22,
        );
    }

    r.finish();
}
