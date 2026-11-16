//! t-kcmp - `kcmp(2)`, comparing two processes' kernel resources by identity.
//!
//! It was undeclared, so the dispatch answered `ENOSYS`. The callers are
//! checkpoint/restore tools (`criu` and friends): what they need is "is this
//! descriptor the *same open file* in both processes", and no other door answers
//! that.
//!
//! # The answer is not a verdict
//!
//! A successful `kcmp` returns the comparison of two kernel pointers: **`0` equal,
//! `1` the first is lower, `2` the second is lower, `3` "equal but distinct"**. So
//! `0` here means *equal*, not *success* - the opposite of every other call in
//! this interface - and `1`/`2` both mean *not equal*, with **which** one
//! depending on address order and therefore not being a contract. The claims
//! below assert equality or inequality, never "returns 2".
//!
//! Measured on Linux 7.3.4 (`build/scratch/opathmmap/{kcmp,kcmp2}.c`):
//!
//! | what | Linux |
//! |---|---|
//! | `KCMP_FILE` (0) on one fd | `0` |
//! | `KCMP_FILE` on `dup`'d fds | `0` - one open file description |
//! | `KCMP_FILE` on two `open`s | **not `0`** |
//! | `KCMP_FILE` on a descriptor the process lacks | `EBADF` |
//! | `KCMP_VM`/`FS`/`SIGHAND`/`IO` on self | `0` |
//! | the same across `fork` | **not `0`** - `fork` copies, it does not share |
//! | a type at or past `KCMP_TYPES` (8) | `EINVAL` |
//! | a pid naming nothing, including `0` | `ESRCH` |
//!
//! | `KCMP_SYSVSEM` (6) on self | `0` - served, whatever the index |
//! | `KCMP_EPOLL_TFD` (7) with a `NULL` index | `EFAULT` - the index is a **pointer** |
//!
//! The last row corrected a draft that refused both: `SYSVSEM` is **served** (a
//! task's semaphore-undo state belongs to its group, so two lookups in one group
//! are one object), and `EPOLL_TFD`'s refusal is about its *argument shape* - a
//! `struct epoll_slot` pointer - not about the type being unknown.
//!
//! Numbers from libc's table; `rustix` binds no `kcmp`.

use mpos_user_tests::Report;

const KCMP_FILE: i32 = 0;
const KCMP_VM: i32 = 1;
const KCMP_FILES: i32 = 2;
const KCMP_FS: i32 = 3;
const KCMP_SIGHAND: i32 = 4;
const KCMP_IO: i32 = 5;
const KCMP_SYSVSEM: i32 = 6;
const KCMP_EPOLL_TFD: i32 = 7;

fn kcmp(pid1: i32, pid2: i32, kind: i32, idx1: usize, idx2: usize) -> isize {
    // SAFETY: five words, the shape the call takes; the indices are only
    // dereferenced by the kernel for the types that use them as descriptors.
    unsafe { libc::syscall(libc::SYS_kcmp, pid1, pid2, kind, idx1, idx2) as isize }
}
fn errno() -> Option<i32> {
    std::io::Error::last_os_error().raw_os_error()
}
fn refused(rc: isize, want: i32) -> bool {
    rc == -1 && errno() == Some(want)
}
/// The comparison's **equality**, which is the only part of its answer that is a
/// contract: `0` is equal and every other non-negative value is not.
fn equal(rc: isize) -> bool {
    rc == 0
}

