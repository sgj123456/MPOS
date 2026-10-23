//! t-sysv-shm - System V shared memory: `shmget`, `shmat`, `shmdt`, `shmctl`,
//! and the `/proc/sysvipc/shm` row that describes what they made.
//!
//! Every claim is one measured on a current host first (Linux 7.2.11,
//! `.workbuddy/probe_sysv_shm.c`, `probe_shm_perm.c`, `probe_shm_rmid.c`,
//! `probe_shm_ids.c`; the evidence and the decisions are `docs/notes/` §6.13)
//! and then pinned against this kernel, which had no System V objects at all
//! before the change this program ships with:
//!
//! * a segment is **sized as asked** and mapped in whole pages - `shm_segsz`
//!   reports 100 for a 100-byte request that maps one page;
//! * the key rules: `IPC_PRIVATE` always makes a new one, a key makes one once,
//!   `IPC_CREAT|IPC_EXCL` on a live key is `EEXIST`, no `IPC_CREAT` on a dead
//!   one is `ENOENT`, and a size larger than the segment is `EINVAL` (a smaller
//!   one is accepted);
//! * the mode is the low nine bits of `shmflg` and **no umask** is applied;
//! * attach placement: `NULL` gets a page-aligned address, a hint is taken
//!   exactly when free, a busy hint is `EINVAL`, a misaligned one is `EINVAL`
//!   unless `SHM_RND` rounds it, and two attaches alias the same pages;
//! * `SHM_RDONLY` maps a page the process may not write (the child that tries
//!   dies on `SIGSEGV`);
//! * `shmdt` names an attachment: an offset into one and a second detach are
//!   both `EINVAL`;
//! * `shm_nattch` counts address spaces, so a `fork` raises it and the child's
//!   exit lowers it again;
//! * `IPC_RMID` marks a segment: while attached its number keeps answering
//!   `IPC_STAT` and `shmat`, its key reads back as `IPC_PRIVATE`, a `shmget` on
//!   that key makes a *different* segment, and the number is `EINVAL` only after
//!   the last detach;
//! * `IPC_STAT` on another user's segment is `EACCES`, `IPC_SET`/`IPC_RMID` are
//!   `EPERM`, and `shmat` of one is `EACCES` unless the caller may read;
//! * the table-wide `IPC_INFO`/`SHM_INFO`/`SHM_STAT` reports answer Linux's
//!   values and this kernel's limits, and `/proc/sysvipc/shm` prints the row
//!   `ipcs` parses.
//!
//! `libc` rather than `rustix`: `rustix` binds no System V IPC call at all,
//! which is the first clause of `user/tests/Cargo.toml`'s rule. The three
//! `shmctl` records are spelled here as `#[repr(C)]` structs because the layout
//! is the ABI - it is the asm-generic one on both machines, and the offsets this
//! file relies on are the ones the host probe printed.
//!
//! No claim blocks: every wait is a `fork`/`waitpid`, which the sweep already
//! relies on elsewhere.

use mpos_user_tests::Report;

/// `struct ipc64_perm` + `struct shmid64_ds`, as `/usr/include/asm-generic/
/// shmbuf.h` lays them out. Both machines are 64-bit, so one spelling serves
/// both; the `_reserved` fields are what keep `shm_segsz` at offset 48.
#[repr(C)]
#[derive(Clone, Copy)]
struct ShmidDs {
    key: i32,
    uid: u32,
    gid: u32,
    cuid: u32,
    cgid: u32,
    mode: u32,
    seq: u16,
    pad2: u16,
    _unused1: u64,
    _unused2: u64,
    segsz: usize,
    atime: i64,
    dtime: i64,
    ctime: i64,
    cpid: i32,
    lpid: i32,
    nattch: u64,
    _unused4: u64,
    _unused5: u64,
}

/// `struct shminfo64`.
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Shminfo64 {
    shmmax: u64,
    shmmin: u64,
    shmmni: u64,
    shmseg: u64,
    shmall: u64,
    _unused: [u64; 4],
}

/// `struct shm_info`: an `int` and five longs, with the int padded out.
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct ShmInfo {
    used_ids: i32,
    _pad: i32,
    shm_tot: u64,
    shm_rss: u64,
    shm_swp: u64,
    swap_attempts: u64,
    swap_successes: u64,
}

