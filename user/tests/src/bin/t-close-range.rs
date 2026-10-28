//! t-close-range - `close_range(2)`: a span of descriptors, closed or marked.
//!
//! Pinned here is the shape measured on Linux 7.2.11
//! (`build/scratch/closerange/probe.c`, `probe2.c`): a span closes exactly the
//! numbers it covers and leaves the neighbours alone, `first > last` is
//! `EINVAL`, `~0` as the last number means "to the end of the table",
//! `CLOSE_RANGE_CLOEXEC` marks a span without closing it, and
//! `CLOSE_RANGE_UNSHARE` gives the caller a descriptor table of its own before
//! the range is applied - so a sibling thread's copy survives.
//!
//! The claims that need a sibling are the reason this program uses threads at
//! all: without one there is nothing to be shared with, and `UNSHARE` would
//! look like a no-op whether or not it did anything.
//!
//! On a host run this is the reference direction - Linux answers every claim,
//! so the same binary passing there says the expectations are Linux's and not
//! this kernel's.

use mpos_user_tests::Report;

const CLOSE_RANGE_UNSHARE: u32 = 1 << 1;
const CLOSE_RANGE_CLOEXEC: u32 = 1 << 2;

fn close_range(first: u32, last: u32, flags: u32) -> Result<(), i32> {
    let rc = unsafe {
        libc::syscall(
            libc::SYS_close_range,
            first as libc::c_uint,
            last as libc::c_uint,
            flags as libc::c_uint,
        )
    };
    if rc < 0 {
        return Err(-std::io::Error::last_os_error().raw_os_error().unwrap_or(0));
    }
    Ok(())
}

/// A fresh descriptor over the console, so a test never has to guess a number.
///
/// Plain `i32`, not `OwnedFd`: the subject of these claims is a number that
/// `close_range` is about to close, and an owning wrapper would abort the
/// process at its own `Drop` for the closure the test just asked for.
fn dup_console() -> i32 {
    let fd = unsafe { libc::dup(0) };
    assert!(fd >= 0, "dup failed");
    fd
}

fn is_open(fd: i32) -> bool {
    (unsafe { libc::fcntl(fd, libc::F_GETFD) }) >= 0
}

fn is_cloexec(fd: i32) -> bool {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    flags >= 0 && (flags & libc::FD_CLOEXEC) != 0
}

fn number(fd: i32) -> u32 {
    fd as u32
}

/// Close a descriptor this program still owns - not the subject of a claim,
/// just tidying up after one.
fn release(fd: i32) {
    if is_open(fd) {
        unsafe { libc::close(fd) };
    }
}

// A descriptor a sibling thread closes, to show the unshare really happened.
static mut SHARED: i32 = -1;

extern "C" fn close_shared_in_thread(_arg: *mut libc::c_void) -> *mut libc::c_void {
    let fd = unsafe { SHARED };
    let rc = unsafe { libc::syscall(libc::SYS_close_range, fd as u32, fd as u32, 0u32) };
    let _ = rc;
    std::ptr::null_mut()
}

fn main() {
    println!("t-close-range");
    let mut r = Report::new();

    // ---- a span closes exactly its own numbers --------------------------
    {
        let before = dup_console();
        let a = dup_console();
        let b = dup_console();
        let after = dup_console();
        let closed = close_range(number(a), number(b), 0);
        r.check("a span close answers 0", closed.is_ok());
        r.check("the first number in the span is closed", !is_open(a));
        r.check("the last number in the span is closed", !is_open(b));
        r.check("one below the span stays open", is_open(before));
        r.check("one above the span stays open", is_open(after));
        release(before);
        release(after);
    }

    // ---- validation ----------------------------------------------------
    r.check(
        "a first number past the last is EINVAL",
        close_range(10, 5, 0) == Err(-libc::EINVAL),
    );
    r.check(
        "a flag bit outside the two known ones is EINVAL",
        close_range(900, 910, 8) == Err(-libc::EINVAL),
    );
    r.check(
        "an unknown flag bit high in the word is EINVAL",
        close_range(900, 910, 0x8000_0000) == Err(-libc::EINVAL),
    );

    // ---- a span over free slots is not an error ------------------------
    r.check(
        "a span of numbers nothing holds is 0",
        close_range(900, 910, 0).is_ok(),
    );

    // ---- CLOEXEC marks without closing ---------------------------------
    {
        let a = dup_console();
        let b = dup_console();
        let outside = dup_console();
        r.check(
            "marking a span answers 0",
            close_range(number(a), number(b), CLOSE_RANGE_CLOEXEC).is_ok(),
        );
        r.check(
            "a marked descriptor is still open",
            is_open(a) && is_open(b),
        );
        r.check(
            "both numbers in the span carry FD_CLOEXEC",
            is_cloexec(a) && is_cloexec(b),
        );
        r.check(
            "a descriptor outside the span carries no flag",
            !is_cloexec(outside),
        );
        release(a);
        release(b);
        release(outside);
    }

    // ---- UNSHARE, and the thread that proves it ------------------------
    {
        let shared = dup_console();
        let fd = shared;
        // A sibling shares this table; without UNSHARE its close would take
        // the parent's number with it.
        unsafe { SHARED = fd };
        let mut thread: libc::pthread_t = std::ptr::null_mut();
        let started = unsafe {
            libc::pthread_create(
                &mut thread,
                std::ptr::null(),
                close_shared_in_thread,
                std::ptr::null_mut(),
            )
        };
        assert_eq!(started, 0, "pthread_create failed");
        unsafe { libc::pthread_join(thread, std::ptr::null_mut()) };
        r.check(
            "a sibling thread's close_range closes the shared number",
            !is_open(shared),
        );

        // Now the same shape with UNSHARE in the child: the parent keeps its
        // own number, because the child got a table of its own first.
        let kept = dup_console();
        let mut fork_ok = false;
        let pid = unsafe { libc::fork() };
        if pid == 0 {
            let _ = close_range(number(kept), number(kept), CLOSE_RANGE_UNSHARE);
            unsafe { libc::_exit(0) };
        }
        if pid > 0 {
            let mut status = 0;
            unsafe { libc::waitpid(pid, &mut status, 0) };
            fork_ok = is_open(kept);
        }
        r.check(
            "a child's UNSHARE range leaves the parent's number open",
            fork_ok,
        );
        release(kept);
    }

    // ---- ~0 means "to the end of the table" ----------------------------
    // Run last, in a forked child: it closes everything above the standard
    // three, this program's own pipes included.
    {
        let pid = unsafe { libc::fork() };
        if pid == 0 {
            let extra = dup_console();
            let n = number(extra);
            let ok = close_range(3, u32::MAX, 0).is_ok();
            let closed = (unsafe { libc::fcntl(n as i32, libc::F_GETFD) }) < 0;
            unsafe { libc::_exit(if ok && closed { 0 } else { 1 }) };
        }
        let mut status = 0;
        unsafe { libc::waitpid(pid, &mut status, 0) };
        r.check(
            "a range ending at ~0 closes every descriptor above the standard three",
            libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0,
        );
    }

    r.finish()
}
