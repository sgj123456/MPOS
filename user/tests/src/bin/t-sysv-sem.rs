//! t-sysv-sem - System V semaphores: `semget`, `semop`, `semtimedop`,
//! `semctl`, and the `/proc/sysvipc/sem` row that describes what they made.
//!
//! Every claim is one measured on a current host first (Linux 7.2.11,
//! `.workbuddy/probe_sysv_sem.c`, `probe_sem2.c`, `probe_sem3.c`,
//! `probe_sem_unshare.c`, `probe_ipc_order.c`; the evidence and the decisions
//! are `docs/notes/` §6.14) and then pinned against this kernel, which had no
//! System V objects at all before the two landings this program's family is the
//! second of:
//!
//! * the set's shape: `nsems` as asked, the mode's low nine bits with **no
//!   umask**, `sem_otime` zero until a `semop` and `sem_ctime` set at creation;
//! * the key rules: a new key makes one, `IPC_CREAT|IPC_EXCL` on a live key is
//!   `EEXIST`, no `IPC_CREAT` on a dead one is `ENOENT`, a **larger** `nsems`
//!   than the set is `EINVAL` and a smaller one is answered with the set;
//! * `semop`: `+1`/`-1`, `0` waits for zero, `IPC_NOWAIT` on an unsatisfiable
//!   operation is `EAGAIN`, the array is **atomic**, `sem_num` past the end is
//!   `EFBIG`, more than 500 entries is `E2BIG` and an empty array is `EINVAL`;
//! * the values never leave `0..=SEMVMX`: `semop(+1)` on 32767 and `SETVAL` of
//!   32768 or -1 are `ERANGE`;
//! * `SETALL`/`GETALL` are `unsigned short` arrays, and `GETVAL` of a slot past
//!   the end is `EINVAL` (where `semop`'s is `EFBIG` - two calls, two answers);
//! * blocking: a waiter is counted by `GETNCNT`, woken by another process's
//!   raise, ended by a **handler-bearing** signal with `EINTR`, and woken by
//!   `IPC_RMID` with `EIDRM`;
//! * `semtimedop`'s timeout is **relative** (the clock is the claim) and an
//!   expired one is `EAGAIN`, not `ETIMEDOUT`;
//! * `SEM_UNDO` is per process: a child's exit does not apply its parent's
//!   record, a child's own record is applied by its own exit, and
//!   `unshare(CLONE_SYSVSEM)` keeps the records;
//! * permissions: the triads gate `semop` and every `GET*`/`SET*` (`EACCES`,
//!   even for the owner of a `0000` set), the creator or `CAP_SYS_ADMIN` gates
//!   `IPC_SET`/`IPC_RMID` (`EPERM`), and `IPC_SET` reads its buffer before it
//!   asks who is calling (`EFAULT` first);
//! * `/proc/sysvipc/sem` prints the header and the column widths `ipcs` parses,
//!   and `IPC_INFO`/`SEM_INFO`/`SEM_STAT` answer this kernel's own limits.
//!
//! `libc` rather than `rustix`: `rustix` binds no System V IPC call at all,
//! which is the first clause of `user/tests/Cargo.toml`'s rule. `libc`'s
//! `semctl` wants the *old* `struct semid_ds`, and the 64-bit record the kernel
//! writes differs between the two machines anyway (x86_64 pads each clock with
//! an unused word; aarch64 does not), so the record is a byte buffer here and
//! the offsets are the machine's - spelled per `target_arch`, exactly as the
//! kernel's `abi` does.
//!
//! No claim blocks forever: every wait has a deadline, a waker, or a child.

use mpos_user_tests::Report;

/// The 64-bit `semid64_ds` the *calling machine* writes: 104 bytes on x86_64
/// with `sem_ctime` at 64 and `sem_nsems` at 80, 88 bytes on aarch64 with them
/// at 56 and 64. Measured off the headers and off the kernel's own writes.
#[cfg(target_arch = "aarch64")]
const SEMID_DS_BYTES: usize = 88;
#[cfg(not(target_arch = "aarch64"))]
const SEMID_DS_BYTES: usize = 104;
#[cfg(target_arch = "aarch64")]
const SEMID_CTIME: usize = 56;
#[cfg(not(target_arch = "aarch64"))]
const SEMID_CTIME: usize = 64;
#[cfg(target_arch = "aarch64")]
const SEMID_NSEMS: usize = 64;
#[cfg(not(target_arch = "aarch64"))]
const SEMID_NSEMS: usize = 80;
const SEMID_OTIME: usize = 48;
const SEM_PERM_UID: usize = 4;
const SEM_PERM_MODE: usize = 20;

