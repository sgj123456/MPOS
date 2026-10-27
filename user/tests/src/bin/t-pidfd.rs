//! t-pidfd - the process-descriptor family: `pidfd_open`, `pidfd_getfd`,
//! `pidfd_send_signal` and `waitid(P_PIDFD)`.
//!
//! A pid is a number a process can lose and somebody else can inherit; a pidfd
//! is the descriptor that names the task itself. This program pins the shape
//! Linux gives that descriptor, measured on a current host before the kernel's
//! side was written (`.workbuddy/probe_pidfd{,2,3,5,6,7}.c`,
//! `probe_waitid_pidfd_zombie.c`, `probe_pidfd_getfd{,2}.c`, Linux 7.2.11):
//!
//! * **the descriptor**: `F_GETFL` is `O_RDWR` (`0x2`), `FD_CLOEXEC` is set
//!   whatever the flags word said, and `O_NONBLOCK` is the only flag the door
//!   accepts - `O_CLOEXEC` alone is `EINVAL`, because the close-on-exec bit is
//!   not the caller's to ask for here;
//! * **the inode**: `fstat` reports `0700` with **no** `S_IF*` bits (an
//!   anonymous inode, not a character device), size 0 and one name;
//!   `/proc/self/fd/<n>` reads back `anon_inode:[pidfd]`; `read`/`pread`/
//!   `write` are `EINVAL` - even for a zero-length call - and `lseek` is
//!   `ESPIPE`;
//! * **readiness**: a live process is not readable, a dead-but-unreaped one is
//!   `POLLIN` without `POLLHUP`, and only the reap adds `POLLHUP`. The
//!   question is the **process's** death (the last thread leaving), not the
//!   leader's, which is why a leader that exits under a live worker is still
//!   not readable;
//! * **`waitid(P_PIDFD)`**: the descriptor resolves the child, and a
//!   descriptor whose task has been reaped is `ECHILD` rather than a wait for
//!   whatever took the slot. A dead child under `WSTOPPED` alone is `ECHILD`,
//!   not "nothing yet" - the exit class has to be asked for;
//! * **`pidfd_send_signal`**: the refusal order is flags, descriptor, record,
//!   signal number, existence, authority - which is not the order the
//!   arguments are written in. `PIDFD_SIGNAL_THREAD` (1) and
//!   `PIDFD_SIGNAL_THREAD_GROUP` (2) are the process, `PIDFD_SIGNAL_PROCESS_GROUP`
//!   (4) is the group the target *leads* (`ESRCH` otherwise, even while a
//!   worker keeps the group alive), and `1|2` is an invalid word;
//! * **the record**: a NULL `siginfo_t` becomes `SI_USER` naming the sender,
//!   a caller's negative `si_code` is carried through, a kernel-made code
//!   (`SI_USER`, `SI_TKILL`, any non-negative) is `EPERM` for another process
//!   but passes verbatim when the target is the caller's own group;
//! * **`pidfd_getfd`**: the refusal order is flags, pidfd, task, authority,
//!   target fd - so a non-zero flag word is `EINVAL` beside a bad pidfd, a
//!   zombie or reaped process is `ESRCH`, and a non-dumpable same-uid target
//!   is `EPERM` even when the target fd does not exist. The fetch is the same
//!   open file description (two fetches share one offset), the new number is
//!   the lowest free one, and it is `FD_CLOEXEC` whatever the flags said.
//!
//! # Why raw doors
//!
//! `rustix::process::pidfd_open` exists, but it takes a `PidfdFlags` bitflags
//! and returns an `OwnedFd`: the *flags word* this program is about - including
//! the words Linux must refuse - and the `siginfo_t` a send carries have no
//! binding to go through. `pidfd_send_signal` in rustix takes a signal only.
//! The calls are therefore `libc::syscall`, with the numbers from libc's own
//! per-machine table (424, 434 and 438 on both machines), and the record is read
//! back through `libc::siginfo_t`'s accessors rather than through offsets this
//! file would have to remember.
//!
//! The one record this program *makes* is written through [`Record`], whose
//! first three words and `sigchld` tail are the asm-generic `siginfo_t` both
//! machines use. musl's `libc::siginfo_t` exposes `si_pid`/`si_uid` only as
//! **read** accessors, so a caller-authored record has no settable field to
//! put them in; the struct below is that record, and the syscall takes its
//! address.
//!
//! # What is deliberately not here
//!
//! `clone3`'s `CLONE_PIDFD` is a named gap in `docs/ROADMAP.md`, not a claim:
//! it is not served, and a test cannot pin a call the kernel refuses with
//! `ENOSYS`. `process_madvise(2)` was the other gap when this program landed
//! and is served now; its pin is `t-process-madvise`, because the door is a
//! *memory* call with a fixture arena of its own, not a descriptor claim.

use mpos_user_tests::Report;
use rustix::thread::gettid;
use std::mem::size_of;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::time::Duration;

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

/// `pidfd_getfd(pidfd, targetfd, flags)`, answered as the new descriptor.
fn pidfd_getfd(pidfd: i32, target_fd: i32, flags: u32) -> Result<i32, i32> {
    let r = unsafe {
        libc::syscall(
            libc::SYS_pidfd_getfd,
            pidfd as libc::c_long,
            target_fd as libc::c_long,
            flags as libc::c_long,
        )
    };
    if r < 0 {
        Err(std::io::Error::last_os_error().raw_os_error().unwrap_or(0))
    } else {
        Ok(r as i32)
    }
}

/// The two descriptor numbers a getfd fixture reports, read as one small
/// atomic pipe write.
fn pipe_report(fd: i32, out: &mut [i32; 2]) -> bool {
    let n = unsafe { libc::read(fd, out.as_mut_ptr().cast(), 8) };
    n == 8
}

/// The `siginfo_t` a `pidfd_send_signal` may carry, as the asm-generic record
/// both machines use: `{signo, errno, code, pad, pid, uid, value}`, then the
/// union's tail. 128 bytes, so the kernel's own bounds check sees a whole
/// record rather than the 32 bytes the named fields occupy.
#[repr(C)]
#[derive(Clone, Copy)]
struct Record {
    signo: i32,
    errno: i32,
    code: i32,
    _pad: i32,
    pid: i32,
    uid: u32,
    value: i64,
    _tail: [u8; 96],
}

/// A record with the fields a caller can choose; the tail is zero.
fn record(signo: i32, code: i32, pid: i32, uid: u32, value: i32) -> Record {
    Record {
        signo,
        errno: 0,
        code,
        _pad: 0,
        pid,
        uid,
        value: value as i64,
        _tail: [0; 96],
    }
}

/// `pidfd_send_signal(fd, sig, info, flags)`: the one refusal or success.
fn pidfd_send_signal(fd: i32, sig: i32, info: Option<&Record>, flags: u32) -> Result<(), i32> {
    let uinfo = info.map_or(0, |r| r as *const Record as usize);
    let r = unsafe {
        libc::syscall(
            libc::SYS_pidfd_send_signal,
            fd as libc::c_long,
            sig as libc::c_long,
            uinfo as libc::c_long,
            flags as libc::c_long,
        )
    };
    if r < 0 {
        Err(std::io::Error::last_os_error().raw_os_error().unwrap_or(0))
    } else {
        Ok(())
    }
}

