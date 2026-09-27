//! t-sync - the flush calls: fsync, fdatasync, syncfs and sync.
//!
//! Pinned here, per fsync(2)/fdatasync(2)/syncfs(2)/sync(2): a regular
//! file answers 0 to all three descriptor forms, every one of them answers
//! EBADF for a number the table does not hold, and `fsync` validates the
//! *kind* of what the descriptor holds rather than merely its existence -
//! a channel with no storage cannot be synchronised, which the one shared
//! handler spells as EINVAL for its `CharDev` and `Socket` kinds. An mpos
//! `SOCK_DGRAM` socketpair end is a `CharDev` inode, so it is the
//! descriptor this test reaches EINVAL through; `sync(2)` takes no
//! argument and is answered 0 at the dispatch table itself, where the
//! number has no handler to run.
//!
//! The two byte channels whose kind cannot answer for them are refused by
//! name, not by kind: a pipe and a `SOCK_STREAM` socketpair end report
//! `File`, because the vocabulary has no word for "anonymous byte channel"
//! (see `pipe.rs`), so the handler asks the *node* whether it is one and
//! answers EINVAL the way Linux does. Reading the kind alone answered 0 for
//! both, which is the bug those checks would catch coming back. Every write
//! here already reached its storage, so the success answer is only the
//! descriptor check and no flush is a divergence.

#![no_std]
#![no_main]

#[path = "../../../lib/lin.rs"]
mod lin;

use core::panic::PanicInfo;

static PATH: &[u8] = b"/tmp/t-sync\0";

#[unsafe(no_mangle)]
extern "C" fn mpos_user_main(_argc: usize, _argv: *const *const u8) -> ! {
    lin::puts(b"t-sync\n");
    let mut failed = false;

    let fd = lin::openat(lin::AT_FDCWD, PATH, lin::O_CREAT | lin::O_RDWR, 0o644) as i32;
    lin::check_true(&mut failed, b"the scratch file is created", fd >= 0);
    lin::check_eq(
        &mut failed,
        b"fsync on a regular file succeeds",
        lin::fsync(fd),
        0,
    );
    lin::check_eq(
        &mut failed,
        b"fdatasync on a regular file succeeds",
        lin::fdatasync(fd),
        0,
    );
    lin::check_eq(
        &mut failed,
        b"syncfs on a regular file succeeds",
        lin::syncfs(fd),
        0,
    );

    // The kind check: an mpos SOCK_DGRAM socketpair end reports CharDev, so
    // the shared handler refuses it EINVAL the way Linux does a socket.
    let mut sv: [i32; 2] = [-1; 2];
    lin::check_eq(
        &mut failed,
        b"the unix socketpair is made",
        lin::socketpair(lin::AF_UNIX, lin::SOCK_DGRAM, 0, &mut sv),
        0,
    );
    lin::check_eq(
        &mut failed,
        b"fsync on a socket answers EINVAL",
        lin::fsync(sv[0]),
        lin::EINVAL,
    );
    lin::check_eq(
        &mut failed,
        b"fdatasync on a socket answers EINVAL",
        lin::fdatasync(sv[0]),
        lin::EINVAL,
    );
    lin::check_eq(
        &mut failed,
        b"syncfs on a socket still succeeds",
        lin::syncfs(sv[0]),
        0,
    );

    // A pipe is refused like the socket above, and for the same reason: a
    // byte channel with no storage behind it. Its `kind` still cannot say so
    // - a pipe reports File - so the handler reaches it through `as_pipe`.
    let mut pipes: [i32; 2] = [-1; 2];
    lin::check_eq(
        &mut failed,
        b"the pipe pair is made",
        lin::pipe2(&mut pipes, 0),
        0,
    );
    lin::check_eq(
        &mut failed,
        b"fsync on a pipe answers EINVAL",
        lin::fsync(pipes[0]),
        lin::EINVAL,
    );
    lin::check_eq(
        &mut failed,
        b"fdatasync on a pipe answers EINVAL",
        lin::fdatasync(pipes[1]),
        lin::EINVAL,
    );

    // The other half of that family: a `SOCK_STREAM` socketpair end is a
    // duplex `File` here rather than the `CharDev` the datagram pair reports,
    // so this is the check that reaches the handler's `as_duplex` arm.
    let mut stream: [i32; 2] = [-1; 2];
    lin::check_eq(
        &mut failed,
        b"the stream socketpair is made",
        lin::socketpair(lin::AF_UNIX, lin::SOCK_STREAM, 0, &mut stream),
        0,
    );
    lin::check_eq(
        &mut failed,
        b"fsync on a stream socketpair end answers EINVAL",
        lin::fsync(stream[0]),
        lin::EINVAL,
    );

    lin::check_eq(
        &mut failed,
        b"fsync on a closed descriptor answers EBADF",
        lin::fsync(99),
        lin::EBADF,
    );
    lin::check_eq(
        &mut failed,
        b"fdatasync on a closed descriptor answers EBADF",
        lin::fdatasync(99),
        lin::EBADF,
    );
    lin::check_eq(
        &mut failed,
        b"syncfs on a closed descriptor answers EBADF",
        lin::syncfs(99),
        lin::EBADF,
    );

    // sync(2) has no argument to read and no handler to run: the number is
    // answered 0 directly, the whole surface being "all writes reached
    // their storage".
    lin::check_eq(
        &mut failed,
        b"sync takes no argument and answers 0",
        lin::raw_sys(lin::nr::SYNC, 0, 0, 0),
        0,
    );

    let _ = lin::close(pipes[0]);
    let _ = lin::close(pipes[1]);
    let _ = lin::close(stream[0]);
    let _ = lin::close(stream[1]);
    let _ = lin::close(sv[0]);
    let _ = lin::close(sv[1]);
    let _ = lin::close(fd);
    let _ = lin::unlinkat(lin::AT_FDCWD, PATH, 0);
    lin::done(failed)
}

#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    lin::die(b"t-sync: panicked\n")
}
