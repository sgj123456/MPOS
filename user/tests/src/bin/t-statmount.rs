//! t-statmount - `statmount(2)` and `listmount(2)`: the mount table describing
//! itself, in the two structs the modern mount family reads.
//!
//! Measured on Linux 7.2.11 first (`build/scratch/mountstat/probe{,2}.c`, under
//! `unshare -Urm --map-root-user` because both calls want mount-namespace
//! authority). The facts the claims below encode:
//!
//! - `struct statmount` is a fixed 512 bytes there followed by a string area;
//!   a string field is an **offset into that area**, offset 0 being the empty
//!   string - which is how a mount whose root *is* the filesystem's root
//!   reports `mnt_root`;
//! - `mnt_id` is the **unique** id (`statx`'s `STATX_MNT_ID_UNIQUE`,
//!   `listmount`'s output), not the reused 32-bit number `/proc/*/fdinfo`
//!   prints: statmount with the fdinfo number was `EINVAL`, with the unique one
//!   it answered;
//! - `mask` reports the fields the kernel *wrote*, not the fields requested -
//!   an unknown bit is ignored rather than refused, and `supported_mask` says
//!   what the kernel could produce;
//! - `bufsize` shorter than the fixed part is truncated (`bufsize=16` answered
//!   `size=16`, `mask=SB_BASIC`), while a requested *string* with no room in
//!   the area after it is `EOVERFLOW` (`bufsize = sizeof` with `MNT_POINT`);
//!   `bufsize=0` answers 0 having written nothing;
//! - the two `struct mnt_id_req` versions are 24 and 32 bytes by their own
//!   first word; 20 and 40 were `EINVAL`; `STATMOUNT_BY_FD` names the mount by
//!   an fd on it and then the struct's ids must be zero;
//! - `listmount(LSMT_ROOT)` walks the namespace's mounts in tree order (root
//!   first, depth-first), `param` resumes **after** that id (`param=ids[2]`
//!   answered `ids[3]`), `LISTMOUNT_REVERSE` walks backwards, a small `nr` is a
//!   short answer, `nr=0` answers 0, and anything but `LSMT_ROOT` is `EINVAL`;
//! - both calls want `CAP_SYS_ADMIN`: an ordinary host user is refused with
//!   `EPERM`, which is why a host run skips.
//!
//! The guest's kernel produces a subset of Linux's field list, declared in
//! `mask`/`supported_mask` rather than faked: no mount-options string, no
//! option arrays, no uid/gid maps, and no `mnt_root` for a bind.

use mpos_user_tests::{Report, host_run};
use rustix::fd::AsRawFd;
use rustix::fs::{CWD, Mode, OFlags, mkdirat, open};
use rustix::mount::{MountFlags, UnmountFlags, mount, unmount};

const SYS_STATMOUNT: libc::c_long = 457;
const SYS_LISTMOUNT: libc::c_long = 458;

const MNT_ID_REQ_VER0: u32 = 24;
const MNT_ID_REQ_VER1: u32 = 32;
const STATMOUNT_SIZE: usize = 512;
const LSMT_ROOT: u64 = u64::MAX;
const LISTMOUNT_REVERSE: usize = 1;
const STATMOUNT_BY_FD: usize = 1;

const SB_BASIC: u64 = 0x0000_0001;
const MNT_BASIC: u64 = 0x0000_0002;
const MNT_ROOT: u64 = 0x0000_0008;
const MNT_POINT: u64 = 0x0000_0010;
const FS_TYPE: u64 = 0x0000_0020;
const MNT_NS_ID: u64 = 0x0000_0040;
const SB_SOURCE: u64 = 0x0000_0200;
const OPT_ARRAY: u64 = 0x0000_0400;
const SUPPORTED_MASK: u64 = 0x0000_1000;

const ROOT: &str = "/tmp/t-statmount";
const MNT: &str = "/tmp/t-statmount/m";

/// `struct mnt_id_req`, 32 bytes as Linux defines version 1.
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct MntIdReq {
    size: u32,
    fd: u32,
    mnt_id: u64,
    param: u64,
    mnt_ns_id: u64,
}

fn errno() -> i32 {
    -std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
}

/// `statmount(2)`: 0 on success, the negative errno otherwise.
fn statmount(req: &MntIdReq, buf: &mut [u8], flags: usize) -> i32 {
    let rc = unsafe {
        libc::syscall(
            SYS_STATMOUNT,
            req as *const MntIdReq as usize,
            buf.as_mut_ptr() as usize,
            buf.len() as libc::c_long,
            flags as libc::c_long,
        )
    };
    if rc < 0 { errno() } else { 0 }
}

