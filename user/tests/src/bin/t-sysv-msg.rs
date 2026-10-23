//! t-sysv-msg - System V message queues: `msgget`, `msgsnd`, `msgrcv`,
//! `msgctl`, and the `/proc/sysvipc/msg` row that describes what they made.
//!
//! Every claim is one measured on a current host first (Linux 7.2.11,
//! `.workbuddy/probe_sysv_msg.c`, `probe_sysv_msg_qbytes.c`; the evidence and
//! the decisions are `docs/notes/` §6.15) and then pinned against this kernel,
//! which had no System V message queue at all before the landing this program
//! is the pin of - the third and last of the families `CLONE_NEWIPC` waits on:
//!
//! * the queue's shape: `msg_qbytes` is 16384 (`MSGMNB`) at creation, the
//!   mode's low nine bits with **no umask**, `msg_stime`/`msg_rtime` zero
//!   until traffic and `msg_ctime` set at creation;
//! * the key rules: a new key makes one, `IPC_CREAT|IPC_EXCL` on a live key is
//!   `EEXIST`, no `IPC_CREAT` on a dead one is `ENOENT`;
//! * a send's `msgsz` past `MSGMAX` (8192) is `EINVAL` and a non-positive
//!   `mtype` is `EINVAL`; two 8192s fill a fresh queue and a third is `EAGAIN`
//!   under `IPC_NOWAIT`, which is also the errno a **blocking** send never
//!   sees - it is woken by the receive that frees the budget;
//! * selection is the receive's rule: `type 0` takes the front (the queue is
//!   FIFO), `type > 0` the first of that type, `type < 0` the message of the
//!   **lowest** type that is `<= -type` (with `{4, 10, 1}` queued a `-4` takes
//!   the `1`), `MSG_EXCEPT` the first of another type, `ENOMSG` when nothing
//!   matches under `IPC_NOWAIT`;
//! * a message too big for the buffer, without `MSG_NOERROR`, stays queued
//!   (`E2BIG` under `IPC_NOWAIT`); with the flag it is truncated to the
//!   buffer and the rest of it discarded;
//! * `MSG_COPY` answers by **queue index** without taking, requires
//!   `IPC_NOWAIT` (`EINVAL` without it) and answers `ENOMSG` past the end;
//! * the receive's `msgsz` is gated by its **sign**, not by a size cap:
//!   `LONG_MAX` is accepted and `LONG_MAX + 1` is `EINVAL` with the message
//!   still queued - and a destination the kernel cannot write is `EFAULT`
//!   **after** the message has been taken, so the failed call loses it;
//! * blocking: a send is woken by a receive, a receive by a send, either by
//!   `IPC_RMID` with `EIDRM`, and either by a handler-bearing signal with
//!   `EINTR`;
//! * `IPC_SET` reads its buffer before it asks who is calling, rewrites
//!   uid/gid/mode, and answers `EPERM` for a `msg_qbytes` above `MSGMNB`
//!   without `CAP_SYS_RESOURCE` while a budget **below** the queued bytes -
//!   even below one message - is accepted and stored;
//! * permissions: the triads gate `msgsnd`/`msgrcv`/`IPC_STAT` (`EACCES`), the
//!   creator or `CAP_SYS_ADMIN` gates `IPC_SET`/`IPC_RMID` (`EPERM`);
//! * `/proc/sysvipc/msg` prints the header and the column widths `ipcs`
//!   parses, and `IPC_INFO`/`MSG_INFO`/`MSG_STAT` answer this kernel's own
//!   limits.
//!
//! `libc` rather than `rustix`: `rustix` binds no System V IPC call at all,
//! which is the first clause of `user/tests/Cargo.toml`'s rule. `libc`'s
//! `msgctl` wants the *old* `struct msqid_ds`, so the record is a byte buffer
//! and the offsets are the kernel's own - and unlike `semid64_ds`, which the
//! two machines spell differently, `msqid64_ds` is 120 bytes with the same
//! offsets on both (x86_64's `asm/msgbuf.h` already carries the time64
//! padding), so one table serves both ports.
//!
//! No claim blocks forever: every wait has a waker, a killer, or a child.

use mpos_user_tests::Report;

/// `struct msqid64_ds`, measured on the host: 120 bytes, the three clocks at
/// 48/56/64, `cbytes`/`qnum`/`qbytes` at 72/80/88, the two pids at 96/100.
const MSQID_DS_BYTES: usize = 120;
const MSQ_STIME: usize = 48;
const MSQ_RTIME: usize = 56;
const MSQ_CTIME: usize = 64;
const MSQ_CBYTES: usize = 72;
const MSQ_QNUM: usize = 80;
const MSQ_QBYTES: usize = 88;
const MSQ_LSPID: usize = 96;
const MSQ_LRPID: usize = 100;
const PERM_UID: usize = 4;
const PERM_GID: usize = 8;
const PERM_MODE: usize = 20;

