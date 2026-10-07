//! t-fionbio - `ioctl(fd, FIONBIO, &int)`: non-blocking mode set through `ioctl`
//! rather than `fcntl`.
//!
//! Rust's std uses this one and not `fcntl` on Linux. `read_output` - what
//! `Command::output()` uses to read a child's stdout and stderr at the same time
//! - begins by putting both pipes in non-blocking mode, and a kernel that
//! answers `ENOTTY` to a request it does not know therefore makes every Rust
//! `Command::output()` panic with "Not a tty". That is how `rustup-init` died.
//!
//! Pinned here: the call succeeds on a pipe, it sets the same `O_NONBLOCK` that
//! `fcntl(F_GETFL)` reports, it actually bites - an empty non-blocking pipe
//! answers `EAGAIN` instead of parking the caller - clearing it puts the
//! descriptor back, and a descriptor that is not open is still `EBADF`.
//!
//! Nothing here touches the filesystem: the fixture is a pipe, which both
//! machines' test payloads make the same way.

use mpos_user_tests::Report;
use rustix::fd::BorrowedFd;
use rustix::fs::{OFlags, fcntl_getfl};
use rustix::io::{Errno, ioctl_fionbio, read, write};

fn main() {
    println!("t-fionbio");
    let mut r = Report::new();

    let Ok((read_fd, write_fd)) = rustix::pipe::pipe() else {
        r.check("pipe2 makes a pipe", false);
        r.finish()
    };
    r.check("pipe2 makes a pipe", true);

    // The argument is a pointer to an int, not the int itself - which is what
    // `ioctl_fionbio`'s signature is guarding against.
    r.check(
        "FIONBIO on a pipe succeeds",
        ioctl_fionbio(&read_fd, true).is_ok(),
    );

    // FIONBIO and F_SETFL write the same bit, so F_GETFL has to see it - a
    // kernel that only stored the status word without telling the pipe would
    // pass the call and still block.
    r.check(
        "F_GETFL reports O_NONBLOCK after FIONBIO",
        fcntl_getfl(&read_fd).is_ok_and(|flags| flags.contains(OFlags::NONBLOCK)),
    );

    // And it bites: an empty non-blocking pipe is EAGAIN, not a wait.
    let mut buf = [0u8; 8];
    r.check(
        "an empty non-blocking pipe reads EAGAIN",
        read(&read_fd, &mut buf) == Err(Errno::AGAIN),
    );

    r.check("a write to the pipe lands", write(&write_fd, b"x") == Ok(1));
    r.check("and the byte reads back", read(&read_fd, &mut buf) == Ok(1));

    // Zero clears it, and F_GETFL agrees again.
    r.check(
        "FIONBIO with zero succeeds",
        ioctl_fionbio(&read_fd, false).is_ok(),
    );
    r.check(
        "F_GETFL no longer reports O_NONBLOCK",
        fcntl_getfl(&read_fd).is_ok_and(|flags| !flags.contains(OFlags::NONBLOCK)),
    );

    // A descriptor number nothing is open on. The `unsafe` is the borrow's own
    // contract - the fd must not be *used* for anything that assumes it is open
    // - and an ioctl that answers EBADF is exactly the use this test wants.
    let closed = unsafe { BorrowedFd::borrow_raw(999) };
    r.check(
        "FIONBIO on a descriptor that is not open is EBADF",
        ioctl_fionbio(closed, true) == Err(Errno::BADF),
    );

    r.finish()
}
