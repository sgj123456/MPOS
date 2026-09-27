//! t-fallocate - the storage a range is promised.
//!
//! Pinned here, per fallocate(2): the default operation grows the file to
//! the end of the range (and the new bytes read as zeros), a bad range is
//! EINVAL before a thing is touched, an unknown mode bit is EOPNOTSUPP
//! the way Linux answers a mode word from a newer kernel, a punch without
//! KEEP_SIZE is EINVAL (and the punch itself is the unsupported interior
//! operation this kernel answers EOPNOTSUPP for), a descriptor that is
//! not a file is the man page's ENODEV, and KEEP_SIZE inside an existing
//! file changes nothing - the flag's promise is about the writes that
//! come, and on this kernel they cannot fail for lack of room anyway.

#![no_std]
#![no_main]

#[path = "../../../lib/lin.rs"]
mod lin;

use core::panic::PanicInfo;

static PATH: [u8; 17] = *b"/tmp/t-fallocate\0";

#[unsafe(no_mangle)]
extern "C" fn mpos_user_main(_argc: usize, _argv: *const *const u8) -> ! {
    lin::puts(b"t-fallocate\n");
    let mut failed = false;

    let fd = lin::openat(
        lin::AT_FDCWD,
        &PATH,
        lin::O_CREAT | lin::O_RDWR | 0o1000,
        0o644,
    ) as i32;
    lin::check_true(&mut failed, b"the file is created", fd >= 0);

    // The default operation: the range becomes the file's new size, and
    // the bytes inside it read as zeros.
    lin::check_eq(
        &mut failed,
        b"allocating 4096 bytes succeeds",
        lin::fallocate(fd, 0, 0, 4096),
        0,
    );
    let mut st = lin::Stat {
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
    };
    lin::check_eq(
        &mut failed,
        b"fstat succeeds",
        lin::fstat(fd, &raw mut st),
        0,
    );
    lin::check_eq(
        &mut failed,
        b"the file grew to the range's end",
        st.st_size as isize,
        4096,
    );
    static mut ZEROS: [u8; 16] = [1; 16];
    let r = lin::read(fd, unsafe {
        core::slice::from_raw_parts_mut(&raw mut ZEROS as *mut u8, 16)
    });
    lin::check_eq(&mut failed, b"16 bytes read back", r, 16);
    lin::check_true(&mut failed, b"the allocated range reads as zeros", unsafe {
        core::slice::from_raw_parts(&raw const ZEROS as *const u8, 16)
            .iter()
            .all(|&b| b == 0)
    });

    // The range's own validation, before any storage is touched.
    lin::check_eq(
        &mut failed,
        b"a zero length answers EINVAL",
        lin::fallocate(fd, 0, 0, 0),
        lin::EINVAL,
    );
    lin::check_eq(
        &mut failed,
        b"a past-the-end offset answers EINVAL",
        lin::fallocate(fd, 0, 1usize << 63, 1),
        lin::EINVAL,
    );

    // The mode vocabulary: unknown bits are EOPNOTSUPP, and the interior
    // operations this kernel cannot keep are answered the same way, after
    // the punch's own EINVAL when KEEP_SIZE is missing beside it.
    lin::check_eq(
        &mut failed,
        b"unknown mode bits answer EOPNOTSUPP",
        lin::fallocate(fd, 0x8000, 0, 1),
        lin::EOPNOTSUPP,
    );
    lin::check_eq(
        &mut failed,
        b"a punch without KEEP_SIZE answers EINVAL",
        lin::fallocate(fd, lin::FALLOC_FL_PUNCH_HOLE, 0, 1),
        lin::EINVAL,
    );
    lin::check_eq(
        &mut failed,
        b"the punch is unsupported here",
        lin::fallocate(
            fd,
            lin::FALLOC_FL_PUNCH_HOLE | lin::FALLOC_FL_KEEP_SIZE,
            0,
            1,
        ),
        lin::EOPNOTSUPP,
    );
    lin::check_eq(
        &mut failed,
        b"collapsing a range is unsupported here",
        lin::fallocate(fd, lin::FALLOC_FL_COLLAPSE_RANGE, 0, 1),
        lin::EOPNOTSUPP,
    );

    // KEEP_SIZE inside an existing file: accepted, and the size is the
    // flag's whole promise - unchanged.
    lin::check_eq(
        &mut failed,
        b"KEEP_SIZE inside the file succeeds",
        lin::fallocate(fd, lin::FALLOC_FL_KEEP_SIZE, 0, 100),
        0,
    );
    lin::check_eq(
        &mut failed,
        b"fstat still succeeds",
        lin::fstat(fd, &raw mut st),
        0,
    );
    lin::check_eq(
        &mut failed,
        b"KEEP_SIZE changed nothing",
        st.st_size as isize,
        4096,
    );

    // A descriptor that is not a file: the pipe gets the man page's
    // ENODEV, and a closed number gets EBADF.
    let mut fds: [i32; 2] = [-1; 2];
    lin::check_eq(
        &mut failed,
        b"the pipe pair is made",
        lin::pipe2(&mut fds, 0),
        0,
    );
    lin::check_eq(
        &mut failed,
        b"a pipe answers ENODEV",
        lin::fallocate(fds[0], 0, 0, 1),
        lin::ENODEV,
    );
    lin::check_eq(
        &mut failed,
        b"a closed number answers EBADF",
        lin::fallocate(99, 0, 0, 1),
        lin::EBADF,
    );

    let _ = lin::close(fds[0]);
    let _ = lin::close(fds[1]);
    let _ = lin::close(fd);
    let _ = lin::unlinkat(lin::AT_FDCWD, &PATH, 0);
    lin::done(failed)
}

#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    lin::die(b"t-fallocate: panicked\n")
}