/// `shmctl`'s commands (`linux/ipc.h`, `linux/shm.h`).
const IPC_RMID: i32 = 0;
const IPC_SET: i32 = 1;
const IPC_STAT: i32 = 2;
const IPC_INFO: i32 = 3;
const SHM_LOCK: i32 = 11;
const SHM_UNLOCK: i32 = 12;
const SHM_STAT: i32 = 13;
const SHM_INFO: i32 = 14;

const IPC_PRIVATE: i32 = 0;
const IPC_CREAT: i32 = 0o1000;
const IPC_EXCL: i32 = 0o2000;

const SHM_RDONLY: i32 = 0o10000;
const SHM_RND: i32 = 0o20000;

fn errno() -> i32 {
    std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
}

/// `shmctl`, through the syscall - `libc`'s wrapper takes its own `shmid_ds`
/// whose field names differ between the crate's targets, and the record's
/// *bytes* are the claim here.
fn shmctl_raw(id: i32, cmd: i32, buf: usize) -> i64 {
    unsafe { libc::syscall(libc::SYS_shmctl, id, cmd, buf) as i64 }
}

fn stat(id: i32) -> Option<ShmidDs> {
    let mut ds: ShmidDs = unsafe { std::mem::zeroed() };
    let rc = shmctl_raw(id, IPC_STAT, &mut ds as *mut ShmidDs as usize);
    (rc == 0).then_some(ds)
}

fn get(key: i32, size: usize, flags: i32) -> i32 {
    unsafe { libc::shmget(key as libc::key_t, size, flags) }
}

fn at(id: i32, addr: usize, flags: i32) -> usize {
    unsafe { libc::shmat(id, addr as *const libc::c_void, flags) as usize }
}

fn detach(addr: usize) -> i32 {
    unsafe { libc::shmdt(addr as *const libc::c_void) }
}

/// A key no other claim in this program uses, so claims cannot collide with each
/// other or with a previous run's leftovers (a keyed segment outlives its maker
/// until somebody removes it).
fn key(n: i32) -> i32 {
    0x5100_0000 + n
}

