//! t-thread-sighand - the handler table is one per **process**, not one per
//! thread, and a fork is a copy rather than a share.
//!
//! Linux keeps one `sighand_struct` per thread group (`CLONE_SIGHAND`, implied
//! by `CLONE_THREAD`), so a handler installed by any thread is the handler the
//! leader reads back. Measured on a current host first
//! (`.workbuddy/probe_thread_sighand.c`, Linux 7.2.11): a `pthread` that
//! installs `SIGUSR1` leaves the main thread reading that same address, a
//! disposition the leader sets is what a later thread reads, and a `fork` child
//! holds its own copy - a change the parent makes after the fork does not move
//! it. This kernel used to copy the table on the thread path, so the two
//! threads could disagree about who catches a signal.
//!
//! `clone(CLONE_SIGHAND)` without `CLONE_THREAD` shares the same object too,
//! because the promise is the flag and not "this task is a thread" - that is
//! the branch `src/sched/runtime/child_exit.rs`'s fork path tests, and it is
//! named rather than pinned here: a raw child needs a per-machine `clone`
//! argument order, which is the one thing this std-side program cannot spell
//! without an architecture split of its own.

use mpos_user_tests::Report;
use std::sync::atomic::{AtomicUsize, Ordering};

static FIRED: AtomicUsize = AtomicUsize::new(0);

extern "C" fn handler(_: libc::c_int) {
    FIRED.fetch_add(1, Ordering::SeqCst);
}

fn handler_addr() -> libc::sighandler_t {
    handler as *const () as libc::sighandler_t
}

/// What the caller's table says about `sig`, read through the wrapper every
/// program uses.
fn read_action(sig: libc::c_int) -> libc::sighandler_t {
    let mut sa: libc::sigaction = unsafe { core::mem::zeroed() };
    // SAFETY: the null pointer is "do not install", and `sa` is this frame's.
    unsafe {
        libc::sigaction(sig, core::ptr::null(), &mut sa);
    }
    sa.sa_sigaction
}

fn write_byte(fd: libc::c_int, byte: u8) {
    // SAFETY: `fd` is a pipe end this test opened, and the buffer is one byte.
    unsafe {
        libc::write(fd, &byte as *const u8 as *const libc::c_void, 1);
    }
}

fn read_byte(fd: libc::c_int) -> u8 {
    let mut byte = 0u8;
    // SAFETY: as above; a short read leaves the byte as it was, and the
    // caller treats anything but 1 as a failed handshake.
    unsafe {
        libc::read(fd, &mut byte as *mut u8 as *mut libc::c_void, 1);
    }
    byte
}

