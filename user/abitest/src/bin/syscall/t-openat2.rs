//! t-openat2 - the extensible open, and the size word that versions it.
//!
//! Pinned here, per openat2(2): `struct open_how` is 24 bytes and the size
//! word is the version handshake - smaller is EINVAL ("before this struct
//! existed"), larger is E2BIG ("fields this kernel has never heard of") -
//! an unknown resolve bit is EINVAL while a known one the resolver cannot
//! keep is EOPNOTSUPP, a mode word outside a create is EINVAL, a missing
//! path without O_CREAT is ENOENT, and the flags are the newer open
//! vocabulary O_CLOEXEC belongs to.
//!
//! mpos keeps exactly one resolve bit: `RESOLVE_CACHED`. Nothing here
//! caches, which makes every walk a cached one, so it is accepted and the
//! open proceeds. Every other documented bit - NO_XDEV, NO_MAGICLINKS,
//! NO_SYMLINKS, BENEATH, IN_ROOT - is a promise about a walk this resolver
//! does not perform, and is refused EOPNOTSUPP rather than pretended; that
//! is a deliberate narrowing from Linux, which can keep them. The create
//! mode is masked by the process umask exactly as `openat`'s is, so 0o600
//! under the default 0o022 umask lands as 0o600.

#![no_std]
#![no_main]

#[path = "../../../lib/lin.rs"]
mod lin;

use core::panic::PanicInfo;

static MOTD: &[u8] = b"/etc/motd\0";
static SCRATCH: &[u8] = b"/tmp/t-openat2\0";
static MISSING: &[u8] = b"/tmp/t-openat2-missing\0";

static mut BUF: [u8; 32] = [0; 32];

fn empty_stat() -> lin::Stat {
    lin::Stat {
        st_dev: 0,
        st_ino: 0,
        st_nlink: 0,
        st_mode: 0,
        st_uid: 0,
        st_gid: 0,
        _pad: 0,
        st_rdev: 0,
        st_size: 0,
        st_blksize: 0,
        st_blocks: 0,
        st_atime: 0,
        st_atime_nsec: 0,
        st_mtime: 0,
        st_mtime_nsec: 0,
        st_ctime: 0,
        st_ctime_nsec: 0,
        _unused: [0; 3],
    }
}