/// `msgctl`'s commands.
const IPC_RMID: i32 = 0;
const IPC_SET: i32 = 1;
const IPC_STAT: i32 = 2;
const IPC_INFO: i32 = 3;
const MSG_STAT: i32 = 11;
const MSG_STAT_ANY: i32 = 13;
const MSG_INFO: i32 = 12;

const IPC_PRIVATE: i32 = 0;
const IPC_CREAT: i32 = 0o1000;
const IPC_EXCL: i32 = 0o2000;
const IPC_NOWAIT: i32 = 0o4000;
const MSG_NOERROR: i32 = 0o10000;
const MSG_EXCEPT: i32 = 0o20000;
const MSG_COPY: i32 = 0o40000;

/// One message buffer: the type then the bytes, the wire shape both doors
/// read. `MSGMAX`-sized so the budget claims can fill a queue with two.
#[repr(C)]
struct Msg {
    mtype: i64,
    mtext: [u8; 8192],
}

impl Msg {
    fn of(mtype: i64, len: usize, fill: u8) -> Msg {
        let mut m = Msg {
            mtype,
            mtext: [0; 8192],
        };
        m.mtext[..len].fill(fill);
        m
    }
}

struct Ds {
    bytes: [u8; MSQID_DS_BYTES],
}

impl Ds {
    fn zeroed() -> Ds {
        Ds {
            bytes: [0; MSQID_DS_BYTES],
        }
    }
    fn u32_at(&self, off: usize) -> u32 {
        u32::from_ne_bytes(self.bytes[off..off + 4].try_into().unwrap())
    }
    fn u64_at(&self, off: usize) -> u64 {
        u64::from_ne_bytes(self.bytes[off..off + 8].try_into().unwrap())
    }
    fn ptr(&mut self) -> usize {
        self.bytes.as_mut_ptr() as usize
    }
}

fn errno() -> i32 {
    std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
}

fn msgget(key: i32, flags: i32) -> i32 {
    unsafe { libc::syscall(libc::SYS_msgget, key, flags) as i32 }
}

fn msgsnd(id: i32, m: &Msg, len: usize, flags: i32) -> i32 {
    unsafe { libc::syscall(libc::SYS_msgsnd, id, m, len, flags) as i32 }
}

fn msgrcv(id: i32, m: &mut Msg, len: usize, typ: i64, flags: i32) -> i64 {
    unsafe { libc::syscall(libc::SYS_msgrcv, id, m, len, typ, flags) as i64 }
}

fn msgctl_raw(id: i32, cmd: i32, buf: usize) -> i64 {
    unsafe { libc::syscall(libc::SYS_msgctl, id, cmd, buf) as i64 }
}

fn stat(id: i32) -> Option<Ds> {
    let mut ds = Ds::zeroed();
    let rc = msgctl_raw(id, IPC_STAT, ds.ptr());
    (rc == 0).then_some(ds)
}

/// A key no other claim in this program uses (and no other test's family:
/// `t-sysv-sem` starts its own at `0x53_0000`).
fn key(n: i32) -> i32 {
    0x4d_0000 + n
}

/// The header `/proc/sysvipc/msg` starts with, measured character for
/// character on a current host.
const PROC_HEADER: &str = "       key      msqid perms      cbytes       qnum lspid lrpid   uid   gid  cuid  cgid      stime      rtime      ctime";

extern "C" fn on_signal(_sig: libc::c_int) {}

