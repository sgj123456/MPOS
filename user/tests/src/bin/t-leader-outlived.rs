//! t-leader-outlived - a group's threads keep running after their leader is gone,
//! and the leader is collected only when the group ends.
//!
//! The shape a compositor's death makes when its main thread ends on the thread
//! door: the leader becomes a zombie, its worker threads keep running on the
//! address space their **leader** owns - a thread has no space of its own - and
//! every question a worker asks about its own memory is answered by looking the
//! leader's slot up. A kernel that frees that slot before the group's last
//! thread is gone leaves its threads pointing at nothing: the next one to fault,
//! map or unmap asks for a task that is not there, and panics with `there is no
//! task <n>`.
//!
//! Measured on the machine this file was written against: a compositor that
//! aborted was collected by the shell and a moment later its own threads took
//! the kernel down with `there is no task 10`.
//!
//! # What Linux does with the parent's wait, measured
//!
//! The first version of this program expected the shell's `waitpid` to answer
//! the leader while the workers still ran. A leader that leaves on the thread
//! door is a **zombie**, not a corpse: measured on a current host
//! (`.workbuddy/probe_exit_group_thread.c`, C/C2), `/proc/<leader>/stat` says
//! `Z`, `waitpid(leader, WNOHANG)` answers `0` (not `ECHILD`), and the parent
//! reaps it only when the group's last thread leaves - with that thread's code.
//! So the claims below are Linux's two halves: the threads run while the leader
//! is dead and unreapable, and the group's end is what the parent collects.
//!
//! The group's threads step a counter in a shared page the parent reads, which
//! is the live proof that the address space their leader owns is still there;
//! the parent then tells them to end the group through that same page.

use mpos_user_tests::Report;
use rustix::mm::{MapFlags, ProtFlags, mmap_anonymous};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

const PAGE: usize = 4096;
/// How long to give a condition before answering it as a failed claim.
const BOUND: Duration = Duration::from_secs(3);
/// The code the worker ends the group with, distinct from the leader's `0` so a
/// wait answered by the leader's own exit could not pass.
const GROUP_CODE: i32 = 7;

/// The shared words the child's threads step and the parent steers.
struct Shared {
    steps: AtomicU32,
    go: AtomicU32,
}

/// The shared page as the two processes hold it.
struct Handle(*mut Shared);

// The page is one object every thread of the group and the parent alike hold,
// and it outlives all of them, so a handle to it crosses a thread boundary as
// safely as a reference into a `static` would.
unsafe impl Send for Handle {}
unsafe impl Sync for Handle {}

impl Handle {
    fn step(&self) {
        // SAFETY: one page this process mapped, shared with its child and never
        // unmapped while either holds it.
        unsafe { &*self.0 }.steps.fetch_add(1, Ordering::Relaxed);
    }
    fn read(&self) -> u32 {
        // SAFETY: as above.
        unsafe { &*self.0 }.steps.load(Ordering::Relaxed)
    }
    fn go(&self) -> bool {
        // SAFETY: as above.
        unsafe { &*self.0 }.go.load(Ordering::Relaxed) != 0
    }
    fn release(&self) {
        // SAFETY: as above.
        unsafe { &*self.0 }.go.store(1, Ordering::Relaxed);
    }
}

fn main() {
    println!("t-leader-outlived");
    let mut r = Report::new();

    // SAFETY: a fresh page-sized shared anonymous mapping, zeroed by the kernel.
    let page = unsafe {
        mmap_anonymous(
            std::ptr::null_mut(),
            PAGE,
            ProtFlags::READ | ProtFlags::WRITE,
            MapFlags::SHARED,
        )
    }
    .expect("a shared anonymous page is served");
    let shared = Handle(page.cast::<Shared>());

    // SAFETY: the child's leader ends its own thread while two others run; the
    // parent watches, then tells the group to end, and collects the leader.
    let pid = unsafe { libc::fork() };
    if pid == 0 {
        let address = shared.0 as usize;
        for _ in 0..2 {
            std::thread::spawn(move || {
                let shared = Handle(address as *mut Shared);
                loop {
                    shared.step();
                    if shared.go() {
                        // The last thread out names the group's code, which is
                        // the code the parent's wait answers with.
                        unsafe { libc::syscall(libc::SYS_exit_group, GROUP_CODE as libc::c_long) };
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
            });
        }
        // Let the threads prove they are running before the leader leaves them,
        // so the parent's first read is of a counter that is moving.
        while shared.read() < 4 {
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
    while Instant::now() < deadline && shared.read() < 4 {
        std::thread::sleep(Duration::from_millis(5));
    }
    r.check(
        "the group's threads are running when the leader leaves them",
        shared.read() >= 4,
    );

    // The leader is a zombie now, and Linux does not let the parent take it
    // while the group still has a live task: `WNOHANG` answers 0, not `ECHILD`.
    let path = format!("/proc/{pid}/stat");
    let mut zombie = false;
    let deadline = Instant::now() + BOUND;
    while Instant::now() < deadline {
        let state = std::fs::read_to_string(&path).ok().and_then(|text| {
            let rest = &text[text.rfind(')')? + 1..];
            rest.split_whitespace().next()?.chars().next()
        });
        if state == Some('Z') {
            zombie = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    r.check("the leader is a zombie while its threads run", zombie);
    let mut status: libc::c_int = 0;
    // SAFETY: a child this process forked, and a status word it owns.
    let early = unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) };
    r.check(
        "and it is not reapable yet: WNOHANG answers 0, not ECHILD",
        early == 0,
    );

    // The threads go on. Read twice with a pause between them, so a thread that
    // was mid-sleep when the leader left cannot look stopped.
    let first = shared.read();
    std::thread::sleep(Duration::from_millis(500));
    let second = shared.read();
    r.check(
        &format!(
            "and they go on running after it, on the space their leader owned ({first} then {second})"
        ),
        second > first,
    );

    // The group's last thread ends it, and the parent's wait answers the
    // **leader** with that thread's code - Linux's `group_dead` rule.
    shared.release();
    // Polled rather than parked: a wrong kernel must fail this claim, not hang
    // the sweep, and `WNOHANG` is the same question about the same record.
    let mut reaped = 0;
    let deadline = Instant::now() + BOUND;
    while Instant::now() < deadline {
        // SAFETY: as above.
        reaped = unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) };
        if reaped != 0 {
            break;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    r.check("the group's end is collected as the leader", reaped == pid);
    r.check(
        "with the last thread's code",
        libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == GROUP_CODE,
    );

    r.finish()
}