/// `semctl`'s commands.
const IPC_RMID: i32 = 0;
const IPC_SET: i32 = 1;
const IPC_STAT: i32 = 2;
const IPC_INFO: i32 = 3;
const GETPID: i32 = 11;
const GETVAL: i32 = 12;
const GETALL: i32 = 13;
const GETNCNT: i32 = 14;
const GETZCNT: i32 = 15;
const SETVAL: i32 = 16;
const SETALL: i32 = 17;
const SEM_STAT: i32 = 18;
const SEM_INFO: i32 = 19;
const SEM_STAT_ANY: i32 = 20;

const IPC_PRIVATE: i32 = 0;
const IPC_CREAT: i32 = 0o1000;
const IPC_EXCL: i32 = 0o2000;
const SEM_UNDO: i16 = 0x1000;
const IPC_NOWAIT: i16 = 0x4000;

/// `struct sembuf`.
#[repr(C)]
#[derive(Clone, Copy)]
struct SemBuf {
    num: u16,
    op: i16,
    flags: i16,
}

/// A `semid64_ds` as its machine writes it: a byte buffer and the offsets.
struct Ds {
    bytes: [u8; SEMID_DS_BYTES],
}

impl Ds {
    fn zeroed() -> Ds {
        Ds {
            bytes: [0; SEMID_DS_BYTES],
        }
    }
    fn u32_at(&self, off: usize) -> u32 {
        u32::from_ne_bytes(self.bytes[off..off + 4].try_into().unwrap())
    }
    fn i64_at(&self, off: usize) -> i64 {
        i64::from_ne_bytes(self.bytes[off..off + 8].try_into().unwrap())
    }
    fn ptr(&mut self) -> usize {
        self.bytes.as_mut_ptr() as usize
    }
}

fn errno() -> i32 {
    std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
}

fn semget(key: i32, nsems: usize, flags: i32) -> i32 {
    unsafe { libc::syscall(libc::SYS_semget, key, nsems, flags) as i32 }
}

fn semop_raw(id: i32, ops: &[SemBuf]) -> i32 {
    unsafe { libc::syscall(libc::SYS_semop, id, ops.as_ptr(), ops.len()) as i32 }
}

fn semtimedop(id: i32, ops: &[SemBuf], rel: std::time::Duration) -> i32 {
    let ts = libc::timespec {
        tv_sec: rel.as_secs() as _,
        tv_nsec: rel.subsec_nanos() as _,
    };
    unsafe { libc::syscall(libc::SYS_semtimedop, id, ops.as_ptr(), ops.len(), &ts) as i32 }
}

fn semctl_raw(id: i32, num: i32, cmd: i32, arg: usize) -> i64 {
    unsafe { libc::syscall(libc::SYS_semctl, id, num, cmd, arg) as i64 }
}

fn stat(id: i32) -> Option<Ds> {
    let mut ds = Ds::zeroed();
    let rc = semctl_raw(id, 0, IPC_STAT, ds.ptr());
    (rc == 0).then_some(ds)
}

fn val(id: i32, sem: i32) -> i64 {
    semctl_raw(id, sem, GETVAL, 0)
}

fn one(num: u16, op: i16, flags: i16) -> [SemBuf; 1] {
    [SemBuf { num, op, flags }]
}

/// A key no other claim in this program uses.
fn key(n: i32) -> i32 {
    0x53_0000 + n
}

