//! t-fifo — the node `mknod(S_IFIFO)` makes, and the **rendezvous** its
//! `open(2)` runs.
//!
//! A FIFO is the one node in this filesystem whose *open* is a conversation:
//! a reader and a writer meet at a name neither of them owns, and Linux parks
//! each until the other shows up. Before this pass `mknodat(S_IFIFO)` made a
//! regular file here, so a name a reader met end-of-file at once could not be
//! told from an empty channel — the wrong *shape*, worse than a refusal. The
//! whole contract was measured on a current host through `syscall(2)`
//! (`.workbuddy/probe_fifo.c`, Linux 7.2.9) before any of it was built, and
//! `docs/uapi/` §5.2 carries the measurement and the design.
//!
//! What is claimed here, in the order the sections run:
//!
//!   * the **node**: `S_IFIFO` to `stat`, size 0, one name; a second `mknodat`
//!     on the name is `EEXIST` (`t-mknod` owns that half);
//!   * the **name at an open description**: `fstat` of an opened FIFO reports
//!     `S_IFIFO` too, and `lseek` on it is `ESPIPE`;
//!   * the **rendezvous**: `open(O_RDONLY)` with no writer and `open(O_WRONLY)`
//!     with no reader both **park** until a peer opens the other end — measured
//!     without a clock by forking the opener and watching whether its report
//!     arrives, then releasing it the ABI's way; `open(O_RDWR)` does not park;
//!     `O_RDONLY|O_NONBLOCK` returns at once; `O_WRONLY|O_NONBLOCK` with no
//!     reader is **`ENXIO`**;
//!   * what a **reader** and a **writer** each see: end-of-stream with no
//!     writer, `EAGAIN` with a live writer that wrote nothing, the bytes after a
//!     write, `EPIPE` once the last reader closes;
//!   * the **buffer is discarded** when both counts reach zero, so the next open
//!     starts empty;
//!   * the **file-shaped calls**: `ftruncate` `EINVAL`, `fallocate` `ESPIPE`,
//!     `truncate(name)` `EINVAL`, `execve` `EACCES`, `poll` reporting the queue.
//!
//! # One residual, named rather than hidden
//!
//! A **fatal** signal (`SIGKILL`) delivered while a task is parked in a FIFO's
//! `open` ends the task inside the wait, so the read/write count that open took
//! is never given back and the FIFO remembers a peer that never arrived. Linux
//! rolls the count back on `ERESTARTSYS`; doing the same here wants the pending
//! open owned by something the exit pass drops, which an `open` only becomes
//! after it returns. `docs/uapi/` §5.2 names it. The rendezvous claims below
//! therefore *release* every parked opener rather than killing it.
//!
//! # Why the raw syscall
//!
//! Every open here goes through `libc::syscall(libc::SYS_openat, …)`: a libc's
//! `open` is allowed to answer questions of its own (glibc's `O_LARGEFILE`,
//! its own `errno` bookkeeping), and the **flag word is the subject** — the
//! whole point of `O_NONBLOCK` and `O_RDWR` here is the kernel's answer to
//! that word.

use mpos_user_tests::Report;

/// The name every scenario starts from.
const PATH: &str = "/tmp/t-fifo";

const AT_FDCWD: libc::c_long = -100;
const S_IFIFO: u32 = 0o010000;

fn c(s: &str) -> std::ffi::CString {
    std::ffi::CString::new(s).unwrap()
}

fn errno() -> i32 {
    std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
}

/// `mknodat(AT_FDCWD, path, mode, dev)`, answering `(result, errno)`.
fn mknodat(mode: u32, dev: u64, path: &str) -> (isize, i32) {
    let r = unsafe {
        libc::syscall(
            libc::SYS_mknodat,
            AT_FDCWD,
            c(path).as_ptr(),
            mode as libc::c_long,
            dev as libc::c_long,
        )
    } as isize;
    (r, if r < 0 { errno() } else { 0 })
}