fn main() {
    println!("t-thread-sighand");
    let mut r = Report::new();

    // ---- a thread installs, the leader reads the same table -------------
    std::thread::spawn(|| {
        // SAFETY: `handler_addr` is the one shape `signal` takes.
        unsafe {
            libc::signal(libc::SIGUSR1, handler_addr());
        }
    })
    .join()
    .unwrap();
    r.check(
        "a handler installed by a thread is the leader's handler",
        read_action(libc::SIGUSR1) == handler_addr(),
    );

    // ---- the leader changes the table under a live thread --------------
    // The thread must be *already running* when the leader writes: a copy made
    // at spawn already held whatever the leader had, so only a change after the
    // fact can tell a shared table from equal copies.
    // SAFETY: `SIG_DFL` is the first sentinel `signal` accepts.
    unsafe {
        libc::signal(libc::SIGUSR2, libc::SIG_DFL);
    }
    let (go_tx, go_rx) = std::sync::mpsc::channel::<()>();
    let (back_tx, back_rx) = std::sync::mpsc::channel::<libc::sighandler_t>();
    let worker = {
        let back = back_tx.clone();
        std::thread::spawn(move || {
            back.send(read_action(libc::SIGUSR2)).unwrap();
            go_rx.recv().unwrap();
            back.send(read_action(libc::SIGUSR2)).unwrap();
        })
    };
    let before = back_rx.recv().unwrap();
    // SAFETY: `SIG_IGN` is the second sentinel `signal` accepts.
    unsafe {
        libc::signal(libc::SIGUSR2, libc::SIG_IGN);
    }
    go_tx.send(()).unwrap();
    let after = back_rx.recv().unwrap();
    worker.join().unwrap();
    r.check(
        "the live thread read SIG_DFL before the change",
        before == libc::SIG_DFL,
    );
    r.check(
        "a disposition the leader set is what the live thread reads",
        after == libc::SIG_IGN,
    );

    // ---- one delivery runs the shared handler once ---------------------
    // Installed here as well as by the thread above: this claim is about the
    // delivery arithmetic, and it must not be *killed* by the state a
    // counterfactual leaves the leader in.
    // SAFETY: `handler_addr` is the one shape `signal` takes.
    unsafe {
        libc::signal(libc::SIGUSR1, handler_addr());
    }
    FIRED.store(0, Ordering::SeqCst);
    // SAFETY: the set is this frame's, and `kill` names this process.
    unsafe {
        let mut set: libc::sigset_t = core::mem::zeroed();
        libc::sigemptyset(&mut set);
        libc::sigaddset(&mut set, libc::SIGUSR1);
        libc::sigprocmask(libc::SIG_UNBLOCK, &set, core::ptr::null_mut());
        libc::kill(libc::getpid(), libc::SIGUSR1);
    }
    r.check(
        "one signal runs the shared handler exactly once",
        FIRED.load(Ordering::SeqCst) == 1,
    );

    // ---- a fork child owns its copy ------------------------------------
    // The parent's `SIGUSR2` is `SIG_IGN` at the fork. The child reads it,
    // announces, waits for the parent to change its own table to `SIG_DFL`,
    // and reads again: a shared table would show the change.
    let mut first = [0 as libc::c_int; 2];
    let mut second = [0 as libc::c_int; 2];
    // SAFETY: both arrays hold two descriptors this frame owns.
    unsafe {
        libc::pipe(first.as_mut_ptr());
        libc::pipe(second.as_mut_ptr());
    }
    // SAFETY: `fork` has no arguments; the child's calls below are the plain
    // libc ones, which are safe to make in a forked child.
    let child = unsafe { libc::fork() };
    assert!(child >= 0, "fork failed");
    if child == 0 {
        // SAFETY: the child closes the ends it does not use and writes its two
        // readings down the pipes as booleans.
        unsafe {
            libc::close(first[0]);
            libc::close(second[1]);
            let before = read_action(libc::SIGUSR2) == libc::SIG_IGN;
            write_byte(first[1], before as u8);
            let go = read_byte(second[0]);
            let after = read_action(libc::SIGUSR2) == libc::SIG_IGN;
            write_byte(first[1], (go == 1 && after) as u8);
            libc::_exit(0);
        }
    }
    // SAFETY: the parent keeps the other ends.
    unsafe {
        libc::close(first[1]);
        libc::close(second[0]);
    }
    let before = read_byte(first[0]);
    // The parent changes **its own** table after the fork. A copy means the
    // child still reads `SIG_IGN`.
    // SAFETY: `SIG_DFL` is the first sentinel `signal` accepts.
    unsafe {
        libc::signal(libc::SIGUSR2, libc::SIG_DFL);
    }
    write_byte(second[1], 1);
    let after = read_byte(first[0]);
    let mut status = 0;
    // SAFETY: `child` is this frame's own child, and `status` is its word.
    unsafe {
        libc::waitpid(child, &mut status, 0);
    }
    r.check(
        "a fork child inherits the parent's dispositions",
        before == 1,
    );
    r.check(
        "the child's table does not follow a later change to the parent's",
        after == 1,
    );
    r.check(
        "the parent's own table did change",
        read_action(libc::SIGUSR2) == libc::SIG_DFL,
    );

    r.finish();
}
