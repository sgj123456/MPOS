//! t-leader-outlived - a group's threads keep running after their leader is gone.
//!
//! The shape a compositor's death makes: its main thread ends (a signal, a bare
//! `exit`), the shell beside it collects the pid, and the worker threads are
//! still running. They are running on the address space their **leader** owns -
//! a thread has no space of its own - so every question a worker asks about its
//! own memory is answered by looking the leader's slot up. A kernel that frees
//! that slot when the leader's exit code is collected leaves its threads
//! pointing at nothing: the next one to fault, map or unmap asks for a task
//! that is not there, and panics with `there is no task <n>`.
//!
//! Measured on the machine this file was written against: a compositor that
//! aborted was collected by the shell (its prompt came back) and a moment later
//! its own threads took the kernel down with `there is no task 10`.
//!
//! The claims are the two halves of "the leader is gone but the group is not":
//!
//! * the threads are still running when the leader's death is reported - the
//!   counter they step in shared memory moves, which is the state the panic
//!   needs;
//! * **they go on running after it**, stepping the counter the whole time a
//!   kernel that freed the leader would have been looking the slot up.
//!
//! Linux keeps a thread group leader until the group's last thread is gone for
//! exactly this reason, and this is the pin for that here: the slot outlives the
//! exit code, and a pid stays taken while its threads are running.

use mpos_user_tests::Report;
use rustix::mm::{MapFlags, ProtFlags, mmap_anonymous};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

const PAGE: usize = 4096;
/// How long to give a condition before answering it as a failed claim.
const BOUND: Duration = Duration::from_secs(3);

/// The shared counter the child's threads step and the parent reads back.
struct Counter(*mut AtomicU32);
// The page is one object every thread of the group and the parent alike hold,
// and it outlives all of them, so a handle to it crosses a thread boundary as
// safely as a reference into a `static` would.
unsafe impl Send for Counter {}
unsafe impl Sync for Counter {}

impl Counter {
    fn step(&self) {
        // SAFETY: one page this process mapped, shared with its child and never
        // unmapped while either holds it.
        unsafe { &*self.0 }.fetch_add(1, Ordering::Relaxed);
    }
    fn read(&self) -> u32 {
        // SAFETY: as above.
        unsafe { &*self.0 }.load(Ordering::Relaxed)
    }
}

fn main() {
    println!("t-leader-outlived");
    let mut r = Report::new();

    // SAFETY: a fresh page-sized shared anonymous mapping.
    let page = unsafe {
        mmap_anonymous(
            std::ptr::null_mut(),
            PAGE,
            ProtFlags::READ | ProtFlags::WRITE,
            MapFlags::SHARED,
        )
    }
    .expect("a shared anonymous page is served");
    let counter = Counter(page.cast::<AtomicU32>());

    // SAFETY: the child's leader ends its own thread while two others run; the
    // parent collects the pid and reads the shared counter.
    let pid = unsafe { libc::fork() };
    if pid == 0 {
        let shared = counter.0 as usize;
        for _ in 0..2 {
            std::thread::spawn(move || {
                let counter = Counter(shared as *mut AtomicU32);
                loop {
                    counter.step();
                    std::thread::sleep(Duration::from_millis(10));
                }
            });
        }
        // Let the threads prove they are running before the leader leaves them,
        // so the parent's first read is of a counter that is moving.
        while counter.read() < 4 {
            std::thread::sleep(Duration::from_millis(5));
        }
        // The bare `exit_group`-less exit: this *thread* ends, which is what a
        // `pthread_exit` from a program's first thread makes and the shape every
        // libc's `exit` is not. Only the group's other shape - `exit_group` -
        // would take the threads with it, so the two below are left behind.
        unsafe { libc::syscall(libc::SYS_exit, 0) };
        unreachable!()
    }

    let deadline = Instant::now() + BOUND;
    while Instant::now() < deadline && counter.read() < 4 {
        std::thread::sleep(Duration::from_millis(5));
    }
    r.check(
        "the group's threads are running when the leader leaves them",
        counter.read() >= 4,
    );

    // The leader's death is collected, which is the moment a kernel that frees
    // the slot on collection frees it: this is what the shell beside a dying
    // compositor does, and what puts the prompt back on the wire.
    let mut status: libc::c_int = 0;
    // SAFETY: a child this process forked, and a status word it owns.
    let reaped = unsafe { libc::waitpid(pid, &mut status, 0) };
    r.check("and the leader's death is collected", reaped == pid);

    // The threads go on. Read twice with a pause between them, so a thread that
    // was mid-sleep when the leader left cannot look stopped.
    let first = counter.read();
    std::thread::sleep(Duration::from_millis(500));
    let second = counter.read();
    r.check(
        &format!(
            "and they go on running after it, on the space their leader owned ({first} then {second})"
        ),
        second > first,
    );

    r.finish()
}