/// `openat(AT_FDCWD, path, flags, 0644)`, answering `(fd, errno)`.
fn openat(path: &str, flags: i32) -> (isize, i32) {
    let r = unsafe {
        libc::syscall(
            libc::SYS_openat,
            AT_FDCWD,
            c(path).as_ptr(),
            flags as libc::c_long,
            0o644 as libc::c_long,
        )
    } as isize;
    (r, if r < 0 { errno() } else { 0 })
}

fn close(fd: i32) {
    unsafe { libc::close(fd) };
}

fn write_fd(fd: i32, buf: &[u8]) -> (isize, i32) {
    let r = unsafe {
        libc::syscall(
            libc::SYS_write,
            fd as libc::c_long,
            buf.as_ptr(),
            buf.len() as libc::c_long,
        )
    } as isize;
    (r, if r < 0 { errno() } else { 0 })
}

fn read_fd(fd: i32, buf: &mut [u8]) -> (isize, i32) {
    let r = unsafe {
        libc::syscall(
            libc::SYS_read,
            fd as libc::c_long,
            buf.as_mut_ptr(),
            buf.len() as libc::c_long,
        )
    } as isize;
    (r, if r < 0 { errno() } else { 0 })
}

/// `newfstatat(AT_FDCWD, path, &st, 0)` for the name, `fstat` for a description.
fn stat_name(path: &str) -> Option<libc::stat> {
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    let r = unsafe {
        libc::syscall(
            libc::SYS_newfstatat,
            AT_FDCWD,
            c(path).as_ptr(),
            &mut st as *mut libc::stat,
            0 as libc::c_long,
        )
    };
    if r < 0 { None } else { Some(st) }
}

fn fstat_fd(fd: i32) -> Option<libc::stat> {
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    let r = unsafe {
        libc::syscall(
            libc::SYS_fstat,
            fd as libc::c_long,
            &mut st as *mut libc::stat,
        )
    };
    if r < 0 { None } else { Some(st) }
}

fn lseek_fd(fd: i32) -> (isize, i32) {
    let r = unsafe {
        libc::syscall(
            libc::SYS_lseek,
            fd as libc::c_long,
            0 as libc::c_long,
            0 as libc::c_long,
        )
    } as isize;
    (r, if r < 0 { errno() } else { 0 })
}

fn ftruncate_fd(fd: i32, len: u64) -> (isize, i32) {
    let r = unsafe { libc::syscall(libc::SYS_ftruncate, fd as libc::c_long, len as libc::c_long) }
        as isize;
    (r, if r < 0 { errno() } else { 0 })
}

fn fallocate_fd(fd: i32) -> (isize, i32) {
    let r = unsafe {
        libc::syscall(
            libc::SYS_fallocate,
            fd as libc::c_long,
            0 as libc::c_long,
            0 as libc::c_long,
            1 as libc::c_long,
        )
    } as isize;
    (r, if r < 0 { errno() } else { 0 })
}

fn truncate_name(path: &str, len: u64) -> (isize, i32) {
    let r = unsafe { libc::syscall(libc::SYS_truncate, c(path).as_ptr(), len as libc::c_long) }
        as isize;
    (r, if r < 0 { errno() } else { 0 })
}

fn poll_fd(fd: i32, events: i16, timeout_ms: i32) -> i16 {
    let mut p = libc::pollfd {
        fd,
        events,
        revents: 0,
    };
    let n = unsafe { libc::poll(&mut p, 1, timeout_ms) };
    if n <= 0 { 0 } else { p.revents }
}

