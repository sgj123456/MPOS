//! t-inotify-wake - a change wakes a *parked* watcher, in this process or
//! another one.
//!
//! `t-inotify` pins the records - what the queue says, in what order, with
//! which wd and name. This program asks the question a boot's `udevadm settle`
//! actually lives by (`docs/notes/` §7.9e): a watcher that has parked in
//! `poll(2)`/`epoll_wait(2)` on an inotify descriptor must be woken when
//! another task changes the watched file. Reading the queue is not the same
//! claim: a queue that fills without the waiter being woken is a process
//! asleep on a change it cannot see, and a desktop boot spends its first two
//! minutes in exactly that shape - `udevd`, the queue file under
//! `/run/udev/queue`, and `udevadm settle` waiting for an `inotify` event that
//! never arrived.
//!
//! The changer runs in a child process, not a thread, because the boot's
//! writer and watcher *are* different processes: a wake that works for a
//! thread in this process says nothing about the cross-process case, and the
//! cross-process case is the one measured broken on the image (`data files: 3`
//! before and after a trigger, `settle` timing out at 3 s).
//!
//! The fixture lives under `/tmp` (both payloads back it with a tmpfs), is
//! named uniquely per run, and every wait carries a budget: a claim that hangs
//! takes the sweep with it.

use std::io::Write;

use mpos_user_tests::Report;

const IN_MODIFY: u32 = 0x0000_0002;
/// Park with no timeout: the claim is the *wake*, not readiness.
///
/// A `poll(2)` with a timeout re-asks the descriptor every tick, so it answers
/// "the queue has a record" even when nobody woke the waiter - measured: with
/// `deliver`'s wake loop deleted the timed version still passed. Only a park
/// with `-1` puts the whole claim on the wake, and a regression that queues
/// without waking then leaves the watcher asleep - where the sweep's
/// per-program budget kills it as a failure, which is the honest red.
const PARK_FOREVER: i32 = -1;

fn fixture(tag: &str) -> String {
    let path = format!("/tmp/t-inotify-wake-{}-{tag}", std::process::id());
    let mut f = std::fs::File::create(&path).expect("the fixture file");
    f.write_all(b"start\n").expect("the fixture's first bytes");
    path
}

/// `inotify_init1(0)`: a blocking descriptor, the shape `udevadm settle` opens.
fn watcher() -> libc::c_int {
    unsafe { libc::inotify_init1(0) }
}

fn watch(fd: libc::c_int, path: &str) -> libc::c_int {
    let c = std::ffi::CString::new(path).expect("no interior NUL");
    unsafe { libc::inotify_add_watch(fd, c.as_ptr(), IN_MODIFY) }
}

/// Append a byte to the file - one `IN_MODIFY`, with the descriptor closed so a
/// `CLOSE_WRITE` is not also queued.
fn change(path: &str) {
    if let Ok(mut f) = std::fs::OpenOptions::new().append(true).open(path) {
        let _ = f.write_all(b"x");
    }
}

/// Sleep, then change the file: the child's half of every claim here.
fn change_after(path: String, ms: u64) {
    let nap = libc::timespec {
        tv_sec: 0,
        tv_nsec: (ms * 1_000_000) as libc::c_long,
    };
    let mut rem = nap;
    unsafe {
        libc::nanosleep(&nap, &mut rem);
    }
    change(&path);
}

