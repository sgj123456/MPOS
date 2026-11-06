//! t-affinity-rules - the two affinity calls, against the rules Linux actually
//! answers with, measured on the host (`build/scratch/aff/q2.c`, `r2.c`,
//! `r4.c`, musl static, 16-CPU host).
//!
//! The shape this pins is not "which CPUs" - one CPU, bit 0 - but every rule
//! *around* the answer, because those are what a caller branches on:
//!
//! * a `pid` names a task, and a number that no longer names one is `ESRCH` -
//!   a collected child included. Before this pin, both calls ignored the pid
//!   entirely and answered `0` for a stranger's number.
//! * the getter's length is a whole number of words (`len % 8 == 0`, at least
//!   one word); the setter takes **any** length from 1 up, because it only
//!   reads the bits the caller gave it.
//! * the order of refusals differs by direction, and a caller does see it: the
//!   setter copies the mask first (`EFAULT`), then resolves the pid
//!   (`ESRCH`), then judges the mask (`EINVAL`); the getter judges the length
//!   (`EINVAL`) before it looks at the pid at all.
//! * `-1` is not "the caller" here - Linux answers `ESRCH` (measured), unlike
//!   `wait4`'s meaning of the same number.
//!
//! Two things are deliberately *not* claimed. The answer's width is the byte
//! count of the mask filled (8 here, 64 for a caller's `cpu_set_t` on a
//! 16-CPU host) - `t-sched-getaffinity` already pins that shape.
//!
//! The permission half **is** claimed, by a fixture: a child that drops to
//! `nobody` and asks the two calls about its own root parent. Measured on the
//! host (`build/scratch/affperm/s.c`, `t.c`, `v.c`, as uid 1000 with an empty
//! `CapEff`):
//!
//! * the setter for another user's task is `EPERM` - pid 1, and this file's
//!   own parent shell when it belongs to another user;
//! * the setter for **the same** uid is served whether or not the task is the
//!   caller's child, so blood relation is not the rule;
//! * the getter asks for nothing and reads root's task happily.
//!
//! The fixture reports the uid it actually ended up as, because on a host run
//! this program is not root and the drop fails: the child is then the same
//! user as its parent and *is* allowed, which is the same rule answering the
//! other way. That is why the claim reads the reported uid rather than
//! assuming the drop happened.

use mpos_user_tests::Report;

/// Linux's answer for a number that names no live task.
const ESRCH: i32 = 3;
const EPERM: i32 = 1;
const EINVAL: i32 = 22;
const EFAULT: i32 = 14;

fn refused(ret: isize, want: i32) -> bool {
    ret == -1 && std::io::Error::last_os_error().raw_os_error() == Some(want)
}

fn get(pid: i32, mask: &mut [u8]) -> isize {
    unsafe {
        libc::syscall(
            libc::SYS_sched_getaffinity,
            pid,
            mask.len(),
            mask.as_mut_ptr() as usize,
        ) as isize
    }
}

fn set(pid: i32, mask: &[u8]) -> isize {
    unsafe {
        libc::syscall(
            libc::SYS_sched_setaffinity,
            pid,
            mask.len(),
            mask.as_ptr() as usize,
        ) as isize
    }
}

/// The raw call with an explicit length and pointer, so the length and the
/// buffer can disagree the way the refusal-order claims need them to.
fn set_raw(pid: i32, len: usize, mask: usize) -> isize {
    unsafe { libc::syscall(libc::SYS_sched_setaffinity, pid, len, mask) as isize }
}

fn get_raw(pid: i32, len: usize, mask: usize) -> isize {
    unsafe { libc::syscall(libc::SYS_sched_getaffinity, pid, len, mask) as isize }
}

/// A child that has been forked and collected: its number names nothing now.
fn collected_child() -> i32 {
    let pid = unsafe { libc::fork() };
    assert!(pid >= 0, "fork");
    if pid == 0 {
        unsafe { libc::_exit(0) };
    }
    let mut status = 0;
    unsafe { libc::waitpid(pid, &mut status, 0) };
    pid
}

/// The ids the authority fixture drops to - `nobody`, present in both images.
const NOBODY: u32 = 65_534;

/// `errno` of the call just made.
fn last_errno() -> i32 {
    std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
}