/// Fork a child that opens `flags`; if it is a reader, `do_read` makes it take
/// one read once the open returns. The child reports `[fd, errno, nread]` (four
/// words) and then any bytes it read, down a pipe, and exits *after* its fd is
/// closed - so the scenario releases it and never kills a parked opener (see
/// the header's residual). Returns `(kid, read_end_of_the_report_pipe)`.
fn spawn_opener(flags: i32, do_read: bool) -> (libc::pid_t, i32) {
    let mut p = [0i32; 2];
    assert_eq!(unsafe { libc::pipe(p.as_mut_ptr()) }, 0);
    let (r, w) = (p[0], p[1]);
    let kid = unsafe { libc::fork() };
    if kid == 0 {
        unsafe { libc::close(r) };
        let (fd, e) = openat(PATH, flags);
        let mut got = [0u8; 8];
        let mut nread: isize = -1;
        if fd >= 0 && do_read {
            let (n, _) = read_fd(fd as i32, &mut got);
            nread = n;
        }
        let rec = [fd as i32, e, nread as i32, 0];
        unsafe {
            libc::write(w, rec.as_ptr() as *const libc::c_void, 16);
            if nread > 0 {
                libc::write(w, got.as_ptr() as *const libc::c_void, nread as usize);
            }
            if fd >= 0 {
                libc::close(fd as i32);
            }
            libc::_exit(0);
        }
    }
    unsafe { libc::close(w) };
    (kid, r)
}

/// Whether the report pipe has a whole record within `limit_ms`. `None` means
/// the child is still parked - which is how "this open blocks" is measured
/// without a clock inside the kernel.
fn report_within(r: i32, limit_ms: i32) -> Option<[i32; 4]> {
    if poll_fd(r, libc::POLLIN, limit_ms) == 0 {
        return None;
    }
    let mut rec = [0i32; 4];
    let n = unsafe { libc::read(r, rec.as_mut_ptr() as *mut libc::c_void, 16) };
    if n == 16 { Some(rec) } else { None }
}

