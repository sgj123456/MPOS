//! t-truncate - the resize, and the trap a descriptor's cursor sets for it.
//!
//! Pinned here, per truncate(2)/ftruncate(2): the resize is real in both
//! directions - a shorter file keeps no tail, and reading it back answers
//! only what is left; a negative length is EINVAL before anything is
//! touched; a path that does not exist is ENOENT; and the trap the man page
//! is built around - after reading to end-of-file, `ftruncate` to zero
//! leaves the *descriptor's* offset where it was, so the next write lands
//! past the new end and the file grows back with a hole of zeros in front of
//! it. Linux behaves exactly this way, and so does mpos: the hole reads as
//! zeros and the size is the write's end, not the write's length.
//!
//! One mpos divergence, named where it is checked: `ftruncate` on a pipe is
//! EOPNOTSUPP here rather than Linux's EINVAL. mpos builds a pipe out of a
//! `File` inode, so the handler's "not a regular file" test does not catch
//! it, and the refusal comes from the VFS's default `truncate` instead.

#![no_std]
#![no_main]

#[path = "../../../lib/lin.rs"]
mod lin;

use core::panic::PanicInfo;

static PATH: &[u8] = b"/tmp/t-truncate\0";
static MISSING: &[u8] = b"/tmp/t-truncate-gone\0";

/// A zeroed 144-byte Linux `struct stat`, so every check reads one field of
/// a known-clean buffer rather than of whatever the last fill left.
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

#[unsafe(no_mangle)]
extern "C" fn mpos_user_main(_argc: usize, _argv: *const *const u8) -> ! {
    lin::puts(b"t-truncate\n");
    let mut failed = false;

    let fd = lin::openat(
        lin::AT_FDCWD,
        PATH,
        lin::O_CREAT | lin::O_RDWR | lin::O_TRUNC,
        0o644,
    ) as i32;
    lin::check_true(&mut failed, b"the file is created", fd >= 0);

    lin::check_eq(
        &mut failed,
        b"ten bytes are written",
        lin::write(fd, b"0123456789"),
        10,
    );
    let mut st = zero_stat();
    lin::check_eq(
        &mut failed,
        b"fstat succeeds",
        lin::fstat(fd, &raw mut st),
        0,
    );
    lin::check_eq(
        &mut failed,
        b"the file is ten bytes long",
        st.st_size as isize,
        10,
    );

    // A shorter file keeps no tail.
    lin::check_eq(
        &mut failed,
        b"ftruncate to 4 succeeds",
        lin::ftruncate(fd, 4),
        0,
    );
    lin::check_eq(
        &mut failed,
        b"fstat succeeds",
        lin::fstat(fd, &raw mut st),
        0,
    );
    lin::check_eq(
        &mut failed,
        b"the shrunk file is four bytes long",
        st.st_size as isize,
        4,
    );
    lin::check_eq(
        &mut failed,
        b"the offset is rewound",
        lin::lseek(fd, 0, lin::SEEK_SET),
        0,
    );
    let mut buf = [0u8; 16];
    lin::check_eq(
        &mut failed,
        b"a read answers the four bytes that are left",
        lin::read(fd, &mut buf[..4]),
        4,
    );
    lin::check_true(
        &mut failed,
        b"and they are the front of the old contents",
        buf[..4] == *b"0123",
    );

    // The length's own validation, before the descriptor is looked at.
    lin::check_eq(
        &mut failed,
        b"a negative length answers EINVAL",
        lin::ftruncate(fd, usize::MAX),
        lin::EINVAL,
    );

    // A descriptor that is not a regular file. The named divergence.
    let mut pipes: [i32; 2] = [-1; 2];
    lin::check_eq(
        &mut failed,
        b"the pipe pair is made",
        lin::pipe2(&mut pipes, 0),
        0,
    );
    lin::check_eq(
        &mut failed,
        b"mpos divergence: a pipe is a File inode here, so ftruncate answers EOPNOTSUPP, not Linux's EINVAL",
        lin::ftruncate(pipes[0], 4),
        lin::EOPNOTSUPP,
    );
    let _ = lin::close(pipes[0]);
    let _ = lin::close(pipes[1]);

    // The path spelling: a resize to zero, and the empty file reads as empty.
    lin::check_eq(
        &mut failed,
        b"truncate(path, 0) succeeds",
        lin::truncate(PATH, 0),
        0,
    );
    lin::check_eq(
        &mut failed,
        b"fstat succeeds",
        lin::fstat(fd, &raw mut st),
        0,
    );
    lin::check_eq(
        &mut failed,
        b"the emptied file is zero bytes long",
        st.st_size as isize,
        0,
    );
    lin::check_eq(
        &mut failed,
        b"the offset is rewound",
        lin::lseek(fd, 0, lin::SEEK_SET),
        0,
    );
    lin::check_eq(
        &mut failed,
        b"a read at offset 0 answers end-of-file",
        lin::read(fd, &mut buf[..16]),
        0,
    );

    // A path that names nothing cannot be resized.
    lin::check_eq(
        &mut failed,
        b"truncate on a missing path answers ENOENT",
        lin::truncate(MISSING, 0),
        lin::ENOENT,
    );

    // The trap: the descriptor's cursor is not the file's length. Read to
    // end-of-file (offset 10), resize to zero, and write three bytes - they
    // land at offset 10, so the file is 13 bytes with a ten-byte hole.
    lin::check_eq(
        &mut failed,
        b"the offset is rewound",
        lin::lseek(fd, 0, lin::SEEK_SET),
        0,
    );
    lin::check_eq(
        &mut failed,
        b"ten bytes are written back",
        lin::write(fd, b"0123456789"),
        10,
    );
    lin::check_eq(
        &mut failed,
        b"the offset is rewound",
        lin::lseek(fd, 0, lin::SEEK_SET),
        0,
    );
    lin::check_eq(
        &mut failed,
        b"reading to end-of-file leaves the offset at 10",
        lin::read(fd, &mut buf[..16]),
        10,
    );
    lin::check_eq(
        &mut failed,
        b"ftruncate to 0 succeeds",
        lin::ftruncate(fd, 0),
        0,
    );
    lin::check_eq(
        &mut failed,
        b"a write after the resize lands at the unchanged offset",
        lin::write(fd, b"XYZ"),
        3,
    );
    lin::check_eq(
        &mut failed,
        b"fstat succeeds",
        lin::fstat(fd, &raw mut st),
        0,
    );
    lin::check_eq(
        &mut failed,
        b"the file is 13 bytes: the hole plus the write",
        st.st_size as isize,
        13,
    );
    lin::check_eq(
        &mut failed,
        b"the offset is rewound",
        lin::lseek(fd, 0, lin::SEEK_SET),
        0,
    );
    lin::check_eq(
        &mut failed,
        b"the whole file reads back",
        lin::read(fd, &mut buf[..13]),
        13,
    );
    lin::check_true(
        &mut failed,
        b"the ten-byte hole reads as zeros",
        buf[..10].iter().all(|&b| b == 0),
    );
    lin::check_true(
        &mut failed,
        b"the write follows the hole",
        buf[10..13] == *b"XYZ",
    );

    let _ = lin::close(fd);
    let _ = lin::unlinkat(lin::AT_FDCWD, PATH, 0);
    lin::done(failed)
}

#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    lin::die(b"t-truncate: panicked\n")
}
