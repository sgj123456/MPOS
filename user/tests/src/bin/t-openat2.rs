//! t-openat2 - the extensible open, and the size word that versions it.
//!
//! Pinned here, per openat2(2): `struct open_how` is 24 bytes and the size
//! word is the version handshake - smaller is EINVAL ("before this struct
//! existed"), larger is E2BIG ("fields this kernel has never heard of") - an
//! unknown resolve bit is EINVAL while a known one the resolver cannot keep is
//! EOPNOTSUPP, a mode word outside a create is EINVAL, a missing path without
//! O_CREAT is ENOENT, and the flags are the newer open vocabulary O_CLOEXEC
//! belongs to.
//!
//! mpos keeps exactly one resolve bit: `RESOLVE_CACHED`. Nothing here caches,
//! which makes every walk a cached one, so it is accepted and the open
//! proceeds. Every other documented bit - NO_XDEV, NO_MAGICLINKS, NO_SYMLINKS,
//! BENEATH, IN_ROOT - is a promise about a walk this resolver does not perform,
//! and is refused EOPNOTSUPP rather than pretended; that is a deliberate
//! narrowing from Linux, which can keep them. The create mode is masked by the
//! process umask exactly as `openat`'s is, so 0o600 under the default 0o022
//! umask lands as 0o600.
//!
//! # Why the whole program is `libc::syscall`
//!
//! The size word *is* this call's subject, and `rustix::fs::openat2` cannot
//! express it: the wrapper writes `size_of::<open_how>()` itself, so 23 and 25
//! are unreachable through it. That alone is the `libc` rule's clause (c) - a
//! wrapper that binds the right syscall but whose *shape* cannot express the
//! claim (`user/tests/Cargo.toml`) - and the program is `libc::syscall` with the
//! number from `libc::SYS_openat2` and a local `#[repr(C)]` `OpenHow`, the
//! resolve-bit claims riding along rather than forcing the choice.
//!
//! The resolve bit is *not* the reason, and it is worth saying so because the
//! obvious version of it is wrong: `ResolveFlags` is a bitflags type, and
//! rustix's flag types carry bitflags' unnamed flag (`const _ = !0`), so
//! `ResolveFlags::from_bits_retain` holds any word at all. An unnamed bit is
//! expressible through `rustix`; the size word is what is not.
//!
//! What still comes from `rustix` is every number the flag word and the mode
//! are spelled with (`OFlags`, `Mode`, `ResolveFlags`), so the words this
//! program hands the syscall come from a maintained per-machine table rather
//! than from this repository's memory of them. The one exception is the unknown
//! bit, written down on purpose: the claim is precisely that a bit nothing
//! names is refused.
//!
//! # What this pins on each machine
//!
//! `openat2` was added after the two machines' syscall tables had diverged, so
//! unlike the older calls it carries the *same* number on both
//! (`nr_aarch64.rs` and `nr_x86_64.rs` both say `OPENAT2 = 437`) and both reach
//! the same `handle_openat2` - the per-claim output is the same on both
//! machines.
//!
//! The readable file is one this test writes itself under `/tmp`, in place of
//! the no-std version's `/etc/motd` - the fixture is the test's own rather than
//! one the image happens to carry. `/tmp` is the directory both runners mount a
//! tmpfs on before they run any program, so the two `/tmp/...` names the
//! earlier claims need are the test's to create and remove.

use mpos_user_tests::Report;
use rustix::fd::OwnedFd;
use rustix::fs::{Mode, OFlags, ResolveFlags, fstat};
use rustix::io::{Errno, FdFlags, fcntl_getfd, read};
use std::ffi::{CStr, CString};
use std::io;
use std::os::fd::FromRawFd;

/// The `struct open_how` of `openat2(2)`: the flags, the create mode and the
/// path-resolution the caller wants, each a `__u64`. 24 bytes, which is what
/// the size word's whole handshake is about.
#[repr(C)]
struct OpenHow {
    flags: u64,
    mode: u64,
    resolve: u64,
}

impl OpenHow {
    const fn new(flags: u64, mode: u64, resolve: u64) -> Self {
        OpenHow {
            flags,
            mode,
            resolve,
        }
    }
}

/// The 24-byte struct plus the bytes a `size = 25` caller sends past it.
/// Linux's `copy_struct_from_user` reads that tail: all-zero is
/// forward-compatible, a nonzero byte is a field the kernel never heard of.
#[repr(C)]
struct PaddedHow {
    how: OpenHow,
    tail: [u8; 8],
}

