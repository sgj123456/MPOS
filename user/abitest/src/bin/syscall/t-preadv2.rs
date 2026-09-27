//! t-preadv2 - the vectored pair at a position, with the advice words.
//!
//! Pinned here, per preadv2(2)/pwritev2(2): the offset names the position
//! and the descriptor's cursor does not move, the offset of -1 is the
//! cursor-carrying spelling (and the cursor moves), an advice bit the
//! kernel does not know is EOPNOTSUPP, and `RWF_APPEND` writes at the end
//! of the file wherever the offset named.

#![no_std]
#![no_main]

#[path = "../../../lib/lin.rs"]
mod lin;

use core::panic::PanicInfo;

static PATH: [u8; 15] = *b"/tmp/t-preadv2\0";

#[unsafe(no_mangle)]
extern "C" fn mpos_user_main(_argc: usize, _argv: *const *const u8) -> ! {
    lin::puts(b"t-preadv2\n");
    let mut failed = false;

    let fd = lin::openat(
        lin::AT_FDCWD,
        &PATH,
        lin::O_CREAT | lin::O_RDWR | 0o1000,
        0o644,
    ) as i32;
    lin::check_true(&mut failed, b"the file is created", fd >= 0);
    lin::check_eq(
        &mut failed,
        b"the file takes 10 bytes",
        lin::write(fd, b"0123456789"),
        10,
    );

    static mut A: [u8; 3] = [b'X'; 3];
    static mut B: [u8; 4] = [b'X'; 4];
    let mut iov = [
        lin::Iovec {
            base: &raw mut A as *mut u8,
            len: 3,
        },
        lin::Iovec {
            base: &raw mut B as *mut u8,
            len: 4,
        },
    ];

    // The explicit position: bytes 2..9 land across the two vectors, and
    // the descriptor's own cursor stays where it was.
    let n = lin::preadv2(
        fd,
        unsafe { core::slice::from_raw_parts_mut(&raw mut iov as *mut lin::Iovec, 2) },
        2,
        0,
    );
    lin::check_eq(&mut failed, b"the vectored read takes 7 bytes", n, 7);
    lin::check_true(&mut failed, b"the first vector read bytes 2,3,4", unsafe {
        A == *b"234"
    });
    lin::check_true(
        &mut failed,
        b"the second vector read bytes 5,6,7,8",
        unsafe { B == *b"5678" },
    );
    lin::check_eq(
        &mut failed,
        b"the descriptor's cursor did not move",
        lin::lseek(fd, 0, lin::SEEK_CUR),
        10,
    );

    // A bad position is EINVAL - a negative offset is not a place in a
    // file, except the one negative the ABI gives a meaning to.
    lin::check_eq(
        &mut failed,
        b"a negative offset other than -1 answers EINVAL",
        lin::preadv2(
            fd,
            unsafe { core::slice::from_raw_parts_mut(&raw mut iov as *mut lin::Iovec, 2) },
            -5,
            0,
        ),
        lin::EINVAL,
    );

    // The -1 spelling: the cursor's own position, moved by the call.
    lin::check_eq(
        &mut failed,
        b"the cursor seeks to 0",
        lin::lseek(fd, 0, lin::SEEK_SET),
        0,
    );
    let n = lin::preadv2(
        fd,
        unsafe { core::slice::from_raw_parts_mut(&raw mut iov as *mut lin::Iovec, 2) },
        -1,
        0,
    );
    lin::check_eq(&mut failed, b"the cursor-mode read takes 7 bytes", n, 7);
    lin::check_eq(
        &mut failed,
        b"the cursor-mode read moved the cursor",
        lin::lseek(fd, 0, lin::SEEK_CUR),
        7,
    );

    // The advice vocabulary: every bit the kernel knows is accepted (its
    // reads never wait and its writes already reach storage), and a bit
    // it does not know is EOPNOTSUPP.
    lin::check_eq(
        &mut failed,
        b"RWF_NOWAIT on an eager read is accepted",
        lin::preadv2(
            fd,
            unsafe { core::slice::from_raw_parts_mut(&raw mut iov as *mut lin::Iovec, 2) },
            0,
            lin::RWF_NOWAIT,
        ),
        7,
    );
    lin::check_eq(
        &mut failed,
        b"an unknown advice bit answers EOPNOTSUPP",
        lin::preadv2(
            fd,
            unsafe { core::slice::from_raw_parts_mut(&raw mut iov as *mut lin::Iovec, 2) },
            0,
            0x1000,
        ),
        lin::EOPNOTSUPP,
    );

    // The per-write append: RWF_APPEND writes at the end of the file,
    // wherever the offset named.
    static mut APPEND: [u8; 3] = *b"xyz";
    let wiov = [lin::Iovec {
        base: &raw mut APPEND as *mut u8,
        len: 3,
    }];
    let n = lin::pwritev2(fd, &wiov, 0, lin::RWF_APPEND);
    lin::check_eq(&mut failed, b"the append write takes 3 bytes", n, 3);
    static mut TAIL: [u8; 3] = [0; 3];
    lin::check_eq(
        &mut failed,
        b"the tail seeks to 10",
        lin::lseek(fd, 10, lin::SEEK_SET),
        10,
    );
    let r = lin::read(fd, unsafe {
        core::slice::from_raw_parts_mut(&raw mut TAIL as *mut u8, 3)
    });
    lin::check_eq(&mut failed, b"the tail read takes 3 bytes", r, 3);
    lin::check_true(
        &mut failed,
        b"the appended bytes landed at the end",
        unsafe { TAIL == *b"xyz" },
    );

    lin::check_eq(
        &mut failed,
        b"a closed descriptor answers EBADF",
        lin::preadv2(
            99,
            unsafe { core::slice::from_raw_parts_mut(&raw mut iov as *mut lin::Iovec, 2) },
            0,
            0,
        ),
        lin::EBADF,
    );

    let _ = lin::close(fd);
    let _ = lin::unlinkat(lin::AT_FDCWD, &PATH, 0);
    lin::done(failed)
}

#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    lin::die(b"t-preadv2: panicked\n")
}
