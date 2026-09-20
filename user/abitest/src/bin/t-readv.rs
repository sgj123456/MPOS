//! t-readv - one test for readv, the scatter read glibc's stdio drains
//! sockets and files with.
//!
//! Pinned here: the iovecs fill in order from the file's cursor, the return
//! is the total across all of them (bounded by what the iovecs can hold),
//! the bytes land where the caller aimed them - iov1 gets the file's first
//! bytes and iov2 the ones right after, checked against a plain re-read of
//! the same file - and a descriptor that does not exist answers EBADF.

#![no_std]
#![no_main]

#[path = "../../lib/lin.rs"]
mod lin;

use core::panic::PanicInfo;

static MOTD: [u8; 10] = *b"/etc/motd\0";
static mut IOV1: [u8; 4] = [0; 4];
static mut IOV2: [u8; 8] = [0; 8];
static mut WHOLE: [u8; 4096] = [0; 4096];

#[unsafe(no_mangle)]
extern "C" fn mpos_user_main(_argc: usize, _argv: *const *const u8) -> ! {
    lin::puts(b"t-readv\n");
    let mut failed = false;
    let base = lin::free_fd();

    let fd = lin::openat(lin::AT_FDCWD, &MOTD, lin::O_RDONLY, 0);
    lin::check_eq(&mut failed, b"the file opens on the lowest free fd", fd, base);

    let mut iov = [
        lin::Iovec {
            base: &raw mut IOV1 as *mut u8,
            len: 4,
        },
        lin::Iovec {
            base: &raw mut IOV2 as *mut u8,
            len: 8,
        },
    ];
    let total = lin::readv(fd as i32, &mut iov);
    lin::check_true(&mut failed, b"readv hands back bytes", total > 0);
    lin::check_true(
        &mut failed,
        b"the total is bounded by the iovecs (4 + 8)",
        total <= 12,
    );

    // How much of each iovec filled: in order, until the file runs out.
    let t = total.max(0) as usize;
    let n1 = t.min(4);
    let n2 = (t - n1).min(8);

    let r = lin::lseek(fd as i32, 0, lin::SEEK_SET);
    lin::check_eq(&mut failed, b"rewinding for the plain re-read reports 0", r, 0);
    let whole = unsafe { core::slice::from_raw_parts_mut(&raw mut WHOLE as *mut u8, 4096) };
    let n = lin::read(fd as i32, whole);
    lin::check_true(
        &mut failed,
        b"the plain re-read covers everything readv took",
        n >= total && n > 0,
    );

    let iov1 = unsafe { core::slice::from_raw_parts(&raw mut IOV1 as *const u8, 4) };
    let iov2 = unsafe { core::slice::from_raw_parts(&raw mut IOV2 as *const u8, 8) };
    let mut in_order = true;
    for i in 0..n1 {
        if iov1[i] != whole[i] {
            in_order = false;
        }
    }
    for i in 0..n2 {
        if iov2[i] != whole[n1 + i] {
            in_order = false;
        }
    }
    lin::check_true(
        &mut failed,
        b"the iovecs hold the file's bytes in order",
        in_order,
    );

    let r = lin::readv(99, &mut iov);
    lin::check_eq(
        &mut failed,
        b"a descriptor that does not exist answers EBADF",
        r,
        lin::EBADF,
    );

    let _ = lin::close(fd as i32);
    lin::done(failed)
}

#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    lin::die(b"t-readv: panicked\n")
}
