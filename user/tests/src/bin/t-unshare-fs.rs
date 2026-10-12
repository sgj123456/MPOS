//! t-unshare-fs — the process's filesystem context: shared by `CLONE_FS`,
//! copied by a `fork`.
//!
//! Linux keeps a process's root, cwd and creation mask in one `fs_struct`, and
//! the promise a thread creator makes is that it *shares* that object: a thread
//! that `chdir`s moves its whole process, and `unshare(CLONE_FS)` is the call
//! that says "not for me". This kernel kept the four facts as per-task fields,
//! so the sharing direction was the opposite of Linux's — a thread's `chdir`
//! moved only the thread — and this file is where the difference is measured and
//! pinned. Measured on a current host (7.2.9), thread and process in the same
//! program: after a thread `chdir`s to `/tmp` the creating thread's own `getcwd`
//! answers `/tmp`; after the same thread calls `unshare(CLONE_FS)` first, it
//! stays where it was. `.workbuddy/probe_unshare_flags.c` is the probe.
//!
//! The two claims below are one fact seen from both ends, which is why they are
//! one test and not two: a *shared* handle (the thread's `chdir` moves the
//! process) and a *copied* one (a forked child's does not) are the two halves of
//! "the object is shared or copied, never approximated", and a kernel that got
//! either direction wrong would pass the other claim. Each reading is taken
//! while the thread is still alive, so it is a reading of the sharing and not of
//! a task that had already gone.

use mpos_user_tests::Report;

fn main() {
    println!("t-unshare-fs");
    let mut r = Report::new();

    let start = std::env::current_dir().expect("the run has a cwd");
    let dir = "/tmp/t-unshare-fs-dir";
    std::fs::create_dir_all(dir).expect("a directory to move into");

    // ---- A thread's chdir moves its process ----
    //
    // The thread reports that it has moved and then *waits*, so the reading
    // below is taken while it is still alive and cannot be a reading of a thread
    // that had already gone.
    let (said, heard) = std::sync::mpsc::channel::<()>();
    let (release, held) = std::sync::mpsc::channel::<()>();
    let thread = std::thread::spawn(move || {
        std::env::set_current_dir(dir).expect("chdir from the thread");
        let _ = said.send(());
        let _ = held.recv();
    });
    heard.recv().expect("the thread's handshake");
    let after = std::env::current_dir().expect("a cwd");
    r.check(
        "a thread's chdir moves its process (CLONE_FS shares the fs)",
        after == std::path::Path::new(dir),
    );
    std::env::set_current_dir(&start).expect("back to the start");
    let _ = release.send(());
    let _ = thread.join();

    // ---- A forked child's chdir does not ----
    //
    // The counterfactual of the claim above: the handle is *copied* wherever the
    // caller did not ask to share it, so what moved the process above was
    // sharing and not a cwd that every task happens to write to.
    let pid = unsafe { libc::fork() };
    if pid == 0 {
        let _ = std::env::set_current_dir(dir);
        unsafe { libc::_exit(0) };
    }
    r.check("the child is forked", pid > 0);
    let mut status = 0;
    let reaped = unsafe { libc::waitpid(pid, &mut status, 0) };
    r.check("and reaped", reaped == pid);
    let still = std::env::current_dir().expect("a cwd");
    r.check(
        "a forked child's chdir does not move its parent (the fs is copied)",
        still == start,
    );

    let _ = std::fs::remove_dir(dir);
    r.finish()
}
