//! t-dup3 - one test for dup3, dup2 plus the descriptor-flag argument.
//!
//! Pinned here: flags = 0 behaves as dup2, oldfd == newfd answers EINVAL
//! (the one way dup3 differs from dup2's no-op), a flag word with bits
//! beyond O_CLOEXEC answers EINVAL, O_CLOEXEC alone is accepted, and the
//! copy lands exactly on newfd, replacing whatever named it before - the
//! old description is closed, which is what makes redirections safe.

#![no_std]
#![no_main]

#[path = "../../lib/lin.rs"]
mod lin;

use core::panic::PanicInfo;

static MOTD: [u8; 10] = *b"/etc/motd\0";
static LINE: &[u8; 9] = b"replaced\n";

#[unsafe(no_mangle)]
extern "C" fn mpos_user_main(_argc: usize, _argv: *const *const u8) -> ! {
    lin::puts(b"t-dup3\n");
    let mut failed = false;
    let base = lin::free_fd();

    let r = lin::dup3(1, 5, 0);
    lin::check_eq(&mut failed, b"dup3(stdout, 5, 0) returns 5", r, 5);
    let _ = lin::close(5);

    let r = lin::dup3(1, 1, 0);
    lin::check_eq(
        &mut failed,
        b"dup3 with oldfd == newfd answers EINVAL",
        r,
        lin::EINVAL,
    );

    let r = lin::dup3(1, 6, 0xFFFF_FFFF);
    lin::check_eq(
        &mut failed,
        b"flag bits beyond O_CLOEXEC answer EINVAL",
        r,
        lin::EINVAL,
    );

    let r = lin::dup3(1, 6, lin::O_CLOEXEC);
    lin::check_eq(&mut failed, b"dup3 with O_CLOEXEC succeeds", r, 6);
    let _ = lin::close(6);

    let a = lin::openat(lin::AT_FDCWD, &MOTD, lin::O_RDONLY, 0);
    lin::check_eq(
        &mut failed,
        b"the file opens on the lowest free fd",
        a,
        base,
    );

    let r = lin::dup3(1, 3, 0);
    lin::check_eq(&mut failed, b"dup3 onto the open file fd replaces it", r, 3);
    let n = lin::write(3, LINE);
    lin::check_eq(
        &mut failed,
        b"the replaced fd now writes to the console, every byte",
        n,
        LINE.len() as isize,
    );

    let r = lin::close(3);
    lin::check_eq(&mut failed, b"closing the replaced fd succeeds", r, 0);

    lin::done(failed)
}

#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    lin::die(b"t-dup3: panicked\n")
}