fn main() {
    let mut r = Report::new();
    let mut status = 0i32;

    // --- creation and the queue's shape -----------------------------------
    let a = msgget(IPC_PRIVATE, 0o600 | IPC_CREAT);
    r.check("IPC_PRIVATE makes a queue", a >= 0);
    let ds = stat(a).expect("IPC_STAT on a fresh queue");
    r.check(
        "a fresh queue's qbytes is MSGMNB",
        ds.u64_at(MSQ_QBYTES) == 16384,
    );
    r.check(
        "a fresh queue's mode is its msgflg's low nine bits",
        ds.u32_at(PERM_MODE) == 0o600,
    );
    r.check(
        "owner and creator are the caller",
        ds.u32_at(PERM_UID) == unsafe { libc::geteuid() },
    );
    r.check("a fresh queue's stime is zero", ds.u64_at(MSQ_STIME) == 0);
    r.check("a fresh queue's rtime is zero", ds.u64_at(MSQ_RTIME) == 0);
    r.check("a fresh queue's ctime is set", ds.u64_at(MSQ_CTIME) != 0);
    r.check("a fresh queue holds nothing", ds.u64_at(MSQ_QNUM) == 0);

    // --- no umask ----------------------------------------------------------
    let old = unsafe { libc::umask(0o777) };
    let wide = msgget(IPC_PRIVATE, 0o666 | IPC_CREAT);
    unsafe { libc::umask(old) };
    r.check(
        "msgget stores the mode it was asked for (no umask)",
        stat(wide).is_some_and(|d| d.u32_at(PERM_MODE) == 0o666),
    );

    // --- the key rules -----------------------------------------------------
    let k = key(1);
    let k1 = msgget(k, 0o600 | IPC_CREAT);
    r.check("a new key makes a queue", k1 >= 0);
    r.check(
        "the same key with no IPC_CREAT finds it",
        msgget(k, 0o600) == k1,
    );
    r.check(
        "IPC_CREAT|IPC_EXCL on a live key is EEXIST",
        msgget(k, 0o600 | IPC_CREAT | IPC_EXCL) == -1 && errno() == libc::EEXIST,
    );
    r.check(
        "a key with no IPC_CREAT and no queue is ENOENT",
        msgget(key(2), 0o600) == -1 && errno() == libc::ENOENT,
    );
    r.check(
        "msgctl on an unknown number is EINVAL",
        msgctl_raw(0x3fff_ffff, IPC_STAT, 0) == -1 && errno() == libc::EINVAL,
    );

    // --- a send's own refusals ---------------------------------------------
    let mut one = Msg::of(3, 5, b'a');
    r.check("msgsnd queues a typed message", msgsnd(a, &one, 5, 0) == 0);
    one.mtype = 0;
    r.check(
        "a send with mtype 0 is EINVAL",
        msgsnd(a, &one, 1, IPC_NOWAIT) == -1 && errno() == libc::EINVAL,
    );
    one.mtype = -1;
    r.check(
        "a send with a negative mtype is EINVAL",
        msgsnd(a, &one, 1, IPC_NOWAIT) == -1 && errno() == libc::EINVAL,
    );
    let big = Msg::of(1, 8192, b'b');
    r.check(
        "8193 bytes is EINVAL",
        msgsnd(a, &big, 8193, IPC_NOWAIT) == -1 && errno() == libc::EINVAL,
    );

    // --- the byte budget ---------------------------------------------------
    let f = msgget(IPC_PRIVATE, 0o600 | IPC_CREAT);
    r.check(
        "the first 8192 fits",
        msgsnd(f, &big, 8192, IPC_NOWAIT) == 0,
    );
    r.check(
        "the second 8192 fits",
        msgsnd(f, &big, 8192, IPC_NOWAIT) == 0,
    );
    let small = Msg::of(1, 1, b'c');
    r.check(
        "a full queue with IPC_NOWAIT is EAGAIN",
        msgsnd(f, &small, 1, IPC_NOWAIT) == -1 && errno() == libc::EAGAIN,
    );
    let ds = stat(f).expect("IPC_STAT on the full queue");
    r.check("the budget claim: qnum is 2", ds.u64_at(MSQ_QNUM) == 2);
    r.check(
        "the budget claim: cbytes is 16384",
        ds.u64_at(MSQ_CBYTES) == 16384,
    );

    // --- a blocking send is woken by the receive ---------------------------
    let child = unsafe { libc::fork() };
    if child == 0 {
        let m = Msg::of(1, 1, b'd');
        unsafe { libc::_exit(if msgsnd(f, &m, 1, 0) == 0 { 0 } else { 42 }) };
    }
    // The child is asleep on the budget; the receive frees it.
    let mut sink = Msg::of(0, 0, 0);
    r.check(
        "the receive drains one 8192",
        msgrcv(f, &mut sink, 8192, 0, 0) == 8192,
    );
    unsafe { libc::waitpid(child, &mut status, 0) };
    r.check(
        "a blocked send completes after the receive",
        libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0,
    );
    msgctl_raw(f, IPC_RMID, 0);

    // --- EIDRM and EINTR ---------------------------------------------------
    let e = msgget(IPC_PRIVATE, 0o600 | IPC_CREAT);
    msgsnd(e, &big, 8192, IPC_NOWAIT);
    msgsnd(e, &big, 8192, IPC_NOWAIT);
    let child = unsafe { libc::fork() };
    if child == 0 {
        let m = Msg::of(1, 1, b'e');
        msgsnd(e, &m, 1, 0);
        unsafe { libc::_exit(if errno() == libc::EIDRM { 0 } else { 43 }) };
    }
    std::thread::sleep(std::time::Duration::from_millis(150));
    msgctl_raw(e, IPC_RMID, 0);
    unsafe { libc::waitpid(child, &mut status, 0) };
    r.check(
        "a blocked send reads EIDRM at removal",
        libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0,
    );

    let e = msgget(IPC_PRIVATE, 0o600 | IPC_CREAT);
    msgsnd(e, &big, 8192, IPC_NOWAIT);
    msgsnd(e, &big, 8192, IPC_NOWAIT);
    let child = unsafe { libc::fork() };
    if child == 0 {
        unsafe { libc::signal(libc::SIGUSR1, on_signal as *const () as libc::sighandler_t) };
        let m = Msg::of(1, 1, b'f');
        let rc = msgsnd(e, &m, 1, 0);
        unsafe {
            libc::_exit(if rc == -1 && errno() == libc::EINTR {
                0
            } else {
                44
            })
        };
    }
    std::thread::sleep(std::time::Duration::from_millis(150));
    unsafe { libc::kill(child, libc::SIGUSR1) };
    unsafe { libc::waitpid(child, &mut status, 0) };
    r.check(
        "a blocked send reads EINTR on a signal",
        libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0,
    );
    msgctl_raw(e, IPC_RMID, 0);

    // a blocked receive, both endings
    let e = msgget(IPC_PRIVATE, 0o600 | IPC_CREAT);
    let child = unsafe { libc::fork() };
    if child == 0 {
        let mut m = Msg::of(0, 0, 0);
        let rc = msgrcv(e, &mut m, 8192, 0, 0);
        unsafe {
            libc::_exit(if rc == -1 && errno() == libc::EIDRM {
                0
            } else {
                45
            })
        };
    }
    std::thread::sleep(std::time::Duration::from_millis(150));
    msgctl_raw(e, IPC_RMID, 0);
    unsafe { libc::waitpid(child, &mut status, 0) };
    r.check(
        "a blocked receive reads EIDRM at removal",
        libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0,
    );

    // --- selection ---------------------------------------------------------
    let s = msgget(IPC_PRIVATE, 0o600 | IPC_CREAT);
    for (i, mtype) in [5i64, 3, 3, 4, 10, 1].iter().enumerate() {
        let m = Msg::of(*mtype, 1, b'a' + i as u8);
        msgsnd(s, &m, 1, IPC_NOWAIT);
    }
    let mut m = Msg::of(0, 0, 0);
    r.check(
        "type 0 takes the front (FIFO)",
        msgrcv(s, &mut m, 8, 0, 0) == 1 && m.mtype == 5,
    );
    r.check(
        "type 3 takes the first 3",
        msgrcv(s, &mut m, 8, 3, 0) == 1 && m.mtype == 3,
    );
    r.check(
        "the second 3 follows",
        msgrcv(s, &mut m, 8, 3, 0) == 1 && m.mtype == 3,
    );
    r.check(
        "no message of the type is ENOMSG with IPC_NOWAIT",
        msgrcv(s, &mut m, 8, 3, IPC_NOWAIT) == -1 && errno() == libc::ENOMSG,
    );
    r.check(
        "type -4 takes the lowest type <= 4 (the 1, not the 4)",
        msgrcv(s, &mut m, 8, -4, 0) == 1 && m.mtype == 1,
    );
    r.check(
        "type -10 takes the lowest type <= 10 (the 4)",
        msgrcv(s, &mut m, 8, -10, 0) == 1 && m.mtype == 4,
    );
    r.check(
        "MSG_EXCEPT with only the named type left is ENOMSG",
        msgrcv(s, &mut m, 8, 10, MSG_EXCEPT | IPC_NOWAIT) == -1 && errno() == libc::ENOMSG,
    );
    r.check(
        "the plain type 10 takes it",
        msgrcv(s, &mut m, 8, 10, 0) == 1 && m.mtype == 10,
    );
    r.check(
        "an empty queue with IPC_NOWAIT is ENOMSG",
        msgrcv(s, &mut m, 8, 0, IPC_NOWAIT) == -1 && errno() == libc::ENOMSG,
    );

    // --- the fit rules -----------------------------------------------------
    let seven = Msg::of(9, 7, b'g');
    msgsnd(s, &seven, 7, IPC_NOWAIT);
    r.check(
        "a short buffer without MSG_NOERROR is E2BIG, message kept",
        msgrcv(s, &mut m, 4, 9, IPC_NOWAIT) == -1 && errno() == libc::E2BIG,
    );
    r.check(
        "MSG_NOERROR truncates to the buffer",
        msgrcv(s, &mut m, 4, 9, MSG_NOERROR | IPC_NOWAIT) == 4,
    );
    r.check(
        "the queue is drained",
        stat(s).is_some_and(|d| d.u64_at(MSQ_QNUM) == 0),
    );

    // --- MSG_COPY ----------------------------------------------------------
    let m1 = Msg::of(1, 1, b'h');
    let m2 = Msg::of(2, 1, b'i');
    msgsnd(s, &m1, 1, IPC_NOWAIT);
    msgsnd(s, &m2, 1, IPC_NOWAIT);
    r.check(
        "MSG_COPY answers by index without taking",
        msgrcv(s, &mut m, 8, 1, MSG_COPY | IPC_NOWAIT) == 1 && m.mtype == 2 && m.mtext[0] == b'i',
    );
    r.check(
        "the copy left both messages queued",
        stat(s).is_some_and(|d| d.u64_at(MSQ_QNUM) == 2),
    );
    r.check(
        "MSG_COPY past the end is ENOMSG",
        msgrcv(s, &mut m, 8, 9, MSG_COPY | IPC_NOWAIT) == -1 && errno() == libc::ENOMSG,
    );
    r.check(
        "MSG_COPY without IPC_NOWAIT is EINVAL",
        msgrcv(s, &mut m, 8, 0, MSG_COPY) == -1 && errno() == libc::EINVAL,
    );
    // The index is a count, and every shape of "not there" answers `ENOMSG`
    // rather than `EINVAL` - a negative index included (measured, `probe_stat_any.c`:
    // -1 and `INT_MAX` both answer 42, and the queue is untouched).
    r.check(
        "MSG_COPY with a negative index is ENOMSG, not EINVAL",
        msgrcv(s, &mut m, 8, -1, MSG_COPY | IPC_NOWAIT) == -1 && errno() == libc::ENOMSG,
    );
    r.check(
        "and a huge one too",
        msgrcv(s, &mut m, 8, 0x7fff_ffff, MSG_COPY | IPC_NOWAIT) == -1 && errno() == libc::ENOMSG,
    );
    r.check(
        "no copy attempt took anything: both are still queued",
        stat(s).is_some_and(|d| d.u64_at(MSQ_QNUM) == 2),
    );

    // --- the counters and IPC_SET -----------------------------------------
    msgctl_raw(s, IPC_RMID, 0);
    let t = msgget(IPC_PRIVATE, 0o600 | IPC_CREAT);
    let hello = Msg::of(3, 5, b'j');
    msgsnd(t, &hello, 5, 0);
    msgrcv(t, &mut m, 8, 0, 0);
    let ds = stat(t).expect("IPC_STAT after traffic");
    r.check(
        "lspid and lrpid name the caller",
        ds.u32_at(MSQ_LSPID) as u32 == unsafe { libc::getpid() as u32 }
            && ds.u32_at(MSQ_LRPID) as u32 == unsafe { libc::getpid() as u32 },
    );
    r.check("stime moved", ds.u64_at(MSQ_STIME) != 0);
    r.check("rtime moved", ds.u64_at(MSQ_RTIME) != 0);
    // `ipc_update_perm` rewrites uid, gid and mode from the record, and the
    // only id question it asks is whether the calling *user namespace* maps the
    // id named - an unmapped one is `EINVAL`, measured on a host whose own
    // namespace maps a single id (renaming a queue to uid 0 there answers
    // `EINVAL`, where a machine whose namespace maps everything accepts it).
    // So the mode half is pinned by every caller here, and the id rewrite
    // where every id is mapped: below, as root.
    let mut set = stat(t).expect("IPC_STAT before the set");
    set.bytes[PERM_MODE..PERM_MODE + 4].copy_from_slice(&0o640u32.to_ne_bytes());
    r.check(
        "IPC_SET accepts a new mode through the record",
        msgctl_raw(t, IPC_SET, set.ptr()) == 0,
    );
    let ds = stat(t).expect("IPC_STAT after the set");
    r.check(
        "IPC_STAT reads the mode back",
        ds.u32_at(PERM_MODE) == 0o640,
    );
    r.check(
        "and the uid it did not name is still the caller's",
        ds.u32_at(PERM_UID) == unsafe { libc::geteuid() } as u32,
    );
    if unsafe { libc::geteuid() } == 0 {
        let mut set = stat(t).expect("IPC_STAT before the id rewrite");
        set.bytes[PERM_UID..PERM_UID + 4].copy_from_slice(&65534u32.to_ne_bytes());
        set.bytes[PERM_GID..PERM_GID + 4].copy_from_slice(&65534u32.to_ne_bytes());
        r.check(
            "IPC_SET rewrites the owner's ids",
            msgctl_raw(t, IPC_SET, set.ptr()) == 0
                && stat(t).map(|d| (d.u32_at(PERM_UID), d.u32_at(PERM_GID)))
                    == Some((65534, 65534)),
        );
        // Hand it back: the rest of the program still acts as its owner, and
        // as root every id is mapped in either direction.
        let mut back = stat(t).expect("IPC_STAT before the restore");
        back.bytes[PERM_UID..PERM_UID + 4].copy_from_slice(&0u32.to_ne_bytes());
        back.bytes[PERM_GID..PERM_GID + 4].copy_from_slice(&0u32.to_ne_bytes());
        msgctl_raw(t, IPC_SET, back.ptr());
    } else {
        println!("skip:  not root, so IPC_SET's id rewrite is named here and not made");
    }

    // `qbytes`' two directions, in a child that owns its queue and holds no
    // capability: the raise is `EPERM`, the lowering - below one queued
    // message - is accepted and stored (measured on the host first).
    let child = unsafe { libc::fork() };
    if child == 0 {
        unsafe {
            libc::setgid(65534);
            libc::setuid(65534);
        }
        let q = msgget(IPC_PRIVATE, 0o600 | IPC_CREAT);
        let b = Msg::of(1, 8192, b'k');
        let mut code = 0;
        if msgsnd(q, &b, 8192, IPC_NOWAIT) == 0 {
            code |= 1;
        }
        let mut ds = Ds::zeroed();
        // A raise above MSGMNB.
        msgctl_raw(q, IPC_STAT, ds.ptr());
        ds.bytes[MSQ_QBYTES..MSQ_QBYTES + 8].copy_from_slice(&32768u64.to_ne_bytes());
        if msgctl_raw(q, IPC_SET, ds.ptr()) == -1 && errno() == libc::EPERM {
            code |= 2;
        }
        // A lowering below what is queued.
        msgctl_raw(q, IPC_STAT, ds.ptr());
        ds.bytes[MSQ_QBYTES..MSQ_QBYTES + 8].copy_from_slice(&16u64.to_ne_bytes());
        if msgctl_raw(q, IPC_SET, ds.ptr()) == 0 {
            code |= 4;
        }
        let mut after = Ds::zeroed();
        msgctl_raw(q, IPC_STAT, after.ptr());
        if after.u64_at(MSQ_QBYTES) == 16 {
            code |= 8;
        }
        msgctl_raw(q, IPC_RMID, 0);
        unsafe { libc::_exit(code as i8 as i32) };
    }
    unsafe { libc::waitpid(child, &mut status, 0) };
    let code = if libc::WIFEXITED(status) {
        libc::WEXITSTATUS(status) as u32
    } else {
        0xffff
    };
    r.check("a child's queue fills", code & 1 != 0);
    r.check(
        "a qbytes raise without CAP_SYS_RESOURCE is EPERM",
        code & 2 != 0,
    );
    r.check(
        "a qbytes lowering below the queued bytes is accepted",
        code & 4 != 0,
    );
    r.check("the lowered budget reads back", code & 8 != 0);

    // --- MSG_INFO and MSG_STAT --------------------------------------------
    let mut info = [0i32; 8];
    let max = msgctl_raw(0, MSG_INFO, info.as_mut_ptr() as usize);
    r.check("MSG_INFO answers the highest slot", max >= 0);
    r.check(
        "msgmax and msgmnb are the measured pair",
        info[2] == 8192 && info[3] == 16384,
    );
    let mut ds = Ds::zeroed();
    r.check(
        "MSG_STAT on a live id answers the id",
        msgctl_raw(t, MSG_STAT, ds.ptr()) == t as i64,
    );
    let gone = msgget(IPC_PRIVATE, 0o600 | IPC_CREAT);
    msgctl_raw(gone, IPC_RMID, 0);
    r.check(
        "MSG_STAT on a freed slot is EINVAL",
        msgctl_raw(gone, MSG_STAT, ds.ptr()) == -1 && errno() == libc::EINVAL,
    );

    // The commands' *argument form* is deliberately not claimed here: two
    // probes on the same host disagreed about whether the plain `MSG_STAT`
    // accepts a slot index at all (`.workbuddy/probe_stat_gen.c` said `EINVAL`,
    // `probe_stat_key.c` answered the full id), so these claims ask only what
    // both readings agree on - the full id works, and the `_ANY` spelling
    // accepts the slot - and the question is named open in `docs/notes/` §6.16.
    let reused = msgget(key(41), 0o600 | IPC_CREAT);
    msgctl_raw(reused, IPC_RMID, 0);
    let reused = msgget(key(41), 0o600 | IPC_CREAT);
    let reused_slot = reused % 32768;
    r.check(
        "the reused slot's queue has a nonzero generation",
        reused >= 32768,
    );
    let mut gds = Ds::zeroed();
    r.check(
        "MSG_STAT takes the full id",
        msgctl_raw(reused, MSG_STAT, gds.ptr()) == reused as i64,
    );

    r.check(
        "MSG_STAT_ANY answers the same queue from its slot",
        msgctl_raw(reused_slot, MSG_STAT_ANY, gds.ptr()) == reused as i64,
    );

    // And the permission split: `MSG_STAT` asks read permission, `MSG_STAT_ANY`
    // does not - which is the whole reason the `_ANY` spelling exists (ipcs and
    // monitoring tools must see a queue they may not read). A mode of `0000`
    // refuses even the owner, which is what makes it the witness - but *only*
    // for a caller without `CAP_IPC_OWNER` over the queue's user namespace, so
    // the question is asked in a child that has dropped to nobody: root in a
    // fresh machine has the capability and Linux serves it, which is the half
    // the first draft of this claim missed.
    msgctl_raw(reused, IPC_STAT, gds.ptr());
    gds.bytes[PERM_MODE..PERM_MODE + 4].copy_from_slice(&0u32.to_ne_bytes());
    r.check(
        "the mode is set to 0000 for the permission claim",
        msgctl_raw(reused, IPC_SET, gds.ptr()) == 0,
    );
    let unprivileged = unsafe { libc::fork() };
    if unprivileged == 0 {
        unsafe {
            libc::setgid(65534);
            libc::setuid(65534);
        }
        let mut d = Ds::zeroed();
        let mut code = 0i32;
        if msgctl_raw(reused, MSG_STAT, d.ptr()) == -1 && errno() == libc::EACCES {
            code |= 1;
        }
        if msgctl_raw(reused_slot, MSG_STAT_ANY, d.ptr()) == reused as i64 {
            code |= 2;
        }
        if msgctl_raw(reused, IPC_STAT, d.ptr()) == -1 && errno() == libc::EACCES {
            code |= 4;
        }
        unsafe { libc::_exit(code) };
    }
    let mut status = 0;
    unsafe { libc::waitpid(unprivileged, &mut status, 0) };
    let code = if libc::WIFEXITED(status) {
        libc::WEXITSTATUS(status)
    } else {
        0x7f
    };
    r.check(
        "MSG_STAT on a queue nobody may read is EACCES",
        code & 1 != 0,
    );
    r.check("MSG_STAT_ANY answers it anyway", code & 2 != 0);
    r.check(
        "while IPC_STAT asks the same question, for contrast",
        code & 4 != 0,
    );
    msgctl_raw(reused, IPC_RMID, 0);

    // --- /proc/sysvipc/msg -------------------------------------------------
    let p = Msg::of(7, 3, b'l');
    msgsnd(t, &p, 3, IPC_NOWAIT);
    let text = std::fs::read_to_string("/proc/sysvipc/msg").unwrap_or_default();
    let mut lines = text.lines();
    r.check(
        "the proc header is the kernel's own",
        lines.next() == Some(PROC_HEADER),
    );
    // The queue this section made: find its row by id.
    let mine = text
        .lines()
        .skip(1)
        .find(|l| l.split_whitespace().nth(1) == Some(t.to_string().as_str()));
    r.check("the queue has a row", mine.is_some());
    if let Some(row) = mine {
        let f: Vec<&str> = row.split_whitespace().collect();
        // The mode `IPC_SET` stored above, which is also the proof that the
        // row renders the queue's own record rather than a fresh one.
        r.check("the row's perms are the 640 IPC_SET stored", f[2] == "640");
        r.check("the row's cbytes is 3", f[3] == "3");
        r.check("the row's qnum is 1", f[4] == "1");
    }

    // --- the size word, and a destination that cannot be written ------------
    // Two facts about `msgrcv` that a receive's happy path never shows, both
    // measured on the host first (§6.15): Linux's gate is the *sign* of the
    // `size_t` (`if (msqid < 0 || (long) msgsz < 0) return -EINVAL`), so
    // `LONG_MAX` is accepted and `LONG_MAX + 1` is refused with the message
    // still queued; and the message is unlinked *before* it is copied out, so
    // a destination the kernel cannot write loses it.
    let w = msgget(IPC_PRIVATE, 0o600 | IPC_CREAT);
    let one = Msg::of(9, 3, b'z');
    msgsnd(w, &one, 3, 0);
    let mut m = Msg::of(0, 0, 0);
    // The first draft computed the destination range as `8 + msgsz`, which
    // wraps for this value and would then validate almost nothing before
    // writing a whole message through the pointer it was handed.
    r.check(
        "a msgsz whose sign bit is set is EINVAL, and the message stays queued",
        msgrcv(w, &mut m, usize::MAX, 0, IPC_NOWAIT) == -1
            && errno() == libc::EINVAL
            && stat(w).map(|d| d.u64_at(MSQ_QNUM)) == Some(1),
    );
    r.check(
        "msgsz == LONG_MAX is accepted - the gate is the sign, not a size cap",
        msgrcv(w, &mut m, isize::MAX as usize, 0, IPC_NOWAIT) == 3 && m.mtype == 9,
    );
    msgsnd(w, &one, 3, 0);
    let bad =
        unsafe { libc::syscall(libc::SYS_msgrcv, w, 0usize, 3usize, 0i64, IPC_NOWAIT) } as i64;
    r.check(
        "a destination that cannot be written is EFAULT",
        bad == -1 && errno() == libc::EFAULT,
    );
    r.check(
        "and the message it could not write is gone, as Linux loses it",
        stat(w).map(|d| d.u64_at(MSQ_QNUM)) == Some(0),
    );
    msgctl_raw(w, IPC_RMID, 0);

    // --- another user ------------------------------------------------------
    // Root reaches any queue through `CAP_IPC_OWNER`; a caller without it is
    // refused by the triads, and the two refusals are two different errnos.
    // The child needs to *become* another user, which only root may - the
    // host's unprivileged run names the six claims and skips them (the shape
    // `t-reboot-libc` set).
    let other = msgget(IPC_PRIVATE, 0o600 | IPC_CREAT);
    let keyed = msgget(key(3), 0o600 | IPC_CREAT);
    let root = unsafe { libc::geteuid() } == 0;
    if !root {
        println!("skip:  not root, so the six another-user claims are named here and not made");
    }
    let child = if root { unsafe { libc::fork() } } else { -1 };
    if child == 0 {
        unsafe {
            libc::setgid(65534);
            libc::setuid(65534);
        }
        let mut m = Msg::of(1, 1, b'm');
        let mut ds = Ds::zeroed();
        let mut code = 0i32;
        if msgsnd(other, &m, 1, IPC_NOWAIT) == -1 && errno() == libc::EACCES {
            code |= 1;
        }
        if msgrcv(other, &mut m, 8, 0, IPC_NOWAIT) == -1 && errno() == libc::EACCES {
            code |= 2;
        }
        if msgctl_raw(other, IPC_STAT, ds.ptr()) == -1 && errno() == libc::EACCES {
            code |= 4;
        }
        if msgctl_raw(other, IPC_SET, ds.ptr()) == -1 && errno() == libc::EPERM {
            code |= 8;
        }
        if msgctl_raw(other, IPC_RMID, 0) == -1 && errno() == libc::EPERM {
            code |= 16;
        }
        if msgget(key(3), 0o600) == -1 && errno() == libc::EACCES {
            code |= 32;
        }
        unsafe { libc::_exit(code as i8 as i32) };
    }
    let mut code = 0xffffu32;
    if root {
        unsafe { libc::waitpid(child, &mut status, 0) };
        code = if libc::WIFEXITED(status) {
            libc::WEXITSTATUS(status) as u32
        } else {
            0xffff
        };
    }
    if root {
        r.check("another user's msgsnd is EACCES", code & 1 != 0);
        r.check("another user's msgrcv is EACCES", code & 2 != 0);
        r.check("another user's IPC_STAT is EACCES", code & 4 != 0);
        r.check("another user's IPC_SET is EPERM", code & 8 != 0);
        r.check("another user's IPC_RMID is EPERM", code & 16 != 0);
        r.check("another user's msgget on the key is EACCES", code & 32 != 0);
    }

    // Clean up what is still ours.
    for id in [a, wide, k1, s, t, other, keyed, f, e] {
        if id >= 0 {
            msgctl_raw(id, IPC_RMID, 0);
        }
    }
    r.finish();
}