/// Fork a child that drops to `nobody` if it can, asks the two calls about
/// `target` (its own parent, which stays root), and writes the whole reading
/// back through a pipe as `uid=… get=… geterrno=… set=… seterrno=…`.
///
/// The drop is best-effort on purpose: a host run has no privilege to drop, so
/// the child stays the caller's own user - and then the answer it should get is
/// the *allowed* one, which is the same rule read from the other side.
fn authority_probe(target: i32) -> String {
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
        let mut mask = [0u8; 8];
        let got = get(target, &mut mask);
        let get_errno = last_errno();
        let put = set(target, &[1u8, 0, 0, 0]);
        let set_errno = last_errno();
        let line =
            format!("uid={uid} get={got} geterrno={get_errno} set={put} seterrno={set_errno}");
        unsafe {
            libc::write(fds[1], line.as_ptr() as *const libc::c_void, line.len());
            libc::close(fds[1]);
            libc::_exit(0);
        }
    }
    unsafe { libc::close(fds[1]) };
    let mut out = Vec::new();
    let mut buf = [0u8; 128];
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
    println!("t-affinity-rules");
    let mut r = Report::new();
    let me = unsafe { libc::getpid() };

    // ---- the setter takes any length that carries a bit ------------------
    // Linux reads only the bytes the caller gave it, so a one-byte request
    // that keeps CPU 0 is a request it can honour. This kernel used to insist
    // on a whole word and answer EINVAL for the same call.
    let mut one = [0u8; 1];
    one[0] = 1;
    r.check(
        "a setter takes a length of one byte when it names CPU 0",
        set(me, &one) == 0,
    );
    let four = [1u8, 0, 0, 0];
    r.check(
        "and a length of four bytes is not too short to honour",
        set(me, &four) == 0,
    );

    // A mask with no usable bit is still refused, and that rule survives the
    // shorter lengths: nothing in it names a CPU this machine has.
    let empty = [0u8; 8];
    r.check(
        "a mask that names no CPU is refused",
        refused(set(me, &empty), EINVAL),
    );

    // ---- the getter's length is whole words ------------------------------
    let mut buf = [0u8; 32];
    r.check(
        "a getter's length must be a whole number of words",
        refused(get_raw(me, 12, buf.as_mut_ptr() as usize), EINVAL),
    );
    r.check(
        "and a zero length is refused before anything else",
        refused(get_raw(me, 0, buf.as_mut_ptr() as usize), EINVAL),
    );
    r.check(
        "two words answer the one word this machine fills",
        get(me, &mut buf[0..16]) == 8,
    );

    // ---- a number that names no task is ESRCH ----------------------------
    let gone = collected_child();
    let mut mask = [0u8; 8];
    r.check(
        "a getter for a collected child answers ESRCH",
        refused(get(gone, &mut mask), ESRCH),
    );
    r.check(
        "a setter for a collected child answers ESRCH",
        refused(set(gone, &[1u8; 8]), ESRCH),
    );
    r.check(
        "a pid of -1 is not the caller",
        refused(get(-1, &mut mask), ESRCH),
    );

    // ---- the two directions order their refusals differently -------------
    // Setter: the mask is copied before the pid is looked up, so a NULL with a
    // usable length is EFAULT even for a number that names nothing; with no
    // length there is nothing to copy and the pid is what answers.
    r.check(
        "a setter copies its mask before it resolves the pid",
        refused(set_raw(0x7FFF_FFFF, 8, 0), EFAULT),
    );
    r.check(
        "a setter with no mask at all is refused by the pid, not the length",
        refused(set_raw(0x7FFF_FFFF, 0, 0), ESRCH),
    );
    // Getter: the length is judged first, so the same bad number with a
    // zero-length buffer is EINVAL and never fills anything in.
    r.check(
        "a getter judges its length before it resolves the pid",
        refused(get_raw(0x7FFF_FFFF, 0, 0), EINVAL),
    );
    r.check(
        "a getter reads its pid before it touches the caller's buffer",
        refused(get_raw(0x7FFF_FFFF, 8, 0), ESRCH),
    );

    // ---- the two directions are not equally privileged -------------------
    // A child that has dropped to another user asks the same two calls about
    // its own parent, which stayed root. Linux serves the read and refuses the
    // write (`EPERM`, measured `v.c`), so the number is the whole question for
    // the getter and only half of it for the setter.
    let reading = authority_probe(me);
    let uid = field(&reading, "uid").unwrap_or(-1);
    let mine = unsafe { libc::getuid() } as i32;
    let dropped = uid != mine;
    if dropped {
        r.check(
            &format!("a dropped program cannot retarget its root parent (uid {uid})"),
            field(&reading, "seterrno") == Some(EPERM),
        );
    } else {
        // A host run is not root, so the drop fails and the child is the same
        // user as its parent: same uid is served, which is the same rule.
        r.check(
            &format!("a child of the same user retargets its parent (uid {uid})"),
            field(&reading, "set") == Some(0),
        );
    }
    r.check(
        &format!("and a getter asks for no authority at all ({reading})"),
        field(&reading, "get").map(|n| n > 0).unwrap_or(false),
    );

    r.finish();
}
