//! `name_to_handle_at`/`open_by_handle_at` on a filesystem that has no handles.
//!
//! The pair exists so a caller can name an object by something a rename cannot
//! invalidate. This kernel issues no handles at all - there is no export table
//! in any of its filesystems - so the honest answer for a served call is
//! `EOPNOTSUPP`, the same answer Linux gives on a filesystem without export
//! operations. Measured on a current host (Linux 7.2.14,
//! `build/scratch/nhandle/{p,q,r,t,u,v,w}.c`), on `/proc/self/stat`, which is
//! such a filesystem there:
//!
//! ```
//! plain call                             = EOPNOTSUPP(95)
//! null handle / null mount_id / ptr=1    = EOPNOTSUPP(95)   <- no write, no check
//! flags 0x100 / 0x800                    = EINVAL(22)
//! flags 0x200 / 0x400 / 0x1000           = EOPNOTSUPP(95)
//! bad dirfd, flags 0                     = EBADF(9)
//! bad dirfd, flags 0x100                 = EINVAL(22)       <- flags first
//! null path                              = EFAULT(14)
//! missing path                           = ENOENT(2)
//! open_by_handle_at, handle_bytes 0       = EINVAL(22)
//! open_by_handle_at, handle_bytes 8       = EPERM(1) without CAP_DAC_READ_SEARCH
//! ```
//!
//! Two of those are the ones a wrapper would get wrong: the flag word is read
//! before the descriptor (so `(999, flags=0x100)` is `EINVAL`, not `EBADF`),
//! and the output pointers are never validated on a filesystem that has
//! nothing to write there (so a null `handle` is `EOPNOTSUPP`, not `EFAULT` -
//! the opposite of what the same call answers on `tmpfs`, which does export).
//! The last line is capability-shaped: this kernel answers `EPERM` to a caller
//! without `CAP_DAC_READ_SEARCH` and `EOPNOTSUPP` to one that has it, so the
//! case is judged against `/proc/self/status`'s `CapEff` rather than assumed.

use mpos_user_tests::Report;

/// `EOPNOTSUPP`: present, cannot do that.
const EOPNOTSUPP: i32 = 95;
const EPERM: i32 = 1;
const EINVAL: i32 = 22;
const EBADF: i32 = 9;
const EFAULT: i32 = 14;
const ENOENT: i32 = 2;
const OK: i32 = 0;

/// `CAP_DAC_READ_SEARCH`, the capability `open_by_handle_at` asks for.
const CAP_DAC_READ_SEARCH: u32 = 2;

/// The `struct file_handle` both doors take: a size, a type and a body. The
/// kernel reads only the first field unless it is about to fill the rest, but
/// the call takes a whole record, so a fixture that shaped it smaller would be
/// measuring a different call.
#[repr(C)]
struct FileHandle {
    handle_bytes: u32,
    handle_type: i32,
    f_handle: [u8; 128],
}

const _: () = assert!(core::mem::size_of::<FileHandle>() == 136);

fn handle() -> FileHandle {
    FileHandle {
        handle_bytes: 128,
        handle_type: -1,
        f_handle: [0; 128],
    }
}

/// One call, its answer, and errno read immediately after it - never in the
/// same expression as the call, because Rust does happen to order those, but
/// the probes that measured this (C) did not.
fn name_to_handle_at(
    dirfd: i32,
    path: *const u8,
    handle: *mut FileHandle,
    mount_id: *mut i32,
    flags: i32,
) -> i32 {
    let ret = unsafe {
        libc::syscall(
            libc::SYS_name_to_handle_at,
            dirfd,
            path,
            handle,
            mount_id,
            flags,
        )
    };
    if ret == -1 {
        std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
    } else {
        OK
    }
}

fn open_by_handle_at(mount_fd: i32, handle: *mut FileHandle, flags: i32) -> i32 {
    let ret = unsafe { libc::syscall(libc::SYS_open_by_handle_at, mount_fd, handle, flags) };
    if ret == -1 {
        std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
    } else {
        OK
    }
}

/// One `name_to_handle_at` against `/proc/self/stat` - a path that exists on
/// both machines and is served by a filesystem with no export operations on
/// either.
fn probe(flags: i32) -> i32 {
    let path = c"/proc/self/stat";
    let mut h = handle();
    let mut mid: i32 = 0;
    name_to_handle_at(
        libc::AT_FDCWD,
        path.as_ptr() as *const u8,
        &mut h,
        &mut mid,
        flags,
    )
}

/// The effective capability set, as `/proc/self/status` reports it.
fn capabilities() -> Option<u64> {
    let text = std::fs::read_to_string("/proc/self/status").ok()?;
    let line = text
        .lines()
        .find(|line| line.starts_with("CapEff:"))
        .and_then(|line| line.split_whitespace().nth(1))?;
    u64::from_str_radix(line, 16).ok()
}

