//! t-unshare-fs — the process's filesystem context and descriptor table, shared
//! by `CLONE_FS`/`CLONE_FILES` and split by `unshare`.
//!
//! Linux keeps a process's root, cwd and creation mask in one `fs_struct`, and
//! the promise a thread creator makes is that it *shares* that object: a thread
//! that `chdir`s moves its whole process, and `unshare(CLONE_FS)` is the call
//! that says "not for me". This kernel kept the four facts as per-task fields,
//! so the sharing direction was the opposite of Linux's — a thread's `chdir`
//! moved only the thread. The descriptor table was already one object, but
//! `unshare(CLONE_FILES)` could not split it, so a caller was told `0` for a
//! promise nothing kept.
//!
//! Measured on a current host (7.2.9), thread and process in the same program,
//! one child per flag (`.workbuddy/probe_unshare_flags.c`):
//!
//! * after a thread `chdir`s to `/tmp` the creating thread's own `getcwd`
//!   answers `/tmp`; after the same thread calls `unshare(CLONE_FS)` first, it
//!   stays where it was;
//! * a thread that `unshare`s `CLONE_FILES` and then `close`s a descriptor
//!   leaves the sibling's copy open, where without the unshare the close reaches
//!   the sibling;
//! * `unshare(CLONE_VM)`, `CLONE_SIGHAND` and `CLONE_THREAD` answer `0` for a
//!   single-threaded caller and `EINVAL` the moment a sibling exists — the three
//!   whose resource really is shared and cannot be split off the caller alone.
//!
//! Every claim below is a pair of readings, because each of these is one fact
//! seen from both ends and a kernel that got either direction wrong would pass
//! the other: shared against split for the two objects, and refused against
//! served for the three flags. The readings are taken with the sibling still
//! alive, so they are readings of the sharing and not of a task that had gone.

use mpos_user_tests::Report;

/// The raw door, so the errno is the answer rather than a `-1` (musl's protocol,
/// the shape the other namespace tests carry).
fn unshare(flags: i32) -> isize {
    let r = unsafe { libc::syscall(libc::SYS_unshare, flags as libc::c_long) };
    if r < 0 {
        -(std::io::Error::last_os_error().raw_os_error().unwrap_or(0) as isize)
    } else {
        r as isize
    }
}

/// Run `body` in a forked child and read its verdict from the exit status. The
/// child is the isolation: three of the claims below move a process's cwd or
/// split its descriptor table, and neither may leak into the rest of the run.
fn in_child(body: impl FnOnce() -> bool) -> bool {
    let pid = unsafe { libc::fork() };
    if pid == 0 {
        let ok = body();
        unsafe { libc::_exit(if ok { 0 } else { 1 }) };
    }
    let mut status = 0;
    let reaped = unsafe { libc::waitpid(pid, &mut status, 0) };
    reaped == pid && libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0
}

/// A thread that (optionally) unshares the fs context, then `chdir`s; the
/// process must follow only when it did not unshare. Answers "did the process
/// follow?".
fn thread_chdir_moves_the_process(unshare_first: bool) -> bool {
    std::env::set_current_dir("/").expect("a cwd to start from");
    let (said, heard) = std::sync::mpsc::channel::<()>();
    let (release, held) = std::sync::mpsc::channel::<()>();
    let thread = std::thread::spawn(move || {
        if unshare_first {
            unshare(libc::CLONE_FS);
        }
        std::env::set_current_dir("/tmp").expect("chdir from the thread");
        let _ = said.send(());
        let _ = held.recv();
    });
    heard.recv().expect("the thread's handshake");
    let followed = std::env::current_dir()
        .map(|p| p == std::path::Path::new("/tmp"))
        .unwrap_or(false);
    let _ = release.send(());
    let _ = thread.join();
    followed
}

/// A thread that (optionally) unshares the descriptor table, then waits while
/// the *caller* closes a descriptor. Answers "does the sibling still have it?" -
/// which is `true` only when the unshare split the table.
fn sibling_keeps_a_closed_descriptor(unshare_first: bool) -> bool {
    let file = std::fs::File::open("/dev/null").expect("a descriptor to watch");
    let fd = std::os::fd::AsRawFd::as_raw_fd(&file);
    let (ready_tx, ready_rx) = std::sync::mpsc::channel::<()>();
    let (go_tx, go_rx) = std::sync::mpsc::channel::<()>();
    let (answer_tx, answer_rx) = std::sync::mpsc::channel::<bool>();
    let thread = std::thread::spawn(move || {
        if unshare_first {
            unshare(libc::CLONE_FILES);
        }
        let _ = ready_tx.send(());
        let _ = go_rx.recv();
        // The sibling's own view of the number the caller has just closed.
        let still_open = unsafe { libc::fcntl(fd, libc::F_GETFD) } >= 0;
        let _ = answer_tx.send(still_open);
    });
    ready_rx.recv().expect("the thread is ready");
    drop(file);
    let _ = go_tx.send(());
    let answer = answer_rx.recv().expect("the thread's answer");
    let _ = thread.join();
    answer
}