/// `sizeof(struct open_how)`, the size word's one valid value.
const SIZEOF_OPEN_HOW: usize = 24;

/// The file this test owns, in place of the no-std version's `/etc/motd`.
const FILE: &str = "/tmp/t-openat2_file";
/// The two names the create claims make and remove.
const SCRATCH: &str = "/tmp/t-openat2";
const MISSING: &str = "/tmp/t-openat2-missing";

/// A resolve bit outside the documented six. Written down because it is
/// deliberately outside every table - the claim is that a bit nothing names is
/// refused.
const NOT_A_RESOLVE_BIT: u64 = 0x40;

/// `openat2(dirfd, path, how, size)` through `libc::syscall`, with the dirfd
/// this program does not vary. See the module comment for why the raw call
/// rather than `rustix::fs::openat2`.
fn openat2(path: &CStr, how: &OpenHow, size: usize) -> Result<OwnedFd, Errno> {
    openat2_ptr(path, how as *const OpenHow, size)
}

fn openat2_ptr(path: &CStr, how: *const OpenHow, size: usize) -> Result<OwnedFd, Errno> {
    // `syscall` is a libc entry point, so libc's own errno protocol is what
    // applies - read immediately, before anything else can set it.
    let r = unsafe { libc::syscall(libc::SYS_openat2, libc::AT_FDCWD, path.as_ptr(), how, size) };
    if r < 0 {
        return Err(Errno::from_raw_os_error(
            io::Error::last_os_error().raw_os_error().unwrap_or(0),
        ));
    }
    // The number is a descriptor this process owns, so it becomes an `OwnedFd`
    // and every later claim is made through `rustix` on it - including the
    // close, which is implicit in the drop.
    Ok(unsafe { OwnedFd::from_raw_fd(r as i32) })
}

