//! t-ns-ipc - the `ipcs` objects a process sees, which are an IPC namespace's
//! and not the machine's.
//!
//! `CLONE_NEWIPC` is the seventh rung of §6.3's ladder, and the last of the
//! kinds whose *contents* had to exist before the namespace could: the POSIX
//! message queues (`t-mqueue`), the SysV segments (`t-sysv-shm`), semaphore sets
//! (`t-sysv-sem`) and message queues (`t-sysv-msg`) are all served, so this
//! program holds the pair every rung claims - **what was made before the call is
//! not there after it, and what is made after it is not there for anyone else** -
//! and walks it through all four families, because Linux moves them together.
//!
//! # What was measured first
//!
//! `.workbuddy/probe_ipcns.c`, as root inside a user namespace (the only way an
//! unprivileged caller may make one on this host), recorded in `docs/notes/`
//! §6.16 - and every claim below was run against a real kernel before it was
//! pointed at this one, which is what caught three of them:
//!
//! * `unshare(CLONE_NEWIPC)` needs `CAP_SYS_ADMIN`; an unprivileged caller gets
//!   `EPERM` - the first claim, and on a host run it is the *only* one (the
//!   program notes that and exits green, the shape `t-ns-time` has);
//! * an id from the namespace left behind answers `EINVAL`, because a lookup is
//!   a lookup *in this namespace* and nothing else;
//! * a key is the namespace's too: the same key inside makes a **new** object,
//!   and the number it answers can be the same one, because the number's slot
//!   space restarts at 0 per namespace. That coincidence is why an object also
//!   carries a machine-wide handle inside the kernel, and why the claims here
//!   compare *live objects* rather than numbers;
//! * `fork` stays where its parent is, and `setns` back into the first namespace
//!   makes its objects live again while the second namespace's stop resolving;
//! * `/proc/sysvipc/*` is the **reader's** view, so a queue of the namespace
//!   left behind must have no row inside - proved by its queued byte count,
//!   since its number may be shared with an inner queue;
//! * `IPC_STAT` needs a real record: a null buffer against a *valid* id is
//!   `EFAULT`, and a probe built on `IPC_STAT, NULL` reads every live object as
//!   dead. That is the mistake this file's first draft made.

use std::io::Error;

use mpos_user_tests::Report;
use rustix::thread::{UnshareFlags, unshare_unsafe};

/// The commands this program asks for.
const IPC_RMID: i32 = 0;
const IPC_STAT: i32 = 2;
const IPC_CREAT: i32 = 0o1000;

/// A key this program owns, distinct from every other pin's.
const KEY: i32 = 0x4d50_4e53;
/// A second key, for the queue whose *queued bytes* prove which namespace a
/// `/proc/sysvipc/msg` row belongs to.
const KEY_MARKED: i32 = 0x4d50_4e54;

/// The widest SysV record this program hands to `IPC_STAT`: `msqid64_ds` is 120
/// bytes on both machines and the segment and set records are smaller, so one
/// buffer serves all three.
#[repr(C, align(8))]
struct Ds {
    bytes: [u8; 128],
}

impl Ds {
    fn new() -> Ds {
        Ds { bytes: [0; 128] }
    }
    fn ptr(&mut self) -> usize {
        self.bytes.as_mut_ptr() as usize
    }
}

fn errno() -> i32 {
    Error::last_os_error().raw_os_error().unwrap_or(0)
}

fn msgget(key: i32, flags: i32) -> i32 {
    unsafe { libc::syscall(libc::SYS_msgget, key, flags) as i32 }
}

fn msgctl(id: i32, cmd: i32, buf: usize) -> i64 {
    unsafe { libc::syscall(libc::SYS_msgctl, id, cmd, buf) as i64 }
}

/// One typed message of `len` bytes, the wire shape `msgsnd` reads.
fn msgsnd(id: i32, len: usize) -> i64 {
    #[repr(C, align(8))]
    struct M {
        mtype: i64,
        mtext: [u8; 32],
    }
    let mut m = M {
        mtype: 1,
        mtext: [b'x'; 32],
    };
    unsafe { libc::syscall(libc::SYS_msgsnd, id, &mut m as *mut M, len, 0) as i64 }
}

