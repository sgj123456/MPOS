//! t-copy-file-range - the copy the kernel makes.
//!
//! Pinned here, per copy_file_range(2): both ends must be regular files
//! (a pipe is the man page's EINVAL), a non-zero flags word is EINVAL, a
//! copy into the same file at an overlapping range is EINVAL, the offsets
//! are in-out loff_t words that advance by the copy, and the null-offset
//! spelling is the descriptors' own cursors, which the copy moves.

#![no_std]
#![no_main]

#[path = "../../lib/lin.rs"]
mod lin;

use core::panic::PanicInfo;

static SRC: [u8; 15] = *b"/tmp/t_cfr_src\0";
static DST: [u8; 15] = *b"/tmp/t_cfr_dst\0";

#[unsafe(no_mangle)]
extern "C" fn mpos_user_main(_argc: usize, _argv: *const *const u8) -> ! {
    lin::puts(b"t-copy-file-range\n");
    let mut failed = false;

    let src = lin::openat(lin::AT_FDCWD, &SRC, lin::O_CREAT | lin::O_RDWR | 0o1000, 0o644) as i32;
    lin::check_true(&mut failed, b"the source is created", src >= 0);
    let dst = lin::openat(lin::AT_FDCWD, &DST, lin::O_CREAT | lin::O_RDWR | 0o1000, 0o644) as i32;
    lin::check_true(&mut failed, b"the destination is created", dst >= 0);
    lin::check_eq(&mut failed, b"the source takes 11 bytes", lin::write(src, b"hello world"), 11);

    // The explicit-offset spelling: both words name byte 0, and the copy
    // hands back the full length and moves them to its end.
    let mut off_in: i64 = 0;
    let mut off_out: i64 = 0;
    let n = lin::copy_file_range(src, &raw mut off_in, dst, &raw mut off_out, 11, 0);
    lin::check_eq(&mut failed, b"the copy takes all 11 bytes", n, 11);
    lin::check_eq(&mut failed, b"the source offset advanced", off_in as isize, 11);
    lin::check_eq(&mut failed, b"the destination offset advanced", off_out as isize, 11);

    // What landed is what was copied, read back from byte 0.
    lin::check_eq(&mut failed, b"the destination seeks to 0", lin::lseek(dst, 0, lin::SEEK_SET), 0);
    static mut BACK: [u8; 16] = [0; 16];
    let r = lin::read(dst, unsafe { core::slice::from_raw_parts_mut(&raw mut BACK as *mut u8, 16) });
    lin::check_eq(&mut failed, b"the destination reads back 11 bytes", r, 11);
    lin::check_true(
        &mut failed,
        b"the bytes that arrived are the bytes copied",
        unsafe { core::slice::from_raw_parts(&raw const BACK as *const u8, 11) == *b"hello world" },
    );

    // A non-zero flags word is the man page's own EINVAL.
    lin::check_eq(
        &mut failed,
        b"a non-zero flags word answers EINVAL",
        lin::copy_file_range(src, &raw mut off_in, dst, &raw mut off_out, 1, 1),
        lin::EINVAL,
    );

    // The same file, overlapping ranges: the one way a copy corrupts what
    // it is copying, and EINVAL is the man page's answer.
    let mut a: i64 = 0;
    let mut b: i64 = 5;
    lin::check_eq(
        &mut failed,
        b"an overlapping self-copy answers EINVAL",
        lin::copy_file_range(src, &raw mut a, src, &raw mut b, 8, 0),
        lin::EINVAL,
    );

    // A descriptor that is not a regular file: the pipe is EINVAL.
    let mut fds: [i32; 2] = [-1; 2];
    lin::check_eq(&mut failed, b"the pipe pair is made", lin::pipe2(&mut fds, 0), 0);
    let mut a: i64 = 0;
    let mut b: i64 = 0;
    lin::check_eq(
        &mut failed,
        b"a pipe source answers EINVAL",
        lin::copy_file_range(fds[0], &raw mut a, dst, &raw mut b, 1, 0),
        lin::EINVAL,
    );
    lin::check_eq(
        &mut failed,
        b"a pipe destination answers EINVAL",
        lin::copy_file_range(src, &raw mut a, fds[1], &raw mut b, 1, 0),
        lin::EINVAL,
    );

    // The cursor spelling: no offset words, the descriptors' own positions
    // name the ranges, and the copy moves them.
    lin::check_eq(&mut failed, b"the source seeks to 0", lin::lseek(src, 0, lin::SEEK_SET), 0);
    lin::check_eq(&mut failed, b"the destination seeks to 0", lin::lseek(dst, 0, lin::SEEK_SET), 0);
    let n = lin::copy_file_range(src, core::ptr::null_mut(), dst, core::ptr::null_mut(), 5, 0);
    lin::check_eq(&mut failed, b"the cursor-mode copy takes 5 bytes", n, 5);
    lin::check_eq(&mut failed, b"the source cursor advanced", lin::lseek(src, 0, lin::SEEK_CUR), 5);
    lin::check_eq(&mut failed, b"the destination cursor advanced", lin::lseek(dst, 0, lin::SEEK_CUR), 5);
    lin::check_eq(
        &mut failed,
        b"a closed descriptor answers EBADF",
        lin::copy_file_range(99, core::ptr::null_mut(), dst, core::ptr::null_mut(), 1, 0),
        lin::EBADF,
    );

    let _ = lin::close(fds[0]);
    let _ = lin::close(fds[1]);
    let _ = lin::close(src);
    let _ = lin::close(dst);
    let _ = lin::unlinkat(lin::AT_FDCWD, &SRC, 0);
    let _ = lin::unlinkat(lin::AT_FDCWD, &DST, 0);
    lin::done(failed)
}

#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    lin::die(b"t-copy-file-range: panicked\n")
}
