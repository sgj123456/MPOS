//! t-dup - one test for dup, the fd-table copy musl's startup and the
//! shell's redirections lean on.
//!
//! Pinned here: dup hands back the lowest free descriptor, and the duplicate
//! names the *same* open file description - one cursor moved through one fd is
//! visible through the other, which is what makes `cat a b > out` append
//! instead of overwrite. A descriptor that does not exist answers EBADF.
//!
//! "Lowest free" is measured, not assumed to be 3. A process inherits whatever
//! descriptors its parent had: under mpos's own shell that is 0-2 and nothing
//! else, but a real shell like Alpine's ash holds one of its own, and the
//! kernel answering 4 when 3 is taken is correct. What this pins is that dup
//! and open agree on the same next number.

#![no_std]
#![no_main]

#[path = "../../../lib/lin.rs"]
mod lin;

use core::panic::PanicInfo;

static MOTD: [u8; 10] = *b"/etc/motd\0";

#[unsafe(no_mangle)]
extern "C" fn mpos_user_main(_argc: usize, _argv: *const *const u8) -> ! {
    lin::puts(b"t-dup\n");
    let mut failed = false;

    // What the lowest free descriptor is, discovered rather than assumed: dup
    // and open must both answer it.
    let d = lin::dup(1);
    lin::check_true(&mut failed, b"dup(stdout) hands out a free fd", d >= 3);
    let _ = lin::close(d as i32);

    let a = lin::openat(lin::AT_FDCWD, &MOTD, lin::O_RDONLY, 0);
    lin::check_eq(&mut failed, b"the file opens on the fd dup had", a, d);

    let b = lin::dup(a as i32);
    lin::check_eq(&mut failed, b"dup lands on the next free fd", b, d + 1);

    let r = lin::lseek(a as i32, 2, lin::SEEK_SET);
    lin::check_eq(&mut failed, b"lseek through the original reports 2", r, 2);
    let r = lin::lseek(b as i32, 0, lin::SEEK_CUR);
    lin::check_eq(&mut failed, b"the duplicate shares the file offset", r, 2);

    let end = lin::lseek(b as i32, 0, lin::SEEK_END);
    lin::check_true(
        &mut failed,
        b"SEEK_END through the duplicate reports the size",
        end > 0,
    );
    let r = lin::lseek(a as i32, 0, lin::SEEK_CUR);
    lin::check_eq(
        &mut failed,
        b"the original sees the duplicate's seek",
        r,
        end,
    );

    let r = lin::close(a as i32);
    lin::check_eq(&mut failed, b"closing the original succeeds", r, 0);
    let r = lin::close(b as i32);
    lin::check_eq(&mut failed, b"closing the duplicate succeeds", r, 0);

    let r = lin::dup(99);
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
    lin::die(b"t-dup: panicked\n")
}