#[unsafe(no_mangle)]
extern "C" fn mpos_user_main(_argc: usize, _argv: *const *const u8) -> ! {
    lin::puts(b"t-openat2\n");
    let mut failed = false;

    // The size word: sizeof(struct open_how) is 24, and the two sides of it
    // are the two refusals the man page names.
    lin::check_eq(
        &mut failed,
        b"a size below struct open_how answers EINVAL",
        lin::openat2(
            lin::AT_FDCWD,
            MOTD,
            &lin::OpenHow::new(lin::O_RDONLY, 0, 0),
            23,
        ),
        lin::EINVAL,
    );
    lin::check_eq(
        &mut failed,
        b"a size above struct open_how answers E2BIG",
        lin::openat2(
            lin::AT_FDCWD,
            MOTD,
            &lin::OpenHow::new(lin::O_RDONLY, 0, 0),
            25,
        ),
        lin::E2BIG,
    );
    let fd = lin::openat2(
        lin::AT_FDCWD,
        MOTD,
        &lin::OpenHow::new(lin::O_RDONLY, 0, 0),
        24,
    );
    lin::check_true(&mut failed, b"a size of 24 opens /etc/motd", fd >= 0);
    if fd >= 0 {
        let buf = unsafe { core::slice::from_raw_parts_mut(&raw mut BUF as *mut u8, 32) };
        let n = lin::read(fd as i32, buf);
        lin::check_true(&mut failed, b"the size-24 descriptor reads bytes", n > 0);
        let _ = lin::close(fd as i32);
    }

    // The resolve vocabulary: an unknown bit is EINVAL, a known bit mpos
    // cannot keep is EOPNOTSUPP, and RESOLVE_CACHED is the one it can.
    lin::check_eq(
        &mut failed,
        b"an unknown resolve bit answers EINVAL",
        lin::openat2(
            lin::AT_FDCWD,
            MOTD,
            &lin::OpenHow::new(lin::O_RDONLY, 0, 0x40),
            24,
        ),
        lin::EINVAL,
    );
    lin::check_eq(
        &mut failed,
        b"a resolve bit mpos cannot keep answers EOPNOTSUPP",
        lin::openat2(
            lin::AT_FDCWD,
            MOTD,
            &lin::OpenHow::new(lin::O_RDONLY, 0, lin::RESOLVE_NO_XDEV),
            24,
        ),
        lin::EOPNOTSUPP,
    );
    let fd = lin::openat2(
        lin::AT_FDCWD,
        MOTD,
        &lin::OpenHow::new(lin::O_RDONLY, 0, lin::RESOLVE_CACHED),
        24,
    );
    lin::check_true(
        &mut failed,
        b"RESOLVE_CACHED, the one bit mpos keeps, opens /etc/motd",
        fd >= 0,
    );
    if fd >= 0 {
        let buf = unsafe { core::slice::from_raw_parts_mut(&raw mut BUF as *mut u8, 32) };
        let n = lin::read(fd as i32, buf);
        lin::check_true(
            &mut failed,
            b"the RESOLVE_CACHED descriptor reads bytes",
            n > 0,
        );
        let _ = lin::close(fd as i32);
    }

    // The mode is a create's business: with no O_CREAT and no O_TMPFILE a
    // non-zero mode is EINVAL.
    lin::check_eq(
        &mut failed,
        b"a non-zero mode without a create answers EINVAL",
        lin::openat2(
            lin::AT_FDCWD,
            MOTD,
            &lin::OpenHow::new(lin::O_RDWR, 0o600, 0),
            24,
        ),
        lin::EINVAL,
    );

    // With O_CREAT the mode is stored, masked by the umask (default 0o022,
    // which leaves 0o600 untouched).
    let _ = lin::unlinkat(lin::AT_FDCWD, SCRATCH, 0);
    let fd = lin::openat2(
        lin::AT_FDCWD,
        SCRATCH,
        &lin::OpenHow::new(lin::O_CREAT | lin::O_RDWR, 0o600, 0),
        24,
    ) as i32;
    lin::check_true(&mut failed, b"a create with a mode succeeds", fd >= 0);
    if fd >= 0 {
        let mut st = empty_stat();
        lin::check_eq(
            &mut failed,
            b"fstat on the created file succeeds",
            lin::fstat(fd, &raw mut st),
            0,
        );
        lin::check_eq(
            &mut failed,
            b"the created file carries the mode asked for, masked by the umask",
            (st.st_mode & 0o777) as isize,
            0o600,
        );
        let _ = lin::close(fd);
    }
    let _ = lin::unlinkat(lin::AT_FDCWD, SCRATCH, 0);

    // A missing path: ENOENT without O_CREAT, a fresh descriptor with it.
    let _ = lin::unlinkat(lin::AT_FDCWD, MISSING, 0);
    lin::check_eq(
        &mut failed,
        b"a missing path without a create answers ENOENT",
        lin::openat2(
            lin::AT_FDCWD,
            MISSING,
            &lin::OpenHow::new(lin::O_RDONLY, 0, 0),
            24,
        ),
        lin::ENOENT,
    );
    let fd = lin::openat2(
        lin::AT_FDCWD,
        MISSING,
        &lin::OpenHow::new(lin::O_CREAT | lin::O_RDWR, 0, 0),
        24,
    ) as i32;
    lin::check_true(
        &mut failed,
        b"the same missing path with a create opens",
        fd >= 0,
    );
    if fd >= 0 {
        let _ = lin::close(fd);
    }
    let _ = lin::unlinkat(lin::AT_FDCWD, MISSING, 0);

    // The newer open vocabulary: O_CLOEXEC is a flag word openat2 shares
    // with openat, and it must reach the descriptor it made.
    let fd = lin::openat2(
        lin::AT_FDCWD,
        MOTD,
        &lin::OpenHow::new(lin::O_RDONLY | lin::O_CLOEXEC, 0, 0),
        24,
    ) as i32;
    lin::check_true(&mut failed, b"O_RDONLY|O_CLOEXEC opens /etc/motd", fd >= 0);
    if fd >= 0 {
        lin::check_true(
            &mut failed,
            b"O_CLOEXEC reached the descriptor openat2 made",
            lin::fcntl(fd, lin::F_GETFD, 0) == lin::FD_CLOEXEC as isize,
        );
        let _ = lin::close(fd);
    }

    lin::done(failed)
}

#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    lin::die(b"t-openat2: panicked\n")
}
