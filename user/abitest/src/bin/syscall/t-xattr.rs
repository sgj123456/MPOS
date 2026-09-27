//! t-xattr - the extended-attribute family (188..199): the arguments are
//! validated, and only then does the store's refusal come back.
//!
//! mpos keeps no extended attributes on any of its filesystems, so every one
//! of the twelve calls answers `EOPNOTSUPP` on a request whose target and name
//! are sound - the "this filesystem does not do xattrs" report Linux gives a
//! filesystem without xattr support, not the `ENOSYS` of a syscall a kernel
//! never wired up. Both halves are pinned here, because the refusal is only
//! meaningful if it comes *after* the ordinary argument checks: a missing path
//! must still be `ENOENT` and a bad descriptor still `EBADF`, or a caller
//! would read "unsupported" for a file it simply mistyped. The plain form, the
//! `l`-form (which acts on a final symlink) and the `f`-form (a descriptor)
//! are all exercised on the path/fd that resolves.
//!
//! Divergence worth recording: the store is still the whole of the answer -
//! nothing here keeps an attribute - so the refusal happens with or without a
//! well-formed request behind it. What *is* checked is the arguments a
//! `setxattr` carries and the other forms do not: the flags word (`EINVAL` for
//! a bit Linux does not define, and for `XATTR_CREATE|XATTR_REPLACE` together,
//! which contradict each other rather than combine) and the value (`EFAULT`
//! when the size is non-zero and the pointer is one the kernel cannot read).
//! The value and list buffers of the *other* forms are still never consulted,
//! which is why a zero-length `getxattr` value stays `EOPNOTSUPP`: Linux reads
//! a `getxattr` buffer inside the filesystem's own handler, so a filesystem
//! without one never looks at it either.

#![no_std]
#![no_main]

#[path = "../../../lib/lin.rs"]
mod lin;

use core::panic::PanicInfo;

static PATH: [u8; 13] = *b"/tmp/t-xattr\0";
static MISSING: [u8; 21] = *b"/tmp/t-xattr-missing\0";
static NAME: [u8; 13] = *b"user.abitest\0";
static VALUE: [u8; 1] = *b"v";