fn main() {
    println!("t-openat2");
    let mut r = Report::new();

    let _ = std::fs::remove_file(FILE);
    if std::fs::write(FILE, b"mpos t-openat2 fixture\n").is_err() {
        r.check("a size of 24 opens the file", false);
        r.finish()
    }
    let file = CString::new(FILE).expect("the path has no interior NUL");
    let scratch = CString::new(SCRATCH).expect("the path has no interior NUL");
    let missing = CString::new(MISSING).expect("the path has no interior NUL");

    // The flag words and the mode, each from the table that names it.
    let (ronly, rdwr) = (OFlags::RDONLY.bits() as u64, OFlags::RDWR.bits() as u64);
    let (create, cloexec) = (OFlags::CREATE.bits() as u64, OFlags::CLOEXEC.bits() as u64);
    let mode600 = Mode::from(0o600).bits() as u64;

    // The size word: sizeof(struct open_how) is 24. Below it is EINVAL; above
    // is Linux's forward-compatible `copy_struct_from_user` - the extra bytes
    // must be zero, and a nonzero one is E2BIG. Measured on Linux 7.2.11:
    // size 25 with byte 24 zero opens the file, byte 24 = 1 is E2BIG.
    r.check(
        "a size below struct open_how answers EINVAL",
        matches!(
            openat2(&file, &OpenHow::new(ronly, 0, 0), SIZEOF_OPEN_HOW - 1),
            Err(Errno::INVAL)
        ),
    );
    let mut padded = PaddedHow {
        how: OpenHow::new(ronly, 0, 0),
        tail: [0; 8],
    };
    r.check(
        "a size above struct open_how with a zero tail opens",
        openat2_ptr(&file, &padded.how as *const OpenHow, SIZEOF_OPEN_HOW + 1).is_ok(),
    );
    padded.tail[0] = 1;
    r.check(
        "a nonzero byte past struct open_how answers E2BIG",
        matches!(
            openat2_ptr(&file, &padded.how as *const OpenHow, SIZEOF_OPEN_HOW + 1),
            Err(Errno::TOOBIG)
        ),
    );
    let fd = openat2(&file, &OpenHow::new(ronly, 0, 0), SIZEOF_OPEN_HOW);
    r.check("a size of 24 opens the file", fd.is_ok());
    if let Ok(fd) = &fd {
        let mut buf = [0u8; 32];
        r.check(
            "the size-24 descriptor reads bytes",
            matches!(read(fd, &mut buf), Ok(n) if n > 0),
        );
    }
    drop(fd);

    // The resolve vocabulary: an unknown bit is EINVAL, a known bit mpos cannot
    // keep is EOPNOTSUPP, and RESOLVE_CACHED is the one it can.
    r.check(
        "an unknown resolve bit answers EINVAL",
        matches!(
            openat2(
                &file,
                &OpenHow::new(ronly, 0, NOT_A_RESOLVE_BIT),
                SIZEOF_OPEN_HOW
            ),
            Err(Errno::INVAL)
        ),
    );
    let no_xdev = openat2(
        &file,
        &OpenHow::new(ronly, 0, ResolveFlags::NO_XDEV.bits()),
        SIZEOF_OPEN_HOW,
    );
    r.check(
        "a resolve bit mpos cannot keep answers EOPNOTSUPP",
        matches!(no_xdev, Err(Errno::OPNOTSUPP))
            || (mpos_user_tests::host_run()
                && (no_xdev.is_ok() || matches!(no_xdev, Err(Errno::XDEV)))),
    );
    let fd = openat2(
        &file,
        &OpenHow::new(ronly, 0, ResolveFlags::CACHED.bits()),
        SIZEOF_OPEN_HOW,
    );
    r.check(
        "RESOLVE_CACHED, the one bit mpos keeps, opens the file",
        fd.is_ok(),
    );
    if let Ok(fd) = &fd {
        let mut buf = [0u8; 32];
        r.check(
            "the RESOLVE_CACHED descriptor reads bytes",
            matches!(read(fd, &mut buf), Ok(n) if n > 0),
        );
    }
    drop(fd);

    // The mode is a create's business: with no O_CREAT and no O_TMPFILE a
    // non-zero mode is EINVAL.
    r.check(
        "a non-zero mode without a create answers EINVAL",
        matches!(
            openat2(&file, &OpenHow::new(rdwr, mode600, 0), SIZEOF_OPEN_HOW),
            Err(Errno::INVAL)
        ),
    );

    // With O_CREAT the mode is stored, masked by the umask (default 0o022,
    // which leaves 0o600 untouched).
    let _ = std::fs::remove_file(SCRATCH);
    let fd = openat2(
        &scratch,
        &OpenHow::new(create | rdwr, mode600, 0),
        SIZEOF_OPEN_HOW,
    );
    r.check("a create with a mode succeeds", fd.is_ok());
    if let Ok(fd) = &fd {
        match fstat(fd) {
            Ok(st) => {
                r.check("fstat on the created file succeeds", true);
                r.check(
                    "the created file carries the mode asked for, masked by the umask",
                    st.st_mode & 0o777 == 0o600,
                );
            }
            Err(_) => {
                r.check("fstat on the created file succeeds", false);
                r.check(
                    "the created file carries the mode asked for, masked by the umask",
                    false,
                );
            }
        }
    }
    drop(fd);
    let _ = std::fs::remove_file(SCRATCH);

    // A missing path: ENOENT without O_CREAT, a fresh descriptor with it.
    let _ = std::fs::remove_file(MISSING);
    r.check(
        "a missing path without a create answers ENOENT",
        matches!(
            openat2(&missing, &OpenHow::new(ronly, 0, 0), SIZEOF_OPEN_HOW),
            Err(Errno::NOENT)
        ),
    );
    let fd = openat2(
        &missing,
        &OpenHow::new(create | rdwr, 0, 0),
        SIZEOF_OPEN_HOW,
    );
    r.check("the same missing path with a create opens", fd.is_ok());
    drop(fd);
    let _ = std::fs::remove_file(MISSING);

    // The newer open vocabulary: O_CLOEXEC is a flag word openat2 shares with
    // openat, and it must reach the descriptor it made.
    let fd = openat2(&file, &OpenHow::new(ronly | cloexec, 0, 0), SIZEOF_OPEN_HOW);
    r.check("O_RDONLY|O_CLOEXEC opens the file", fd.is_ok());
    if let Ok(fd) = &fd {
        r.check(
            "O_CLOEXEC reached the descriptor openat2 made",
            matches!(fcntl_getfd(fd), Ok(f) if f == FdFlags::CLOEXEC),
        );
    }
    drop(fd);

    let _ = std::fs::remove_file(FILE);
    r.finish()
}
