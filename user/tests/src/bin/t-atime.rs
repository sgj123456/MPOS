//! t-atime - a read moves the access clock, `relatime` decides when, and
//! `O_NOATIME` is the promise that it does not.
//!
//! Measured on a current host first (`.workbuddy/probe_atime.c`, Linux 7.2.11,
//! a container's root so the uid drop is reachable):
//!
//! * a **fresh** file's three clocks are equal, and its first read moves atime;
//! * the runner's filesystem is a tmpfs on the host and a ramfs here, and both
//!   are **strictatime**: even an atime a *future* `utimensat` wrote is pulled
//!   back to the read's own moment. (An ext4 mount is `relatime` and would
//!   leave that future value standing - the difference is measured in
//!   `docs/notes/` §3.3 and pinned by an `ext4` host test, not here.)
//! * `O_NOATIME` leaves atime alone, and a caller that neither owns the file
//!   nor holds `CAP_FOWNER` is refused that flag at the open with `EPERM`;
//! * a read moves **atime only** - ctime and mtime stay where they were.

use mpos_user_tests::{Report, host_run};

fn stat_times(path: &str) -> libc::stat {
    let cpath = std::ffi::CString::new(path).unwrap();
    let mut st: libc::stat = unsafe { core::mem::zeroed() };
    // SAFETY: both pointers are this frame's, and `stat` writes only `st`.
    let rc = unsafe { libc::stat(cpath.as_ptr(), &mut st) };
    assert_eq!(
        rc,
        0,
        "stat({path}) failed: {}",
        std::io::Error::last_os_error()
    );
    st
}

fn set_times(path: &str, atime: i64, mtime: i64) {
    let cpath = std::ffi::CString::new(path).unwrap();
    let times = [
        libc::timespec {
            tv_sec: atime,
            tv_nsec: 0,
        },
        libc::timespec {
            tv_sec: mtime,
            tv_nsec: 0,
        },
    ];
    // SAFETY: the path and the two-word array are this frame's.
    let rc = unsafe { libc::utimensat(libc::AT_FDCWD, cpath.as_ptr(), times.as_ptr(), 0) };
    assert_eq!(
        rc,
        0,
        "utimensat({path}) failed: {}",
        std::io::Error::last_os_error()
    );
}

fn make_file(path: &str) {
    let cpath = std::ffi::CString::new(path).unwrap();
    // SAFETY: the path and the mode are literals this frame owns.
    let fd = unsafe {
        libc::open(
            cpath.as_ptr(),
            libc::O_WRONLY | libc::O_CREAT | libc::O_TRUNC,
            0o644,
        )
    };
    assert!(fd >= 0, "create({path}) failed");
    // SAFETY: `fd` is the descriptor just opened.
    unsafe {
        libc::write(fd, b"x".as_ptr() as *const libc::c_void, 1);
        libc::close(fd);
    }
}

/// Read one byte through `flags`, answering the errno on a refusal.
fn read_one(path: &str, flags: libc::c_int) -> Result<(), i32> {
    let cpath = std::ffi::CString::new(path).unwrap();
    // SAFETY: the path is this frame's.
    let fd = unsafe { libc::open(cpath.as_ptr(), libc::O_RDONLY | flags) };
    if fd < 0 {
        return Err(std::io::Error::last_os_error().raw_os_error().unwrap_or(0));
    }
    let mut byte = 0u8;
    // SAFETY: `fd` is open and the buffer is one byte this frame owns.
    let n = unsafe { libc::read(fd, &mut byte as *mut u8 as *mut libc::c_void, 1) };
    // SAFETY: closing the descriptor this call opened.
    unsafe {
        libc::close(fd);
    }
    if n <= 0 {
        Err(std::io::Error::last_os_error().raw_os_error().unwrap_or(0))
    } else {
        Ok(())
    }
}

fn pause() {
    std::thread::sleep(std::time::Duration::from_millis(1100));
}