#[unsafe(no_mangle)]
extern "C" fn mpos_user_main(_argc: usize, _argv: *const *const u8) -> ! {
    lin::puts(b"t-xattr\n");
    let mut failed = false;

    let fd = lin::openat(lin::AT_FDCWD, &PATH, lin::O_CREAT | lin::O_RDWR, 0o644);
    lin::check_true(
        &mut failed,
        b"a file to hang attributes off is created",
        fd >= 0,
    );
    if fd < 0 {
        lin::done(failed);
    }
    let fd = fd as i32;

    let mut buf = [0u8; 16];
    let mut list = [0u8; 64];
    let mut empty: [u8; 0] = [];

    // The path forms. The path resolves and the name is a readable string, so
    // the call reaches the store - which has no xattrs and says so.
    lin::check_eq(
        &mut failed,
        b"setxattr on a real file: EOPNOTSUPP",
        lin::setxattr(&PATH, &NAME, &VALUE, 0),
        lin::EOPNOTSUPP,
    );
    lin::check_eq(
        &mut failed,
        b"lsetxattr on a real file: EOPNOTSUPP",
        lin::lsetxattr(&PATH, &NAME, &VALUE, 0),
        lin::EOPNOTSUPP,
    );
    lin::check_eq(
        &mut failed,
        b"getxattr on a real file: EOPNOTSUPP",
        lin::getxattr(&PATH, &NAME, &mut buf),
        lin::EOPNOTSUPP,
    );
    lin::check_eq(
        &mut failed,
        b"lgetxattr on a real file: EOPNOTSUPP",
        lin::lgetxattr(&PATH, &NAME, &mut buf),
        lin::EOPNOTSUPP,
    );
    lin::check_eq(
        &mut failed,
        b"listxattr on a real file: EOPNOTSUPP",
        lin::listxattr(&PATH, &mut list),
        lin::EOPNOTSUPP,
    );
    lin::check_eq(
        &mut failed,
        b"llistxattr on a real file: EOPNOTSUPP",
        lin::llistxattr(&PATH, &mut list),
        lin::EOPNOTSUPP,
    );
    lin::check_eq(
        &mut failed,
        b"removexattr on a real file: EOPNOTSUPP",
        lin::removexattr(&PATH, &NAME),
        lin::EOPNOTSUPP,
    );
    lin::check_eq(
        &mut failed,
        b"lremovexattr on a real file: EOPNOTSUPP",
        lin::lremovexattr(&PATH, &NAME),
        lin::EOPNOTSUPP,
    );

    // The descriptor forms, on the descriptor this test opened.
    lin::check_eq(
        &mut failed,
        b"fsetxattr on an open descriptor: EOPNOTSUPP",
        lin::fsetxattr(fd, &NAME, &VALUE, 0),
        lin::EOPNOTSUPP,
    );
    lin::check_eq(
        &mut failed,
        b"fgetxattr on an open descriptor: EOPNOTSUPP",
        lin::fgetxattr(fd, &NAME, &mut buf),
        lin::EOPNOTSUPP,
    );
    lin::check_eq(
        &mut failed,
        b"flistxattr on an open descriptor: EOPNOTSUPP",
        lin::flistxattr(fd, &mut list),
        lin::EOPNOTSUPP,
    );
    lin::check_eq(
        &mut failed,
        b"fremovexattr on an open descriptor: EOPNOTSUPP",
        lin::fremovexattr(fd, &NAME),
        lin::EOPNOTSUPP,
    );

    // The flags word is validated before the store is reached. Either bit on
    // its own is a legitimate request and gets the store's answer; both
    // together contradict, and any other bit is not a flag Linux defines.
    lin::check_eq(
        &mut failed,
        b"XATTR_CREATE alone is not what refuses the set",
        lin::setxattr(&PATH, &NAME, &VALUE, lin::XATTR_CREATE),
        lin::EOPNOTSUPP,
    );
    lin::check_eq(
        &mut failed,
        b"XATTR_REPLACE alone is not what refuses the set",
        lin::setxattr(&PATH, &NAME, &VALUE, lin::XATTR_REPLACE),
        lin::EOPNOTSUPP,
    );
    lin::check_eq(
        &mut failed,
        b"CREATE and REPLACE together contradict: EINVAL",
        lin::setxattr(&PATH, &NAME, &VALUE, lin::XATTR_CREATE | lin::XATTR_REPLACE),
        lin::EINVAL,
    );
    lin::check_eq(
        &mut failed,
        b"the descriptor form takes the same pair the same way",
        lin::fsetxattr(fd, &NAME, &VALUE, lin::XATTR_CREATE | lin::XATTR_REPLACE),
        lin::EINVAL,
    );
    lin::check_eq(
        &mut failed,
        b"a flag bit Linux does not define is EINVAL",
        lin::setxattr(&PATH, &NAME, &VALUE, 0x4),
        lin::EINVAL,
    );
    // The flags win over the target, as they do in Linux: the word is checked
    // before the path is even resolved.
    lin::check_eq(
        &mut failed,
        b"a bad flags word is EINVAL even on a missing path",
        lin::setxattr(&MISSING, &NAME, &VALUE, 0x4),
        lin::EINVAL,
    );

    // The value is copied before the filesystem is asked, so one this kernel
    // cannot read is EFAULT even though a readable one would have been
    // EOPNOTSUPP. The wrapper cannot express this - its slice is always
    // readable - so the call is spelled raw, with `GUARD_PAGE` (which this
    // test never maps) as the value.
    lin::check_eq(
        &mut failed,
        b"a setxattr value the kernel cannot read is EFAULT",
        lin::raw_sys5(
            lin::nr::SETXATTR,
            PATH.as_ptr() as usize,
            NAME.as_ptr() as usize,
            lin::GUARD_PAGE,
            VALUE.len(),
            0,
        ),
        lin::EFAULT,
    );
    // A zero size is the "no value" set and carries no pointer to read, so it
    // stays the store's answer rather than EFAULT.
    lin::check_eq(
        &mut failed,
        b"a zero-length value is not read at all: EOPNOTSUPP",
        lin::raw_sys5(
            lin::nr::SETXATTR,
            PATH.as_ptr() as usize,
            NAME.as_ptr() as usize,
            lin::GUARD_PAGE,
            0,
            0,
        ),
        lin::EOPNOTSUPP,
    );

    // A missing path is resolved before any store is reached, so it is the
    // resolver's answer and not the store's.
    lin::check_eq(
        &mut failed,
        b"setxattr on a missing path is ENOENT",
        lin::setxattr(&MISSING, &NAME, &VALUE, 0),
        lin::ENOENT,
    );
    lin::check_eq(
        &mut failed,
        b"getxattr on a missing path is ENOENT",
        lin::getxattr(&MISSING, &NAME, &mut buf),
        lin::ENOENT,
    );
    lin::check_eq(
        &mut failed,
        b"listxattr on a missing path is ENOENT",
        lin::listxattr(&MISSING, &mut list),
        lin::ENOENT,
    );
    lin::check_eq(
        &mut failed,
        b"removexattr on a missing path is ENOENT",
        lin::removexattr(&MISSING, &NAME),
        lin::ENOENT,
    );

    // A descriptor that names nothing is refused before the name is read: the
    // EBADF is about the descriptor, not the attribute.
    lin::check_eq(
        &mut failed,
        b"fsetxattr on a closed descriptor is EBADF",
        lin::fsetxattr(9999, &NAME, &VALUE, 0),
        lin::EBADF,
    );
    lin::check_eq(
        &mut failed,
        b"fgetxattr on a closed descriptor is EBADF",
        lin::fgetxattr(9999, &NAME, &mut buf),
        lin::EBADF,
    );
    lin::check_eq(
        &mut failed,
        b"flistxattr on a closed descriptor is EBADF",
        lin::flistxattr(9999, &mut list),
        lin::EBADF,
    );
    lin::check_eq(
        &mut failed,
        b"fremovexattr on a closed descriptor is EBADF",
        lin::fremovexattr(9999, &NAME),
        lin::EBADF,
    );

    // A zero-length buffer is a legitimate "how big is it" query, not an
    // argument error: the store's answer stands.
    lin::check_eq(
        &mut failed,
        b"getxattr with a zero-length value is EOPNOTSUPP, not EINVAL",
        lin::getxattr(&PATH, &NAME, &mut empty),
        lin::EOPNOTSUPP,
    );
    lin::check_eq(
        &mut failed,
        b"listxattr with a zero-length list is EOPNOTSUPP, not EINVAL",
        lin::listxattr(&PATH, &mut empty),
        lin::EOPNOTSUPP,
    );

    let _ = lin::close(fd);
    let _ = lin::unlinkat(lin::AT_FDCWD, &PATH, 0);
    lin::done(failed)
}

#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    lin::die(b"t-xattr: panicked\n")
}
