//! t-eventfd - the counter a process waits on, reached through a descriptor.
//!
//! Pinned here, per eventfd(2): the create flags are `EFD_NONBLOCK` and its
//! siblings, the counter starts at the requested initial value, a read of a
//! non-empty counter takes the whole value and leaves zero behind, two writes
//! add before one read takes the sum, and a read or a write shorter than the
//! eight bytes the ABI fixes is `EINVAL`.
//!
//! Where mpos diverges: this kernel offers only the whole-count take. Linux's
//! `EFD_SEMAPHORE` asks for a read to decrement by one and hand back one; that
//! mode is not implemented here, and the create call says so with
//! `EOPNOTSUPP` rather than granting a descriptor whose reads would quietly
//! mean something else. The check below pins that refusal, not a semaphore.
//!
//! A blocking read of a zero counter parks until someone writes; every empty
//! read here is therefore on an `EFD_NONBLOCK` descriptor, where the ABI's
//! answer is `EAGAIN`.

#![no_std]
#![no_main]

#[path = "../../../lib/lin.rs"]
mod lin;

use core::panic::PanicInfo;

#[unsafe(no_mangle)]
extern "C" fn mpos_user_main(_argc: usize, _argv: *const *const u8) -> ! {
    lin::puts(b"t-eventfd\n");
    let mut failed = false;

    // A fresh counter, nonblocking: an empty read answers EAGAIN rather than
    // parking. A blocking read of a zero counter would never return.
    let fd = lin::eventfd(0, lin::EFD_NONBLOCK) as i32;
    lin::check_true(
        &mut failed,
        b"eventfd(0, EFD_NONBLOCK) hands back a descriptor",
        fd >= 0,
    );

    let mut eight = [0u8; 8];
    lin::check_eq(
        &mut failed,
        b"a read of an empty counter answers EAGAIN, not a park",
        lin::read(fd, &mut eight),
        lin::EAGAIN,
    );

    // A write of 8 bytes carrying 5, then a read that takes the counter whole.
    let five = 5u64.to_ne_bytes();
    lin::check_eq(
        &mut failed,
        b"a write of 8 bytes carrying 5 takes 8",
        lin::write(fd, &five),
        8,
    );
    lin::check_eq(
        &mut failed,
        b"the read takes 8 bytes",
        lin::read(fd, &mut eight),
        8,
    );
    lin::check_eq(
        &mut failed,
        b"the value read back is 5, native-endian",
        u64::from_ne_bytes(eight) as isize,
        5,
    );
    lin::check_eq(
        &mut failed,
        b"the counter was taken whole, so the next read is EAGAIN",
        lin::read(fd, &mut eight),
        lin::EAGAIN,
    );

    // Taken whole: two writes add, one read takes the sum.
    let three = 3u64.to_ne_bytes();
    let four = 4u64.to_ne_bytes();
    lin::check_eq(
        &mut failed,
        b"a write of 3 takes 8",
        lin::write(fd, &three),
        8,
    );
    lin::check_eq(
        &mut failed,
        b"a write of 4 takes 8",
        lin::write(fd, &four),
        8,
    );
    lin::check_eq(
        &mut failed,
        b"the read takes 8 bytes",
        lin::read(fd, &mut eight),
        8,
    );
    lin::check_eq(
        &mut failed,
        b"the counter is the sum, 7, not 3 or 4",
        u64::from_ne_bytes(eight) as isize,
        7,
    );

    // The read and the write are 8 bytes wide and no less.
    let mut four_bytes = [0u8; 4];
    lin::check_eq(
        &mut failed,
        b"a 4-byte read answers EINVAL",
        lin::read(fd, &mut four_bytes),
        lin::EINVAL,
    );
    lin::check_eq(
        &mut failed,
        b"a 4-byte write answers EINVAL",
        lin::write(fd, &four_bytes),
        lin::EINVAL,
    );

    // The initial value is the counter's starting point.
    let start = lin::eventfd(7, lin::EFD_NONBLOCK) as i32;
    lin::check_true(
        &mut failed,
        b"eventfd(7, EFD_NONBLOCK) hands back a descriptor",
        start >= 0,
    );
    lin::check_eq(
        &mut failed,
        b"the counter starts at 7",
        lin::read(start, &mut eight),
        8,
    );
    lin::check_eq(
        &mut failed,
        b"the first read is 7",
        u64::from_ne_bytes(eight) as isize,
        7,
    );

    // The semaphore mode mpos deliberately does not offer.
    lin::check_eq(
        &mut failed,
        b"EFD_SEMAPHORE answers EOPNOTSUPP (whole-count take only)",
        lin::eventfd(0, lin::EFD_SEMAPHORE),
        lin::EOPNOTSUPP,
    );

    // The flag word is validated, not ignored: a bit outside the three `eventfd2`
    // knows is `EINVAL`, refused before the descriptor is made. A kernel that
    // accepted an unknown bit and did nothing with it told the caller it had the
    // descriptor it asked for when it did not - the §4.2 false success.
    lin::check_eq(
        &mut failed,
        b"an unknown eventfd flag bit is EINVAL",
        lin::eventfd(0, 0x40),
        lin::EINVAL,
    );
    // EFD_CLOEXEC actually sets close-on-exec - read it back, the way SFD_CLOEXEC
    // is pinned. Silently dropping it would leak the descriptor across exec.
    let cloexec = lin::eventfd(0, lin::EFD_CLOEXEC) as i32;
    lin::check_true(
        &mut failed,
        b"eventfd(0, EFD_CLOEXEC) hands back a descriptor",
        cloexec >= 0,
    );
    let fl = lin::fcntl(cloexec, lin::F_GETFD, 0);
    lin::check_true(
        &mut failed,
        b"EFD_CLOEXEC is reflected in the descriptor's close-on-exec flag",
        fl >= 0 && (fl as usize & lin::FD_CLOEXEC) != 0,
    );
    let _ = lin::close(cloexec);

    let _ = lin::close(fd);
    let _ = lin::close(start);
    lin::done(failed)
}

#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    lin::die(b"t-eventfd: panicked\n")
}