fn main() {
    let mut r = Report::new();

    // --- creation and the set's shape -------------------------------------
    let a = semget(IPC_PRIVATE, 3, 0o600);
    r.check("IPC_PRIVATE makes a set", a >= 0);
    let ds = stat(a).expect("IPC_STAT on a fresh set");
    r.check(
        "sem_nsems is the count asked for",
        ds.i64_at(SEMID_NSEMS) == 3,
    );
    r.check(
        "a fresh set's mode is its semflg's low nine bits",
        ds.u32_at(SEM_PERM_MODE) == 0o600,
    );
    r.check(
        "owner and creator are the caller",
        ds.u32_at(SEM_PERM_UID) == unsafe { libc::geteuid() },
    );
    r.check(
        "a fresh set's sem_otime is zero",
        ds.i64_at(SEMID_OTIME) == 0,
    );
    r.check(
        "a fresh set's sem_ctime is set",
        ds.i64_at(SEMID_CTIME) != 0,
    );
    r.check(
        "semget refuses a zero count",
        semget(IPC_PRIVATE, 0, 0o600) == -1 && errno() == libc::EINVAL,
    );
    r.check(
        "semget refuses a count past SEMMSL",
        semget(IPC_PRIVATE, 1025, 0o600) == -1 && errno() == libc::EINVAL,
    );

    // --- no umask ----------------------------------------------------------
    let old = unsafe { libc::umask(0o777) };
    let wide = semget(IPC_PRIVATE, 1, 0o666);
    unsafe { libc::umask(old) };
    r.check(
        "semget stores the mode it was asked for (no umask)",
        stat(wide).is_some_and(|d| d.u32_at(SEM_PERM_MODE) == 0o666),
    );

    // --- the key rules -----------------------------------------------------
    let k = key(1);
    let k1 = semget(k, 2, IPC_CREAT | 0o600);
    r.check("a new key makes a set", k1 >= 0);
    r.check(
        "the same key with no IPC_CREAT finds it",
        semget(k, 2, 0o600) == k1,
    );
    r.check(
        "a smaller nsems than the set is answered with the set",
        semget(k, 1, 0o600) == k1,
    );
    r.check(
        "a larger nsems than the set is EINVAL",
        semget(k, 3, 0o600) == -1 && errno() == libc::EINVAL,
    );
    r.check(
        "IPC_CREAT|IPC_EXCL on a live key is EEXIST",
        semget(k, 2, IPC_CREAT | IPC_EXCL | 0o600) == -1 && errno() == libc::EEXIST,
    );
    r.check(
        "a key with no IPC_CREAT and no set is ENOENT",
        semget(key(2), 1, 0o600) == -1 && errno() == libc::ENOENT,
    );
    r.check(
        "semctl on an unknown number is EINVAL",
        semctl_raw(0x3fff_ffff, 0, IPC_STAT, 0) == -1 && errno() == libc::EINVAL,
    );

    // --- semop's arithmetic ------------------------------------------------
    r.check("semop(+1) succeeds", semop_raw(a, &one(0, 1, 0)) == 0);
    r.check("GETVAL reads 1", val(a, 0) == 1);
    r.check("semop(-1) succeeds", semop_raw(a, &one(0, -1, 0)) == 0);
    r.check("GETVAL reads 0 after the -1", val(a, 0) == 0);
    r.check(
        "semop(-1) at 0 with IPC_NOWAIT is EAGAIN",
        semop_raw(a, &one(0, -1, IPC_NOWAIT)) == -1 && errno() == libc::EAGAIN,
    );
    r.check(
        "semop(0) at 0 succeeds (it already is zero)",
        semop_raw(a, &one(0, 0, IPC_NOWAIT)) == 0,
    );
    r.check("semop(+3) succeeds", semop_raw(a, &one(0, 3, 0)) == 0);
    r.check(
        "semop(0) at 3 with IPC_NOWAIT is EAGAIN",
        semop_raw(a, &one(0, 0, IPC_NOWAIT)) == -1 && errno() == libc::EAGAIN,
    );
    r.check(
        "an operation array is atomic: one refused entry leaves the others alone",
        {
            let s = semget(IPC_PRIVATE, 2, 0o600);
            let two = [
                SemBuf {
                    num: 0,
                    op: 1,
                    flags: 0,
                },
                SemBuf {
                    num: 1,
                    op: -1,
                    flags: IPC_NOWAIT,
                },
            ];
            let refused = semop_raw(s, &two) == -1 && errno() == libc::EAGAIN;
            refused && val(s, 0) == 0
        },
    );
    r.check(
        "semop with no operations is EINVAL",
        unsafe {
            libc::syscall(
                libc::SYS_semop,
                a,
                [SemBuf {
                    num: 0,
                    op: 0,
                    flags: 0,
                }]
                .as_ptr(),
                0,
            ) as i32
        } == -1
            && errno() == libc::EINVAL,
    );
    r.check(
        "sem_num past the set is EFBIG",
        semop_raw(a, &one(3, 0, 0)) == -1 && errno() == libc::EFBIG,
    );
    // A `+1` carrying a flag bit the kernel does not know: the bit is ignored
    // and the operation is the `+1`. (A *zero* operation is what this claim was
    // first written as, and it hung the sweep - a zero operation on a non-zero
    // set waits, which is the whole reason the flag-alphabet claim needs an
    // operation that cannot wait.)
    r.check(
        "an unknown sem_flg bit is ignored",
        semop_raw(a, &one(0, 1, 0x2000)) == 0 && val(a, 0) == 4,
    );

    // --- the value range ---------------------------------------------------
    r.check("SETVAL(0, 32767)", semctl_raw(a, 0, SETVAL, 32767) == 0);
    r.check(
        "semop(+1) at 32767 is ERANGE",
        semop_raw(a, &one(0, 1, IPC_NOWAIT)) == -1 && errno() == libc::ERANGE,
    );
    r.check(
        "SETVAL(32768) is ERANGE",
        semctl_raw(a, 0, SETVAL, 32768) == -1 && errno() == libc::ERANGE,
    );
    r.check(
        "SETVAL(-1) is ERANGE too",
        semctl_raw(a, 0, SETVAL, (-1i64) as usize) == -1 && errno() == libc::ERANGE,
    );

    // --- the per-semaphore readers and the arrays --------------------------
    r.check("SETALL({1,2,3})", {
        let values: [u16; 3] = [1, 2, 3];
        semctl_raw(a, 0, SETALL, values.as_ptr() as usize) == 0
    });
    r.check("GETALL reads them back", {
        let mut values: [u16; 3] = [9, 9, 9];
        let rc = semctl_raw(a, 0, GETALL, values.as_mut_ptr() as usize);
        rc == 0 && values == [1, 2, 3]
    });
    r.check(
        "GETVAL past the set is EINVAL",
        semctl_raw(a, 3, GETVAL, 0) == -1 && errno() == libc::EINVAL,
    );
    r.check("GETNCNT and GETZCNT read zero with no waiters", {
        semctl_raw(a, 0, GETNCNT, 0) == 0 && semctl_raw(a, 0, GETZCNT, 0) == 0
    });
    r.check("GETPID reports this task's pid after a semop", {
        // A `-1` rather than a zero operation: the record is what this claim
        // reads, and a zero operation on a non-zero set would wait.
        semop_raw(a, &one(0, -1, 0)) == 0
            && semctl_raw(a, 0, GETPID, 0) == unsafe { libc::getpid() } as i64
    });
    r.check(
        "sem_otime is set by a semop that changed something",
        stat(a).is_some_and(|d| d.i64_at(SEMID_OTIME) != 0),
    );

    // --- blocking ----------------------------------------------------------
    // A child sleeps on -1; this process counts it, raises the value, and the
    // child's own semop is what reports the wake.
    let b = semget(IPC_PRIVATE, 1, 0o600);
    let child = unsafe { libc::fork() };
    if child == 0 {
        unsafe { libc::_exit(semop_raw(b, &one(0, -1, 0))) };
    }
    // Give the child a moment to reach its sleep before counting it: the count
    // *is* the claim, and a count taken too early would be zero on any kernel.
    std::thread::sleep(std::time::Duration::from_millis(100));
    r.check(
        "GETNCNT counts the blocked decrementer",
        semctl_raw(b, 0, GETNCNT, 0) == 1,
    );
    r.check(
        "raising the value wakes it",
        semop_raw(b, &one(0, 1, 0)) == 0,
    );
    let mut status = 0;
    unsafe { libc::waitpid(child, &mut status, 0) };
    r.check(
        "the woken child's semop succeeded",
        libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0,
    );

    // The relative timeout: a 120 ms wait is a 120 ms wait, and it ends EAGAIN.
    let c = semget(IPC_PRIVATE, 1, 0o600);
    let started = std::time::Instant::now();
    let rc = semtimedop(c, &one(0, -1, 0), std::time::Duration::from_millis(120));
    let elapsed = started.elapsed();
    r.check(
        "semtimedop's timeout is relative and expires with EAGAIN",
        rc == -1 && errno() == libc::EAGAIN && elapsed.as_millis() >= 100,
    );

    // A handler-bearing signal ends the wait with EINTR.
    let d = semget(IPC_PRIVATE, 1, 0o600);
    unsafe { libc::signal(libc::SIGUSR1, on_signal as *const () as libc::sighandler_t) };
    let child = unsafe { libc::fork() };
    if child == 0 {
        std::thread::sleep(std::time::Duration::from_millis(60));
        unsafe { libc::kill(libc::getppid(), libc::SIGUSR1) };
        unsafe { libc::_exit(0) };
    }
    let rc = semop_raw(d, &one(0, -1, 0));
    let interrupted = rc == -1 && errno() == libc::EINTR;
    unsafe { libc::waitpid(child, &mut status, 0) };
    r.check(
        "a handler-bearing signal ends the wait with EINTR",
        interrupted,
    );

    // --- IPC_RMID with a waiter -------------------------------------------
    let e = semget(IPC_PRIVATE, 1, 0o600);
    let child = unsafe { libc::fork() };
    if child == 0 {
        let rc = semop_raw(e, &one(0, -1, 0));
        unsafe {
            libc::_exit(if rc == -1 && errno() == libc::EIDRM {
                0
            } else {
                1
            })
        };
    }
    std::thread::sleep(std::time::Duration::from_millis(100));
    r.check(
        "IPC_RMID on a waited-on set succeeds",
        semctl_raw(e, 0, IPC_RMID, 0) == 0,
    );
    unsafe { libc::waitpid(child, &mut status, 0) };
    r.check(
        "the waiter wakes with EIDRM",
        libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0,
    );
    r.check(
        "a removed set's number is EINVAL for everything else",
        stat(e).is_none()
            && semop_raw(e, &one(0, 0, 0)) == -1
            && errno() == libc::EINVAL
            && val(e, 0) == -1,
    );

    // --- SEM_UNDO ----------------------------------------------------------
    let u = semget(key(3), 1, IPC_CREAT | 0o600);
    r.check(
        "SEM_UNDO: this process records +1",
        semop_raw(u, &one(0, 1, SEM_UNDO)) == 0,
    );
    r.check("the record is applied to the value at once", val(u, 0) == 1);
    let child = unsafe { libc::fork() };
    if child == 0 {
        // Exits without touching the set: a plain fork does not inherit the
        // parent's undo list, so nothing may be applied here.
        unsafe { libc::_exit(0) };
    }
    unsafe { libc::waitpid(child, &mut status, 0) };
    r.check(
        "a forked child's exit does not apply its parent's record",
        val(u, 0) == 1,
    );
    let child = unsafe { libc::fork() };
    if child == 0 {
        semop_raw(u, &one(0, 1, SEM_UNDO));
        unsafe { libc::_exit(0) };
    }
    unsafe { libc::waitpid(child, &mut status, 0) };
    r.check(
        "a child's own record is applied by its own exit",
        val(u, 0) == 1,
    );

    // unshare(CLONE_SYSVSEM) keeps the records: a child that records +1,
    // unshares and exits still has its adjustment applied.
    let child = unsafe { libc::fork() };
    if child == 0 {
        semop_raw(u, &one(0, 1, SEM_UNDO));
        unsafe { libc::syscall(libc::SYS_unshare, 0x0004_0000) };
        unsafe { libc::_exit(0) };
    }
    unsafe { libc::waitpid(child, &mut status, 0) };
    r.check(
        "unshare(CLONE_SYSVSEM) keeps the undo records",
        val(u, 0) == 1,
    );
    // A `pthread_create`d thread **shares** the process's list (that is what
    // `CLONE_SYSVSEM` is for, and musl passes it). The observable is the exit:
    // the thread's `+1` joins this process's records, so the thread's death is
    // not the list's last holder and applies nothing - the value stays at 2
    // where a per-thread list would have taken its own `+1` back to 1.
    r.check("a thread's undo record joins the process's list", {
        let before = val(u, 0);
        let id = u;
        std::thread::spawn(move || {
            semop_raw(id, &one(0, 1, SEM_UNDO));
        })
        .join()
        .expect("the thread runs");
        before == 1 && val(u, 0) == 2
    });
    // Take the value back down without an undo record so the claims above are
    // about the children's exits and not about an accumulated total.
    semop_raw(u, &one(0, -1, 0));
    semop_raw(u, &one(0, -1, 0));

    // --- the table-wide reports -------------------------------------------
    let mut info = [0i32; 10];
    let max = semctl_raw(0, 0, IPC_INFO, info.as_mut_ptr() as usize);
    r.check("IPC_INFO answers a slot index", max >= 0);
    r.check(
        "IPC_INFO reports this kernel's own limits",
        info[1] == 4096 && info[2] == 65_536 && info[4] == 1024 && info[5] == 500,
    );
    r.check(
        "IPC_INFO reports SEMVMX and SEMAEM",
        info[8] == 32767 && info[9] == 32767,
    );
    let mut usage = [0i32; 10];
    r.check(
        "SEM_INFO answers a slot index and the same limits",
        semctl_raw(0, 0, SEM_INFO, usage.as_mut_ptr() as usize) >= 0
            && usage[1] == 4096
            && usage[2] == 65_536,
    );
    r.check("SEM_STAT by slot names a real set", {
        let mut found = None;
        for slot in 0..=max {
            let mut ds = Ds::zeroed();
            let rc = semctl_raw(slot as i32, 0, SEM_STAT, ds.ptr());
            if rc >= 0 {
                found = Some((rc as i32, ds));
                break;
            }
        }
        found.is_some_and(|(id, _)| stat(id).is_some())
    });

    // `SEM_STAT_ANY` is the monitoring spelling: it takes the table *slot* and
    // asks no read permission, which is the whole reason it exists (`ipcs` must
    // list a set nobody may read). Measured on a host first, in the machine's
    // own IPC namespace - where this caller holds no `CAP_IPC_OWNER`, which is
    // the capability that would let the plain spelling answer anyway - and asked
    // here from a child that has dropped to nobody, so the claim reads the same
    // on a machine whose root holds every capability.
    let marked = semget(key(9), 1, IPC_CREAT | 0o600);
    r.check("a set for the monitoring spelling", marked >= 0);
    if marked >= 0 {
        let mut ds = Ds::zeroed();
        r.check(
            "its mode is set to 0000",
            semctl_raw(marked, 0, IPC_STAT, ds.ptr()) == 0 && {
                ds.bytes[SEM_PERM_MODE..SEM_PERM_MODE + 4].copy_from_slice(&0u32.to_ne_bytes());
                semctl_raw(marked, 0, IPC_SET, ds.ptr()) == 0
            },
        );
        let slot = marked % 32768;
        let child = unsafe { libc::fork() };
        if child == 0 {
            unsafe {
                libc::setgid(65534);
                libc::setuid(65534);
            }
            let mut d = Ds::zeroed();
            let mut code = 0i32;
            if semctl_raw(marked, 0, SEM_STAT, d.ptr()) == -1
                && std::io::Error::last_os_error().raw_os_error() == Some(libc::EACCES)
            {
                code |= 1;
            }
            if semctl_raw(slot, 0, SEM_STAT_ANY, d.ptr()) == marked as i64 {
                code |= 2;
            }
            unsafe { libc::_exit(code) };
        }
        let mut status = 0;
        unsafe { libc::waitpid(child, &mut status, 0) };
        let code = if libc::WIFEXITED(status) {
            libc::WEXITSTATUS(status)
        } else {
            0x7f
        };
        r.check("SEM_STAT on a set nobody may read is EACCES", code & 1 != 0);
        r.check(
            "SEM_STAT_ANY answers the same set from its slot",
            code & 2 != 0,
        );
        semctl_raw(marked, 0, IPC_RMID, 0);
    }

    // --- /proc/sysvipc/sem -------------------------------------------------
    let p = semget(key(4), 2, IPC_CREAT | 0o600);
    let text = std::fs::read_to_string("/proc/sysvipc/sem").expect("/proc/sysvipc/sem reads");
    r.check(
        "/proc/sysvipc/sem starts with the header ipcs parses",
        text.starts_with(
            "       key      semid perms      nsems   uid   gid  cuid  cgid      otime      ctime",
        ),
    );
    let row = text
        .lines()
        .find(|line| line.split_whitespace().nth(1) == Some(&p.to_string()))
        .unwrap_or_default();
    r.check("/proc/sysvipc/sem carries our set's row", !row.is_empty());
    // The column *positions* are the ABI for a reader that slices the line: the
    // first three fields must be the header's own widths.
    let expected_prefix = format!("{:10}{:11}{:6o}", key(4), p, 0o600);
    r.check(
        "the row's key, id and permissions sit in the header's columns",
        row.starts_with(&expected_prefix),
    );
    r.check(
        "the row reports the set's nsems",
        row.split_whitespace().nth(3) == Some("2"),
    );

    // --- another user ------------------------------------------------------
    // Root reaches any set through `CAP_IPC_OWNER`; a caller without it is
    // refused by the triads, and the two refusals are two different errnos.
    let other = semget(IPC_PRIVATE, 1, 0o600);
    let child = unsafe { libc::fork() };
    if child == 0 {
        unsafe {
            libc::setgid(65534);
            libc::setuid(65534);
        }
        let mut ds = Ds::zeroed();
        let mut values: [u16; 1] = [0];
        let mut code = 0i32;
        let mut mark = |bit: i32, ok: bool| {
            if ok {
                code |= 1 << bit;
            }
        };
        mark(
            0,
            semop_raw(other, &one(0, 0, IPC_NOWAIT)) == -1 && errno() == libc::EACCES,
        );
        mark(
            1,
            semctl_raw(other, 0, GETVAL, 0) == -1 && errno() == libc::EACCES,
        );
        mark(
            2,
            semctl_raw(other, 0, GETALL, values.as_mut_ptr() as usize) == -1
                && errno() == libc::EACCES,
        );
        mark(
            3,
            semctl_raw(other, 0, SETVAL, 1) == -1 && errno() == libc::EACCES,
        );
        mark(
            4,
            semctl_raw(other, 0, IPC_STAT, ds.ptr()) == -1 && errno() == libc::EACCES,
        );
        mark(
            5,
            semctl_raw(other, 0, IPC_SET, ds.ptr()) == -1 && errno() == libc::EPERM,
        );
        mark(
            6,
            semctl_raw(other, 0, IPC_RMID, 0) == -1 && errno() == libc::EPERM,
        );
        // An **existing** key (the one the `/proc` claim made, owned by root
        // with mode 0600): a key that names nothing is `ENOENT`, not `EACCES`.
        mark(7, semget(key(4), 1, 0o600) == -1 && errno() == libc::EACCES);
        unsafe { libc::_exit(code) };
    }
    unsafe { libc::waitpid(child, &mut status, 0) };
    let code = if libc::WIFEXITED(status) {
        libc::WEXITSTATUS(status)
    } else {
        -1
    };
    r.check("another user's semop is EACCES", code & 1 != 0);
    r.check("another user's GETVAL is EACCES", code & 2 != 0);
    r.check("another user's GETALL is EACCES", code & 4 != 0);
    r.check("another user's SETVAL is EACCES", code & 8 != 0);
    r.check("another user's IPC_STAT is EACCES", code & 16 != 0);
    r.check("another user's IPC_SET is EPERM", code & 32 != 0);
    r.check("another user's IPC_RMID is EPERM", code & 64 != 0);
    r.check(
        "another user's semget on the key is EACCES",
        code & 128 != 0,
    );
    // A ninth claim needs a **second** child: an exit status carries eight bits,
    // and the first draft folded this one into bit 8, where it was silently
    // truncated and read as a failure by a bit that could never be set.
    let child = unsafe { libc::fork() };
    if child == 0 {
        unsafe {
            libc::setgid(65534);
            libc::setuid(65534);
        }
        // The owner of a `0000` set still gets EACCES: the triads are the whole
        // answer and ownership does not override them.
        let mine = semget(IPC_PRIVATE, 1, 0);
        let refused = mine >= 0 && semctl_raw(mine, 0, GETVAL, 0) == -1 && errno() == libc::EACCES;
        unsafe { libc::_exit(if refused { 0 } else { 1 }) };
    }
    unsafe { libc::waitpid(child, &mut status, 0) };
    r.check(
        "the owner of a 0000 set is refused by the triads too",
        libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0,
    );

    // Clean up what is still ours.
    for id in [a, wide, k1, b, c, d, u, p, other] {
        semctl_raw(id, 0, IPC_RMID, 0);
    }
    r.finish();
}

extern "C" fn on_signal(_sig: libc::c_int) {}