fn shmget(key: i32, size: usize, flags: i32) -> i32 {
    unsafe { libc::syscall(libc::SYS_shmget, key, size, flags) as i32 }
}

fn shmctl(id: i32, cmd: i32, buf: usize) -> i64 {
    unsafe { libc::syscall(libc::SYS_shmctl, id, cmd, buf) as i64 }
}

fn semget(key: i32, nsems: i32, flags: i32) -> i32 {
    unsafe { libc::syscall(libc::SYS_semget, key, nsems, flags) as i32 }
}

fn semctl(id: i32, semnum: i32, cmd: i32, arg: usize) -> i64 {
    unsafe { libc::syscall(libc::SYS_semctl, id, semnum, cmd, arg) as i64 }
}

fn cstring(s: &str) -> std::ffi::CString {
    std::ffi::CString::new(s).expect("no NUL in a test name")
}

fn mq_open(name: &str, flags: libc::c_int) -> libc::mqd_t {
    unsafe {
        libc::mq_open(
            cstring(name).as_ptr(),
            flags,
            0o600,
            std::ptr::null::<libc::mq_attr>(),
        )
    }
}

/// Whether `id` names a *live* object here, asked with a real record.
fn msg_lives(id: i32) -> bool {
    let mut ds = Ds::new();
    msgctl(id, IPC_STAT, ds.ptr()) == 0
}

fn shm_lives(id: i32) -> bool {
    let mut ds = Ds::new();
    shmctl(id, IPC_STAT, ds.ptr()) == 0
}

fn sem_lives(id: i32) -> bool {
    let mut ds = Ds::new();
    semctl(id, 0, IPC_STAT, ds.ptr()) == 0
}

/// `msg_cbytes` out of the record `IPC_STAT` filled: the queued bytes at offset
/// 72 on both machines, which is the mark a `/proc` claim and a namespace claim
/// both read.
fn msg_cbytes(id: i32) -> Option<u64> {
    const MSQ_CBYTES: usize = 72;
    let mut ds = Ds::new();
    if msgctl(id, IPC_STAT, ds.ptr()) != 0 {
        return None;
    }
    Some(u64::from_ne_bytes(
        ds.bytes[MSQ_CBYTES..MSQ_CBYTES + 8].try_into().ok()?,
    ))
}

/// A semaphore's value: `semctl(GETVAL)`.
fn sem_val(id: i32) -> Option<i32> {
    const GETVAL: i32 = 12;
    let value = semctl(id, 0, GETVAL, 0) as i32;
    (value >= 0).then_some(value)
}

/// A semaphore's value set: `semctl(SETVAL)`.
fn sem_set(id: i32, value: i32) -> bool {
    const SETVAL: i32 = 16;
    semctl(id, 0, SETVAL, value as usize) == 0
}

/// The number `/proc/self/ns/ipc` reads back as, and the file itself so it can
/// be joined later.
fn ipc_ns_link() -> Option<(u64, std::fs::File)> {
    let target = std::fs::read_link("/proc/self/ns/ipc").ok()?;
    let text = target.to_str()?;
    let inside = text.strip_prefix("ipc:[")?.strip_suffix(']')?;
    let id = inside.parse().ok()?;
    let file = std::fs::File::open("/proc/self/ns/ipc").ok()?;
    Some((id, file))
}

/// Every data row of `/proc/sysvipc/<which>`, as whitespace-separated columns.
fn proc_rows(which: &str) -> Vec<Vec<String>> {
    std::fs::read_to_string(format!("/proc/sysvipc/{which}"))
        .unwrap_or_default()
        .lines()
        .skip(1)
        .map(|line| line.split_whitespace().map(str::to_string).collect())
        .collect()
}

/// Whether the reader's `/proc/sysvipc/msg` holds a row whose `cbytes` is
/// `bytes` - the mark this program puts on a queue it wants to prove is *not* in
/// the namespace being read. The number is deliberately not the mark: it can be
/// shared between namespaces.
fn proc_has_cbytes(bytes: &str) -> bool {
    proc_rows("msg")
        .iter()
        .any(|row| row.get(3).map(String::as_str) == Some(bytes))
}

