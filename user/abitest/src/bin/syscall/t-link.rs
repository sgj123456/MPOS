//! t-link - a second name for one inode, and a name that stands for another.
//!
//! Pinned here, per link(2)/linkat(2): a hard link is a second directory
//! entry for the *same* inode - both names report one `st_ino`, `st_nlink`
//! rises to 2 for both, a read through either answers the same bytes, and
//! unlinking one leaves the other reading exactly what it read before while
//! `st_nlink` falls back to 1. A link to a name that does not exist is
//! ENOENT, and a link onto a name already taken is EEXIST. A missing source
//! is refused before anything is created, which is why ENOENT and not EPERM
//! is the answer.
//!
//! And per symlink(2)/readlink(2): the target is *stored, not checked*, so a
//! link to a path that does not exist is made happily; `readlink` hands back
//! exactly the bytes of the target with no NUL and no termination, which is
//! the whole of the call; `newfstatat(AT_SYMLINK_NOFOLLOW)` sees S_IFLNK
//! where a following stat would see the target's kind; and a link onto a
//! name already taken is EEXIST.
//!
//! One divergence, recorded where it would be visible: `st_nlink` for a file
//! whose *last* name has been unlinked but whose descriptor is still open
//! reads 1, not Linux's 0. The count lives on the node and the trait's zero
//! means "does not track one" - see `RamFile`'s `links` field - so an
//! unlinked-but-open file is the one case the shared sentinel cannot say.

#![no_std]
#![no_main]

#[path = "../../../lib/lin.rs"]
mod lin;

use core::panic::PanicInfo;

static A: &[u8] = b"/tmp/t-link-a\0";
static B: &[u8] = b"/tmp/t-link-b\0";
static S: &[u8] = b"/tmp/t-link-s\0";
static S2: &[u8] = b"/tmp/t-link-s2\0";
static GONE: &[u8] = b"/tmp/t-link-gone\0";