/// What `unshare` answers for the three flags a sibling makes impossible, asked
/// from a thread so the group has one.
fn unsplittable_flags_answers() -> [isize; 3] {
    let (answer_tx, answer_rx) = std::sync::mpsc::channel::<[isize; 3]>();
    let thread = std::thread::spawn(move || {
        let _ = answer_tx.send([
            unshare(libc::CLONE_VM),
            unshare(libc::CLONE_SIGHAND),
            unshare(libc::CLONE_THREAD),
        ]);
    });
    let answers = answer_rx.recv().expect("the thread's answers");
    let _ = thread.join();
    answers
}

fn main() {
    println!("t-unshare-fs");
    let mut r = Report::new();

    let start = std::env::current_dir().expect("the run has a cwd");
    let dir = "/tmp/t-unshare-fs-dir";
    std::fs::create_dir_all(dir).expect("a directory to move into");

    // ---- The default: a thread's chdir moves its process ----
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

    // ---- The counterfactual: a forked child's chdir does not ----
    //
    // The handle is *copied* wherever the caller did not ask to share it, so
    // what moved the process above was sharing and not a cwd that every task
    // happens to write to.
    let pid = unsafe { libc::fork() };
    if pid == 0 {
        let _ = std::env::set_current_dir(dir);
        unsafe { libc::_exit(0) };
    }
    r.check("the child is forked", pid > 0);
    let mut status = 0;
    let reaped = unsafe { libc::waitpid(pid, &mut status, 0) };
    r.check("and reaped", reaped == pid);
    r.check(
        "a forked child's chdir does not move its parent (the fs is copied)",
        std::env::current_dir().expect("a cwd") == start,
    );

    // ---- `unshare(CLONE_FS)`: the same thread, now on its own ----
    //
    // The complement of the first claim, and the whole reason the call exists:
    // after it, the process must *not* follow. Both readings are needed - a
    // kernel that never shared would pass this one alone.
    r.check(
        "a thread that unshared CLONE_FS chdirs only itself",
        in_child(|| !thread_chdir_moves_the_process(true)),
    );

    // ---- The descriptor table: shared by default, split by the call ----
    r.check(
        "a thread's close reaches its process (CLONE_FILES shares the table)",
        in_child(|| !sibling_keeps_a_closed_descriptor(false)),
    );
    r.check(
        "a thread that unshared CLONE_FILES closes only its own number",
        in_child(|| sibling_keeps_a_closed_descriptor(true)),
    );

    // ---- The three flags a sibling makes impossible ----
    //
    // `EINVAL` is Linux's answer and its reason: the address space or the
    // handler table really is shared with the sibling, so the request cannot be
    // honoured - and answering `0`, which this kernel used to, was a caller told
    // its memory was private when it was not.
    r.check(
        "CLONE_VM/CLONE_SIGHAND/CLONE_THREAD are EINVAL from a thread",
        unsplittable_flags_answers() == [-22, -22, -22],
    );
    // ...and served for a caller with no sibling to split from, which is what
    // makes the refusal above about the sibling rather than about the flag.
    //
    // Taken *after* this program has made and joined three threads, on purpose:
    // a thread's slot is not given back at once, and a tgid is a slot number, so
    // a caller forked into a slot a dead thread once occupied can be handed that
    // thread's number as its own group id and refused on a machine where it is
    // the only thread. That is what the first run of this claim measured - the
    // virgin process answered `0` and the forked child answered `EINVAL` - and
    // the rule it moved was the kernel's walk, not this claim.
    let single = in_child(|| {
        [
            unshare(libc::CLONE_VM),
            unshare(libc::CLONE_SIGHAND),
            unshare(libc::CLONE_THREAD),
        ] == [0, 0, 0]
    });
    r.check("and 0 for a caller with no sibling to split from", single);

    let _ = std::fs::remove_dir(dir);
    r.finish()
}