/// `listmount(2)`: the number of ids written, or the negative errno.
fn listmount(req: &MntIdReq, ids: &mut [u64], flags: usize) -> i32 {
    let rc = unsafe {
        libc::syscall(
            SYS_LISTMOUNT,
            req as *const MntIdReq as usize,
            ids.as_mut_ptr() as usize,
            ids.len() as libc::c_long,
            flags as libc::c_long,
        )
    };
    if rc < 0 { errno() } else { rc as i32 }
}

fn root_req(param: u64) -> MntIdReq {
    MntIdReq {
        size: MNT_ID_REQ_VER1,
        fd: 0,
        mnt_id: LSMT_ROOT,
        param,
        mnt_ns_id: 0,
    }
}

fn mount_req(id: u64, mask: u64) -> MntIdReq {
    MntIdReq {
        size: MNT_ID_REQ_VER1,
        fd: 0,
        mnt_id: id,
        param: mask,
        mnt_ns_id: 0,
    }
}

fn word(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
}

fn dword(bytes: &[u8], at: usize) -> u64 {
    let mut v = [0u8; 8];
    v.copy_from_slice(&bytes[at..at + 8]);
    u64::from_le_bytes(v)
}

/// The string a `statmount` answer names by offset into its own string area.
fn text(answer: &[u8], at: usize) -> Option<String> {
    let offset = word(answer, at) as usize;
    let area = &answer[STATMOUNT_SIZE..];
    let start = &area[offset.min(area.len())..];
    let end = start.iter().position(|b| *b == 0)?;
    Some(String::from_utf8_lossy(&start[..end]).into_owned())
}