fn main() {
    println!("t-kcmp");
    let mut r = Report::new();

    let me = unsafe { libc::getpid() };
    let one = std::ffi::CString::new("/tmp/t-kcmp.one").unwrap();
    let two = std::ffi::CString::new("/tmp/t-kcmp.two").unwrap();
    // SAFETY: paths this test owns.
    let fd = unsafe { libc::open(one.as_ptr(), libc::O_RDWR | libc::O_CREAT, 0o640) };
    // SAFETY: a duplicate of a live descriptor.
    let dup = if fd >= 0 {
        unsafe { libc::dup(fd) }
    } else {
        -1
    };
    // SAFETY: a second path this test owns.
    let other = unsafe { libc::open(two.as_ptr(), libc::O_RDWR | libc::O_CREAT, 0o640) };
    r.check(
        &format!("three descriptors are open ({fd}, {dup}, {other})"),
        fd >= 0 && dup >= 0 && other >= 0,
    );
    if fd < 0 || dup < 0 || other < 0 {
        r.finish();
    }

    // === KCMP_FILE: one open file description or two =======================
    r.check(
        "KCMP_FILE on one fd compared with itself is 0",
        equal(kcmp(me, me, KCMP_FILE, fd as usize, fd as usize)),
    );
    r.check(
        "KCMP_FILE on dup'd fds is 0 - one open file description",
        equal(kcmp(me, me, KCMP_FILE, fd as usize, dup as usize)),
    );
    // **The claim the call exists for**: two `open`s of two paths are different
    // descriptions. Asserted as *not equal*, because which of `1`/`2` comes back
    // is address order.
    let distinct = kcmp(me, me, KCMP_FILE, fd as usize, other as usize);
    r.check(
        &format!("KCMP_FILE on two open()s is not equal ({distinct})"),
        distinct > 0,
    );

    // === the group-owned objects: equal on self ============================
    for (label, kind) in [
        ("KCMP_FILES", KCMP_FILES),
        ("KCMP_VM", KCMP_VM),
        ("KCMP_FS", KCMP_FS),
        ("KCMP_SIGHAND", KCMP_SIGHAND),
        ("KCMP_IO", KCMP_IO),
    ] {
        r.check(
            &format!("{label} on self/self is 0"),
            equal(kcmp(me, me, kind, 0, 0)),
        );
    }

    // === and across fork, the copies are not the same object ===============
    //
    // `fork` builds a new address space, a new handler table and a new cwd
    // triple; it does **not** share them (`clone` with `CLONE_VM` does, and that
    // is a different call). So the comparison is *not equal* - again asserted as
    // inequality rather than a value.
    let kid = unsafe { libc::fork() };
    if kid == 0 {
        // SAFETY: the child parks until the parent kills it.
        unsafe {
            libc::pause();
            libc::_exit(0);
        }
    }
    if kid > 0 {
        std::thread::sleep(std::time::Duration::from_millis(150));
        for (label, kind) in [
            ("KCMP_VM", KCMP_VM),
            ("KCMP_FS", KCMP_FS),
            ("KCMP_SIGHAND", KCMP_SIGHAND),
        ] {
            let rc = kcmp(me, kid, kind, 0, 0);
            r.check(&format!("{label} across fork is not equal ({rc})"), rc > 0);
        }
        // **But the descriptor table is copied, so a descriptor inherited across
        // `fork` still names the same open file description** - the judgement
        // `criu` actually makes.
        r.check(
            "a descriptor inherited across fork is still the same open file",
            equal(kcmp(me, kid, KCMP_FILE, fd as usize, fd as usize)),
        );
        // SAFETY: the child this block forked.
        unsafe {
            libc::kill(kid, libc::SIGKILL);
            libc::waitpid(kid, core::ptr::null_mut(), 0);
        }
    }

    // === the refusals ======================================================
    r.check(
        "a descriptor the process lacks is EBADF",
        refused(kcmp(me, me, KCMP_FILE, 9999, fd as usize), libc::EBADF),
    );
    r.check(
        "a type at KCMP_TYPES is EINVAL",
        refused(kcmp(me, me, 8, 0, 0), libc::EINVAL),
    );
    r.check(
        "a negative type is EINVAL",
        refused(kcmp(me, me, -1, 0, 0), libc::EINVAL),
    );
    // **`KCMP_SYSVSEM` is served**, not refused - a draft assumed otherwise and
    // the measurement (`kcmp3.c`) says `self/self` is `0` for any index. A task's
    // semaphore-undo state is its group's, so one group is one object.
    r.check(
        "KCMP_SYSVSEM on self/self is 0 - served, not refused",
        equal(kcmp(me, me, KCMP_SYSVSEM, 0, 0)),
    );
    r.check(
        "and it does not depend on the index",
        equal(kcmp(me, me, KCMP_SYSVSEM, 7, 7)),
    );
    // `KCMP_EPOLL_TFD`'s `idx` is a **pointer** to a `struct epoll_slot`, so a
    // `NULL` one is `EFAULT` - which is Linux's own answer (`kcmp3.c`), not this
    // door refusing the type.
    r.check(
        "KCMP_EPOLL_TFD with a NULL index is EFAULT",
        refused(kcmp(me, me, KCMP_EPOLL_TFD, 0, 0), libc::EFAULT),
    );
    r.check(
        "a pid naming nothing is ESRCH",
        refused(
            kcmp(me, 99999, KCMP_FILE, fd as usize, fd as usize),
            libc::ESRCH,
        ),
    );
    // `0` is not "the caller" here - it names no process (measured).
    r.check(
        "and pid 0 is ESRCH, not the caller",
        refused(
            kcmp(0, me, KCMP_FILE, fd as usize, fd as usize),
            libc::ESRCH,
        ),
    );

    for d in [fd, dup, other] {
        // SAFETY: descriptors this test opened and has not closed.
        unsafe { libc::close(d) };
    }
    let _ = std::fs::remove_file("/tmp/t-kcmp.one");
    let _ = std::fs::remove_file("/tmp/t-kcmp.two");
    r.finish()
}