/// `waitid(idtype, id, info, options, NULL)` through the raw door, with the
/// caller's own record and a nullable pointer (Linux accepts a null record).
fn waitid_raw(idtype: u32, id: i64, info: *mut libc::siginfo_t, options: i32) -> Result<(), i32> {
    let r = unsafe {
        libc::syscall(
            libc::SYS_waitid,
            idtype as libc::c_long,
            id as libc::c_long,
            info as libc::c_long,
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

/// `waitid(P_PIDFD, ...)` with a borrowed record.
fn waitid_pidfd(fd: i32, info: Option<&mut libc::siginfo_t>, options: i32) -> Result<(), i32> {
    let ptr = info.map_or(core::ptr::null_mut(), |i| i as *mut libc::siginfo_t);
    waitid_raw(libc::P_PIDFD as u32, fd as i64, ptr, options)
}

/// `waitid(P_PID, ...)` - the door that lets this program *look* at a child's
/// zombie record without collecting it.
fn waitid_pid(pid: libc::pid_t, info: &mut libc::siginfo_t, options: i32) -> Result<(), i32> {
    waitid_raw(
        libc::P_PID as u32,
        pid as i64,
        info as *mut libc::siginfo_t,
        options,
    )
}

/// A record seeded with 0xFF, so a field the kernel zeroes is visible.
fn poisoned() -> libc::siginfo_t {
    let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
    unsafe {
        std::ptr::write_bytes(
            (&raw mut info).cast::<u8>(),
            0xFF,
            size_of::<libc::siginfo_t>(),
        );
    }
    info
}

/// The six words `waitid` owns, read out of the ABI's own accessors.
fn reported_pid(info: &libc::siginfo_t) -> libc::pid_t {
    unsafe { info.si_pid() }
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

/// Leave through `exit_group`, so a forked child never flushes the copy of the
/// parent's buffered stdout it inherited.
fn leave(code: i32) -> ! {
    unsafe {
        libc::syscall(libc::SYS_exit_group, code as libc::c_long);
        libc::_exit(code)
    }
}

/// A child that exits with `code` at once.
fn child_exit(code: i32) -> libc::pid_t {
    let kid = unsafe { libc::fork() };
    if kid == 0 {
        leave(code);
    }
    kid
}

/// A child that sleeps and then exits 0 - the fixture for "a live child has
/// nothing to report yet".
fn child_sleeps(ms: i64) -> libc::pid_t {
    let kid = unsafe { libc::fork() };
    if kid == 0 {
        sleep_ms(ms);
        leave(0);
    }
    kid
}

/// One delivery as the child's handler saw it.
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Seen {
    signo: i32,
    code: i32,
    value: i32,
    pid: i32,
    uid: u32,
}

/// The pipe a reporter child writes each delivery to. Set before the child
/// announces readiness, so the handler never reads a stale descriptor.
static REPORT_FD: AtomicI32 = AtomicI32::new(-1);

extern "C" fn reporter(sig: i32, info: *mut libc::siginfo_t, _uc: *mut libc::c_void) {
    let info = unsafe { &*info };
    let seen = Seen {
        signo: sig,
        code: info.si_code,
        // `libc`'s `sigval` on Linux exposes only the pointer member (the C
        // union is "actually a union of an int and a void*"). The int the
        // senders use here is `1234`, so the pointer's low word is the value
        // on both of this repository's little-endian machines.
        value: unsafe { info.si_value() }.sival_ptr as usize as i32,
        pid: unsafe { info.si_pid() },
        uid: unsafe { info.si_uid() },
    };
    let fd = REPORT_FD.load(Ordering::Relaxed);
    unsafe {
        libc::write(
            fd,
            (&raw const seen).cast::<libc::c_void>(),
            size_of::<Seen>(),
        );
    }
}

/// A child with a `SIGUSR1` reporter, in the caller's group (or in a group of
/// its own when `leads_group`), that announces itself and parks. Returns the
/// child and the read end of its report pipe.
fn child_reporter(leads_group: bool) -> (libc::pid_t, i32) {
    let mut pipe = [0i32; 2];
    let made = unsafe { libc::pipe(pipe.as_mut_ptr()) } == 0;
    let kid = unsafe { libc::fork() };
    if kid == 0 {
        if !made {
            leave(2);
        }
        unsafe {
            libc::close(pipe[0]);
        }
        if leads_group {
            unsafe {
                libc::setpgid(0, 0);
            }
        }
        REPORT_FD.store(pipe[1], Ordering::Relaxed);
        let mut sa: libc::sigaction = unsafe { std::mem::zeroed() };
        sa.sa_sigaction = reporter as *const () as usize;
        sa.sa_flags = libc::SA_SIGINFO;
        unsafe {
            libc::sigaction(libc::SIGUSR1, &sa, core::ptr::null_mut());
            let ready = [b'r'];
            libc::write(pipe[1], ready.as_ptr().cast::<libc::c_void>(), 1);
        }
        loop {
            unsafe {
                libc::pause();
            }
        }
    }
    if made {
        unsafe {
            libc::close(pipe[1]);
        }
    }
    if kid > 0 && made {
        let mut ready = [0u8; 1];
        unsafe {
            libc::read(pipe[0], ready.as_mut_ptr().cast::<libc::c_void>(), 1);
        }
    }
    (kid, if made { pipe[0] } else { -1 })
}

/// Read one report, or `None` if none arrived inside the budget.
fn drain(rfd: i32) -> Option<Seen> {
    let mut p = libc::pollfd {
        fd: rfd,
        events: libc::POLLIN as i16,
        revents: 0,
    };
    if unsafe { libc::poll(&mut p, 1, 500) } <= 0 {
        return None;
    }
    let mut seen = Seen::default();
    let n = unsafe {
        libc::read(
            rfd,
            (&raw mut seen).cast::<libc::c_void>(),
            size_of::<Seen>(),
        )
    };
    (n == size_of::<Seen>() as isize).then_some(seen)
}

/// Wait until `kid` is a zombie, *without* collecting it: a `WNOWAIT` look is
/// how a parent tells "exited but still in the table" from "still running",
/// which is the state a pidfd's poll and send differ on.
fn wait_until_zombie(kid: libc::pid_t) -> bool {
    for _ in 0..400 {
        let mut info = poisoned();
        if waitid_pid(
            kid,
            &mut info,
            libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
        )
        .is_ok()
            && reported_pid(&info) == kid
        {
            return true;
        }
        sleep_ms(5);
    }
    false
}

/// The `revents` a zero-time `poll(POLLIN)` answers for one descriptor.
fn poll_in(fd: i32) -> i16 {
    let mut p = libc::pollfd {
        fd,
        events: libc::POLLIN as i16,
        revents: 0,
    };
    unsafe {
        libc::poll(&mut p, 1, 0);
    }
    p.revents
}

/// Reap a child through the ordinary door, so the pidfd can then be asked about
/// a task that is gone for good.
fn reap(kid: libc::pid_t) {
    let mut status: i32 = 0;
    unsafe {
        libc::waitpid(kid, &mut status, 0);
    }
}

/// Kill and reap, for the fixtures whose claim has already been made.
fn dispose(kid: libc::pid_t) {
    if kid > 0 {
        unsafe {
            libc::kill(kid, libc::SIGKILL);
        }
        reap(kid);
    }
}

fn main() {
    println!("t-pidfd");
    let mut r = Report::new();

    // The record this program makes is the record the ABI names. A wrong size
    // would make the kernel's 128-byte read touch a byte the caller never
    // wrote, and the value at +24 is the `sigchld` union member both machines'
    // asm-generic layout puts there.
    r.check(
        "the caller-authored record is the ABI's 128 bytes",
        size_of::<Record>() == 128 && size_of::<Record>() == size_of::<libc::siginfo_t>(),
    );

    // ---- 1. the descriptor's shape ---------------------------------------
    match pidfd_open(unsafe { libc::getpid() }, 0) {
        Ok(fd) => {
            r.check("pidfd_open(self, 0) succeeds", true);
            let fl = unsafe { libc::fcntl(fd, libc::F_GETFL) };
            let fdl = unsafe { libc::fcntl(fd, libc::F_GETFD) };
            r.check(
                "F_GETFL is O_RDWR, the mode Linux reports for a pidfd",
                fl == libc::O_RDWR,
            );
            r.check(
                "FD_CLOEXEC is set without being asked for",
                fdl & libc::FD_CLOEXEC != 0,
            );

            let mut st: libc::stat = unsafe { std::mem::zeroed() };
            r.check(
                "fstat on the pidfd succeeds",
                unsafe { libc::fstat(fd, &mut st) } == 0,
            );
            r.check(
                "its type bits are absent - an anonymous inode, not a device",
                (st.st_mode as u32) & 0o170_000 == 0,
            );
            r.check("its mode is 0700", (st.st_mode as u32) & 0o7777 == 0o700);
            r.check("its size is 0", st.st_size == 0);
            r.check("it has one name", st.st_nlink == 1);

            match std::fs::read_link(format!("/proc/self/fd/{fd}")) {
                Ok(path) => r.check(
                    "/proc/self/fd/<n> reads back anon_inode:[pidfd]",
                    path.as_os_str() == "anon_inode:[pidfd]",
                ),
                Err(_) => r.check("/proc/self/fd/<n> reads back anon_inode:[pidfd]", false),
            }

            let mut bytes = [0u8; 8];
            let four = unsafe { libc::read(fd, bytes.as_mut_ptr().cast(), 4) };
            r.check(
                "read on a pidfd is EINVAL",
                four < 0 && std::io::Error::last_os_error().raw_os_error() == Some(libc::EINVAL),
            );
            let zero = unsafe { libc::read(fd, bytes.as_mut_ptr().cast(), 0) };
            r.check(
                "and so is a zero-length read, which has no byte stream to answer",
                zero < 0 && std::io::Error::last_os_error().raw_os_error() == Some(libc::EINVAL),
            );
            let pread = unsafe { libc::pread(fd, bytes.as_mut_ptr().cast(), 4, 0) };
            r.check(
                "pread is EINVAL too",
                pread < 0 && std::io::Error::last_os_error().raw_os_error() == Some(libc::EINVAL),
            );
            let wrote = unsafe { libc::write(fd, bytes.as_ptr().cast(), 1) };
            r.check(
                "write on a pidfd is EINVAL",
                wrote < 0 && std::io::Error::last_os_error().raw_os_error() == Some(libc::EINVAL),
            );
            let moved = unsafe { libc::lseek(fd, 0, libc::SEEK_SET) };
            r.check(
                "lseek is ESPIPE, as on a pipe or a terminal",
                moved < 0 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESPIPE),
            );
            r.check(
                "a live self pidfd is not readable",
                poll_in(fd) & libc::POLLIN as i16 == 0,
            );

            let _ = unsafe { libc::close(fd) };
        }
        Err(e) => {
            r.check("pidfd_open(self, 0) succeeds", false);
            println!("note: pidfd_open(self) answered errno {e}");
        }
    }

    // `O_NONBLOCK` is the one flag, and it is observable in `F_GETFL`; the
    // close-on-exec bit is not a caller's request here.
    match pidfd_open(unsafe { libc::getpid() }, libc::O_NONBLOCK as u32) {
        Ok(fd) => {
            let fl = unsafe { libc::fcntl(fd, libc::F_GETFL) };
            r.check(
                "pidfd_open(..., O_NONBLOCK) reports O_RDWR|O_NONBLOCK",
                fl == libc::O_RDWR | libc::O_NONBLOCK,
            );
            let _ = unsafe { libc::close(fd) };
        }
        Err(_) => r.check(
            "pidfd_open(..., O_NONBLOCK) reports O_RDWR|O_NONBLOCK",
            false,
        ),
    }
    r.check(
        "pidfd_open(..., O_CLOEXEC) is EINVAL - the bit is set, not asked for",
        pidfd_open(unsafe { libc::getpid() }, libc::O_CLOEXEC as u32) == Err(libc::EINVAL),
    );
    r.check(
        "O_NONBLOCK|O_CLOEXEC is EINVAL for the same reason",
        pidfd_open(
            unsafe { libc::getpid() },
            (libc::O_NONBLOCK | libc::O_CLOEXEC) as u32,
        ) == Err(libc::EINVAL),
    );
    r.check(
        "an undefined flag word is EINVAL",
        pidfd_open(unsafe { libc::getpid() }, 0x8) == Err(libc::EINVAL),
    );
    r.check("pid 0 is EINVAL", pidfd_open(0, 0) == Err(libc::EINVAL));
    r.check(
        "a negative pid is EINVAL",
        pidfd_open(-1, 0) == Err(libc::EINVAL),
    );
    r.check(
        "a pid naming no task is ESRCH",
        pidfd_open(999_999_999, 0) == Err(libc::ESRCH),
    );
    r.check(
        "the flags are validated before the pid is looked at",
        pidfd_open(999_999_999, 0x8) == Err(libc::EINVAL),
    );

    // A pidfd names a **thread group**, so a worker's tid is ENOENT rather than
    // a descriptor for the worker alone. The worker is kept alive for the
    // duration of the question and joined afterwards.
    static WORKER_TID: AtomicI32 = AtomicI32::new(0);
    static STOP_WORKER: AtomicBool = AtomicBool::new(false);
    let worker = std::thread::spawn(|| {
        WORKER_TID.store(gettid().as_raw_nonzero().get(), Ordering::Release);
        while !STOP_WORKER.load(Ordering::Acquire) {
            std::thread::sleep(Duration::from_millis(5));
        }
    });
    let mut spins = 0;
    while WORKER_TID.load(Ordering::Acquire) == 0 && spins < 2000 {
        std::thread::yield_now();
        spins += 1;
    }
    let tid = WORKER_TID.load(Ordering::Acquire);
    r.check("the worker thread published its tid", tid > 0);
    if tid > 0 {
        r.check(
            "pidfd_open(non-leader tid) is ENOENT - a pidfd names a process",
            pidfd_open(tid, 0) == Err(libc::ENOENT),
        );
    }
    STOP_WORKER.store(true, Ordering::Release);
    let _ = worker.join();

    // A pidfd may be opened on any task, including one this program is not the
    // parent of - what the process *is* to the caller is the wait's question,
    // not the open's.
    match pidfd_open(1, 0) {
        Ok(fd) => {
            r.check("pidfd_open(pid 1) succeeds", true);
            let mut info = poisoned();
            r.check(
                "waitid on a live non-child is ECHILD, not the zero of a wait",
                waitid_pidfd(fd, Some(&mut info), libc::WEXITED | libc::WNOHANG)
                    == Err(libc::ECHILD),
            );
            let _ = unsafe { libc::close(fd) };
        }
        Err(_) => r.check("pidfd_open(pid 1) succeeds", false),
    }

    // ---- 2. waitid's own refusals ----------------------------------------
    let mut info = poisoned();
    r.check(
        "waitid(P_PIDFD) on a descriptor number that is free is EBADF",
        waitid_pidfd(9999, Some(&mut info), libc::WEXITED) == Err(libc::EBADF),
    );
    let devnull = unsafe { libc::open(c"/dev/null".as_ptr(), libc::O_RDONLY) };
    if devnull >= 0 {
        let mut info = poisoned();
        r.check(
            "waitid(P_PIDFD) on a descriptor that is not a pidfd is EBADF",
            waitid_pidfd(devnull, Some(&mut info), libc::WEXITED) == Err(libc::EBADF),
        );
        // A bad record pointer still outranks the descriptor: the record is
        // written after the wait, and its own EFAULT is the answer.
        r.check(
            "an unwritable record is EFAULT even when the descriptor is bad",
            waitid_raw(
                libc::P_PIDFD as u32,
                devnull as i64,
                1usize as *mut _,
                libc::WEXITED,
            ) == Err(libc::EFAULT),
        );
        let _ = unsafe { libc::close(devnull) };
    } else {
        r.check(
            "waitid(P_PIDFD) on a descriptor that is not a pidfd is EBADF",
            false,
        );
        r.check(
            "an unwritable record is EFAULT even when the descriptor is bad",
            false,
        );
    }
    let mut info = poisoned();
    r.check(
        "a negative P_PIDFD id is EINVAL before any lookup",
        waitid_pidfd(-1, Some(&mut info), libc::WEXITED) == Err(libc::EINVAL),
    );

    // ---- 3. a child: live, dead, reaped -----------------------------------
    let kid = child_sleeps(150);
    r.check("a child that will exit on its own was forked", kid > 0);
    if kid > 0 {
        match pidfd_open(kid, 0) {
            Ok(fd) => {
                let mut info = poisoned();
                r.check(
                    "waitid(P_PIDFD, live, WEXITED|WNOHANG) answers the zero record",
                    waitid_pidfd(fd, Some(&mut info), libc::WEXITED | libc::WNOHANG).is_ok()
                        && reported_pid(&info) == 0,
                );
                r.check(
                    "a live pidfd is not readable",
                    poll_in(fd) & libc::POLLIN as i16 == 0,
                );
                r.check(
                    "while an invalid option word is EINVAL, options first",
                    waitid_pidfd(fd, Some(&mut info), 0) == Err(libc::EINVAL),
                );
                r.check(
                    "and an undefined option bit is EINVAL",
                    waitid_pidfd(fd, Some(&mut info), libc::WEXITED | 0x1000_0000)
                        == Err(libc::EINVAL),
                );

                // A null record is "I only wanted the wait", and is legal.
                r.check(
                    "a null record is accepted",
                    waitid_pidfd(fd, None, libc::WEXITED | libc::WNOHANG).is_ok(),
                );

                // The descriptor is readable when the *process* dies. The
                // child here has no worker, so its last thread is its leader.
                let mut seen = false;
                for _ in 0..400 {
                    if poll_in(fd) & libc::POLLIN as i16 != 0 {
                        seen = true;
                        break;
                    }
                    sleep_ms(5);
                }
                r.check("a dead child makes its pidfd readable", seen);
                r.check(
                    "but POLLHUP waits for the reap",
                    poll_in(fd) & libc::POLLHUP as i16 == 0,
                );

                // The exit class has to be asked for: a zombie under WSTOPPED
                // alone is ECHILD, not "nothing yet".
                let mut info = poisoned();
                r.check(
                    "a zombie under WSTOPPED alone is ECHILD",
                    waitid_pidfd(fd, Some(&mut info), libc::WSTOPPED | libc::WNOHANG)
                        == Err(libc::ECHILD),
                );

                // WNOWAIT reports the record and leaves it: twice, then collect.
                let mut first = poisoned();
                r.check(
                    "WNOWAIT reports the exit without collecting it",
                    waitid_pidfd(fd, Some(&mut first), libc::WEXITED | libc::WNOWAIT).is_ok()
                        && reported_pid(&first) == kid
                        && first.si_code == libc::CLD_EXITED
                        && unsafe { first.si_status() } == 0,
                );
                let mut second = poisoned();
                r.check(
                    "and the same exit is still there for the next look",
                    waitid_pidfd(fd, Some(&mut second), libc::WEXITED | libc::WNOWAIT).is_ok()
                        && reported_pid(&second) == kid,
                );

                let mut collected = poisoned();
                r.check(
                    "a wait without WNOWAIT collects it",
                    waitid_pidfd(fd, Some(&mut collected), libc::WEXITED).is_ok()
                        && reported_pid(&collected) == kid
                        && unsafe { collected.si_status() } == 0,
                );
                r.check(
                    "the reap is what adds POLLHUP",
                    poll_in(fd) & (libc::POLLIN | libc::POLLHUP) as i16
                        == (libc::POLLIN | libc::POLLHUP) as i16,
                );
                let mut after = poisoned();
                r.check(
                    "a reaped task is ECHILD, not a wait for the slot's next tenant",
                    waitid_pidfd(fd, Some(&mut after), libc::WEXITED) == Err(libc::ECHILD),
                );
                r.check(
                    "and pidfd_send_signal on it is ESRCH",
                    pidfd_send_signal(fd, libc::SIGTERM, None, 0) == Err(libc::ESRCH),
                );
                let _ = unsafe { libc::close(fd) };
            }
            Err(e) => {
                r.check("a child's pidfd opens", false);
                println!("note: pidfd_open(child) answered errno {e}");
                reap(kid);
            }
        }
    } else {
        for name in [
            "waitid(P_PIDFD, live, WEXITED|WNOHANG) answers the zero record",
            "a live pidfd is not readable",
            "while an invalid option word is EINVAL, options first",
            "and an undefined option bit is EINVAL",
            "a null record is accepted",
            "a dead child makes its pidfd readable",
            "but POLLHUP waits for the reap",
            "a zombie under WSTOPPED alone is ECHILD",
            "WNOWAIT reports the exit without collecting it",
            "and the same exit is still there for the next look",
            "a wait without WNOWAIT collects it",
            "the reap is what adds POLLHUP",
            "a reaped task is ECHILD, not a wait for the slot's next tenant",
            "and pidfd_send_signal on it is ESRCH",
        ] {
            r.check(name, false);
        }
    }

    // ---- 4. epoll: the same readiness, through the other door -------------
    let epoll_kid = child_sleeps(150);
    r.check("a child for the epoll claim was forked", epoll_kid > 0);
    if epoll_kid > 0 {
        let made = pidfd_open(epoll_kid, 0).ok();
        let ep = unsafe { libc::epoll_create1(0) };
        r.check(
            "the epoll instance and the child's pidfd exist",
            made.is_some() && ep >= 0,
        );
        if let (Some(fd), true) = (made, ep >= 0) {
            let mut ev = libc::epoll_event {
                events: libc::EPOLLIN as u32,
                u64: 0x1234,
            };
            r.check(
                "a pidfd can be added to an epoll set",
                unsafe { libc::epoll_ctl(ep, libc::EPOLL_CTL_ADD, fd, &mut ev) } == 0,
            );
            r.check(
                "a live pidfd is not ready in epoll",
                unsafe { libc::epoll_wait(ep, &mut ev, 1, 0) } == 0,
            );
            let mut ready = false;
            for _ in 0..400 {
                if unsafe { libc::epoll_wait(ep, &mut ev, 1, 0) } == 1 {
                    ready = true;
                    break;
                }
                sleep_ms(5);
            }
            r.check("the child's death wakes the epoll set", ready);
            r.check("with the caller's own data word", ready && ev.u64 == 0x1234);
            let _ = unsafe { libc::close(fd) };
        } else {
            r.check("a pidfd can be added to an epoll set", false);
            r.check("a live pidfd is not ready in epoll", false);
            r.check("the child's death wakes the epoll set", false);
            r.check("with the caller's own data word", false);
        }
        if made.is_some() {
            reap(epoll_kid);
        } else {
            dispose(epoll_kid);
        }
        if ep >= 0 {
            let _ = unsafe { libc::close(ep) };
        }
    } else {
        for name in [
            "the epoll instance and the child's pidfd exist",
            "a pidfd can be added to an epoll set",
            "a live pidfd is not ready in epoll",
            "the child's death wakes the epoll set",
            "with the caller's own data word",
        ] {
            r.check(name, false);
        }
    }

    // ---- 5. pidfd_send_signal: the refusals, in Linux's order -------------
    let target = child_reporter(false);
    r.check(
        "a reporting child was forked",
        target.0 > 0 && target.1 >= 0,
    );
    if target.0 > 0 && target.1 >= 0 {
        let (kid, reports) = target;
        match pidfd_open(kid, 0) {
            Ok(fd) => {
                let _ = unsafe { libc::close(reports) };
                let devnull = unsafe { libc::open(c"/dev/null".as_ptr(), libc::O_RDONLY) };
                r.check(
                    "a descriptor that is not a pidfd is EBADF",
                    devnull >= 0 && pidfd_send_signal(devnull, 0, None, 0) == Err(libc::EBADF),
                );
                r.check(
                    "a free descriptor number is EBADF",
                    pidfd_send_signal(9999, 0, None, 0) == Err(libc::EBADF),
                );
                r.check(
                    "an invalid flags word is EINVAL before the descriptor",
                    pidfd_send_signal(9999, 0, None, 3) == Err(libc::EINVAL),
                );
                r.check(
                    "1|2 is an invalid flags word",
                    pidfd_send_signal(fd, 0, None, 3) == Err(libc::EINVAL),
                );
                r.check(
                    "anything above 4 is EINVAL",
                    pidfd_send_signal(fd, 0, None, 5) == Err(libc::EINVAL),
                );
                r.check("an unreadable record is EFAULT", {
                    let r = unsafe {
                        libc::syscall(
                            libc::SYS_pidfd_send_signal,
                            fd as libc::c_long,
                            0 as libc::c_long,
                            1usize as libc::c_long,
                            0 as libc::c_long,
                        )
                    };
                    r < 0 && std::io::Error::last_os_error().raw_os_error() == Some(libc::EFAULT)
                });
                let mismatched = record(libc::SIGUSR2, libc::SI_QUEUE, 0, 0, 0);
                r.check(
                    "a record whose si_signo differs from sig is EINVAL",
                    pidfd_send_signal(fd, libc::SIGUSR1, Some(&mismatched), 0) == Err(libc::EINVAL),
                );
                let user = record(libc::SIGUSR1, libc::SI_USER, 4242, 4242, 0);
                r.check(
                    "SI_USER is the kernel's code, so another process is EPERM",
                    pidfd_send_signal(fd, libc::SIGUSR1, Some(&user), 0) == Err(libc::EPERM),
                );
                let tkill = record(libc::SIGUSR1, libc::SI_TKILL, 4242, 4242, 0);
                r.check(
                    "SI_TKILL is refused the same way",
                    pidfd_send_signal(fd, libc::SIGUSR1, Some(&tkill), 0) == Err(libc::EPERM),
                );
                r.check(
                    "a signal number above _NSIG is EINVAL",
                    pidfd_send_signal(fd, 300, None, 0) == Err(libc::EINVAL),
                );
                r.check(
                    "sig 0 is the existence probe and succeeds",
                    pidfd_send_signal(fd, 0, None, 0).is_ok(),
                );
                let _ = unsafe { libc::close(devnull) };
                let _ = unsafe { libc::close(fd) };
                dispose(kid);
            }
            Err(_) => {
                r.check("a reporter child's pidfd opens", false);
                dispose(kid);
            }
        }
    } else {
        for name in [
            "a reporter child was forked",
            "a descriptor that is not a pidfd is EBADF",
            "a free descriptor number is EBADF",
            "an invalid flags word is EINVAL before the descriptor",
            "1|2 is an invalid flags word",
            "anything above 4 is EINVAL",
            "an unreadable record is EFAULT",
            "a record whose si_signo differs from sig is EINVAL",
            "SI_USER is the kernel's code, so another process is EPERM",
            "SI_TKILL is refused the same way",
            "a signal number above _NSIG is EINVAL",
            "sig 0 is the existence probe and succeeds",
        ] {
            r.check(name, false);
        }
        dispose(target.0);
    }

    // ---- 6. a record and its scopes, delivered ---------------------------
    let (kid, reports) = child_reporter(false);
    r.check(
        "a reporting child was forked for the delivery claims",
        kid > 0 && reports >= 0,
    );
    if kid > 0 && reports >= 0 {
        match pidfd_open(kid, 0) {
            Ok(fd) => {
                r.check(
                    "a NULL record becomes SI_USER naming the sender",
                    pidfd_send_signal(fd, libc::SIGUSR1, None, 0).is_ok(),
                );
                match drain(reports) {
                    Some(seen) => {
                        r.check(
                            "and the target sees SI_USER",
                            seen.signo == libc::SIGUSR1 && seen.code == libc::SI_USER,
                        );
                        r.check(
                            "with the sender's pid",
                            seen.pid == unsafe { libc::getpid() },
                        );
                        r.check(
                            "and the sender's real uid",
                            seen.uid == unsafe { libc::getuid() },
                        );
                    }
                    None => {
                        r.check("and the target sees SI_USER", false);
                        r.check("with the sender's pid", false);
                        r.check("and the sender's real uid", false);
                    }
                }

                let queued = record(libc::SIGUSR1, libc::SI_QUEUE, 0, 0, 1234);
                r.check(
                    "a caller's SI_QUEUE record is accepted",
                    pidfd_send_signal(fd, libc::SIGUSR1, Some(&queued), 0).is_ok(),
                );
                match drain(reports) {
                    Some(seen) => {
                        r.check(
                            "and arrives with the caller's si_code",
                            seen.code == libc::SI_QUEUE,
                        );
                        r.check("and the caller's si_value", seen.value == 1234);
                        r.check(
                            "and the caller's own id fields, untouched",
                            seen.pid == 0 && seen.uid == 0,
                        );
                    }
                    None => {
                        r.check("and arrives with the caller's si_code", false);
                        r.check("and the caller's si_value", false);
                        r.check("and the caller's own id fields, untouched", false);
                    }
                }

                // The two scope bits that name a thread or its group are the
                // process here, because the target has one thread.
                for (scope, name) in [
                    (1u32, "PIDFD_SIGNAL_THREAD reaches the named thread"),
                    (2u32, "PIDFD_SIGNAL_THREAD_GROUP reaches the process"),
                ] {
                    let ok = pidfd_send_signal(fd, libc::SIGUSR1, None, scope).is_ok();
                    let seen = ok && drain(reports).is_some();
                    r.check(name, seen);
                }

                // The process-group scope names the group the target *leads*.
                // This child shares our group, so it leads nothing.
                r.check(
                    "PIDFD_SIGNAL_PROCESS_GROUP on a non-leader is ESRCH",
                    pidfd_send_signal(fd, 0, None, 4) == Err(libc::ESRCH),
                );
                let _ = unsafe { libc::close(fd) };
                dispose(kid);
            }
            Err(_) => {
                r.check("a reporter child's pidfd opens", false);
                dispose(kid);
            }
        }
        let _ = unsafe { libc::close(reports) };
    } else {
        for name in [
            "a reporting child was forked for the delivery claims",
            "a NULL record becomes SI_USER naming the sender",
            "and the target sees SI_USER",
            "with the sender's pid",
            "and the sender's real uid",
            "a caller's SI_QUEUE record is accepted",
            "and arrives with the caller's si_code",
            "and the caller's si_value",
            "and the caller's own id fields, untouched",
            "PIDFD_SIGNAL_THREAD reaches the named thread",
            "PIDFD_SIGNAL_THREAD_GROUP reaches the process",
            "PIDFD_SIGNAL_PROCESS_GROUP on a non-leader is ESRCH",
        ] {
            r.check(name, false);
        }
        dispose(kid);
    }

    // A child that leads its own group: the process-group scope reaches it.
    let (leader, reports) = child_reporter(true);
    r.check(
        "a reporting child that leads its own group was forked",
        leader > 0 && reports >= 0,
    );
    if leader > 0 && reports >= 0 {
        match pidfd_open(leader, 0) {
            Ok(fd) => {
                let sent = pidfd_send_signal(fd, libc::SIGUSR1, None, 4);
                r.check(
                    "PIDFD_SIGNAL_PROCESS_GROUP reaches the group the target leads",
                    sent.is_ok() && drain(reports).is_some(),
                );
                let _ = unsafe { libc::close(fd) };
            }
            Err(_) => r.check(
                "PIDFD_SIGNAL_PROCESS_GROUP reaches the group the target leads",
                false,
            ),
        }
        dispose(leader);
        let _ = unsafe { libc::close(reports) };
    } else {
        r.check(
            "PIDFD_SIGNAL_PROCESS_GROUP reaches the group the target leads",
            false,
        );
        dispose(leader);
    }

    // ---- 7. a zombie takes scopes 0/1/2 and refuses 4 --------------------
    let zombie = child_exit(9);
    r.check("a child that exits 9 was forked", zombie > 0);
    if zombie > 0 {
        match pidfd_open(zombie, 0) {
            Ok(fd) => {
                r.check("its pidfd opened while it was still alive or zombie", true);
                r.check("the child became a zombie", wait_until_zombie(zombie));
                r.check(
                    "the zombie's pidfd is readable",
                    poll_in(fd) & libc::POLLIN as i16 != 0,
                );
                r.check(
                    "a zombie accepts sig 0",
                    pidfd_send_signal(fd, 0, None, 0).is_ok(),
                );
                r.check(
                    "and a real signal, which it will never read",
                    pidfd_send_signal(fd, libc::SIGTERM, None, 0).is_ok(),
                );
                r.check(
                    "the thread scope accepts it too",
                    pidfd_send_signal(fd, libc::SIGTERM, None, 1).is_ok(),
                );
                r.check(
                    "as does the thread-group scope",
                    pidfd_send_signal(fd, libc::SIGTERM, None, 2).is_ok(),
                );
                r.check(
                    "while the process-group scope is ESRCH: a zombie leads nothing",
                    pidfd_send_signal(fd, libc::SIGTERM, None, 4) == Err(libc::ESRCH),
                );
                let mut seen = poisoned();
                r.check(
                    "waitid collects the zombie's exit",
                    waitid_pidfd(fd, Some(&mut seen), libc::WEXITED).is_ok()
                        && reported_pid(&seen) == zombie
                        && unsafe { seen.si_status() } == 9,
                );
                r.check(
                    "and after the reap every scope is ESRCH",
                    pidfd_send_signal(fd, libc::SIGTERM, None, 0) == Err(libc::ESRCH),
                );
                let _ = unsafe { libc::close(fd) };
            }
            Err(_) => {
                r.check("a zombie's pidfd opens", false);
                dispose(zombie);
            }
        }
    } else {
        for name in [
            "its pidfd opened while it was still alive or zombie",
            "the child became a zombie",
            "the zombie's pidfd is readable",
            "a zombie accepts sig 0",
            "and a real signal, which it will never read",
            "the thread scope accepts it too",
            "as does the thread-group scope",
            "while the process-group scope is ESRCH: a zombie leads nothing",
            "waitid collects the zombie's exit",
            "and after the reap every scope is ESRCH",
        ] {
            r.check(name, false);
        }
    }

    // ---- 8. O_NONBLOCK is a wait policy, not just a flag word -------------
    let napper = child_sleeps(250);
    r.check(
        "a napping child was forked for the non-blocking wait",
        napper > 0,
    );
    if napper > 0 {
        match pidfd_open(napper, libc::O_NONBLOCK as u32) {
            Ok(fd) => {
                let mut info = poisoned();
                r.check(
                    "a non-blocking pidfd's wait on a live child is EAGAIN",
                    waitid_pidfd(fd, Some(&mut info), libc::WEXITED) == Err(libc::EAGAIN),
                );
                r.check(
                    "and the refusal still cleared the record",
                    reported_pid(&info) == 0,
                );
                r.check(
                    "WNOHANG wins when both are asked for: the zero record",
                    waitid_pidfd(fd, Some(&mut info), libc::WEXITED | libc::WNOHANG).is_ok()
                        && reported_pid(&info) == 0,
                );
                let _ = unsafe { libc::close(fd) };
            }
            Err(_) => {
                r.check(
                    "a non-blocking pidfd's wait on a live child is EAGAIN",
                    false,
                );
                r.check("and the refusal still cleared the record", false);
                r.check(
                    "WNOHANG wins when both are asked for: the zero record",
                    false,
                );
            }
        }

        // `F_SETFL(O_NONBLOCK)` is the same policy on a descriptor that was
        // opened blocking.
        let mut collected = false;
        match pidfd_open(napper, 0) {
            Ok(fd) => {
                let before = unsafe { libc::fcntl(fd, libc::F_GETFL) };
                let set = unsafe { libc::fcntl(fd, libc::F_SETFL, before | libc::O_NONBLOCK) };
                let after = unsafe { libc::fcntl(fd, libc::F_GETFL) };
                r.check(
                    "F_SETFL stores O_NONBLOCK and F_GETFL shows it",
                    set == 0 && after == before | libc::O_NONBLOCK,
                );
                let mut info = poisoned();
                r.check(
                    "and the wait is then EAGAIN",
                    waitid_pidfd(fd, Some(&mut info), libc::WEXITED) == Err(libc::EAGAIN),
                );
                let cleared = unsafe { libc::fcntl(fd, libc::F_SETFL, before) };
                let mut done = poisoned();
                collected = cleared == 0
                    && waitid_pidfd(fd, Some(&mut done), libc::WEXITED).is_ok()
                    && reported_pid(&done) == napper;
                r.check(
                    "clearing it makes the same wait block until the exit",
                    collected,
                );
                let _ = unsafe { libc::close(fd) };
            }
            Err(_) => {
                r.check("F_SETFL stores O_NONBLOCK and F_GETFL shows it", false);
                r.check("and the wait is then EAGAIN", false);
                r.check(
                    "clearing it makes the same wait block until the exit",
                    false,
                );
            }
        }
        if !collected {
            dispose(napper);
        }
    } else {
        for name in [
            "a non-blocking pidfd's wait on a live child is EAGAIN",
            "and the refusal still cleared the record",
            "WNOHANG wins when both are asked for: the zero record",
            "F_SETFL stores O_NONBLOCK and F_GETFL shows it",
            "and the wait is then EAGAIN",
            "clearing it makes the same wait block until the exit",
        ] {
            r.check(name, false);
        }
    }

    // A non-blocking pidfd of a *live non-child* is ECHILD, not EAGAIN: the
    // non-blocking bit chooses the answer when a wait could be answered, and
    // "nobody this call could ever hear" is the other case.
    match pidfd_open(1, libc::O_NONBLOCK as u32) {
        Ok(fd) => {
            let mut info = poisoned();
            r.check(
                "a non-blocking pidfd of a live non-child is ECHILD",
                waitid_pidfd(fd, Some(&mut info), libc::WEXITED) == Err(libc::ECHILD),
            );
            let _ = unsafe { libc::close(fd) };
        }
        Err(_) => r.check("a non-blocking pidfd of a live non-child is ECHILD", false),
    }

    // ---- 9. a kernel-made record passes only for the caller's own group ----
    //
    // The group check is the measured asymmetry: a caller may not write
    // `SI_USER`/`SI_TKILL` into another process's record, but the same record
    // aimed at itself is Linux's own `raise`-by-descriptor and is delivered
    // verbatim, spoofed id fields and all.
    static SELF_SIGNO: AtomicI32 = AtomicI32::new(0);
    static SELF_CODE: AtomicI32 = AtomicI32::new(-1);
    static SELF_UID: AtomicI32 = AtomicI32::new(-1);
    static SELF_PID: AtomicI32 = AtomicI32::new(-1);
    extern "C" fn self_reporter(sig: i32, info: *mut libc::siginfo_t, _uc: *mut libc::c_void) {
        let info = unsafe { &*info };
        // Atomics rather than a lock: a handler runs on the thread the signal
        // interrupted, and taking a lock there can deadlock on itself.
        SELF_SIGNO.store(sig, Ordering::Relaxed);
        SELF_CODE.store(info.si_code, Ordering::Relaxed);
        SELF_UID.store(unsafe { info.si_uid() } as i32, Ordering::Relaxed);
        SELF_PID.store(unsafe { info.si_pid() }, Ordering::Relaxed);
    }
    let mut sa: libc::sigaction = unsafe { std::mem::zeroed() };
    sa.sa_sigaction = self_reporter as *const () as usize;
    sa.sa_flags = libc::SA_SIGINFO;
    let mut old: libc::sigaction = unsafe { std::mem::zeroed() };
    unsafe {
        libc::sigaction(libc::SIGUSR1, &sa, &mut old);
    }
    match pidfd_open(unsafe { libc::getpid() }, 0) {
        Ok(fd) => {
            let spoofed = record(libc::SIGUSR1, libc::SI_USER, 4242, 4242, 7);
            r.check(
                "SI_USER aimed at the caller's own group is accepted",
                pidfd_send_signal(fd, libc::SIGUSR1, Some(&spoofed), 0).is_ok(),
            );
            let mut arrived = false;
            for _ in 0..200 {
                if SELF_SIGNO.load(Ordering::Relaxed) == libc::SIGUSR1 {
                    arrived = true;
                    break;
                }
                sleep_ms(5);
            }
            r.check(
                "and arrives verbatim, with the caller's own si_pid/si_uid",
                arrived
                    && SELF_CODE.load(Ordering::Relaxed) == libc::SI_USER
                    && SELF_UID.load(Ordering::Relaxed) == 4242
                    && SELF_PID.load(Ordering::Relaxed) == 4242,
            );
            let _ = unsafe { libc::close(fd) };
        }
        Err(_) => {
            r.check("SI_USER aimed at the caller's own group is accepted", false);
            r.check(
                "and arrives verbatim, with the caller's own si_pid/si_uid",
                false,
            );
        }
    }
    unsafe {
        libc::sigaction(libc::SIGUSR1, &old, core::ptr::null_mut());
    }

    // ---- 10. pidfd_getfd: another process's descriptor, by number --------
    //
    // The fixture is two pipes rather than one: the child reports its
    // descriptor numbers up one and takes commands down the other. A single
    // pipe would end the child's read when the parent closed its own write
    // end - which is how the first host probe mistook a live child for a gone
    // one.
    {
        let mut up = [0i32; 2];
        let mut down = [0i32; 2];
        let pipes =
            unsafe { libc::pipe(up.as_mut_ptr()) == 0 && libc::pipe(down.as_mut_ptr()) == 0 };
        let kid = if pipes { unsafe { libc::fork() } } else { -1 };
        if kid == 0 {
            unsafe {
                libc::close(up[0]);
                libc::close(down[1]);
            }
            let path = format!("/tmp/t-pidfd-getfd-{}\0", unsafe { libc::getpid() });
            let fd = unsafe {
                libc::open(
                    path.as_ptr().cast::<libc::c_char>(),
                    libc::O_RDWR | libc::O_CREAT | libc::O_TRUNC,
                    0o600,
                )
            };
            if fd < 0 {
                leave(2);
            }
            unsafe {
                libc::unlink(path.as_ptr().cast::<libc::c_char>());
            }
            let data = b"abcdefgh";
            if unsafe { libc::write(fd, data.as_ptr().cast(), data.len()) } != data.len() as isize {
                leave(3);
            }
            unsafe {
                libc::lseek(fd, 0, libc::SEEK_SET);
            }
            let self_pidfd = pidfd_open(unsafe { libc::getpid() }, 0).unwrap_or(-1);
            let report = [fd, self_pidfd];
            unsafe {
                libc::write(up[1], report.as_ptr().cast(), 8);
            }
            // Block until the parent has made every fetch it asked for; either
            // its explicit byte or the closed pipe ends the wait. A single pipe
            // would end this read as soon as the parent's own write end closed,
            // which is not this fixture's signal to stop.
            let mut command = 0u8;
            let _ = unsafe { libc::read(down[0], (&raw mut command).cast(), 1) };
            leave(0);
        }
        if pipes {
            unsafe {
                libc::close(up[1]);
                libc::close(down[0]);
            }
        }

        let mut report = [0i32; 2];
        let got_report = pipe_report(up[0], &mut report);
        r.check(
            "the getfd fixture reports the descriptor it owns",
            got_report && kid > 0,
        );

        let pfd = pidfd_open(kid, 0);
        let target = report[0];
        let child_pidfd = report[1];
        match pfd {
            Ok(pfd) => {
                r.check(
                    "a non-zero flags word is EINVAL",
                    pidfd_getfd(pfd, target, 1) == Err(libc::EINVAL)
                        && pidfd_getfd(pfd, target, 2) == Err(libc::EINVAL),
                );
                r.check(
                    "the flags word is read before the pidfd itself",
                    pidfd_getfd(-1, -1, 1) == Err(libc::EINVAL),
                );
                r.check(
                    "a number naming no descriptor is EBADF",
                    pidfd_getfd(-1, target, 0) == Err(libc::EBADF),
                );
                r.check(
                    "a descriptor that is not a pidfd is EBADF",
                    pidfd_getfd(up[0], target, 0) == Err(libc::EBADF),
                );
                r.check(
                    "a target fd that is not open is EBADF",
                    pidfd_getfd(pfd, 999_999, 0) == Err(libc::EBADF)
                        && pidfd_getfd(pfd, -1, 0) == Err(libc::EBADF),
                );

                match (pidfd_getfd(pfd, target, 0), pidfd_getfd(pfd, target, 0)) {
                    (Ok(first), Ok(second)) => {
                        r.check("a live same-uid process's fd is fetched", true);
                        r.check(
                            "the new descriptor is close-on-exec",
                            unsafe { libc::fcntl(first, libc::F_GETFD) } & libc::FD_CLOEXEC != 0,
                        );
                        r.check(
                            "it carries the target's access mode",
                            unsafe { libc::fcntl(first, libc::F_GETFL) } & libc::O_ACCMODE
                                == libc::O_RDWR,
                        );
                        let mut bytes = [0u8; 3];
                        let n = unsafe { libc::read(first, bytes.as_mut_ptr().cast(), 3) };
                        r.check(
                            "it reads the target file's bytes",
                            n == 3 && &bytes == b"abc",
                        );
                        r.check(
                            "two fetches name one open file description",
                            unsafe { libc::lseek(second, 0, libc::SEEK_CUR) } == 3,
                        );
                        let n = unsafe { libc::read(second, bytes.as_mut_ptr().cast(), 2) };
                        r.check(
                            "a read through the second advances the first",
                            n == 2 && &bytes[..2] == b"de",
                        );
                        let _ = unsafe { libc::close(first) };
                        let n = unsafe { libc::read(second, bytes.as_mut_ptr().cast(), 1) };
                        r.check(
                            "closing one fetch leaves the other open",
                            n == 1 && bytes[0] == b'f',
                        );
                        let _ = unsafe { libc::close(second) };
                    }
                    _ => {
                        r.check("a live same-uid process's fd is fetched", false);
                        r.check("the new descriptor is close-on-exec", false);
                        r.check("it carries the target's access mode", false);
                        r.check("it reads the target file's bytes", false);
                        r.check("two fetches name one open file description", false);
                        r.check("a read through the second advances the first", false);
                        r.check("closing one fetch leaves the other open", false);
                    }
                }

                if child_pidfd >= 0 {
                    match pidfd_getfd(pfd, child_pidfd, 0) {
                        Ok(fetched) => {
                            r.check(
                                "a fetched pidfd is still a live pidfd",
                                poll_in(fetched) & libc::POLLIN as i16 == 0
                                    && pidfd_send_signal(fetched, 0, None, 0).is_ok(),
                            );
                            let _ = unsafe { libc::close(fetched) };
                        }
                        Err(_) => r.check("a fetched pidfd is still a live pidfd", false),
                    }
                } else {
                    r.check("a fetched pidfd is still a live pidfd", false);
                }

                // The authority rule needs a caller without CAP_SYS_PTRACE:
                // this program runs as root on the guest, so it is measured by
                // the unprivileged fixture below rather than here.
                let _ = unsafe { libc::write(down[1], b"x".as_ptr().cast(), 1) };
                reap(kid);
                let _ = unsafe { libc::close(pfd) };
            }
            Err(_) => {
                r.check("a non-zero flags word is EINVAL", false);
                r.check("the flags word is read before the pidfd itself", false);
                r.check("a number naming no descriptor is EBADF", false);
                r.check("a descriptor that is not a pidfd is EBADF", false);
                r.check("a target fd that is not open is EBADF", false);
                r.check("a live same-uid process's fd is fetched", false);
                r.check("a fetched pidfd is still a live pidfd", false);
                dispose(kid);
            }
        }
        if pipes {
            unsafe {
                libc::close(up[0]);
                libc::close(down[1]);
            }
        }
    }

    // The authority rule, on a caller that has no `CAP_SYS_PTRACE`. The test
    // program is root on the guest, and a root caller passes
    // `ptrace_may_access` before the dumpable flag is read - so the only way
    // to pin the flag is a caller and a target that both dropped to the same
    // unprivileged uid. (The host run, as uid 1000, could measure it on the
    // main process; that measurement does not transfer to root.)
    {
        let mut target_up = [0i32; 2];
        let mut target_down = [0i32; 2];
        let mut caller_up = [0i32; 2];
        let made = unsafe {
            libc::pipe(target_up.as_mut_ptr()) == 0
                && libc::pipe(target_down.as_mut_ptr()) == 0
                && libc::pipe(caller_up.as_mut_ptr()) == 0
        };
        let target = if made { unsafe { libc::fork() } } else { -1 };
        if target == 0 {
            unsafe {
                libc::close(target_up[0]);
                libc::close(target_down[1]);
                libc::close(caller_up[0]);
                libc::close(caller_up[1]);
            }
            if unsafe { libc::setgid(65_534) } != 0 || unsafe { libc::setuid(65_534) } != 0 {
                // Already unprivileged (the host run): the same uid is what
                // the rule needs, and the caller's identical drop below
                // keeps the two sides matched. A root caller that could not
                // drop makes the EPERM claim fail honestly rather than pass
                // for the wrong reason.
            }
            unsafe {
                libc::prctl(libc::PR_SET_DUMPABLE, 0, 0, 0, 0);
            }
            let fd = unsafe { libc::open(b"/dev/null\0".as_ptr().cast(), libc::O_RDONLY) };
            let report = [unsafe { libc::getpid() }, fd];
            unsafe {
                libc::write(target_up[1], report.as_ptr().cast(), 8);
            }
            let mut command = 0u8;
            let _ = unsafe { libc::read(target_down[0], (&raw mut command).cast(), 1) };
            leave(0);
        }

        let mut report = [0i32; 2];
        let reported = made && pipe_report(target_up[0], &mut report);
        let caller = if reported {
            unsafe { libc::fork() }
        } else {
            -1
        };
        if caller == 0 {
            unsafe {
                libc::close(target_up[0]);
                libc::close(target_down[0]);
                libc::close(target_down[1]);
                libc::close(caller_up[0]);
            }
            let mut errors = [-1i32; 3];
            let _ = unsafe { libc::setgid(65_534) };
            let _ = unsafe { libc::setuid(65_534) };
            match pidfd_open(report[0], 0) {
                Ok(pfd) => {
                    errors[0] = 0;
                    errors[1] = pidfd_getfd(pfd, report[1], 0).err().unwrap_or(0);
                    errors[2] = pidfd_getfd(pfd, 999_999, 0).err().unwrap_or(0);
                }
                Err(e) => errors[0] = e,
            }
            unsafe {
                libc::write(caller_up[1], errors.as_ptr().cast(), 12);
            }
            leave(0);
        }

        if reported && caller > 0 {
            let mut errors = [-1i32; 3];
            let read_back =
                unsafe { libc::read(caller_up[0], errors.as_mut_ptr().cast(), 12) } == 12;
            r.check(
                "an unprivileged caller opens a pidfd of a non-dumpable peer",
                read_back && errors[0] == 0,
            );
            r.check(
                "a non-dumpable same-uid target is EPERM",
                read_back && errors[1] == libc::EPERM,
            );
            r.check(
                "authority is asked before the target fd",
                read_back && errors[2] == libc::EPERM,
            );
            reap(caller);
        } else {
            r.check(
                "an unprivileged caller opens a pidfd of a non-dumpable peer",
                false,
            );
            r.check("a non-dumpable same-uid target is EPERM", false);
            r.check("authority is asked before the target fd", false);
            dispose(caller);
        }
        if made {
            let _ = unsafe { libc::write(target_down[1], b"x".as_ptr().cast(), 1) };
        }
        if target > 0 {
            reap(target);
        }
        if made {
            unsafe {
                libc::close(target_up[0]);
                libc::close(target_down[1]);
                libc::close(caller_up[0]);
            }
        }
    }

    // A process may always fetch its own descriptor, with no authority check
    // to pass; and a zombie is gone even before its reap.
    match pidfd_open(unsafe { libc::getpid() }, 0) {
        Ok(self_pidfd) => {
            let own = unsafe { libc::open(b"/dev/null\0".as_ptr().cast(), libc::O_RDONLY) };
            match pidfd_getfd(self_pidfd, own, 0) {
                Ok(fetched) => {
                    r.check("a process may fetch its own descriptor", true);
                    r.check(
                        "and that fetch is close-on-exec too",
                        unsafe { libc::fcntl(fetched, libc::F_GETFD) } & libc::FD_CLOEXEC != 0,
                    );
                    let _ = unsafe { libc::close(fetched) };
                }
                Err(_) => {
                    r.check("a process may fetch its own descriptor", false);
                    r.check("and that fetch is close-on-exec too", false);
                }
            }
            let _ = unsafe { libc::close(own) };
            let _ = unsafe { libc::close(self_pidfd) };
        }
        Err(_) => {
            r.check("a process may fetch its own descriptor", false);
            r.check("and that fetch is close-on-exec too", false);
        }
    }

    {
        let mut zup = [0i32; 2];
        let made = unsafe { libc::pipe(zup.as_mut_ptr()) } == 0;
        let z = if made { unsafe { libc::fork() } } else { -1 };
        if z == 0 {
            unsafe {
                libc::close(zup[0]);
            }
            let fd = unsafe { libc::open(b"/dev/null\0".as_ptr().cast(), libc::O_RDONLY) };
            unsafe {
                libc::write(zup[1], (&raw const fd).cast(), 4);
            }
            leave(0);
        }
        if made {
            unsafe {
                libc::close(zup[1]);
            }
        }
        let mut zfd = 0i32;
        let reported = made && unsafe { libc::read(zup[0], (&raw mut zfd).cast(), 4) } == 4;
        if reported && z > 0 {
            match pidfd_open(z, 0) {
                Ok(zp) => {
                    r.check(
                        "a zombie's pidfd hands out no descriptor",
                        wait_until_zombie(z) && pidfd_getfd(zp, zfd, 0) == Err(libc::ESRCH),
                    );
                    reap(z);
                    r.check(
                        "and a reaped process is ESRCH as well",
                        pidfd_getfd(zp, zfd, 0) == Err(libc::ESRCH),
                    );
                    let _ = unsafe { libc::close(zp) };
                }
                Err(_) => {
                    r.check("a zombie's pidfd hands out no descriptor", false);
                    r.check("and a reaped process is ESRCH as well", false);
                    reap(z);
                }
            }
        } else {
            r.check("a zombie's pidfd hands out no descriptor", false);
            r.check("and a reaped process is ESRCH as well", false);
            dispose(z);
        }
        if made {
            let _ = unsafe { libc::close(zup[0]) };
        }
    }

    r.finish()
}