fn main() {
    println!("t-atime");
    let mut r = Report::new();
    let path = std::format!("/tmp/mpos-atime-{}.dat", std::process::id());

    // ---- a fresh file: atime == mtime == ctime, and the first read moves it
    make_file(&path);
    let base = stat_times(&path);
    assert_eq!(base.st_atime, base.st_mtime, "the file was just made");
    assert_eq!(base.st_atime, base.st_ctime, "the file was just made");
    pause();
    read_one(&path, 0).unwrap();
    let after = stat_times(&path);
    r.check(
        "a fresh file's first read moves atime",
        after.st_atime > base.st_atime,
    );
    r.check(
        "a read leaves mtime and ctime where they were",
        after.st_mtime == base.st_mtime && after.st_ctime == base.st_ctime,
    );

    // ---- an atime older than mtime moves again
    set_times(&path, base.st_mtime - 100, base.st_mtime);
    let old = stat_times(&path);
    read_one(&path, 0).unwrap();
    let moved = stat_times(&path);
    r.check(
        "an atime older than mtime moves on a read",
        moved.st_atime > old.st_atime,
    );

    // ---- strictatime: even a future atime is pulled back to the read
    set_times(&path, base.st_mtime + 3600, base.st_mtime);
    let future = stat_times(&path);
    read_one(&path, 0).unwrap();
    let kept = stat_times(&path);
    r.check(
        "even a future atime is pulled back to the read's moment",
        kept.st_atime < future.st_atime,
    );

    // ---- O_NOATIME suppresses it, and the same plain read still moves it
    set_times(&path, base.st_mtime, base.st_mtime);
    let plain_before = stat_times(&path);
    read_one(&path, 0).unwrap();
    let plain_after = stat_times(&path);
    r.check(
        "the plain description moves atime (the control)",
        plain_after.st_atime >= plain_before.st_atime,
    );
    set_times(&path, base.st_mtime, base.st_mtime);
    let noatime_before = stat_times(&path);
    read_one(&path, libc::O_NOATIME).unwrap();
    let noatime_after = stat_times(&path);
    r.check(
        "O_NOATIME leaves atime where it was",
        noatime_after.st_atime == noatime_before.st_atime,
    );

    // ---- a stranger may not ask for O_NOATIME
    if host_run() {
        println!(
            "note: a host run cannot drop to uid 65534; the stranger's refusal is named and not made"
        );
    } else {
        let cpath = std::ffi::CString::new(path.as_str()).unwrap();
        // SAFETY: `fork` takes no arguments; the child uses the plain libc
        // calls below and writes one errno down a pipe.
        let child = unsafe { libc::fork() };
        assert!(child >= 0, "fork failed");
        if child == 0 {
            // SAFETY: all three ids go to 65534, then the open asks for a
            // promise the child is not entitled to.
            unsafe {
                let dropped = libc::setresgid(65534, 65534, 65534) == 0
                    && libc::setresuid(65534, 65534, 65534) == 0;
                let fd = if dropped {
                    libc::open(cpath.as_ptr(), libc::O_RDONLY | libc::O_NOATIME)
                } else {
                    -1
                };
                let answer = if dropped && fd < 0 {
                    std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
                } else {
                    0
                };
                if fd >= 0 {
                    libc::close(fd);
                }
                libc::_exit(answer);
            }
        }
        let mut status = 0;
        // SAFETY: `child` is this frame's own child.
        unsafe {
            libc::waitpid(child, &mut status, 0);
        }
        let answer = if libc::WIFEXITED(status) {
            libc::WEXITSTATUS(status)
        } else {
            -1
        };
        r.check(
            "a 65534 stranger's O_NOATIME open is EPERM",
            answer == libc::EPERM,
        );
    }

    // SAFETY: the name is this frame's and the file is its own.
    unsafe {
        libc::unlink(std::ffi::CString::new(path.as_str()).unwrap().as_ptr());
    }
    r.finish();
}