fn main() {
    let mut r = Report::new();
    let path = c"/proc/self/stat";
    let path_ptr = path.as_ptr() as *const u8;

    // A served call on a filesystem with no handles, and the three flags the
    // word takes.
    r.check(
        &format!(
            "a served call on a filesystem without handles (got {})",
            probe(0)
        ),
        probe(0) == EOPNOTSUPP,
    );
    for flags in [0x400, 0x1000] {
        r.check(
            &format!(
                "flag {flags:#x} is part of the vocabulary (got {})",
                probe(flags)
            ),
            probe(flags) == EOPNOTSUPP,
        );
    }
    // `0x200` is `AT_HANDLE_FID`, and it is the one flag whose *answer* differs
    // here. Linux builds a fid for it on any inode - `procfs` measured 0 - because
    // the fid is deliberately non-decodable: it exists to be handed to fanotify,
    // which this kernel does not have. So this pin witnesses the vocabulary (the
    // flag is not `EINVAL`) and the divergence is a named gap (§6.140).
    r.check(
        &format!(
            "flag 0x200 is part of the vocabulary (got {})",
            probe(0x200)
        ),
        probe(0x200) != EINVAL,
    );
    r.check(
        &format!(
            "and this kernel has no fid to build for it (got {})",
            probe(0x200)
        ),
        if mpos_user_tests::host_run() {
            true
        } else {
            probe(0x200) == EOPNOTSUPP
        },
    );
    for flags in [0x100, 0x800, 0x2000] {
        r.check(
            &format!("flag {flags:#x} is refused (got {})", probe(flags)),
            probe(flags) == EINVAL,
        );
    }

    // The flag word is read before the descriptor.
    // A *relative* name is the one that has to look at the descriptor.
    let relative = c"thing";
    let relative_ptr = relative.as_ptr() as *const u8;
    let mut h = handle();
    let mut mid: i32 = 0;
    r.check(
        &format!(
            "a bad descriptor with a relative name (got {})",
            name_to_handle_at(999, relative_ptr, &mut h, &mut mid, 0)
        ),
        name_to_handle_at(999, relative_ptr, &mut h, &mut mid, 0) == EBADF,
    );
    let mut h = handle();
    let mut mid: i32 = 0;
    r.check(
        &format!(
            "the flag word is judged before the descriptor (got {})",
            name_to_handle_at(999, relative_ptr, &mut h, &mut mid, 0x100)
        ),
        name_to_handle_at(999, relative_ptr, &mut h, &mut mid, 0x100) == EINVAL,
    );

    // An absolute name ignores the descriptor (Linux's at-family rule), so the
    // same bad descriptor serves this call.
    let mut h = handle();
    let mut mid: i32 = 0;
    r.check(
        &format!(
            "an absolute name never looks at the descriptor (got {})",
            name_to_handle_at(999, path_ptr, &mut h, &mut mid, 0)
        ),
        name_to_handle_at(999, path_ptr, &mut h, &mut mid, 0) == EOPNOTSUPP,
    );

    // The name is the next question, and a null one is `EFAULT`.
    let mut h = handle();
    let mut mid: i32 = 0;
    let null_name = name_to_handle_at(libc::AT_FDCWD, core::ptr::null(), &mut h, &mut mid, 0);
    r.check(
        &format!("a null name is not readable (got {null_name})"),
        null_name == EFAULT,
    );
    let missing = c"/no/such/thing";
    let mut h = handle();
    let mut mid: i32 = 0;
    let absent = name_to_handle_at(
        libc::AT_FDCWD,
        missing.as_ptr() as *const u8,
        &mut h,
        &mut mid,
        0,
    );
    r.check(
        &format!("a name nothing answers to (got {absent})"),
        absent == ENOENT,
    );

    // On a filesystem with nothing to write, the output pointers are never
    // looked at - and that is the half a wrapper would get wrong by validating
    // first.
    let mut mid: i32 = 0;
    let null_handle =
        name_to_handle_at(libc::AT_FDCWD, path_ptr, core::ptr::null_mut(), &mut mid, 0);
    r.check(
        &format!("a null handle is not an error here (got {null_handle})"),
        null_handle == EOPNOTSUPP,
    );
    let mut h = handle();
    let null_mount = name_to_handle_at(libc::AT_FDCWD, path_ptr, &mut h, core::ptr::null_mut(), 0);
    r.check(
        &format!("a null mount_id is not an error here (got {null_mount})"),
        null_mount == EOPNOTSUPP,
    );

    // `open_by_handle_at`: the shape first, then the privilege. The expected
    // answer after the privilege is the one that depends on the caller, so it
    // is computed from the capability set rather than written down.
    let mut h = FileHandle {
        handle_bytes: 0,
        handle_type: 0,
        f_handle: [0; 128],
    };
    let empty = open_by_handle_at(libc::AT_FDCWD, &mut h, libc::O_RDONLY);
    r.check(
        &format!("a handle with no bytes is refused (got {empty})"),
        empty == EINVAL,
    );

    let privileged = capabilities().is_some_and(|set| set & (1 << CAP_DAC_READ_SEARCH) != 0);
    // A real mount descriptor, because `open_by_handle_at` takes one: `AT_FDCWD`
    // is not an open file, and a privileged caller reaching past the capability
    // gets `EBADF` from it on this machine (the same probe on the host never
    // reaches that check, because the capability refuses first).
    let mount = std::fs::File::open("/proc").expect("/proc opens");
    let mount_fd = std::os::fd::AsRawFd::as_raw_fd(&mount);
    let mut h = FileHandle {
        handle_bytes: 8,
        handle_type: 1,
        f_handle: [0x5a; 128],
    };
    let shaped = open_by_handle_at(mount_fd, &mut h, libc::O_RDONLY);
    let want = if privileged { EOPNOTSUPP } else { EPERM };
    r.check(
        &format!(
            "the capability decides which refusal a shaped handle meets (got {shaped}, want {want}, privileged {privileged})"
        ),
        shaped == want,
    );

    // The pair is dispatched at all: `ENOSYS` is the answer this replaced, and
    // a caller that hears it has no fallback.
    r.check(
        &format!("neither door answers ENOSYS (got {})", probe(0)),
        probe(0) != 38,
    );

    r.finish();
}
