//! t-nice-authority - `setpriority` asks *whose* task, and answers two different
//! errnos when it refuses.
//!
//! The call used to accept the whole range from anybody and write it
//! (`docs/notes/` §6.93): a program running as one user could lower the nice of
//! another user's task and the call answered `0`. Measured on the host
//! (`build/scratch/niceperm/{y,g2,s2,w,x2}.c`, musl static, uid 1000, empty
//! `CapEff`), Linux separates the two refusals:
//!
//! * **`EPERM` is ownership** - a target that is not the caller's (another
//!   user's task) is refused whatever the value, in either direction. It is not
//!   blood relation: a scan of every same-uid process this host would show
//!   (`w.c`) had `setpriority(PRIO_PROCESS, <pid>, <its own nice>)` answer `0` for
//!   484 of them and refuse only the ones holding a capability.
//! * **`EACCES` is direction** - a task the caller *does* own may not have its
//!   nice raised (a smaller number) without `CAP_SYS_NICE`, and that is a
//!   different answer from the ownership one.
//!
//! Both are claimed here by a fixture that drops to `nobody` and then asks about
//! two targets: itself, and its parent, which stayed root. The fixture reports
//! the uid it actually ended up as, because a host run has no privilege to drop
//! and the child then stays the caller's own user - a same-uid parent *is*
//! served, which is the same ownership rule answering the other way.
//!
//! `getpriority` is the contrast and asks for nothing: the dropped child reads
//! its root parent's nice happily, which is why the write side needs the check
//! and the read side does not.

use mpos_user_tests::Report;

/// Linux's `which` word for "one process".
const PRIO_PROCESS: libc::c_int = 0;

/// Linux's "you may not touch this task", and its "you may not raise it".
const EPERM: i32 = 1;
const EACCES: i32 = 13;

/// The id the fixture drops to - `nobody`, present in both images.
const NOBODY: u32 = 65_534;

/// `errno` of the call just made.
fn last_errno() -> i32 {
    std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
}

/// `setpriority(PRIO_PROCESS, who, nice)`, as the **raw** call, returning
/// `(ret, errno)` so a reading cannot be lost to a later call clobbering
/// `errno`.
///
/// Raw because the C wrapper's `which` is typed by the platform - `c_int` where
/// the payload's `libc` puts a signed word, `u32` where the host's gnu one does -
/// so one source cannot spell the constant in a way both accept. The syscall
/// takes plain `c_long` arguments and is the same number on both, which is also
/// the convention `t-priority` established for this call.
fn set(pid: i32, nice: i32) -> (isize, i32) {
    let ret = unsafe {
        libc::syscall(
            libc::SYS_setpriority,
            PRIO_PROCESS as libc::c_long,
            pid as libc::c_long,
            nice as libc::c_long,
        ) as isize
    };
    (ret, last_errno())
}