fn main() {
    let mut r = Report::new();
    let root = unsafe { libc::geteuid() } == 0;

    // The door's own gate: the privilege is asked before the kind, which is the
    // order `abi::unshare_verdict` keeps and the errno a caller branches on.
    // SAFETY: `unshare` touches no caller memory; it moves this process into a
    // namespace and answers with an errno.
    let unprivileged = unsafe { unshare_unsafe(UnshareFlags::NEWIPC) };
    if !root {
        r.check(
            "an unprivileged unshare(CLONE_NEWIPC) is EPERM",
            matches!(unprivileged, Err(rustix::io::Errno::PERM)),
        );
        println!("skip:  not root, so the namespace claims are named here and not made");
        r.finish();
    }
    r.check(
        "unshare(CLONE_NEWIPC) as root succeeds",
        unprivileged.is_ok(),
    );

    // --- the "outer" namespace, which is whatever the call above made ------
    // The link is taken *after* the call on purpose: the objects below belong to
    // the namespace that call created, and joining back must name that one - a
    // link taken before it would name the machine's, where they are invisible.
    let outer_ns = ipc_ns_link();
    r.check("the link reads ipc:[<number>]", outer_ns.is_some());

    let outer = msgget(KEY, 0o600 | IPC_CREAT);
    let marked = msgget(KEY_MARKED, 0o600 | IPC_CREAT);
    let outer_shm = shmget(KEY, 4096, 0o600 | IPC_CREAT);
    let outer_sem = semget(KEY, 1, 0o600 | IPC_CREAT);
    let outer_mq = mq_open("/mpos-ns-ipc", libc::O_CREAT | libc::O_RDWR);
    r.check(
        "the outer namespace has a queue, a segment, a set and a POSIX queue",
        outer >= 0 && marked >= 0 && outer_shm >= 0 && outer_sem >= 0 && outer_mq >= 0,
    );
    // Marks **inside** the objects, which is the only way to tell an object from
    // a number: a number is the namespace's, so the same one names a different
    // object after the call. The queue's byte count is also what names its
    // `/proc/sysvipc/msg` row without relying on the number; the semaphore's
    // value is the same mark for the family with no byte count.
    r.check(
        "a message queued there is the mark /proc will show",
        msgsnd(outer, 7) == 0 && proc_has_cbytes("7"),
    );
    r.check(
        "and a semaphore value is the mark for the set family",
        sem_set(outer_sem, 42) && sem_val(outer_sem) == Some(42),
    );

    // --- the call, and what it hides --------------------------------------
    // SAFETY: as above.
    let made = unsafe { unshare_unsafe(UnshareFlags::NEWIPC) };
    r.check("a second unshare(CLONE_NEWIPC) succeeds", made.is_ok());
    let inner_ns = ipc_ns_link();
    r.check(
        "the namespace number changed",
        matches!((outer_ns.as_ref(), inner_ns.as_ref()), (Some((a, _)), Some((b, _))) if a != b),
    );

    r.check(
        "a queue from the namespace left behind is EINVAL",
        msgctl(outer, IPC_STAT, 0) == -1 && errno() == libc::EINVAL,
    );
    r.check(
        "a segment from it is EINVAL too",
        shmctl(outer_shm, IPC_STAT, 0) == -1 && errno() == libc::EINVAL,
    );
    r.check(
        "and a semaphore set",
        semctl(outer_sem, 0, IPC_STAT, 0) == -1 && errno() == libc::EINVAL,
    );
    r.check(
        "a POSIX queue's name is gone: ENOENT without O_CREAT",
        mq_open("/mpos-ns-ipc", libc::O_RDWR) == -1 && errno() == libc::ENOENT,
    );
    r.check(
        "the key names no queue here (ENOENT without IPC_CREAT)",
        msgget(KEY, 0o600) == -1 && errno() == libc::ENOENT,
    );
    r.check(
        "the shm key names no segment here",
        shmget(KEY, 0, 0o600) == -1 && errno() == libc::ENOENT,
    );
    r.check(
        "the sem key names no set here",
        semget(KEY, 0, 0o600) == -1 && errno() == libc::ENOENT,
    );

    // ...and with the creation bit each makes a **new** object. The number may be
    // one the outer namespace used - the slot space restarts at 0 - so what makes
    // it new is that it is live here while the outer one is not.
    let inner = msgget(KEY, 0o600 | IPC_CREAT);
    let inner_private = msgget(0, 0o600 | IPC_CREAT);
    let inner_shm = shmget(KEY, 4096, 0o600 | IPC_CREAT);
    let inner_sem = semget(KEY, 1, 0o600 | IPC_CREAT);
    let inner_mq = mq_open("/mpos-ns-ipc", libc::O_CREAT | libc::O_RDWR);
    r.check(
        "the same key makes a live, empty queue here",
        inner >= 0 && msg_cbytes(inner) == Some(0),
    );
    r.check(
        "and the outer queue's bytes do not cross the namespace",
        msg_cbytes(outer) != Some(7) && msg_cbytes(marked) != Some(7),
    );
    r.check(
        "nor does a semaphore's value",
        sem_val(outer_sem) != Some(42),
    );
    r.check(
        "and a fresh namespace numbers its queues from 0",
        inner_private == 1,
    );
    r.check(
        "the other three families make their own objects here too",
        inner_shm >= 0
            && shm_lives(inner_shm)
            && inner_sem >= 0
            && sem_val(inner_sem) == Some(0)
            && inner_mq >= 0,
    );

    // `/proc/sysvipc/msg` is the reader's, not the machine's: the marked queue's
    // byte count has no row here, and the inner queue does.
    r.check(
        "the namespace left behind has no row inside",
        !proc_has_cbytes("7"),
    );
    let inner_text = inner.to_string();
    r.check(
        "and this one does",
        proc_rows("msg")
            .iter()
            .any(|row| row.get(1) == Some(&inner_text)),
    );

    // --- fork stays where every one of its parents is ----------------------
    let child = unsafe { libc::fork() };
    if child == 0 {
        let sees = msg_cbytes(inner) == Some(0);
        let not_outer = msg_cbytes(outer) != Some(7);
        unsafe { libc::_exit(if sees && not_outer { 0 } else { 1 }) };
    }
    let mut status = 0;
    unsafe { libc::waitpid(child, &mut status, 0) };
    r.check(
        "a fork child sees the same namespace",
        libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0,
    );

    // --- setns: back into the outer namespace, and what it restores --------
    let joined = match outer_ns.as_ref() {
        Some((_, file)) => unsafe {
            libc::syscall(libc::SYS_setns, std::os::fd::AsRawFd::as_raw_fd(file), 0) as i32
        },
        None => -1,
    };
    r.check(
        "setns back into the outer IPC namespace succeeds",
        joined == 0,
    );
    r.check(
        "its queue is live again, with its bytes",
        msg_cbytes(outer) == Some(7),
    );
    r.check("and its semaphore's value", sem_val(outer_sem) == Some(42));
    r.check(
        "while the inner namespace's queue does not answer by that number",
        msg_cbytes(inner) != Some(0) || inner == outer,
    );
    r.check(
        "the key resolves to the number it did before the unshare",
        msgget(KEY, 0o600) == outer,
    );
    r.check(
        "and /proc/sysvipc/msg has the marked queue's row back",
        proc_has_cbytes("7"),
    );

    // Leave nothing behind in the outer namespace; the inner namespace's objects
    // go with it when nothing names it any more.
    for (id, kind) in [
        (outer, "msg"),
        (marked, "msg"),
        (outer_shm, "shm"),
        (outer_sem, "sem"),
    ] {
        if id < 0 {
            continue;
        }
        match kind {
            "msg" => {
                msgctl(id, IPC_RMID, 0);
            }
            "shm" => {
                shmctl(id, IPC_RMID, 0);
            }
            _ => {
                semctl(id, 0, IPC_RMID, 0);
            }
        }
    }
    unsafe {
        if outer_mq >= 0 {
            libc::mq_close(outer_mq);
        }
        libc::mq_unlink(cstring("/mpos-ns-ipc").as_ptr());
    }

    r.finish()
}