fn main() {
    println!("t-fifo");
    let mut r = Report::new();
    // A write to a FIFO whose last reader has gone is `EPIPE`, and the default
    // disposition of `SIGPIPE` would kill this program before it could read the
    // errno. Ignored, so the answer is the errno the claim is about.
    unsafe { libc::signal(libc::SIGPIPE, libc::SIG_IGN) };
    let _ = std::fs::remove_file(PATH);

    // ---- the node ----------------------------------------------------------
    let (made, e) = mknodat(S_IFIFO | 0o644, 0, PATH);
    r.check(
        &format!("mknodat(S_IFIFO) makes the name (got {made}, errno {e})"),
        made == 0,
    );
    let st = stat_name(PATH);
    r.check(
        "and stat reports S_IFIFO at it",
        st.as_ref()
            .is_some_and(|s| s.st_mode as u32 & 0o170000 == S_IFIFO),
    );
    r.check(
        "with size 0 and one name",
        st.as_ref()
            .is_some_and(|s| s.st_size == 0 && s.st_nlink == 1),
    );
    let (made, e) = mknodat(S_IFIFO | 0o600, 0, PATH);
    r.check(
        &format!("a second mknodat on the name is EEXIST (got {made}, errno {e})"),
        made == -1 && e == libc::EEXIST,
    );

    // ---- the name at an opened description ---------------------------------
    //
    // Open read-write: it is its own peer, so it never parks - which lets the
    // description's own answers be read without a second process.
    let (fd, e) = openat(PATH, libc::O_RDWR | libc::O_NONBLOCK);
    r.check(
        &format!("open(O_RDWR) does not park (got {fd}, errno {e})"),
        fd >= 0,
    );
    if fd >= 0 {
        let fd = fd as i32;
        let ost = fstat_fd(fd);
        r.check(
            "and fstat of the description reports S_IFIFO too",
            ost.as_ref()
                .is_some_and(|s| s.st_mode as u32 & 0o170000 == S_IFIFO),
        );
        let (seek, e) = lseek_fd(fd);
        r.check(
            &format!("lseek on it is ESPIPE (got {seek}, errno {e})"),
            seek == -1 && e == libc::ESPIPE,
        );
        close(fd);
    }

    // ---- the rendezvous: a reader parks until a writer arrives -------------
    let (kid, rep) = spawn_opener(libc::O_RDONLY, true);
    r.check(
        "open(O_RDONLY) with no writer parks",
        report_within(rep, 150).is_none(),
    );
    // Release it the ABI's way: the writer arrives, and the parked reader
    // counts as a reader, so the writer's own open returns at once.
    let (w, e) = openat(PATH, libc::O_WRONLY);
    r.check(
        &format!("a writer's arrival returns at once (got {w}, errno {e})"),
        w >= 0,
    );
    if w >= 0 {
        let (n, _) = write_fd(w as i32, b"hey");
        r.check("and it can write straight away", n == 3);
    }
    let reader = report_within(rep, 1000);
    r.check("the parked reader's open returned", reader.is_some());
    r.check(
        "and it read the writer's bytes",
        reader
            .as_ref()
            .is_some_and(|rec| rec[0] >= 0 && rec[2] == 3),
    );
    if w >= 0 {
        close(w as i32);
    }
    unsafe { libc::close(rep) };
    unsafe { libc::waitpid(kid, std::ptr::null_mut(), 0) };

    // ---- the rendezvous: a writer parks until a reader arrives -------------
    let (kid, rep) = spawn_opener(libc::O_WRONLY, false);
    r.check(
        "open(O_WRONLY) with no reader parks",
        report_within(rep, 150).is_none(),
    );
    let (rd, e) = openat(PATH, libc::O_RDONLY);
    r.check(
        &format!("a reader's arrival returns at once (got {rd}, errno {e})"),
        rd >= 0,
    );
    let writer = report_within(rep, 1000);
    r.check(
        "the parked writer's open returned",
        writer.as_ref().is_some_and(|rec| rec[0] >= 0),
    );
    if rd >= 0 {
        close(rd as i32);
    }
    unsafe { libc::close(rep) };
    unsafe { libc::waitpid(kid, std::ptr::null_mut(), 0) };

    // ---- the rendezvous: the doors that do not park ------------------------
    //
    // Each runs on a FIFO with no open ends now (both scenarios released and
    // closed theirs), so the answers are about *this* open alone.
    let (fd, e) = openat(PATH, libc::O_RDONLY | libc::O_NONBLOCK);
    r.check(
        &format!("open(O_RDONLY|O_NONBLOCK) returns with no writer (got {fd}, errno {e})"),
        fd >= 0,
    );
    if fd >= 0 {
        close(fd as i32);
    }
    let (fd, e) = openat(PATH, libc::O_WRONLY | libc::O_NONBLOCK);
    r.check(
        &format!("open(O_WRONLY|O_NONBLOCK) with no reader is ENXIO (got {fd}, errno {e})"),
        fd == -1 && e == libc::ENXIO,
    );

    // ---- what a reader sees ------------------------------------------------
    let (rd, _) = openat(PATH, libc::O_RDONLY | libc::O_NONBLOCK);
    let rd = rd as i32;
    let mut buf = [0u8; 32];
    let (n, _) = read_fd(rd, &mut buf);
    r.check(
        "a nonblocking reader with no writer at all reads 0 (end of stream)",
        n == 0,
    );
    let (wr, _) = openat(PATH, libc::O_WRONLY | libc::O_NONBLOCK);
    let wr = wr as i32;
    let (n, e) = read_fd(rd, &mut buf);
    r.check(
        &format!("with a live writer that wrote nothing it is EAGAIN (got {n}, errno {e})"),
        n == -1 && e == libc::EAGAIN,
    );
    let (n, _) = write_fd(wr, b"hello");
    r.check("the writer lands five bytes", n == 5);
    let (n, _) = read_fd(rd, &mut buf);
    r.check("and the reader takes them", n == 5 && &buf[..5] == b"hello");
    close(wr);
    let (n, _) = read_fd(rd, &mut buf);
    r.check(
        "once the last writer closes, the next read is 0 again",
        n == 0,
    );
    close(rd);

    // ---- what a writer sees: the last reader closing -----------------------
    let (rd, _) = openat(PATH, libc::O_RDONLY | libc::O_NONBLOCK);
    let (wr, _) = openat(PATH, libc::O_WRONLY | libc::O_NONBLOCK);
    close(rd as i32);
    let (n, e) = write_fd(wr as i32, b"x");
    r.check(
        &format!("a write after the last reader closes is EPIPE (got {n}, errno {e})"),
        n == -1 && e == libc::EPIPE,
    );
    close(wr as i32);

    // ---- the buffer is discarded when both counts reach zero ---------------
    //
    // Leave two bytes unread, close the reader and then the writer: both counts
    // reach zero with data still buffered, and the next open must start empty
    // rather than read a dead pipeline's leftovers.
    let (r5, _) = openat(PATH, libc::O_RDONLY | libc::O_NONBLOCK);
    let (w5, _) = openat(PATH, libc::O_WRONLY | libc::O_NONBLOCK);
    let (n, _) = write_fd(w5 as i32, b"xy");
    r.check("two bytes are buffered unread", n == 2);
    close(r5 as i32);
    close(w5 as i32);
    let (r6, _) = openat(PATH, libc::O_RDONLY | libc::O_NONBLOCK);
    let (n, _) = read_fd(r6 as i32, &mut buf);
    r.check(
        "reopening after both closed reads an empty buffer (0)",
        n == 0,
    );
    close(r6 as i32);

    // ---- the file-shaped calls on an opened FIFO ---------------------------
    let (fd, _) = openat(PATH, libc::O_RDWR | libc::O_NONBLOCK);
    if fd >= 0 {
        let fd = fd as i32;
        let (t, e) = ftruncate_fd(fd, 4);
        r.check(
            &format!("ftruncate on a FIFO is EINVAL (got {t}, errno {e})"),
            t == -1 && e == libc::EINVAL,
        );
        let (a, e) = fallocate_fd(fd);
        r.check(
            &format!("fallocate on a FIFO is ESPIPE (got {a}, errno {e})"),
            a == -1 && e == libc::ESPIPE,
        );
        close(fd);
    } else {
        r.check("a read-write FIFO opens for the file-shaped claims", false);
    }
    let (t, e) = truncate_name(PATH, 4);
    r.check(
        &format!("truncate(name) on a FIFO is EINVAL (got {t}, errno {e})"),
        t == -1 && e == libc::EINVAL,
    );
    // `execve` on a FIFO is EACCES: the name is not executable (0644), and
    // Linux refuses a non-regular file before it reads a byte. This kernel
    // answers the permission question first, so the claim holds for the same
    // reason and, like Linux, never reaches the FIFO's own doors.
    let name = c(PATH);
    let argv: [*const libc::c_char; 2] = [name.as_ptr(), std::ptr::null()];
    let envp: [*const libc::c_char; 1] = [std::ptr::null()];
    let (x, e) = unsafe {
        (
            libc::syscall(
                libc::SYS_execve,
                name.as_ptr(),
                argv.as_ptr(),
                envp.as_ptr(),
            ) as isize,
            errno(),
        )
    };
    r.check(
        &format!("execve of a FIFO is EACCES (got {x}, errno {e})"),
        x == -1 && e == libc::EACCES,
    );

    // ---- poll reports the queue --------------------------------------------
    let (rd, _) = openat(PATH, libc::O_RDONLY | libc::O_NONBLOCK);
    let (wr, _) = openat(PATH, libc::O_WRONLY | libc::O_NONBLOCK);
    let empty = poll_fd(rd as i32, libc::POLLIN, 0);
    r.check(
        "poll on an empty FIFO with a live writer reports nothing",
        empty & libc::POLLIN == 0,
    );
    let _ = write_fd(wr as i32, b"q");
    let full = poll_fd(rd as i32, libc::POLLIN, 0);
    r.check(
        "and POLLIN once the writer has put a byte in",
        full & libc::POLLIN != 0,
    );
    close(rd as i32);
    close(wr as i32);

    // ---- cleanup -----------------------------------------------------------
    let gone = std::fs::remove_file(PATH).is_ok();
    r.check("the name unlinks", gone);
    r.check(
        "and stat no longer answers for it",
        stat_name(PATH).is_none(),
    );

    r.finish()
}