/// A zeroed 144-byte Linux `struct stat`.
fn zero_stat() -> lin::Stat {
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

/// The four bytes `A` and its links hold, read through `path`.
fn read_link_bytes(failed: &mut bool, path: &[u8], name: &[u8]) -> bool {
    let mut buf = [0u8; 8];
    let fd = lin::openat(lin::AT_FDCWD, path, lin::O_RDONLY, 0) as i32;
    if fd < 0 {
        lin::check_true(failed, name, false);
        return false;
    }
    let n = lin::read(fd, &mut buf[..4]);
    let _ = lin::close(fd);
    let ok = n == 4 && buf[..4] == *b"LINK";
    lin::check_true(failed, name, ok);
    ok
}

#[unsafe(no_mangle)]
extern "C" fn mpos_user_main(_argc: usize, _argv: *const *const u8) -> ! {
    lin::puts(b"t-link\n");
    let mut failed = false;

    let _ = lin::unlinkat(lin::AT_FDCWD, A, 0);
    let _ = lin::unlinkat(lin::AT_FDCWD, B, 0);
    let _ = lin::unlinkat(lin::AT_FDCWD, S, 0);
    let _ = lin::unlinkat(lin::AT_FDCWD, S2, 0);

    let fd = lin::openat(
        lin::AT_FDCWD,
        A,
        lin::O_CREAT | lin::O_RDWR | lin::O_TRUNC,
        0o644,
    ) as i32;
    lin::check_true(&mut failed, b"the source file is created", fd >= 0);
    lin::check_eq(&mut failed, b"it holds LINK", lin::write(fd, b"LINK"), 4);
    let _ = lin::close(fd);

    // --- the hard link ------------------------------------------------------
    lin::check_eq(
        &mut failed,
        b"linkat makes a second name for the same inode",
        lin::linkat(lin::AT_FDCWD, A, lin::AT_FDCWD, B, 0),
        0,
    );
    let mut at = zero_stat();
    let mut bt = zero_stat();
    lin::check_eq(
        &mut failed,
        b"fstatat on the source succeeds",
        lin::newfstatat(lin::AT_FDCWD, A, &raw mut at, 0),
        0,
    );
    lin::check_eq(
        &mut failed,
        b"fstatat on the link succeeds",
        lin::newfstatat(lin::AT_FDCWD, B, &raw mut bt, 0),
        0,
    );
    lin::check_eq(
        &mut failed,
        b"both names report the same st_ino",
        bt.st_ino as isize,
        at.st_ino as isize,
    );
    lin::check_eq(
        &mut failed,
        b"both names report st_nlink 2",
        bt.st_nlink as isize,
        2,
    );
    lin::check_eq(
        &mut failed,
        b"and so does the source",
        at.st_nlink as isize,
        2,
    );
    read_link_bytes(
        &mut failed,
        B,
        b"the link reads the same bytes as the source",
    );

    // Dropping one name leaves the other reading what it read.
    lin::check_eq(
        &mut failed,
        b"unlinking the second name succeeds",
        lin::unlinkat(lin::AT_FDCWD, B, 0),
        0,
    );
    read_link_bytes(&mut failed, A, b"the source still reads LINK");
    lin::check_eq(
        &mut failed,
        b"fstatat on the source still succeeds",
        lin::newfstatat(lin::AT_FDCWD, A, &raw mut at, 0),
        0,
    );
    lin::check_eq(
        &mut failed,
        b"its st_nlink is back to 1",
        at.st_nlink as isize,
        1,
    );

    // The two refusals, in the order the handler makes them.
    lin::check_eq(
        &mut failed,
        b"a link from a missing source answers ENOENT",
        lin::linkat(lin::AT_FDCWD, GONE, lin::AT_FDCWD, B, 0),
        lin::ENOENT,
    );
    lin::check_eq(
        &mut failed,
        b"the second name is made again",
        lin::linkat(lin::AT_FDCWD, A, lin::AT_FDCWD, B, 0),
        0,
    );
    lin::check_eq(
        &mut failed,
        b"a link onto a name already taken answers EEXIST",
        lin::linkat(lin::AT_FDCWD, A, lin::AT_FDCWD, B, 0),
        lin::EEXIST,
    );

    // --- the symbolic link --------------------------------------------------
    lin::check_eq(
        &mut failed,
        b"symlink stores a target that need not exist",
        lin::symlink(b"nowhere\0", S),
        0,
    );
    let mut target = [0u8; 16];
    lin::check_eq(
        &mut failed,
        b"readlink answers the target's length",
        lin::readlink(S, &mut target),
        7,
    );
    lin::check_true(
        &mut failed,
        b"readlink answers exactly those bytes, with no NUL",
        target[..7] == *b"nowhere",
    );
    let mut st = zero_stat();
    lin::check_eq(
        &mut failed,
        b"fstatat with AT_SYMLINK_NOFOLLOW succeeds",
        lin::newfstatat(lin::AT_FDCWD, S, &raw mut st, lin::AT_SYMLINK_NOFOLLOW),
        0,
    );
    lin::check_eq(
        &mut failed,
        b"the name without following is a link",
        (st.st_mode & lin::S_IFMT) as isize,
        lin::S_IFLNK as isize,
    );

    lin::check_eq(
        &mut failed,
        b"symlinkat makes a link at the dirfd's own spelling",
        lin::symlinkat(b"elsewhere\0", lin::AT_FDCWD, S2),
        0,
    );
    let mut target2 = [0u8; 16];
    lin::check_eq(
        &mut failed,
        b"readlinkat answers the target's length",
        lin::readlinkat(lin::AT_FDCWD, S2, &mut target2),
        9,
    );
    lin::check_true(
        &mut failed,
        b"readlinkat answers exactly those bytes",
        target2[..9] == *b"elsewhere",
    );

    lin::check_eq(
        &mut failed,
        b"a symlink onto a name already taken answers EEXIST",
        lin::symlink(b"another\0", S),
        lin::EEXIST,
    );

    let _ = lin::unlinkat(lin::AT_FDCWD, A, 0);
    let _ = lin::unlinkat(lin::AT_FDCWD, B, 0);
    let _ = lin::unlinkat(lin::AT_FDCWD, S, 0);
    let _ = lin::unlinkat(lin::AT_FDCWD, S2, 0);
    lin::done(failed)
}

#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    lin::die(b"t-link: panicked\n")
}