/// One event record from an inotify descriptor: `(wd, mask, name)`.
fn read_record(fd: libc::c_int) -> Option<(i32, u32, String)> {
    // Only read what the descriptor says is there: a regression that queues a
    // record without waking the watcher must fail this claim, not hang the
    // sweep on a blocking `read(2)` (measured: the counterfactual did exactly
    // that, and the lane paid a 120 s budget for it).
    let mut pfd = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    if unsafe { libc::poll(&mut pfd, 1, 0) } < 1 {
        return None;
    }
    let mut buf = [0u8; 4096];
    let got = unsafe { libc::read(fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
    if got < 16 {
        return None;
    }
    let wd = i32::from_ne_bytes(buf[0..4].try_into().ok()?);
    let mask = u32::from_ne_bytes(buf[4..8].try_into().ok()?);
    let len = u32::from_ne_bytes(buf[12..16].try_into().ok()?) as usize;
    let name = if len == 0 {
        String::new()
    } else {
        let mut raw = &buf[16..(16 + len).min(got as usize)];
        while let Some((last, rest)) = raw.split_last() {
            if *last == 0 {
                raw = rest;
            } else {
                break;
            }
        }
        String::from_utf8_lossy(raw).into_owned()
    };
    Some((wd, mask, name))
}

fn main() {
    println!("t-inotify-wake");
    let mut r = Report::new();

    // ---- one process parks, another changes the file -----------------------
    let path = fixture("poll");
    let fd = watcher();
    r.check("a blocking inotify descriptor opens", fd >= 0);
    if fd < 0 {
        r.finish();
    }
    let wd = watch(fd, &path);
    r.check("and a watch is placed on the file", wd >= 0);

    let changer = unsafe { libc::fork() };
    if changer == 0 {
        change_after(path.clone(), 150);
        unsafe { libc::_exit(0) };
    }
    r.check("the changing child started", changer > 0);
    let mut pfd = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    let ready = unsafe { libc::poll(&mut pfd, 1, PARK_FOREVER) };
    r.check(
        "poll(2) parks until another process changes the file, then reports it readable",
        ready == 1 && pfd.revents & libc::POLLIN != 0,
    );
    match read_record(fd) {
        Some((got_wd, mask, name)) => {
            r.check(
                "and the record read afterwards is that change",
                got_wd == wd && mask & IN_MODIFY != 0 && name.is_empty(),
            );
        }
        None => r.check("and the record read afterwards is that change", false),
    }
    unsafe { libc::waitpid(changer, std::ptr::null_mut(), 0) };

    // ---- the same, through epoll: the spelling the daemons use -------------
    let path = fixture("epoll");
    let fd = watcher();
    let ep = unsafe { libc::epoll_create1(0) };
    r.check("a watcher and an epoll set open", fd >= 0 && ep >= 0);
    if fd >= 0 && ep >= 0 {
        r.check("and the watch is placed", watch(fd, &path) >= 0);
        let mut ev = libc::epoll_event {
            events: libc::EPOLLIN as u32,
            u64: fd as u64,
        };
        r.check(
            "and the watcher joins the set for readability",
            unsafe { libc::epoll_ctl(ep, libc::EPOLL_CTL_ADD, fd, &mut ev) } == 0,
        );
        let changer = unsafe { libc::fork() };
        if changer == 0 {
            change_after(path.clone(), 150);
            unsafe { libc::_exit(0) };
        }
        r.check("the second changing child started", changer > 0);
        let mut out = [libc::epoll_event { events: 0, u64: 0 }; 4];
        let n = unsafe { libc::epoll_wait(ep, out.as_mut_ptr(), 4, PARK_FOREVER) };
        r.check(
            "epoll_wait(2) parks until the change and reports it readable",
            n >= 1 && out[0].events & libc::EPOLLIN as u32 != 0,
        );
        unsafe { libc::waitpid(changer, std::ptr::null_mut(), 0) };
    }

    // ---- and the watcher itself is the other process -----------------------
    //
    // The boot's arrangement with the sides swapped: `udevadm settle` is the
    // process that parks, and the writer is whoever touches the queue file.
    // The child reports through its exit status, so the claim is that it was
    // *woken*, not that its queue eventually held a record.
    let path = fixture("child");
    let mut ready_pipe = [0 as libc::c_int; 2];
    r.check(
        "a pipe carries the watching child's readiness",
        unsafe { libc::pipe(ready_pipe.as_mut_ptr()) } == 0,
    );
    let waker = unsafe { libc::fork() };
    if waker == 0 {
        unsafe { libc::close(ready_pipe[0]) };
        let fd = watcher();
        let watched = fd >= 0 && watch(fd, &path) >= 0;
        let word = [if watched { 1u8 } else { 0u8 }];
        unsafe {
            libc::write(ready_pipe[1], word.as_ptr() as *const libc::c_void, 1);
        }
        if !watched {
            unsafe { libc::_exit(2) };
        }
        let mut pfd = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        let ready = unsafe { libc::poll(&mut pfd, 1, PARK_FOREVER) };
        let woke = ready == 1 && pfd.revents & libc::POLLIN != 0;
        unsafe { libc::_exit(if woke { 0 } else { 1 }) };
    }
    unsafe { libc::close(ready_pipe[1]) };
    let mut word = [0u8; 1];
    let read_back = unsafe { libc::read(ready_pipe[0], word.as_mut_ptr() as *mut libc::c_void, 1) };
    r.check(
        "the watching child is watching before the change",
        read_back == 1 && word[0] == 1,
    );
    change(&path);
    let mut status = 0 as libc::c_int;
    unsafe { libc::waitpid(waker, &mut status, 0) };
    r.check(
        "and it is woken by this process's change",
        libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0,
    );

    r.finish();
}