fn main() {
    let mut r = Report::new();

    // --- creation, size, defaults -----------------------------------------
    let a = get(IPC_PRIVATE, 100, 0o600);
    r.check("IPC_PRIVATE makes a segment", a >= 0);
    let ds = stat(a).expect("IPC_STAT on a fresh segment");
    r.check(
        "shm_segsz is the size asked for, not page-rounded",
        ds.segsz == 100,
    );
    r.check(
        "a fresh segment's mode is its shmflg's low nine bits",
        ds.mode == 0o600,
    );
    r.check(
        "owner and creator are the caller",
        ds.uid == unsafe { libc::geteuid() } && ds.cuid == unsafe { libc::geteuid() },
    );
    r.check("a fresh segment has no attachments", ds.nattch == 0);
    r.check("a fresh segment reports no attach time", ds.atime == 0);
    r.check(
        "shmget refuses a zero size",
        get(IPC_PRIVATE, 0, 0o600) == -1 && errno() == libc::EINVAL,
    );
    r.check(
        "shmget refuses a size past SHMMAX",
        get(IPC_PRIVATE, (64 << 20) + 1, 0o600) == -1 && errno() == libc::EINVAL,
    );

    // --- the umask is not consulted ---------------------------------------
    let old = unsafe { libc::umask(0o777) };
    let wide = get(IPC_PRIVATE, 4096, 0o666);
    unsafe { libc::umask(old) };
    r.check(
        "shmget stores the mode it was asked for (no umask)",
        stat(wide).is_some_and(|d| d.mode == 0o666),
    );

    // --- the key rules -----------------------------------------------------
    let k = key(1);
    let k1 = get(k, 4096, IPC_CREAT | 0o600);
    r.check("a new key makes a segment", k1 >= 0);
    r.check(
        "the same key with no IPC_CREAT finds it",
        get(k, 4096, 0o600) == k1,
    );
    r.check(
        "IPC_CREAT|IPC_EXCL on a live key is EEXIST",
        get(k, 4096, IPC_CREAT | IPC_EXCL | 0o600) == -1 && errno() == libc::EEXIST,
    );
    r.check(
        "a size larger than the segment is EINVAL",
        get(k, 8192, 0o600) == -1 && errno() == libc::EINVAL,
    );
    r.check(
        "a size smaller than the segment is accepted",
        get(k, 100, 0o600) == k1,
    );
    r.check(
        "a key with no IPC_CREAT and no segment is ENOENT",
        get(key(2), 4096, 0o600) == -1 && errno() == libc::ENOENT,
    );
    r.check(
        "an unknown number names no segment",
        detach(0) == -1 && errno() == libc::EINVAL,
    );
    let mut scratch = ShmidDs::default_zero();
    r.check(
        "shmctl on an unknown number is EINVAL",
        shmctl_raw(0x3fff_ffff, IPC_STAT, &mut scratch as *mut ShmidDs as usize) == -1
            && errno() == libc::EINVAL,
    );

    // --- attach ------------------------------------------------------------
    let m1 = at(a, 0, 0);
    r.check(
        "shmat(NULL) returns an address",
        m1 != usize::MAX && m1 != 0,
    );
    r.check("the address is page-aligned", m1 % 4096 == 0);
    unsafe {
        (m1 as *mut u8).write(b'A');
        ((m1 + 4095) as *mut u8).write(b'Z');
    }
    r.check("the mapping is read-write", unsafe {
        (m1 as *const u8).read() == b'A' && ((m1 + 4095) as *const u8).read() == b'Z'
    });
    let m2 = at(a, 0, 0);
    r.check(
        "a second attach aliases the same pages",
        m2 != m1 && unsafe { (m2 as *const u8).read() == b'A' },
    );
    r.check(
        "nattch counts both attachments",
        stat(a).is_some_and(|d| d.nattch == 2),
    );
    r.check(
        "shm_atime and shm_lpid record the attach",
        stat(a).is_some_and(|d| d.atime != 0 && d.lpid == unsafe { libc::getpid() }),
    );

    // A hint is taken exactly when it is free, refused when it is not.
    let free = 0x7000_0000_0000usize;
    let hinted = at(a, free, 0);
    r.check("a free hint is taken exactly", hinted == free);
    r.check(
        "a busy hint is EINVAL rather than relocated",
        at(a, free, 0) == usize::MAX && errno() == libc::EINVAL,
    );
    r.check(
        "a misaligned hint without SHM_RND is EINVAL",
        at(a, free + 0x100, 0) == usize::MAX && errno() == libc::EINVAL,
    );
    let rounded = at(a, 0x7100_0000_0000usize + 0x123, SHM_RND);
    r.check(
        "SHM_RND rounds the hint down to a page",
        rounded % 4096 == 0 && rounded < usize::MAX,
    );
    detach(hinted);
    detach(rounded);
    detach(m2);

    // --- SHM_RDONLY -------------------------------------------------------
    let ro = get(IPC_PRIVATE, 4096, 0o600);
    let ro_child = unsafe { libc::fork() };
    if ro_child == 0 {
        // The Rust runtime installs a `SIGSEGV` handler of its own (for stack
        // overflow), and this child is *asking* for the fault - so it resets the
        // disposition first. Without this the kernel retries the fault three
        // times through a handler that cannot help, which is noise in the log and
        // three chances for the claim to end differently.
        unsafe { libc::signal(libc::SIGSEGV, libc::SIG_DFL) };
        let m = at(ro, 0, SHM_RDONLY);
        if m == usize::MAX {
            unsafe { libc::_exit(100) };
        }
        unsafe { (m as *mut u8).write(1) };
        unsafe { libc::_exit(0) };
    }
    let mut status = 0;
    unsafe { libc::waitpid(ro_child, &mut status, 0) };
    r.check(
        "a SHM_RDONLY mapping refuses a write (SIGSEGV)",
        libc::WIFSIGNALED(status) && libc::WTERMSIG(status) == libc::SIGSEGV,
    );

    // --- detach -----------------------------------------------------------
    r.check("shmdt of an attachment succeeds", detach(m1) == 0);
    r.check(
        "a second shmdt of the same address is EINVAL",
        detach(m1) == -1 && errno() == libc::EINVAL,
    );
    r.check(
        "an offset into an attachment is not an attachment",
        detach(m1 + 8) == -1 && errno() == libc::EINVAL,
    );

    // --- fork counts as an attachment -------------------------------------
    let fk = get(IPC_PRIVATE, 4096, 0o600);
    let fm = at(fk, 0, 0);
    r.check(
        "one attach reads nattch 1",
        stat(fk).is_some_and(|d| d.nattch == 1),
    );
    let child = unsafe { libc::fork() };
    if child == 0 {
        // The child inherits the mapping: Linux copies the VMA and counts the
        // copy, which is the measurement this claim exists for.
        let seen = stat(fk).map(|d| d.nattch).unwrap_or(0);
        unsafe { libc::_exit(seen as i32) };
    }
    let mut status = 0;
    unsafe { libc::waitpid(child, &mut status, 0) };
    r.check(
        "a forked child reads nattch 2",
        libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 2,
    );
    r.check(
        "the child's exit takes its attachment with it",
        stat(fk).is_some_and(|d| d.nattch == 1),
    );
    detach(fm);

    // --- IPC_RMID is a mark ------------------------------------------------
    let rk = key(3);
    let rid = get(rk, 4096, IPC_CREAT | 0o600);
    let rm = at(rid, 0, 0);
    unsafe { (rm as *mut u8).write(b'R') };
    r.check(
        "IPC_RMID on an attached segment succeeds",
        shmctl_raw(rid, IPC_RMID, 0) == 0,
    );
    r.check(
        "a marked segment still answers IPC_STAT",
        stat(rid).is_some_and(|d| d.nattch == 1),
    );
    r.check(
        "a marked segment's key reads back as IPC_PRIVATE",
        stat(rid).is_some_and(|d| d.key == 0),
    );
    let rm2 = at(rid, 0, 0);
    r.check(
        "a marked segment still accepts an attach",
        rm2 != usize::MAX,
    );
    let fresh_id = get(rk, 4096, IPC_CREAT | 0o600);
    r.check(
        "shmget on the old key makes a different segment",
        fresh_id >= 0 && fresh_id != rid,
    );
    r.check("the mapping made before IPC_RMID still reads", unsafe {
        (rm as *const u8).read() == b'R'
    });
    // Detach both attachments and the number must stop answering.
    detach(rm);
    r.check(
        "a marked segment with one attachment left still answers",
        stat(rid).is_some_and(|d| d.nattch == 1),
    );
    detach(rm2);
    r.check(
        "after the last detach the number is EINVAL",
        stat(rid).is_none() && shmctl_raw(rid, IPC_STAT, 0) == -1 && errno() == libc::EINVAL,
    );
    r.check(
        "after the last detach shmat is EINVAL",
        at(rid, 0, 0) == usize::MAX && errno() == libc::EINVAL,
    );

    // --- IPC_SET ----------------------------------------------------------
    let sid = get(IPC_PRIVATE, 4096, 0o600);
    let mut ds = stat(sid).expect("IPC_STAT before IPC_SET");
    ds.mode = 0o666;
    r.check(
        "IPC_SET changes the mode",
        shmctl_raw(sid, IPC_SET, &mut ds as *mut ShmidDs as usize) == 0
            && stat(sid).is_some_and(|d| d.mode == 0o666),
    );
    let mut ds = stat(sid).expect("IPC_STAT before the uid change");
    ds.uid = 12345;
    let rc = shmctl_raw(sid, IPC_SET, &mut ds as *mut ShmidDs as usize);
    r.check(
        "IPC_SET moves the uid and leaves the creator alone",
        rc == 0 && stat(sid).is_some_and(|d| d.uid == 12345 && d.cuid == 0),
    );

    // --- SHM_LOCK ---------------------------------------------------------
    let lid = get(IPC_PRIVATE, 4096, 0o600);
    r.check("SHM_LOCK succeeds", shmctl_raw(lid, SHM_LOCK, 0) == 0);
    r.check(
        "SHM_LOCK reads back as the 02000 bit in the mode",
        stat(lid).is_some_and(|d| d.mode & 0o2000 != 0 && d.mode & 0o777 == 0o600),
    );
    r.check("SHM_UNLOCK succeeds", shmctl_raw(lid, SHM_UNLOCK, 0) == 0);
    r.check(
        "SHM_UNLOCK clears the bit and keeps the triads",
        stat(lid).is_some_and(|d| d.mode == 0o600),
    );

    // --- the table-wide reports -------------------------------------------
    let mut info = Shminfo64::default();
    let max = shmctl_raw(0, IPC_INFO, &mut info as *mut Shminfo64 as usize);
    r.check("IPC_INFO answers a slot index", max >= 0);
    r.check(
        "IPC_INFO reports this kernel's own limits",
        info.shmmax == 64 << 20 && info.shmmin == 1 && info.shmmni == 4096 && info.shmall == 32_768,
    );
    let mut si = ShmInfo::default();
    r.check(
        "SHM_INFO answers a slot index",
        shmctl_raw(0, SHM_INFO, &mut si as *mut ShmInfo as usize) >= 0,
    );
    r.check(
        "SHM_INFO counts the live segments",
        si.used_ids > 0 && si.shm_tot > 0,
    );

    // Walk a slot and ask for the segment there; the id it answers must name a
    // segment `IPC_STAT` accepts.
    let mut slot_ds = ShmidDs::default_zero();
    let mut found = -1i64;
    for slot in 0..=max {
        let rc = shmctl_raw(slot as i32, SHM_STAT, &mut slot_ds as *mut ShmidDs as usize);
        if rc >= 0 {
            found = rc;
            break;
        }
    }
    r.check(
        "SHM_STAT by slot returns the id that names it",
        found >= 0 && stat(found as i32).is_some(),
    );

    // --- /proc/sysvipc/shm -------------------------------------------------
    let pk = key(4);
    let pid = get(pk, 8192, IPC_CREAT | 0o600);
    let line = std::fs::read_to_string("/proc/sysvipc/shm").expect("/proc/sysvipc/shm reads");
    r.check(
        "/proc/sysvipc/shm starts with the header ipcs parses",
        line.starts_with("       key      shmid perms"),
    );
    let row = line
        .lines()
        .find(|l| l.split_whitespace().nth(1) == Some(&pid.to_string()))
        .unwrap_or_default();
    let cols: Vec<&str> = row.split_whitespace().collect();
    r.check(
        "/proc/sysvipc/shm carries our segment's row",
        !row.is_empty(),
    );
    r.check(
        "the row's size and key are the segment's",
        cols.len() >= 4 && cols[0] == pk.to_string() && cols[3] == "8192",
    );

    // --- another user ------------------------------------------------------
    // Root can reach any segment (CAP_IPC_OWNER); a caller without it is
    // refused by the triads, and the three refusals are three different errnos.
    let other = get(IPC_PRIVATE, 4096, 0o600);
    let child = unsafe { libc::fork() };
    if child == 0 {
        unsafe {
            libc::setgid(65534);
            libc::setuid(65534);
        }
        let attach = at(other, 0, 0) == usize::MAX && errno() == libc::EACCES;
        let stat_denied = shmctl_raw(other, IPC_STAT, 0) == -1 && errno() == libc::EACCES;
        // A **valid** buffer: `IPC_SET` reads its record before it asks who is
        // calling, so a null one is `EFAULT` and never reaches the permission
        // check (measured, `.workbuddy/probe_ipc_order.c`). The first draft of
        // this claim passed 0 and read the `EFAULT` as the claim failing.
        let mut record: ShmidDs = unsafe { std::mem::zeroed() };
        let set_denied = shmctl_raw(other, IPC_SET, &mut record as *mut ShmidDs as usize) == -1
            && errno() == libc::EPERM;
        let rmid_denied = shmctl_raw(other, IPC_RMID, 0) == -1 && errno() == libc::EPERM;
        let code = (attach as i32) << 0
            | (stat_denied as i32) << 1
            | (set_denied as i32) << 2
            | (rmid_denied as i32) << 3;
        unsafe { libc::_exit(code) };
    }
    let mut status = 0;
    unsafe { libc::waitpid(child, &mut status, 0) };
    let code = if libc::WIFEXITED(status) {
        libc::WEXITSTATUS(status)
    } else {
        -1
    };
    r.check("another user's shmat is EACCES", code & 1 != 0);
    r.check("another user's IPC_STAT is EACCES", code & 2 != 0);
    r.check("another user's IPC_SET is EPERM", code & 4 != 0);
    r.check("another user's IPC_RMID is EPERM", code & 8 != 0);

    // Clean up what is still ours so a re-run starts from the same table.
    for id in [a, wide, k1, ro, fk, fresh_id, sid, lid, pid, other] {
        shmctl_raw(id, IPC_RMID, 0);
    }
    r.finish();
}

impl ShmidDs {
    /// A zeroed record for a call that will not use it (an unknown id's
    /// `IPC_STAT`, whose answer is the errno).
    fn default_zero() -> ShmidDs {
        unsafe { std::mem::zeroed() }
    }
}
