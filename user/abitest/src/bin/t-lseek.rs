//! t-lseek - one test for lseek, which the strace uses to walk back through
//! /proc/self/maps (`lseek(3, 0, SEEK_CUR)`, `lseek(3, 3070, SEEK_SET)`).
//!
//! Pinned here on a real file: SEEK_END reports the size, SEEK_SET and
//! SEEK_CUR report and move the absolute position, a full re-read from 0
//! hands the whole file back, and a bogus whence answers EINVAL. The edges
//! are pinned too: a seek that would land before byte 0 answers EINVAL for
//! all three origins, and a descriptor that does not exist answers EBADF.

#![no_std]
#![no_main]

#[path = "../../lib/lin.rs"]
mod lin;

use core::panic::PanicInfo;

static MOTD: [u8; 10] = *b"/etc/motd\0";
static mut BUF: [u8; 4096] = [0; 4096];

#[unsafe(no_mangle)]
extern "C" fn mpos_user_main(_argc: usize, _argv: *const *const u8) -> ! {
    lin::puts(b"t-lseek\n");
    let mut failed = false;

    let fd = lin::openat(lin::AT_FDCWD, &MOTD, lin::O_RDONLY, 0);
    lin::check_true(&mut failed, b"the file to seek opens", fd >= 0);

    if fd >= 0 {
        // Drain the file to learn its true size.
        let mut total = 0usize;
        for _ in 0..8 {
            let buf = unsafe { core::slice::from_raw_parts_mut(&raw mut BUF as *mut u8, 4096) };
            let n = lin::read(fd as i32, buf);
            if n <= 0 {
                break;
            }
            total += n as usize;
        }

        let r = lin::lseek(fd as i32, 0, lin::SEEK_END);
        lin::check_eq(
            &mut failed,
            b"SEEK_END reports the file size",
            r,
            total as isize,
        );

        let r = lin::lseek(fd as i32, 0, lin::SEEK_SET);
        lin::check_eq(&mut failed, b"SEEK_SET rewinds to 0", r, 0);

        let buf = unsafe { core::slice::from_raw_parts_mut(&raw mut BUF as *mut u8, 4096) };
        let n = lin::read(fd as i32, buf);
        lin::check_eq(
            &mut failed,
            b"a full re-read hands the whole file back",
            n,
            total as isize,
        );

        let r = lin::lseek(fd as i32, 0, lin::SEEK_CUR);
        lin::check_eq(
            &mut failed,
            b"SEEK_CUR reports the position after the read",
            r,
            total as isize,
        );

        let r = lin::lseek(fd as i32, 0, 5);
        lin::check_eq(
            &mut failed,
            b"an unknown whence answers EINVAL",
            r,
            lin::EINVAL,
        );

        // Negative landings: every origin that would end before byte 0.
        let r = lin::lseek(fd as i32, -1, lin::SEEK_SET);
        lin::check_eq(
            &mut failed,
            b"SEEK_SET to -1 answers EINVAL",
            r,
            lin::EINVAL,
        );

        let r = lin::lseek(fd as i32, 0, lin::SEEK_END);
        lin::check_eq(
            &mut failed,
            b"SEEK_END(0) parks at the size",
            r,
            total as isize,
        );
        let r = lin::lseek(fd as i32, -1, lin::SEEK_CUR);
        lin::check_eq(
            &mut failed,
            b"SEEK_CUR(-1) from the end lands on size - 1",
            r,
            total as isize - 1,
        );
        let r = lin::lseek(fd as i32, -1_000_000, lin::SEEK_CUR);
        lin::check_eq(
            &mut failed,
            b"SEEK_CUR far before byte 0 answers EINVAL",
            r,
            lin::EINVAL,
        );

        let r = lin::lseek(fd as i32, -(total as isize), lin::SEEK_END);
        lin::check_eq(&mut failed, b"SEEK_END(-size) rewinds to 0", r, 0);

        let _ = lin::close(fd as i32);
    }

    let r = lin::lseek(99, 0, lin::SEEK_SET);
    lin::check_eq(
        &mut failed,
        b"a descriptor that does not exist answers EBADF",
        r,
        lin::EBADF,
    );

    lin::done(failed)
}

#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    lin::die(b"t-lseek: panicked\n")
}
