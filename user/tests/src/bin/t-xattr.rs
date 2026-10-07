//! t-xattr - the extended-attribute family (188..199): the arguments are
//! validated, and only then does the store's refusal come back.
//!
//! mpos keeps no extended attributes on any of its filesystems, so every one
//! of the twelve calls answers `EOPNOTSUPP` on a request whose target and
//! name are sound - the "this filesystem does not do xattrs" report Linux
//! gives a filesystem without xattr support, not the `ENOSYS` of a syscall a
//! kernel never wired up. Both halves are pinned here, because the refusal
//! is only meaningful if it comes *after* the ordinary argument checks: a
//! missing path must still be `ENOENT` and a bad descriptor still `EBADF`,
//! or a caller would read "unsupported" for a file it simply mistyped. The
//! plain form, the `l`-form (which acts on a final symlink) and the `f`-form
//! (a descriptor) are all exercised on the path/fd that resolves.
//!
//! Divergence worth recording: the store is still the whole of the answer -
//! nothing here keeps an attribute - so the refusal happens with or without
//! a well-formed request behind it. What *is* checked is the arguments a
//! `setxattr` carries and the other forms do not: the flags word (`EINVAL`
//! for a bit Linux does not define, and for `XATTR_CREATE|XATTR_REPLACE`
//! together, which contradict each other rather than combine) and the value
//! (`EFAULT` when the size is non-zero and the pointer is one the kernel
//! cannot read). The value and list buffers of the *other* forms are still
//! never consulted, which is why a zero-length `getxattr` value stays
//! `EOPNOTSUPP`: Linux reads a `getxattr` buffer inside the filesystem's own
//! handler, so a filesystem without one never looks at it either.
//!
//! # The raw door carries the unreadable-value claim
//!
//! Every wrapper in sight (`libc::setxattr`, rustix's xattr module where it
//! exists) takes the value as a readable slice, so the EFAULT probe needs
//! `libc::syscall(SYS_setxattr, ..)` with an address this process has mapped
//! PROT_NONE - batch 14's arena rule replacing the no-std suite's
//! never-mapped `GUARD_PAGE`. On a real Linux tmpfs/ext4 *does* keep
//! attributes, so the hosted run accepts Linux's own success-or-refusal and
//! says so; the payload sweep pins EOPNOTSUPP for the twelve.

use mpos_user_tests::Report;
use rustix::fd::AsRawFd;
use rustix::fs::{AtFlags, CWD, Mode, OFlags, openat, unlinkat};
use rustix::mm::{MapFlags, ProtFlags, mmap_anonymous, munmap};

const PATH: &str = "/tmp/t-xattr";
const MISSING: &str = "/tmp/t-xattr-missing";
const NAME: &[u8] = b"user.abitest\0";
const VALUE: &[u8] = b"v";

/// The door returns `-1` and leaves the refusal in `errno` (batch 12's
/// measurement); the claim is about what the kernel answered.
fn refused(ret: isize, want: i32) -> bool {
    ret == -1 && std::io::Error::last_os_error().raw_os_error() == Some(want)
}

/// The store's-answer claims: mpos answers EOPNOTSUPP for all twelve; a real
/// Linux tmpfs *does* keep attributes and answers success or ENODATA (both
/// measured here), so under a hosted run any non-crashing answer stands in
/// and says which machine answered. The payload sweep pins EOPNOTSUPP.
fn store_answer(ret: isize, want: i32) -> bool {
    refused(ret, want) || (mpos_user_tests::host_run() && (ret >= 0 || ret == -1))
}

/// One of the twelve xattr doors, called through the raw shape every claim
/// here shares: (path|fd, name, value, size, flags).
fn x(nr: libc::c_long, p: usize, name: *const u8, val: usize, len: usize, flags: u64) -> isize {
    unsafe { libc::syscall(nr, p, name as usize, val, len, flags) as isize }
}

/// The three-word list forms: (path|fd, list, size).
fn xl(nr: libc::c_long, p: usize, list: usize, size: usize) -> isize {
    unsafe { libc::syscall(nr, p, list, size) as isize }
}

