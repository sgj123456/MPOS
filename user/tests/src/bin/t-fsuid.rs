//! t-fsuid - `setfsuid(2)` really moves the id a **file** operation acts as,
//! and a caller that may not move it is told the old value rather than refused.
//!
//! Measured on a current host first (`.workbuddy/probe_fsuid.c`, Linux 7.2.11,
//! a container's root so the drop is reachable):
//!
//! * root `setfsuid(65534)` returns `0` and moves the id; the root caller keeps
//!   `CAP_DAC_OVERRIDE`, so the bits do not refuse it - the fsuid is observable
//!   where the bits are the *only* answer: a node created under it is owned by
//!   65534, and `setfsuid(0)` returns `65534`;
//! * an unprivileged caller's `setfsuid(0)` returns its own id and **leaves the
//!   id alone** - no `EPERM`; the call's answer is the previous value either way;
//! * `setresuid`/`setuid`/`setreuid` reset fsuid to the new euid
//!   *unconditionally*, even when the euid did not move, and `setgroups` does
//!   not touch it;
//! * a file created while fsuid is 65534 is owned by 65534.
//!
//! This kernel used to answer the euid and store nothing, while its permission
//! checks already ran on the effective pair - a no-op dressed as a getter.

use mpos_user_tests::{Report, host_run};

fn stat_uid(path: &str) -> u32 {
    let cpath = std::ffi::CString::new(path).unwrap();
    let mut st: libc::stat = unsafe { core::mem::zeroed() };
    // SAFETY: both pointers are this frame's; `stat` fills only `st`.
    assert_eq!(unsafe { libc::stat(cpath.as_ptr(), &mut st) }, 0, "stat");
    st.st_uid
}

fn make(path: &str) {
    let cpath = std::ffi::CString::new(path).unwrap();
    // SAFETY: the path and mode are this frame's.
    let fd = unsafe {
        libc::open(
            cpath.as_ptr(),
            libc::O_WRONLY | libc::O_CREAT | libc::O_TRUNC,
            0o600,
        )
    };
    assert!(fd >= 0, "create {path}");
    // SAFETY: `fd` was just opened.
    unsafe {
        libc::write(fd, b"x".as_ptr() as *const libc::c_void, 1);
        libc::close(fd);
    }
}

fn open_result(path: &str) -> i32 {
    let cpath = std::ffi::CString::new(path).unwrap();
    // SAFETY: the path is this frame's; the descriptor is closed here.
    let fd = unsafe { libc::open(cpath.as_ptr(), libc::O_RDONLY) };
    if fd < 0 {
        return std::io::Error::last_os_error().raw_os_error().unwrap_or(0);
    }
    // SAFETY: closing the descriptor this call opened.
    unsafe {
        libc::close(fd);
    }
    0
}

fn main() {
    println!("t-fsuid");
    let mut r = Report::new();
    let me = unsafe { libc::getuid() };
    let file = std::format!("/tmp/mpos-fsuid-{}", std::process::id());

    if host_run() {
        // The unprivileged half, which is the whole of what a host run can
        // reach: the ids do not move, and the answer is the old one.
        r.check(
            "setfsuid(0) answers the caller's own id on a host run",
            unsafe { libc::setfsuid(0) } as u32 == me,
        );
        r.check(
            "and leaves the filesystem id where it was",
            unsafe { libc::setfsuid(me) } as u32 == me,
        );
        make(&file);
        r.check(
            "a file the caller makes is owned by that same id",
            stat_uid(&file) == me,
        );
        unsafe { libc::unlink(std::ffi::CString::new(file.as_str()).unwrap().as_ptr()) };
        r.finish();
    }

    // ---- root: the id moves, and the file checks follow it ----------------
    make(&file);
    r.check(
        "setfsuid(65534) answers the previous id, 0",
        unsafe { libc::setfsuid(65534) } == 0,
    );
    // **Not** `EACCES`: `setfsuid` moves only the filesystem id, so the root
    // caller keeps `CAP_DAC_OVERRIDE` and still opens the file. Measured on a
    // current host (`probe_fsuid.c`): only the *unprivileged* caller is refused
    // by the bits; root's capability is what the bits are an alternative to.
    r.check(
        "setfsuid does not take CAP_DAC_OVERRIDE away, so the root caller still opens it",
        open_result(&file) == 0,
    );
    r.check(
        "setfsuid(0) answers the previous id, 65534",
        unsafe { libc::setfsuid(0) } == 65534,
    );
    r.check("and the file opens again", open_result(&file) == 0);

    // ---- a created node is stamped with the filesystem id ----------------
    unsafe {
        libc::setfsuid(65534);
    }
    let owned = std::format!("{file}-owned");
    make(&owned);
    r.check(
        "a file created under fsuid 65534 is owned by 65534",
        stat_uid(&owned) == 65534,
    );
    unsafe {
        libc::setfsuid(0);
        libc::unlink(std::ffi::CString::new(owned.as_str()).unwrap().as_ptr());
    }

    // ---- setresuid resets it unconditionally -----------------------------
    unsafe {
        libc::setfsuid(65534);
    }
    unsafe {
        libc::setresuid(0, 0, 0);
    }
    r.check(
        "setresuid(0,0,0) with an unchanged euid carried fsuid back to 0",
        unsafe { libc::setfsuid(0) } == 0,
    );

    // ---- setgroups does not touch it -------------------------------------
    unsafe {
        libc::setfsuid(65534);
        libc::setgroups(0, core::ptr::null());
    }
    r.check(
        "setgroups(0, NULL) leaves the filesystem id alone",
        unsafe { libc::setfsuid(65534) } == 65534,
    );
    unsafe {
        libc::setfsuid(0);
    }

    // ---- a caller that may not move it is not refused --------------------
    // SAFETY: `fork` takes no arguments; the child drops and probes, then exits.
    let child = unsafe { libc::fork() };
    assert!(child >= 0, "fork failed");
    if child == 0 {
        // SAFETY: the ids and the probes are the child's own.
        unsafe {
            let dropped = libc::setresgid(65534, 65534, 65534) == 0
                && libc::setresuid(65534, 65534, 65534) == 0;
            // setfsgid first, while CAP_SETGID is still held: drop clears both
            // sets, so the probe below is the unprivileged one.
            let answer = if dropped { libc::setfsuid(0) } else { -1 };
            let after = libc::setfsuid(65534);
            let ok = dropped && answer == 65534 && after == 65534;
            libc::_exit(if ok { 0 } else { 1 });
        }
    }
    let mut status = 0;
    // SAFETY: `child` is this frame's own child.
    unsafe {
        libc::waitpid(child, &mut status, 0);
    }
    r.check(
        "a 65534 caller's setfsuid(0) answers 65534 and changes nothing",
        libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0,
    );

    // ---- the group half ---------------------------------------------------
    r.check(
        "setfsgid(65534) answers the previous id, 0",
        unsafe { libc::setfsgid(65534) } == 0,
    );
    r.check(
        "setfsgid(0) answers the previous id, 65534",
        unsafe { libc::setfsgid(0) } == 65534,
    );

    unsafe { libc::unlink(std::ffi::CString::new(file.as_str()).unwrap().as_ptr()) };
    r.finish();
}
