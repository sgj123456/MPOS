//! t-aio - the legacy **POSIX AIO** family: `io_setup`, `io_destroy`,
//! `io_submit`, `io_getevents`.
//!
//! **This is not `io_uring`.** That is a ring of shared memory (number 425,
//! served since item 115); this is a context handle from `io_setup`, a
//! `struct iocb` per request, and a `struct io_event` per completion. All four
//! were undeclared, so a libaio caller heard `ENOSYS` from the first of them.
//!
//! Measured on Linux 7.3.4 (`build/scratch/opathmmap/{aio,aio2,aio3}.c`) before
//! any claim:
//!
//! | what | Linux |
//! |---|---|
//! | `io_setup(8, &ctx)` | `0`, and the handle is written to `*ctx` |
//! | `io_setup(0, …)` | `EINVAL` |
//! | `io_setup(4, NULL)` | `EFAULT` |
//! | `io_destroy` a live handle | `0`; a second time `EINVAL` |
//! | `io_submit` a **PREAD** | `1`, and `io_getevents` reports the bytes read |
//! | `io_submit` a **PWRITE** | `1` |
//! | `FSYNC`, `FDSYNC`, `POLL`, `NOOP`, unknown opcodes | **`EINVAL`** |
//! | a bad `aio_fildes` | `EBADF` |
//! | a bad context on every door | `EINVAL` |
//! | `io_getevents` with `min_nr` > what is queued | returns what **is** queued |
//!
//! **The opcode row is the one that corrected a draft.** A kernel with a
//! synchronous block layer can plausibly "do" `FSYNC` and `NOOP`, and the first
//! version served them - but Linux answers `EINVAL` to both, so serving them
//! would have been a *success* for a call the reference kernel refuses. Only
//! `PREAD` and `PWRITE` are served here, which is what the measurement says.
//!
//! **The synchronous scope is a performance shape, not an ABI one.** Each
//! `iocb` is executed inside `io_submit`, so it may block where Linux's would
//! not; the sequence a caller observes - submit count, then event count, then
//! results - is the same. That is the same scope §6.32 records for `io_uring`
//! and for the same reason.
//!
//! `struct iocb`'s field offsets are measured rather than read off the header:
//! `aio_key` and `aio_rw_flags` share the word at 8 on a little-endian machine,
//! so `aio_lio_opcode` sits at **16** and `aio_fildes` at **20** — a draft that
//! used the field order read the opcode out of `aio_rw_flags` and made every
//! request an unknown opcode.
//!
//! Numbers from libc's table; `rustix` binds no `io_*` call.

use mpos_user_tests::Report;

/// `struct iocb`, 64 bytes, with the **measured** offsets.
#[repr(C)]
struct Iocb {
    data: u64,
    key: u32,
    rw_flags: u32,
    opcode: u16,
    reqprio: i16,
    fildes: u32,
    buf: u64,
    nbytes: u64,
    offset: i64,
    reserved2: u64,
    flags: u32,
    resfd: u32,
}

/// `struct io_event`, 32 bytes.
#[repr(C)]
#[derive(Default, Clone, Copy)]
struct IoEvent {
    data: u64,
    obj: u64,
    res: i64,
    res2: i64,
}

const IOCB_CMD_PREAD: u16 = 0;
const IOCB_CMD_PWRITE: u16 = 1;

fn errno() -> Option<i32> {
    std::io::Error::last_os_error().raw_os_error()
}
fn refused(rc: isize, want: i32) -> bool {
    rc == -1 && errno() == Some(want)
}

fn io_setup(nr: usize) -> (isize, u64) {
    let mut ctx: u64 = 0;
    // SAFETY: a `u64` on this test's own stack.
    let rc = unsafe { libc::syscall(libc::SYS_io_setup, nr, &mut ctx as *mut u64) } as isize;
    (rc, ctx)
}
fn io_destroy(ctx: u64) -> isize {
    // SAFETY: `io_destroy` takes the handle by value.
    (unsafe { libc::syscall(libc::SYS_io_destroy, ctx) }) as isize
}
fn io_submit(ctx: u64, iocbs: &mut [*mut Iocb]) -> isize {
    // SAFETY: the array is this test's, and its length is passed beside it.
    (unsafe { libc::syscall(libc::SYS_io_submit, ctx, iocbs.len(), iocbs.as_mut_ptr()) }) as isize
}
fn io_getevents(ctx: u64, min_nr: usize, events: &mut [IoEvent]) -> isize {
    // SAFETY: the buffer is this test's and its length is passed beside it.
    (unsafe {
        libc::syscall(
            libc::SYS_io_getevents,
            ctx,
            min_nr,
            events.len(),
            events.as_mut_ptr(),
            0usize,
        )
    }) as isize
}

