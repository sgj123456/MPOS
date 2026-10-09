//! `M8`: the IPC family - pipes, the TTY line discipline, signal delivery.

use mpos::println;
use mpos::sched::{self, Priority};
use mpos::syscall::{err, linux::nr as lnr};
use mpos::vfs;

use super::vfs::syscall;
use mpos::sched::kunit::{drain, wait_for_code};

// ---- ipc: pipes, TTY line discipline, signals ---------------------------

/// Prove pipes, TTY line discipline, and signal delivery.
pub(crate) fn run() {
    use mpos::sched::signal::{SIGKILL, SIGTERM};

    // --- pipe: bytes cross tasks, EOF after writer leaves.
    let mut fds = [0i32; 2];
    assert_eq!(
        unsafe { syscall(lnr::PIPE, fds.as_mut_ptr() as usize, 0, 0, 0, 0, 0) },
        0,
        "pipe() failed"
    );
    let (rfd, wfd) = (fds[0], fds[1]);
    assert_ne!(rfd, wfd, "pipe() handed out the same fd twice");

    sched::spawn(
        move || {
            sched::sleep_ticks(2);
            let msg = b"ping through a pipe\n";
            let wrote = unsafe {
                syscall(
                    lnr::WRITE,
                    wfd as usize,
                    msg.as_ptr() as usize,
                    msg.len(),
                    0,
                    0,
                    0,
                )
            };
            assert_eq!(wrote as usize, msg.len(), "the pipe dropped bytes");
            vfs::close(wfd).unwrap();
        },
        Priority::NORMAL,
    );

    let mut buf = [0u8; 64];
    let got = unsafe {
        syscall(
            lnr::READ,
            rfd as usize,
            buf.as_mut_ptr() as usize,
            buf.len(),
            0,
            0,
            0,
        )
    };
    assert_eq!(got as usize, b"ping through a pipe\n".len());
    assert_eq!(&buf[..got as usize], b"ping through a pipe\n");
    let got = vfs::read(rfd, &mut buf).unwrap();
    assert_eq!(got, 0, "a pipe with no writers reads EOF, not an error");
    vfs::close(rfd).unwrap();
    drain();
    println!("pipe: bytes crossed between two tasks, EOF after the writer left");

    // --- pipe: write with no reader is EPIPE.
    let mut fds = [0i32; 2];
    assert_eq!(
        unsafe { syscall(lnr::PIPE, fds.as_mut_ptr() as usize, 0, 0, 0, 0, 0) },
        0
    );
    let (rfd, wfd) = (fds[0], fds[1]);
    sched::spawn(
        move || {
            vfs::close(rfd).unwrap();
        },
        Priority::NORMAL,
    );
    drain();
    let rc = unsafe { syscall(lnr::WRITE, wfd as usize, b"x".as_ptr() as usize, 1, 0, 0, 0) };
    assert_eq!(rc, err::EPIPE, "a write to a readerless pipe must be EPIPE");
    vfs::close(wfd).unwrap();
    println!("pipe: write with no reader answered EPIPE and nobody died");

    // --- tty: editing, echo, canonical reads, Ctrl-D.
    let tty = vfs::tty::global();
    for &b in b"hellx\x7fo\n" {
        tty.receive_byte(b);
    }
    let mut line = [0u8; 32];
    // Every one of these reads is fed before it is asked, so it never blocks -
    // and a boot task with no signal dispositions cannot be interrupted either.
    // The `expect` is the honest form of "this answer cannot be an error here".
    let n = tty.read(&mut line).expect("a fed tty read answers a count");
    assert_eq!(&line[..n], b"hello\n", "backspace did not edit the line");

    for &b in b"gate\n\x04" {
        tty.receive_byte(b);
    }
    let n = tty.read(&mut line).expect("a fed tty read answers a count");
    assert_eq!(&line[..n], b"gate\n");
    let n = tty.read(&mut line).expect("a fed tty read answers a count");
    assert_eq!(n, 0, "Ctrl-D on an empty line is EOF");

    // Blocking fd 0 read through the gate.
    sched::spawn(
        || {
            sched::sleep_ticks(2);
            for &b in b"slow\n" {
                vfs::tty::global().receive_byte(b);
            }
        },
        Priority::NORMAL,
    );
    let n = unsafe {
        syscall(
            lnr::READ,
            0,
            line.as_mut_ptr() as usize,
            line.len(),
            0,
            0,
            0,
        )
    };
    assert_eq!(n as usize, b"slow\n".len());
    assert_eq!(&line[..n as usize], b"slow\n");
    drain();
    println!("tty: line editing, echo, Ctrl-D, and a blocking fd 0 read");

    // --- signals: SIGTERM, SIGKILL, self-sent, kill(0) probe.
    let paused = sched::spawn(|| sched::pause(), Priority::NORMAL);
    sched::sleep_ticks(2);
    assert!(sched::send_signal(paused, SIGTERM));
    wait_for_code(paused);
    assert_eq!(
        sched::exit_code(paused),
        Some(-(SIGTERM as isize)),
        "SIGTERM did not end the paused task"
    );
    drain();

    let mut fds = [0i32; 2];
    assert_eq!(
        unsafe { syscall(lnr::PIPE, fds.as_mut_ptr() as usize, 0, 0, 0, 0, 0) },
        0
    );
    let (rfd, wfd) = (fds[0], fds[1]);
    let blocked = sched::spawn(
        move || {
            let mut buf = [0u8; 8];
            let n = vfs::read(rfd, &mut buf).unwrap();
            unreachable!("a read with no writer and no signal returned {n}");
        },
        Priority::NORMAL,
    );
    sched::sleep_ticks(2);
    assert!(sched::send_signal(blocked, SIGKILL));
    wait_for_code(blocked);
    assert_eq!(
        sched::exit_code(blocked),
        Some(-(SIGKILL as isize)),
        "SIGKILL did not end the blocked reader"
    );
    drain();
    vfs::close(wfd).unwrap();

    let selfsent = sched::spawn(
        || {
            sched::send_signal(sched::current(), SIGTERM);
            sched::pause();
        },
        Priority::NORMAL,
    );
    wait_for_code(selfsent);
    assert_eq!(sched::exit_code(selfsent), Some(-(SIGTERM as isize)));
    drain();

    assert_eq!(
        unsafe { syscall(lnr::KILL, sched::current().0, 0, 0, 0, 0, 0) },
        0
    );
    assert_eq!(
        unsafe { syscall(lnr::KILL, 9999, SIGTERM as usize, 0, 0, 0, 0) },
        err::ESRCH
    );
    // The signal-refusal boundary. Stage B opened the real-time range, so
    // `SIGRTMIN` (32) is now a valid `kill` target (as on Linux); the number
    // that must be refused is the one past `_NSIG` (64), not 32.
    assert_eq!(
        unsafe { syscall(lnr::KILL, 0, 65, 0, 0, 0, 0) },
        err::EINVAL
    );
    println!("signals: SIGTERM, SIGKILL, and the kill(0) probe behave");

    // NB: not M8_OK here. The M8 marker belongs to the init handoff,
    // which is the real "the machine is up" event. This check's marker is
    // its own, so a CI harness that asserts on M8_OK is asserting on
    // init, not on IPC.
    println!("M8_IPC_OK");
}