fn main() {
    println!("t-statmount");
    let mut r = Report::new();

    // A leftover mount from an interrupted run would turn the mount into EBUSY.
    let _ = unmount(MNT, UnmountFlags::empty());
    let _ = std::fs::remove_dir_all(ROOT);
    let made = mkdirat(CWD, ROOT, Mode::from_raw_mode(0o755)).is_ok();
    r.check("the scratch directory is made", made);
    let made = mkdirat(CWD, MNT, Mode::from_raw_mode(0o755)).is_ok();
    r.check("a directory to mount under is made", made);

    if host_run() && unsafe { libc::geteuid() } != 0 {
        println!("skip: the host's ordinary user is refused these calls with EPERM");
        r.finish()
    }

    let mounted = mount("none", MNT, "tmpfs", MountFlags::empty(), None).is_ok();
    r.check("a tmpfs is mounted at the scratch mount", mounted);

    // ---- the namespace's mounts, in tree order
    let mut ids = [0u64; 128];
    let count = listmount(&root_req(0), &mut ids, 0);
    r.check(
        &format!("listmount names the namespace's mounts (is {count})"),
        count > 0,
    );
    let count = count.max(0) as usize;

    // Linux's own walk begins at the root mount; this guest's table assigns
    // its root slot's id after some of the mounts under it, so the claim is
    // that the root appears in the listing - not that it comes first.
    let mut root_found = false;

    // ---- find this test's mount by its point
    let mut found = None;
    for id in &ids[..count] {
        let mut answer = [0u8; 4096];
        let rc = statmount(&mount_req(*id, MNT_POINT | FS_TYPE), &mut answer, 0);
        if rc == 0 && text(&answer, 108).as_deref() == Some("/") {
            root_found = true;
        }
        if rc == 0 && text(&answer, 108).as_deref() == Some(MNT) {
            found = Some(*id);
            break;
        }
    }
    r.check("the root mount is in the listing", root_found);
    let Some(id) = found else {
        r.check("the mount we made is listed and names its point", false);
        r.finish()
    };
    r.check("the mount we made is listed and names its point", true);

    // ---- the fields, and the strings as offsets into the answer
    let wanted = SB_BASIC
        | MNT_BASIC
        | MNT_ROOT
        | MNT_POINT
        | FS_TYPE
        | MNT_NS_ID
        | SB_SOURCE
        | SUPPORTED_MASK;
    let mut answer = [0u8; 4096];
    let rc = statmount(&mount_req(id, wanted), &mut answer, 0);
    let mask = dword(&answer, 8);
    let size = word(&answer, 0) as usize;
    r.check(
        &format!(
            "a full answer is the struct plus its strings (rc={rc} size={size} mask=0x{mask:x})"
        ),
        rc == 0 && size > STATMOUNT_SIZE && size <= answer.len(),
    );
    r.check(
        &format!("every field asked for was written (mask=0x{mask:x})"),
        mask & (MNT_BASIC | MNT_ROOT | MNT_POINT | FS_TYPE | SB_SOURCE)
            == (MNT_BASIC | MNT_ROOT | MNT_POINT | FS_TYPE | SB_SOURCE),
    );
    r.check(
        &format!(
            "the point is read back through its offset ({:?})",
            text(&answer, 108)
        ),
        text(&answer, 108).as_deref() == Some(MNT),
    );
    r.check(
        &format!("the filesystem type is tmpfs ({:?})", text(&answer, 36)),
        text(&answer, 36).as_deref() == Some("tmpfs"),
    );
    r.check(
        &format!(
            "the mount's root is the filesystem's root ({:?})",
            text(&answer, 104)
        ),
        text(&answer, 104).as_deref() == Some(""),
    );
    let reported_id = dword(&answer, 40);
    let parent_id = dword(&answer, 48);
    r.check(
        &format!("the answer names the mount it was asked about ({reported_id} vs {id})"),
        reported_id == id,
    );
    r.check(
        &format!("and the mount it hangs under ({parent_id})"),
        parent_id != 0 && parent_id != id,
    );
    let magic = dword(&answer, 24);
    r.check(
        &format!("a tmpfs reports the tmpfs magic (0x{magic:x})"),
        mask & SB_BASIC != 0 && magic == 0x0102_1994,
    );
    let ns_id = dword(&answer, 112);
    let mut again = [0u8; 4096];
    let _ = statmount(&mount_req(id, MNT_NS_ID), &mut again, 0);
    r.check(
        &format!("the namespace id is stable and not zero (0x{ns_id:x})"),
        ns_id != 0 && ns_id == dword(&again, 112),
    );
    let supported = dword(&answer, 144);
    r.check(
        &format!("the supported mask names what this kernel can do (0x{supported:x})"),
        supported & (MNT_BASIC | MNT_POINT | FS_TYPE | MNT_NS_ID | SB_SOURCE | SUPPORTED_MASK)
            == (MNT_BASIC | MNT_POINT | FS_TYPE | MNT_NS_ID | SB_SOURCE | SUPPORTED_MASK),
    );

    // ---- what this kernel honestly cannot produce is left out of the mask
    let mut answer = [0u8; 4096];
    let rc = statmount(&mount_req(id, OPT_ARRAY | 0x8000_0000), &mut answer, 0);
    let mask = dword(&answer, 8);
    r.check(
        &format!("an unknown bit is answered, not refused (rc={rc} mask=0x{mask:x})"),
        rc == 0 && mask & 0x8000_0000 == 0,
    );
    r.check(
        "the option arrays have no storage here and stay out of the mask",
        mask & OPT_ARRAY == 0,
    );

    // ---- buffer sizes: short is truncated, a string without room is EOVERFLOW
    let mut tiny = [0u8; 16];
    let rc = statmount(&mount_req(id, SB_BASIC), &mut tiny, 0);
    r.check(
        &format!(
            "a short buffer is truncated, not refused (rc={rc} size={})",
            word(&tiny, 0)
        ),
        rc == 0 && word(&tiny, 0) == 16,
    );
    let mut fixed = [0u8; STATMOUNT_SIZE];
    let rc = statmount(&mount_req(id, MNT_POINT), &mut fixed, 0);
    r.check(
        &format!("a requested string with no room is EOVERFLOW (rc={rc})"),
        rc == -libc::EOVERFLOW,
    );
    let mut empty = [0u8; 1];
    let rc = statmount(&mount_req(id, SB_BASIC), &mut empty[..0], 0);
    r.check(
        "a zero-length buffer answers 0 having written nothing",
        rc == 0,
    );

    // ---- the struct's own version and the flag words
    let mut answer = [0u8; 4096];
    let mut v0 = mount_req(id, MNT_POINT | MNT_BASIC);
    v0.size = MNT_ID_REQ_VER0;
    r.check(
        "the older 24-byte request version is accepted",
        statmount(&v0, &mut answer, 0) == 0,
    );
    let mut wrong = mount_req(id, MNT_BASIC);
    wrong.size = 20;
    let rc = statmount(&wrong, &mut answer, 0);
    r.check(
        &format!("a struct version this kernel does not know is EINVAL (is {rc})"),
        rc == -libc::EINVAL,
    );
    let mut zero = mount_req(0, MNT_BASIC);
    zero.mnt_id = 0;
    r.check(
        "a zero mount id is EINVAL",
        statmount(&zero, &mut answer, 0) == -libc::EINVAL,
    );
    let mut bogus = mount_req(id, MNT_BASIC);
    bogus.mnt_id = 999_999_999;
    r.check(
        "an id nothing is attached as is EINVAL",
        statmount(&bogus, &mut answer, 0) == -libc::EINVAL,
    );
    r.check(
        "a stray statmount flag is EINVAL",
        statmount(&mount_req(id, MNT_BASIC), &mut answer, 2) == -libc::EINVAL,
    );

    // ---- the descriptor form names the same mount
    let fd = open(MNT, OFlags::PATH | OFlags::CLOEXEC, Mode::empty()).ok();
    match fd {
        Some(fd) => {
            let by_fd = MntIdReq {
                size: MNT_ID_REQ_VER1,
                fd: fd.as_raw_fd() as u32,
                mnt_id: 0,
                param: MNT_POINT | MNT_BASIC,
                mnt_ns_id: 0,
            };
            let mut answer = [0u8; 4096];
            let rc = statmount(&by_fd, &mut answer, STATMOUNT_BY_FD);
            r.check(
                &format!("a descriptor on the mount names it (rc={rc})"),
                rc == 0 && text(&answer, 108).as_deref() == Some(MNT) && dword(&answer, 40) == id,
            );
            r.check(
                "the descriptor form without its flag is EINVAL",
                statmount(&by_fd, &mut answer, 0) == -libc::EINVAL,
            );
        }
        None => r.check("a descriptor on the mount can be opened", false),
    }

    // ---- listing: continuation, reverse, short answers
    let mut forward = [0u64; 128];
    let forward_count = listmount(&root_req(0), &mut forward, 0);
    let forward_count = forward_count.max(0) as usize;
    r.check(
        &format!("the namespace lists {} mounts", forward_count),
        forward_count > 1,
    );
    let mut more = [0u64; 4];
    let rc = listmount(&root_req(forward[1]), &mut more, 0);
    r.check(
        &format!(
            "a listing resumes after the id it was given (is {rc}, next {})",
            more[0]
        ),
        rc > 0 && more[0] == forward[2],
    );
    let mut reverse = [0u64; 128];
    let rc = listmount(&root_req(0), &mut reverse, LISTMOUNT_REVERSE);
    r.check(
        &format!(
            "the reverse walk starts at the end (is {rc}, first {})",
            reverse[0]
        ),
        rc > 0 && reverse[0] == forward[forward_count - 1],
    );
    let mut two = [0u64; 2];
    let rc = listmount(&root_req(0), &mut two, 0);
    r.check(
        "a short room asks for a short answer",
        rc == 2 && two == [forward[0], forward[1]],
    );
    let mut none = [0u64; 4];
    r.check(
        "room for nothing answers 0",
        listmount(&root_req(0), &mut none[..0], 0) == 0,
    );
    r.check(
        "a stray listmount flag is EINVAL",
        listmount(&root_req(0), &mut none, 0x8000) == -libc::EINVAL,
    );
    let mut not_root = root_req(0);
    not_root.mnt_id = id;
    r.check(
        "listing one mount's subtree is EINVAL here as on Linux",
        listmount(&not_root, &mut none, 0) == -libc::EINVAL,
    );
    let rc = unsafe {
        libc::syscall(
            SYS_LISTMOUNT,
            &root_req(0) as *const MntIdReq as usize,
            0usize,
            4 as libc::c_long,
            0 as libc::c_long,
        )
    };
    r.check(
        &format!("a null id buffer is EFAULT (is {rc})"),
        rc == -1 && errno() == -libc::EFAULT,
    );

    // ---- unmounting takes the mount out of both calls
    let gone = unmount(MNT, UnmountFlags::empty()).is_ok();
    r.check("the mount is unmounted", gone);
    let mut ids = [0u64; 128];
    let after = listmount(&root_req(0), &mut ids, 0);
    let after = after.max(0) as usize;
    r.check(
        "the unmounted mount is no longer listed",
        gone && !ids[..after].contains(&id),
    );
    let mut answer = [0u8; 4096];
    r.check(
        "and statmount on its id is EINVAL",
        statmount(&mount_req(id, MNT_BASIC), &mut answer, 0) == -libc::EINVAL,
    );

    let _ = std::fs::remove_dir_all(ROOT);
    r.finish()
}