fn main() {
    println!("t-aio");
    let mut r = Report::new();

    // === the context lifecycle =============================================
    let (rc, ctx) = io_setup(8);
    r.check(
        &format!("io_setup(8) is served ({rc}, ctx {ctx})"),
        rc == 0 && ctx != 0,
    );
    if rc != 0 || ctx == 0 {
        r.finish();
    }
    r.check(
        "io_setup(0) is EINVAL",
        io_setup(0).0 == -1 && errno() == Some(libc::EINVAL),
    );
    r.check("io_setup(4, NULL) is EFAULT", {
        // SAFETY: a NULL pointer is the subject of the claim.
        let a = unsafe { libc::syscall(libc::SYS_io_setup, 4usize, 0usize) } as isize;
        refused(a, libc::EFAULT)
    });

    // === the two served opcodes, on a real file ============================
    let path = std::ffi::CString::new("/tmp/t-aio.bin").unwrap();
    // SAFETY: a path this test owns.
    let fd = unsafe {
        libc::open(
            path.as_ptr(),
            libc::O_RDWR | libc::O_CREAT | libc::O_TRUNC,
            0o644,
        )
    };
    r.check(&format!("a scratch file is opened ({fd})"), fd >= 0);
    if fd >= 0 {
        // SAFETY: `fd` is the file just opened and the buffer is this test's.
        let wrote = unsafe { libc::write(fd, b"ABCDEFGHIJ".as_ptr().cast::<libc::c_void>(), 10) };
        r.check(&format!("and holds ten bytes ({wrote})"), wrote == 10);

        // A PREAD of five bytes at offset two: the payload must arrive.
        let mut buf = [0u8; 16];
        let mut cb = Iocb {
            data: 0xDEAD_BEEF,
            key: 0,
            rw_flags: 0,
            opcode: IOCB_CMD_PREAD,
            reqprio: 0,
            fildes: fd as u32,
            buf: buf.as_mut_ptr() as u64,
            nbytes: 5,
            offset: 2,
            reserved2: 0,
            flags: 0,
            resfd: 0,
        };
        let mut batch = [&mut cb as *mut Iocb];
        // **Bound once**: `format!` and the assertion both calling
        // `io_submit` submits twice, which is why a first run of this pin saw
        // `io_getevents` answer 2 - the probe was queueing two requests, not
        // reading one twice.
        let submitted = io_submit(ctx, &mut batch);
        r.check(
            &format!("io_submit of a PREAD is served ({submitted})"),
            submitted == 1,
        );
        let mut events = [IoEvent::default(); 4];
        let n = io_getevents(ctx, 0, &mut events);
        r.check(&format!("io_getevents reports it ({n})"), n == 1);
        r.check(
            &format!("with the byte count in res ({})", events[0].res),
            events[0].res == 5,
        );
        // **The `aio_data` field comes back verbatim** - it is the caller's
        // correlation token and the reason `io_event` carries it at all.
        r.check(
            &format!("and the caller's aio_data returned ({:#x})", events[0].data),
            events[0].data == 0xDEAD_BEEF,
        );
        r.check(
            &format!("and the bytes are the file's ({:?})", &buf[..5]),
            &buf[..5] == b"CDEFG",
        );

        // A PWRITE, which is the other served opcode.
        let mut wbuf = *b"xyz\0\0";
        let mut wcb = Iocb {
            data: 7,
            key: 0,
            rw_flags: 0,
            opcode: IOCB_CMD_PWRITE,
            reqprio: 0,
            fildes: fd as u32,
            buf: wbuf.as_mut_ptr() as u64,
            nbytes: 3,
            offset: 0,
            reserved2: 0,
            flags: 0,
            resfd: 0,
        };
        let mut wbatch = [&mut wcb as *mut Iocb];
        let wsubmitted = io_submit(ctx, &mut wbatch);
        r.check(
            &format!("io_submit of a PWRITE is served ({wsubmitted})"),
            wsubmitted == 1,
        );
        let mut wevents = [IoEvent::default(); 2];
        r.check(
            "and its completion says three bytes",
            io_getevents(ctx, 0, &mut wevents) == 1 && wevents[0].res == 3,
        );

        // === the refused opcodes ==========================================
        //
        // The measurement that corrected a draft: Linux answers `EINVAL` for
        // these, so this kernel must too - serving them would be a success for
        // a call the reference kernel refuses.
        // **`PREADV` is not in this list**, and that is a correction: Linux
        // *serves* it when `aio_buf` points at a real `struct iovec`
        // (`build/scratch/opathmmap/aio4.c`: submit 1, `res` 4, buffer filled).
        // The first draft expected `EINVAL` from the header's field order rather
        // than from a measurement. This door does not import the `iov` array, so
        // it refuses `PREADV` - which is a **named gap** in the module header,
        // not a claim that Linux refuses it.
        for (label, opcode) in [
            ("FSYNC", 2u16),
            ("FDSYNC", 3),
            ("POLL", 5),
            ("NOOP", 6),
            ("unknown 99", 99),
        ] {
            let mut b = Iocb {
                data: 0,
                key: 0,
                rw_flags: 0,
                opcode,
                reqprio: 0,
                fildes: fd as u32,
                buf: buf.as_mut_ptr() as u64,
                nbytes: 4,
                offset: 0,
                reserved2: 0,
                flags: 0,
                resfd: 0,
            };
            let mut batch = [&mut b as *mut Iocb];
            r.check(
                &format!("opcode {label} is EINVAL"),
                refused(io_submit(ctx, &mut batch), libc::EINVAL),
            );
        }

        // **A bad descriptor fails the `io_submit` itself.** Measured
        // (`aio4.c`): `io_submit` with `aio_fildes = 9999` answers `-1`/`EBADF`
        // and queues no event. A first draft queued a completion carrying
        // `-EBADF`, which is the shape for a request that failed *after* being
        // accepted - and it tells a caller work happened when it did not.
        let mut bad = Iocb {
            data: 3,
            key: 0,
            rw_flags: 0,
            opcode: IOCB_CMD_PREAD,
            reqprio: 0,
            fildes: 9999,
            buf: buf.as_mut_ptr() as u64,
            nbytes: 4,
            offset: 0,
            reserved2: 0,
            flags: 0,
            resfd: 0,
        };
        let mut badbatch = [&mut bad as *mut Iocb];
        let bad_submitted = io_submit(ctx, &mut badbatch);
        r.check(
            &format!("a bad fildes fails the submit with EBADF ({bad_submitted})"),
            refused(bad_submitted, libc::EBADF),
        );

        // SAFETY: the descriptor this block opened.
        unsafe { libc::close(fd) };
    }
    let _ = std::fs::remove_file("/tmp/t-aio.bin");

    // === the refusals on every door ========================================
    r.check(
        "io_destroy(0) is EINVAL",
        refused(io_destroy(0), libc::EINVAL),
    );
    r.check("io_getevents(0) is EINVAL", {
        let mut e = [IoEvent::default(); 1];
        refused(io_getevents(0, 0, &mut e), libc::EINVAL)
    });
    r.check("io_submit(0) is EINVAL", {
        // SAFETY: a NULL array with a zero count, so nothing is read.
        let a = unsafe { libc::syscall(libc::SYS_io_submit, 0u64, 0usize, 0usize) } as isize;
        refused(a, libc::EINVAL)
    });
    r.check("io_destroy of the live handle is 0", io_destroy(ctx) == 0);
    // **The handle is gone**: asking again is EINVAL, not a silent success -
    // which is the property a table keyed on a recyclable number gets wrong
    // (§6.90's class), and why handles here are never reused.
    r.check(
        "io_destroy a second time is EINVAL",
        refused(io_destroy(ctx), libc::EINVAL),
    );

    r.finish()
}