/// Fork a child that drops to `nobody` if it can, then measures four things
/// about itself and its parent (which stays root), writing them back as
/// `uid=… lower=… lowererrno=… raise=… raiseerrno=… parent=… parenterrno=… getp=…`.
///
/// The order matters: the lowering comes first and sets a known nice (19), so
/// the raise after it is a raise on every machine. A host whose processes are
/// already at 19 reads the lowering back as an equal-nice no-op, which Linux
/// accepts - the claim is "served", not "moved".
fn authority_probe() -> String {
    let mut fds = [0i32; 2];
    assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0, "pipe");
    let pid = unsafe { libc::fork() };
    assert!(pid >= 0, "fork");
    if pid == 0 {
        unsafe { libc::close(fds[0]) };
        unsafe {
            libc::setgroups(0, core::ptr::null());
            libc::setgid(NOBODY);
            libc::setuid(NOBODY);
        }
        let uid = unsafe { libc::getuid() };
        let parent = unsafe { libc::getppid() };
        let (lower, lower_errno) = set(0, 19);
        let (raise, raise_errno) = set(0, 0);
        let (on_parent, parent_errno) = set(parent, 19);
        // `getpriority` answers `-1` both for a real nice of -1 and for a
        // refusal, so Linux's only way to tell them apart is a zeroed `errno`
        // before the call - that is the shape a caller uses, and it is what the
        // claim below has to read.
        unsafe { *libc::__errno_location() = 0 };
        let getp = unsafe {
            libc::syscall(
                libc::SYS_getpriority,
                PRIO_PROCESS as libc::c_long,
                parent as libc::c_long,
            )
        } as i32;
        let get_errno = last_errno();
        let line = format!(
            "uid={uid} lower={lower} lowererrno={lower_errno} raise={raise} \
             raiseerrno={raise_errno} parent={on_parent} parenterrno={parent_errno} \
             getp={getp} geterrno={get_errno}"
        );
        unsafe {
            libc::write(fds[1], line.as_ptr() as *const libc::c_void, line.len());
            libc::close(fds[1]);
            libc::_exit(0);
        }
    }
    unsafe { libc::close(fds[1]) };
    let mut out = Vec::new();
    let mut buf = [0u8; 160];
    loop {
        let n = unsafe { libc::read(fds[0], buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
        if n <= 0 {
            break;
        }
        out.extend_from_slice(&buf[..n as usize]);
    }
    unsafe { libc::close(fds[0]) };
    let mut status = 0;
    unsafe { libc::waitpid(pid, &mut status, 0) };
    String::from_utf8_lossy(&out).into_owned()
}

/// One `key=value` field out of [`authority_probe`]'s line.
fn field(line: &str, key: &str) -> Option<i32> {
    line.split_whitespace()
        .find_map(|word| word.strip_prefix(key)?.strip_prefix('=')?.parse().ok())
}

fn main() {
    println!("t-nice-authority");
    let mut r = Report::new();

    let reading = authority_probe();
    let uid = field(&reading, "uid").unwrap_or(-1);
    let mine = unsafe { libc::getuid() } as i32;
    let dropped = uid != mine;

    // ---- direction: a raise is EACCES, and a lowering is always served -----
    // This is the half a caller can hear on a task it owns. The child lowered
    // itself to 19 first, so the raise back to 0 is a raise on every machine -
    // including one whose processes already sit at 19, where the lowering is an
    // accepted no-op.
    r.check(
        &format!("lowering a task's own nice is served (uid {uid})"),
        field(&reading, "lower") == Some(0),
    );
    r.check(
        &format!(
            "and raising it again is refused (uid {uid}, errno {:?})",
            field(&reading, "raiseerrno")
        ),
        field(&reading, "raiseerrno") == Some(EACCES),
    );

    // ---- ownership: somebody else's task is EPERM -------------------------
    if dropped {
        // The parent stayed root, so this is the other-user case. Note this is
        // a *lowering* on a task the caller does not own: the direction is the
        // permissive one and Linux still refuses it, which is what makes
        // ownership a separate question from direction.
        r.check(
            &format!("a program of another user cannot lower its root parent's nice (uid {uid})"),
            field(&reading, "parenterrno") == Some(EPERM),
        );
    } else {
        // A host run is not root, so the drop failed and the child is the same
        // user as its parent: same uid is served whatever the relation.
        r.check(
            &format!("a child of the same user lowers its parent's nice (uid {uid})"),
            field(&reading, "parent") == Some(0),
        );
    }

    // ---- the read side asks for nothing -----------------------------------
    // Linux's `getpriority` has no authority check at all, and this is the
    // contrast that says where the missing check belonged: the write needs a
    // question, the read does not.
    // A refusal is `-1` with a live `errno`; a real nice of `-1` leaves `errno`
    // alone. That is Linux's own documented test, and it is why the reading
    // carries both numbers.
    let served = field(&reading, "getp") != Some(-1) || field(&reading, "geterrno") == Some(0);
    r.check(
        &format!(
            "and a getter asks for no authority at all (uid {uid}, read {})",
            reading.trim()
        ),
        served,
    );

    r.finish();
}