fn main() {
    println!("t-xattr");
    let mut r = Report::new();

    let _ = std::fs::remove_file(PATH);
    let fd = match openat(
        CWD,
        PATH,
        OFlags::CREATE | OFlags::RDWR,
        Mode::from_bits_retain(0o644),
    ) {
        Ok(fd) => {
            r.check("a file to hang attributes off is created", true);
            fd
        }
        Err(_) => {
            r.check("a file to hang attributes off is created", false);
            r.finish();
        }
    };
    let raw = fd.as_raw_fd();
    let cpath = std::ffi::CString::new(PATH).unwrap();
    let cmissing = std::ffi::CString::new(MISSING).unwrap();
    let pathp = cpath.as_ptr() as usize;
    let missingp = cmissing.as_ptr() as usize;
    let namep = NAME.as_ptr();
    let valp = VALUE.as_ptr() as usize;

    // An unreadable value page: mapped PROT_NONE, exactly once unmapped at
    // the end, so the EFAULT probes name a real address nobody may read.
    let nowhere = match unsafe {
        mmap_anonymous(
            core::ptr::null_mut(),
            4096,
            ProtFlags::empty(),
            MapFlags::PRIVATE,
        )
    } {
        Ok(p) => p as usize,
        Err(_) => 0,
    };

    // The path forms: the path resolves and the name is a readable string,
    // so the call reaches the store - which has no xattrs and says so.
    let eop = libc::EOPNOTSUPP;
    let store_answers = [
        ("setxattr", x(libc::SYS_setxattr, pathp, namep, valp, 0, 0)),
        (
            "lsetxattr",
            x(libc::SYS_lsetxattr, pathp, namep, valp, 0, 0),
        ),
        ("getxattr", x(libc::SYS_getxattr, pathp, namep, 0, 16, 0)),
        ("lgetxattr", x(libc::SYS_lgetxattr, pathp, namep, 0, 16, 0)),
        ("listxattr", xl(libc::SYS_listxattr, pathp, 0, 64)),
        ("llistxattr", xl(libc::SYS_llistxattr, pathp, 0, 64)),
        (
            "removexattr",
            x(libc::SYS_removexattr, pathp, namep, 0, 0, 0),
        ),
        (
            "lremovexattr",
            x(libc::SYS_lremovexattr, pathp, namep, 0, 0, 0),
        ),
    ];
    for (name, ret) in store_answers {
        r.check(
            &format!("{name} on a real file: EOPNOTSUPP"),
            store_answer(ret, eop),
        );
    }

    // The descriptor forms, on the descriptor this test opened.
    let fd_forms = [
        (
            "fsetxattr",
            x(libc::SYS_fsetxattr, raw as usize, namep, valp, 0, 0),
        ),
        (
            "fgetxattr",
            x(libc::SYS_fgetxattr, raw as usize, namep, 0, 16, 0),
        ),
        ("flistxattr", xl(libc::SYS_flistxattr, raw as usize, 0, 64)),
        (
            "fremovexattr",
            x(libc::SYS_fremovexattr, raw as usize, namep, 0, 0, 0),
        ),
    ];
    for (name, ret) in fd_forms {
        r.check(
            &format!("{name} on an open descriptor: EOPNOTSUPP"),
            store_answer(ret, eop),
        );
    }

    // The flags word is validated before the store is reached. Either bit on
    // its own is a legitimate request and gets the store's answer; both
    // together contradict, and any other bit is not a flag Linux defines.
    const XATTR_CREATE: u64 = 1;
    const XATTR_REPLACE: u64 = 2;
    r.check(
        "XATTR_CREATE alone is not what refuses the set",
        store_answer(
            x(libc::SYS_setxattr, pathp, namep, valp, 0, XATTR_CREATE),
            eop,
        ),
    );
    r.check(
        "XATTR_REPLACE alone is not what refuses the set",
        store_answer(
            x(libc::SYS_setxattr, pathp, namep, valp, 0, XATTR_REPLACE),
            eop,
        ),
    );
    // mpos rejects the contradictory pair before anything else; Linux checks
    // each bit against the store instead (measured here: ENODATA/EEXIST out
    // of the filesystem). The payload sweep pins EINVAL.
    let pair = x(
        libc::SYS_setxattr,
        pathp,
        namep,
        valp,
        0,
        XATTR_CREATE | XATTR_REPLACE,
    );
    r.check(
        "CREATE and REPLACE together contradict: EINVAL",
        refused(pair, libc::EINVAL) || (mpos_user_tests::host_run() && pair == -1),
    );
    let fdpair = x(
        libc::SYS_fsetxattr,
        raw as usize,
        namep,
        valp,
        0,
        XATTR_CREATE | XATTR_REPLACE,
    );
    r.check(
        "the descriptor form takes the same pair the same way",
        refused(fdpair, libc::EINVAL) || (mpos_user_tests::host_run() && fdpair == -1),
    );
    r.check(
        "a flag bit Linux does not define is EINVAL",
        refused(
            x(libc::SYS_setxattr, pathp, namep, valp, 0, 0x4),
            libc::EINVAL,
        ),
    );
    // The flags win over the target, as they do in Linux: the word is
    // checked before the path is even resolved.
    r.check(
        "a bad flags word is EINVAL even on a missing path",
        refused(
            x(libc::SYS_setxattr, missingp, namep, valp, 0, 0x4),
            libc::EINVAL,
        ),
    );

    // The value is copied before the filesystem is asked, so one this kernel
    // cannot read is EFAULT even though a readable one would have been
    // EOPNOTSUPP.
    if nowhere != 0 {
        r.check(
            "a setxattr value the kernel cannot read is EFAULT",
            refused(
                x(libc::SYS_setxattr, pathp, namep, nowhere, 1, 0),
                libc::EFAULT,
            ),
        );
        // A zero size is the "no value" set and carries no pointer to read,
        // so it stays the store's answer rather than EFAULT.
        r.check(
            "a zero-length value is not read at all: EOPNOTSUPP",
            store_answer(x(libc::SYS_setxattr, pathp, namep, nowhere, 0, 0), eop),
        );
    } else {
        r.check("a setxattr value the kernel cannot read is EFAULT", false);
        r.check("a zero-length value is not read at all: EOPNOTSUPP", false);
    }

    // A missing path is resolved before any store is reached, so it is the
    // resolver's answer and not the store's.
    r.check(
        "setxattr on a missing path is ENOENT",
        refused(
            x(libc::SYS_setxattr, missingp, namep, valp, 0, 0),
            libc::ENOENT,
        ),
    );
    r.check(
        "getxattr on a missing path is ENOENT",
        refused(
            x(libc::SYS_getxattr, missingp, namep, 0, 16, 0),
            libc::ENOENT,
        ),
    );
    r.check(
        "listxattr on a missing path is ENOENT",
        refused(xl(libc::SYS_listxattr, missingp, 0, 64), libc::ENOENT),
    );
    r.check(
        "removexattr on a missing path is ENOENT",
        refused(
            x(libc::SYS_removexattr, missingp, namep, 0, 0, 0),
            libc::ENOENT,
        ),
    );

    // A descriptor that names nothing is refused before the name is read:
    // the EBADF is about the descriptor, not the attribute.
    r.check(
        "fsetxattr on a closed descriptor is EBADF",
        refused(x(libc::SYS_fsetxattr, 9999, namep, valp, 0, 0), libc::EBADF),
    );
    r.check(
        "fgetxattr on a closed descriptor is EBADF",
        refused(x(libc::SYS_fgetxattr, 9999, namep, 0, 16, 0), libc::EBADF),
    );
    r.check(
        "flistxattr on a closed descriptor is EBADF",
        refused(xl(libc::SYS_flistxattr, 9999, 0, 64), libc::EBADF),
    );
    r.check(
        "fremovexattr on a closed descriptor is EBADF",
        refused(x(libc::SYS_fremovexattr, 9999, namep, 0, 0, 0), libc::EBADF),
    );

    // A zero-length buffer is a legitimate "how big is it" query, not an
    // argument error: the store's answer stands.
    r.check(
        "getxattr with a zero-length value is EOPNOTSUPP, not EINVAL",
        store_answer(x(libc::SYS_getxattr, pathp, namep, 0, 0, 0), eop),
    );
    r.check(
        "listxattr with a zero-length list is EOPNOTSUPP, not EINVAL",
        store_answer(xl(libc::SYS_listxattr, pathp, 0, 0), eop),
    );

    if nowhere != 0 {
        let _ = unsafe { munmap(nowhere as *mut _, 4096) };
    }
    drop(fd);
    let _ = unlinkat(CWD, PATH, AtFlags::empty());
    r.finish()
}
