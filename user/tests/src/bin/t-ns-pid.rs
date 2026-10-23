//! t-ns-pid - the numbers a process *sees*, which are a namespace's and not the
//! machine's.
//!
//! `getpid(2)`, `getppid(2)`, `kill(2)`'s target and the name `/proc/<n>`
//! resolves to are all one question - *which task is this number* - and a pid
//! namespace makes the answer depend on who is asking. §6.3's ladder puts it
//! third (UTS, MNT, PID, NET) and this program holds the pair the plan fixed
//! for it: **the child's view differs and the parent's does not**, which is the
//! assertion a "namespace" that is really a shared global fails:
//!
//! * the caller that unshares keeps the pid it had, and `/proc/<that number>`
//!   still names it - Linux's `unshare(CLONE_NEWPID)` renumbers the caller's
//!   *children*;
//! * its first child is pid 1 to itself: `getpid` answers 1, `/proc/1` is that
//!   child, and its `getppid` is 0 because the parent stayed outside the
//!   namespace and the namespace has no number for it;
//! * the child is still visible to the parent under a number of the *parent's*
//!   namespace - the one `fork` answered with - because a task is a member of
//!   every namespace from the root down to its own;
//! * and the number that reaches nothing is the one that matters for safety:
//!   the parent's own pid, sent into the namespace, is `ESRCH` there rather
//!   than the machine's slot of that number. Without that half, a program whose
//!   `getpid()` answered 1 and which then called `kill(getpid(), ...)` - which
//!   is what a program does with its own pid - would signal the machine's init;
//! * and **the task a namespace is spoken for by has rules of its own**: its own
//!   members cannot end it (a sibling's `kill(1, SIGKILL)` answers 0 and is
//!   discarded), a signal it has a **handler** for is delivered like any other,
//!   only an **ancestor** namespace's SIGKILL lands - and its death takes every
//!   task it speaks for, one in a namespace *nested* beneath it included;
//! * and the `NSpid` chain in `/proc/<n>/status` has **one number per namespace
//!   the task is a member of**: a task two levels in shows *three* - the
//!   machine's, the namespace between's, and the `1` its own namespace gives it -
//!   and every reader sees that same line, because it is the task's own chain
//!   rather than the reader's number. One level (two numbers) was all this
//!   rung's numbers were pinned at until now.
//! * and a **socket's `ucred.pid`** is a pid namespace's number as well: Linux
//!   renders `SO_PEERCRED`'s (and `SCM_CREDENTIALS`'s) pid with `pid_vnr` in the
//!   reader's *active* pid namespace, so a reader inside the namespace names a
//!   peer there by the number *it* gives it (`1`), and a peer in the ancestor
//!   namespace by `0`. The other two fields of the same `struct ucred` are the
//!   *user* namespace's question and are pinned by `t-ns-user`.
//!
//! # The doors
//!
//! `rustix::thread::unshare(UnshareFlags::NEWPID)` makes the namespace,
//! `rustix::process::{getpid, getppid}` read the numbers, and `libc::fork` makes
//! the child (`rustix` binds no `fork` on either machine - the reason
//! `t-fork`/`t-wait4` give). Three claims cannot come from a wrapper at all:
//!
//! * **`kill(pid, 0)`** - the "is this number there at all" probe, which is the
//!   whole of the safety claim - has no spelling in `rustix`: its `Signal` type
//!   has no zero, and zero *is* the request. So the raw `SYS_kill` door, with
//!   the errno read back the way `musl`'s `syscall(2)` leaves it.
//! * **`/proc/<n>/stat`'s first field** is read as *text*, because the claim is
//!   about the name a reader is given rather than about a struct.
//! * **the init's `SIGTERM` handler** is installed through `libc::sigaction`
//!   (`rustix`'s own is behind a feature this crate does not build), and it does
//!   the one thing a handler may: writes a byte to a pipe whose descriptor a
//!   `static` holds.
//!
//! The init section is forked **before** this program unshares, and that is a
//! measured rule rather than a choice: a process may make a pid namespace once
//! (a second `unshare(CLONE_NEWPID)` is `EINVAL`), and a `fork` into a namespace
//! whose init has exited is `ENOMEM`. This kernel refuses both, the pin claims
//! both below, and the section's own placement is the shape a program that
//! respects them has: it runs in a task whose namespace is still the machine's,
//! and raises the namespaces it needs itself.
//!
//! A host run makes no claim: `CLONE_NEWPID` needs `CAP_SYS_ADMIN`, which this
//! container's own capabilities do not hold (measured: `.workbuddy/probe_pidns.c`
//! gets `EPERM`, and `unshare --pid` fails the same way). That is a measurement
//! of *that* call and not of the host, though - `unshare -Ur` creates the
//! namespace fine, which is how every rule below was measured before it was
//! written down (`probe_pid1*.c`, `probe_fork_after_init.c`,
//! `probe_pid_ns_again.c`).

use mpos_user_tests::{Report, host_run};
use rustix::fd::{AsRawFd, OwnedFd};
use rustix::io::{Errno, read, write};
use rustix::pipe::pipe;
use rustix::process::{Pid, WaitOptions, getpid, getppid};
use rustix::thread::{UnshareFlags, unshare_unsafe};
use std::sync::atomic::{AtomicI32, Ordering};
use std::time::Duration;

/// Long enough for the one line the child sends: six decimal numbers.
const REPORT_MAX: usize = 128;

/// Leave without running the inherited atexit list: a forked `std` child that
/// flushed the parent's stdout would duplicate everything printed so far.
fn raw_exit(code: i32) -> ! {
    unsafe { libc::syscall(libc::SYS_exit, code as libc::c_long) };
    loop {
        core::hint::spin_loop();
    }
}

/// `kill(pid, 0)` through the raw door: `rustix`'s `Signal` cannot name signal
/// zero, and zero is the request. Answers the negative errno the sweep's other
/// raw claims are written against.
fn probe_number(pid: i32) -> isize {
    let rc = unsafe { libc::syscall(libc::SYS_kill, pid as libc::c_long, 0 as libc::c_long) };
    if rc == 0 {
        0
    } else {
        -(std::io::Error::last_os_error().raw_os_error().unwrap_or(0) as isize)
    }
}

/// The numbers on a `/proc/<n>/status` file's `NSpid:` line, in order.
///
/// Read as *text* for the same reason the `stat` field below is: the claim is
/// about the name a reader is handed, and this field is a list of them rather
/// than one number.
fn ns_pid_chain(path: &str) -> Vec<u32> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|text| {
            text.lines()
                .find(|line| line.starts_with("NSpid:"))
                .map(|line| {
                    line["NSpid:".len()..]
                        .split_whitespace()
                        .filter_map(|f| f.parse().ok())
                        .collect()
                })
        })
        .unwrap_or_default()
}

/// The first field of `/proc/<n>/stat`, which is the pid that file is about -
/// as text, because the claim is about the name a reader is handed.
fn stat_first_field(path: &str) -> Option<i64> {
    let text = std::fs::read_to_string(path).ok()?;
    text.split_whitespace().next()?.parse().ok()
}

/// An `AF_UNIX` `SOCK_DGRAM` pair, the shape `t-unix-scm` opens.
fn socketpair() -> Option<[i32; 2]> {
    let mut sv = [0i32; 2];
    let rc = unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_DGRAM, 0, sv.as_mut_ptr()) };
    (rc == 0).then_some(sv)
}

/// `getsockopt(SO_PEERCRED)`'s `pid`: `struct ucred` is `{ pid, uid, gid }`,
/// three 32-bit words, and the claim here is about the first.
///
/// The `pid` is the one field of that struct a *pid* namespace renders - Linux
/// fills it with `pid_vnr` in the reader's active pid namespace, so a reader
/// inside one sees the peer's number there and `0` for a peer it cannot see.
fn peer_pid(fd: i32) -> Option<u32> {
    let mut c = [0u32; 3];
    let mut len = core::mem::size_of::<[u32; 3]>() as libc::socklen_t;
    let rc = unsafe {
        libc::getsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            c.as_mut_ptr().cast(),
            &mut len,
        )
    };
    (rc == 0).then(|| c[0])
}

/// Read a pipe to end-of-file, answering the byte count - **within a budget**.
///
/// The budget is the point. This read waits for a forked section to finish, and a
/// section that stops (a child that never runs, a wait that never wakes) used to
/// block here for ever: the lane's 900-second timeout then killed the run and the
/// log said nothing except where it had *started* (`docs/notes/` §6.20's two
/// readings). A bounded read turns that into a failed claim that names how far
/// the section got, which is the whole difference between "it hangs sometimes"
/// and a place to look.
fn drain(ro: &rustix::fd::OwnedFd, buf: &mut [u8], patience_ms: i32) -> usize {
    let mut total = 0;
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(patience_ms as u64);
    while total < buf.len() {
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        if left.is_zero() {
            break;
        }
        let mut pfd = [rustix::event::PollFd::new(ro, rustix::event::PollFlags::IN)];
        let timeout = rustix::time::Timespec {
            tv_sec: left.as_secs() as i64,
            tv_nsec: left.subsec_nanos() as i64,
        };
        let ready = rustix::event::poll(&mut pfd, Some(&timeout)).unwrap_or(0);
        if ready == 0 {
            break;
        }
        match read(ro, &mut buf[total..]) {
            Ok(0) | Err(_) => break,
            Ok(n) => total += n,
        }
    }
    total
}

/// One byte per stage, so a section that stops says *where*.
fn stage_mark(w: &rustix::fd::OwnedFd, n: u8) {
    let _ = write(w, &[n]);
}

/// `unshare(2)` for the flags this program is about. Unsafe in `rustix`'s API
/// because a caller may unshare something another thread is holding; this
/// program is single-threaded and asks for the numbers only.
fn unshare(flags: UnshareFlags) -> Result<(), Errno> {
    unsafe { unshare_unsafe(flags) }
}

/// `kill(2)` with a signal through the raw door, the same one `probe_number`
/// uses - and for a reason of its own: `rustix`'s `Signal` cannot name the
/// kill the init's worker sends as SIGKILL and read back, and a claim about
/// two different numbers is easier to read when both come through one door.
fn send(pid: i32, sig: i32) -> isize {
    let rc = unsafe { libc::syscall(libc::SYS_kill, pid as libc::c_long, sig as libc::c_long) };
    if rc == 0 {
        0
    } else {
        -(std::io::Error::last_os_error().raw_os_error().unwrap_or(0) as isize)
    }
}

/// Two files the init section locks, one from each side of the namespace.
///
/// `F_GETLK`'s `l_pid` is a pid the *reader* is meant to act on, so the answer
/// for one lock differs between a reader inside the namespace and a reader
/// outside it - which is the only way to tell a translated number from the
/// machine's slot, and the reason these two exist.
const LOCK_INNER: &str = "/tmp/t-ns-pid-inner.lock";
const LOCK_OUTER: &str = "/tmp/t-ns-pid-outer.lock";

/// One whole-file write lock, the shape `t-filelock` uses: `F_SETLK` with the
/// range pinned to the file.
fn lock_whole(fd: i32) -> i32 {
    let mut want: libc::flock = unsafe { std::mem::zeroed() };
    want.l_type = libc::F_WRLCK as i16;
    want.l_whence = libc::SEEK_SET as i16;
    // SAFETY: a filled `flock` for a descriptor this process owns.
    let rc = unsafe { libc::fcntl(fd, libc::F_SETLK, &mut want as *mut libc::flock) };
    if rc == 0 {
        0
    } else {
        std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
    }
}

/// `F_GETLK` for a write lock over the whole file: answers `l_pid`, or the
/// negated errno when the call fails - a value no pid can have.
fn lk_pid(fd: i32) -> i32 {
    let mut probe: libc::flock = unsafe { std::mem::zeroed() };
    probe.l_type = libc::F_WRLCK as i16;
    probe.l_whence = libc::SEEK_SET as i16;
    // SAFETY: a filled `flock` for a descriptor this process owns.
    let rc = unsafe { libc::fcntl(fd, libc::F_GETLK, &mut probe as *mut libc::flock) };
    if rc == 0 {
        probe.l_pid
    } else {
        -std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
    }
}

/// The descriptor the init's `SIGTERM` handler writes one byte to.
///
/// A handler runs on a frame the kernel made, so it may do the one thing a
/// signal handler may do - write to a pipe - and nothing that allocates or
/// locks. The descriptor is a `static` because the handler has no argument to
/// carry it in.
static HANDLED_FD: AtomicI32 = AtomicI32::new(-1);

/// One byte, straight from the handler: the claim below is that it *ran*.
extern "C" fn on_term(_sig: libc::c_int) {
    let fd = HANDLED_FD.load(Ordering::Relaxed);
    if fd >= 0 {
        // SAFETY: a descriptor this process opened, and a one-byte write to a
        // pipe - the shape a handler may use.
        unsafe { libc::write(fd, b"x".as_ptr() as *const libc::c_void, 1) };
    }
}

/// Wait up to a second for `ro` to have something to read - a byte for the
/// handler's news. `ro` is non-blocking (the init sets that, and the flag
/// belongs to the pipe, so the parent's end of the same description has it
/// too), so a `EAGAIN` is the retry and not the answer.
fn wait_for_byte(ro: &OwnedFd) -> bool {
    for _ in 0..40 {
        let mut byte = [0u8; 1];
        if let Ok(1) = read(ro, &mut byte) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    false
}

/// The same wait, for **end-of-file**: the answer to "is the task holding this
/// pipe's write end still alive" - the only way a grandchild's death is
/// visible to a process that cannot `wait` for it.
fn wait_for_eof(ro: &OwnedFd) -> bool {
    for _ in 0..40 {
        let mut byte = [0u8; 1];
        if let Ok(0) = read(ro, &mut byte) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    false
}

/// The init section, in a task of its own.
///
/// Forked by `main` **before** it unshares, and for a measured reason: a pid
/// namespace whose init has exited is a namespace nothing can be created in
/// again (`fork` there answers `ENOMEM`, `.workbuddy/probe_fork_after_init.c`),
/// so the task that raises this section's init has to be created while its own
/// namespace is still the machine's. It makes its own namespace, raises an init
/// in it, lets that init's own worker try to kill it, then kills it from here -
/// an ancestor namespace - and reports what each step said, one word per step.
fn run_init_section(verdict: &OwnedFd, stage: &OwnedFd) {
    let mut words = [0isize; 17];
    words[0] = if unshare(UnshareFlags::NEWPID).is_ok() {
        1
    } else {
        0
    };
    stage_mark(stage, 1);
    let life = pipe();
    let said = pipe();
    let crossed = pipe();
    let deeper = pipe();
    let waited = pipe();
    // A sixth pipe for the `NSpid` chain of the task **two** namespaces in - read
    // while it is still alive, because the zap below takes it with the nested
    // init. Kept apart from `said` for a reason of ordering: `said`'s two lines
    // come from the init and its worker, in that order, and a third writer
    // racing them would leave the section reading which line belonged to whom
    // up to the scheduler.
    let chain = pipe();
    if let (
        Ok((life_r, life_w)),
        Ok((said_r, said_w)),
        Ok((crossed_r, crossed_w)),
        Ok((deeper_r, deeper_w)),
        Ok((waited_r, waited_w)),
        Ok((chain_r, chain_w)),
    ) = (life, said, crossed, deeper, waited, chain)
    {
        // The nested namespace's liveness, read the same way and with the same
        // non-blocking retry: the flag belongs to the pipe, so setting it here
        // serves every end of it.
        stage_mark(stage, 2);
        let _ = unsafe { libc::fcntl(deeper_r.as_raw_fd(), libc::F_SETFL, libc::O_NONBLOCK) };
        let _ = unsafe { libc::fcntl(waited_r.as_raw_fd(), libc::F_SETFL, libc::O_NONBLOCK) };
        // The two files, opened *here* so both are inherited: the inner one is
        // the init's to lock, the outer one this task locks now - a lock held
        // from outside the namespace, which a reader inside it has no number
        // for at all.
        let _ = std::fs::remove_file(LOCK_INNER);
        let _ = std::fs::remove_file(LOCK_OUTER);
        let inner_file = std::fs::File::create(LOCK_INNER).ok();
        let outer_file = std::fs::File::create(LOCK_OUTER).ok();
        let outer_held = match &outer_file {
            Some(file) => lock_whole(file.as_raw_fd()) == 0,
            None => false,
        };
        let init = unsafe { libc::fork() };
        if init == 0 {
            // The namespace's init: it arms the one thing that does cross - a
            // handler - and then waits to be killed from the outside.
            HANDLED_FD.store(crossed_w.as_raw_fd(), Ordering::Relaxed);
            let mut act: libc::sigaction = unsafe { std::mem::zeroed() };
            act.sa_sigaction = on_term as extern "C" fn(libc::c_int) as usize;
            // SAFETY: a filled `sigaction` for a number the wrapper serves;
            // musl fills the restorer slot itself.
            unsafe { libc::sigaction(libc::SIGTERM, &act, std::ptr::null_mut()) };
            // And one signal **blocked**, to be asked for as data: the rule this
            // pins is the one Linux states as "blocked signals are never
            // ignored", and without it an init could not use
            // `sigwait`/`sigtimedwait`/`signalfd` for what its members send.
            let mut only_usr1: libc::sigset_t = unsafe { std::mem::zeroed() };
            unsafe {
                libc::sigemptyset(&mut only_usr1);
                libc::sigaddset(&mut only_usr1, libc::SIGUSR1);
                libc::sigprocmask(libc::SIG_BLOCK, &only_usr1, std::ptr::null_mut());
            }
            // The lock this side of the namespace holds, taken before the worker
            // exists so its ask finds it there.
            let mut inner_held = false;
            if let Some(file) = &inner_file {
                inner_held = lock_whole(file.as_raw_fd()) == 0;
            }
            let _ = inner_held;
            // And what `/proc` calls the task *reading this*: the chain, not the
            // one number the reader's own namespace would give. Written before
            // the worker exists, so the two lines this pipe carries have a fixed
            // order.
            let own = ns_pid_chain("/proc/self/status");
            let line = format!("{} {}\n", own.len(), own.last().copied().unwrap_or(0));
            let _ = write(&said_w, line.as_bytes());
            // The liveness pipe's write end must be the worker's alone, or the
            // worker's death would leave a pipe its own corpse still holds open.
            drop(life_w);
            drop(said_r);
            // The reader's end must not block for ever if the handler never
            // runs: the flag belongs to the open file description, so setting
            // it here sets it for every end of it.
            let _ = unsafe { libc::fcntl(crossed_r.as_raw_fd(), libc::F_SETFL, libc::O_NONBLOCK) };
            let worker = unsafe { libc::fork() };
            if worker == 0 {
                drop(crossed_w);
                drop(deeper_w);
                drop(chain_w);
                let killed = send(1, libc::SIGKILL);
                let termed = send(1, libc::SIGTERM);
                let blocked = send(1, libc::SIGUSR1);
                // And what the two locks look like from *inside* the namespace:
                // the init's own lock should name it as this namespace does
                // (`1`), and the lock held outside should name nobody.
                let inner_pid = inner_file.as_ref().map_or(-999, |f| lk_pid(f.as_raw_fd()));
                let outer_pid = outer_file.as_ref().map_or(-999, |f| lk_pid(f.as_raw_fd()));
                let line = format!("{killed} {termed} {blocked} {inner_pid} {outer_pid}\n");
                let _ = write(&said_w, line.as_bytes());
                drop(said_w);
                for _ in 0..1000 {
                    std::thread::sleep(Duration::from_millis(50));
                }
                raw_exit(0);
            }
            // A namespace of its own, and an init in it. The precondition holds
            // for this task and not for the one above it: this task was *created
            // in* the namespace its children go to. The task raised here is the
            // one a non-forced kill could never reach - it is the init of a
            // namespace one level down - which is why the zap the section's
            // parent delivers is a forced one.
            if unshare(UnshareFlags::NEWPID).is_ok() {
                let nested = unsafe { libc::fork() };
                if nested == 0 {
                    drop(crossed_w);
                    // The task **two** namespaces in, reading its own chain
                    // before it parks: three numbers - the machine's, this
                    // namespace's, and the `1` the fresh namespace under it
                    // gives it - which is the depth the rung's numbers were
                    // not pinned at. It is the same line every reader sees,
                    // so the section reads it back from the machine's side
                    // below and expects these numbers unchanged.
                    let own = ns_pid_chain("/proc/self/status");
                    let line = format!(
                        "{}\n",
                        own.iter()
                            .map(|n| n.to_string())
                            .collect::<Vec<_>>()
                            .join(" ")
                    );
                    let _ = write(&chain_w, line.as_bytes());
                    // One byte, so that "the nested init exists" and "it died"
                    // are two different reads of one pipe: a wait for
                    // end-of-file alone would also be satisfied by an init that
                    // never managed to make the namespace at all.
                    let _ = write(&deeper_w, b"n\n");
                    for _ in 0..1000 {
                        std::thread::sleep(Duration::from_millis(50));
                    }
                    raw_exit(0);
                }
            }
            drop(deeper_w);
            drop(chain_w);
            let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
            let mut patience = libc::timespec {
                tv_sec: 3,
                tv_nsec: 0,
            };
            let got = unsafe { libc::sigtimedwait(&only_usr1, &mut info, &mut patience) };
            if got > 0 {
                let _ = write(&waited_w, b"b\n");
            }
            for _ in 0..1000 {
                std::thread::sleep(Duration::from_millis(50));
            }
            raw_exit(0);
        }
        drop(life_w);
        drop(deeper_w);
        drop(waited_w);
        drop(crossed_w);
        drop(chain_w);
        stage_mark(stage, 3);
        if init > 0 {
            // Two lines on this pipe, in order: the init's own view of its
            // chain, then the worker's answers. The first is the read that tells
            // "the chain from where procfs was mounted" apart from "the reader's
            // own number" - the two agree when the reader *is* the machine, and
            // differ for a reader inside the namespace, which is the whole
            // question this field's shape answers.
            let mut report = [0u8; REPORT_MAX];
            let n = read(&said_r, &mut report).unwrap_or(0);
            let inner: Vec<isize> = String::from_utf8_lossy(&report[..n])
                .split_whitespace()
                .filter_map(|f| f.parse().ok())
                .collect();
            if inner.len() >= 2 {
                words[9] = if inner[0] == 2 && inner[1] == 1 { 1 } else { 0 };
            }
            // And the same task read from *here*, the machine's namespace: the
            // same line, with the number this side knows it by first.
            stage_mark(stage, 4);
            let chain = ns_pid_chain(&format!("/proc/{init}/status"));
            words[10] = if chain.len() == 2 && chain[1] == 1 {
                1
            } else {
                0
            };
            // This task is in the machine's namespace, so its own chain is the
            // one number it is called by here - the shape every program on a
            // machine that never unshared sees.
            let mine = ns_pid_chain("/proc/self/status");
            words[11] = if mine.len() == 1 && mine[0] == std::process::id() {
                1
            } else {
                0
            };
            let n = read(&said_r, &mut report).unwrap_or(0);
            let attempts: Vec<isize> = String::from_utf8_lossy(&report[..n])
                .split_whitespace()
                .filter_map(|f| f.parse().ok())
                .collect();
            if attempts.len() >= 5 {
                words[1] = if attempts[0] == 0 { 1 } else { 0 };
                words[2] = if attempts[1] == 0 { 1 } else { 0 };
                words[8] = if attempts[2] == 0 { 1 } else { 0 };
                // Read from inside the namespace: the init's lock names the init
                // as this side calls it, and the lock held from outside names
                // nobody at all.
                words[12] = if attempts[3] == 1 { 1 } else { 0 };
                words[13] = if attempts[4] == 0 { 1 } else { 0 };
            }
            // And the same inner lock read from *here*, outside: the same
            // holder, named by the number this side knows it by.
            if let Some(file) = &inner_file {
                words[14] = if lk_pid(file.as_raw_fd()) == init as i32 {
                    1
                } else {
                    0
                };
            }
            // And the task **two** namespaces in, read while it is still alive
            // (the zap below takes it with the nested init). Its own chain has
            // three numbers - this machine's, the namespace between's, and the
            // `1` its own namespace gives it - which is the depth the rung's
            // numbers were pinned at only one level of until now.
            let mut deepbuf = [0u8; REPORT_MAX];
            let dn = read(&chain_r, &mut deepbuf).unwrap_or(0);
            let deep: Vec<u32> = String::from_utf8_lossy(&deepbuf[..dn])
                .split_whitespace()
                .filter_map(|f| f.parse().ok())
                .collect();
            stage_mark(stage, 5);
            words[15] = if deep.len() == 3 && deep[2] == 1 && deep[1] != 1 {
                1
            } else {
                0
            };
            // And the same line read from *here*, by the machine's own number
            // for it (the chain's first entry): the field is the *task's* chain
            // and not the reader's number, so the two reads must agree - and
            // that first entry must be a machine slot `/proc` resolves.
            if let Some(&machine) = deep.first() {
                words[16] = if ns_pid_chain(&format!("/proc/{machine}/status")) == deep {
                    1
                } else {
                    0
                };
            }
            let _ = outer_held;
            // The blocked one, read as data by the init itself.
            if wait_for_byte(&waited_r) {
                words[8] |= 2;
            }
            // Alive after both: only an ancestor can name it, and this task is
            // one.
            words[3] = if probe_number(init as i32) == 0 { 1 } else { 0 };
            // A signal it has a handler for is delivered.
            words[4] = if wait_for_byte(&crossed_r) { 1 } else { 0 };
            // And the ancestor's SIGKILL lands, and takes the worker with it.
            let sent = send(init as i32, libc::SIGKILL) == 0;
            let status = rustix::process::waitpid(Pid::from_raw(init), WaitOptions::empty());
            if sent
                && matches!(&status, Ok(Some((_, st))) if st.signaled()
                    && st.terminating_signal() == Some(libc::SIGKILL))
            {
                words[5] = 1;
            }
            words[6] = if wait_for_eof(&life_r) { 1 } else { 0 };
            // And the namespace *nested* under the dying one: the naive
            // membership test ("the namespace this task is in") would have left
            // it running, because its own namespace is not the one whose init
            // died. Measured, and the reason the test is `covers`.
            words[7] = if wait_for_byte(&deeper_r) && wait_for_eof(&deeper_r) {
                1
            } else {
                0
            };
        }
    }
    let line = format!(
        "{}\n",
        words
            .iter()
            .map(|w| w.to_string())
            .collect::<Vec<_>>()
            .join(" ")
    );
    stage_mark(stage, 6);
    let _ = write(verdict, line.as_bytes());
}

fn main() {
    let mut r = Report::new();

    if host_run() {
        println!(
            "note: a host cannot create a pid namespace (CLONE_NEWPID needs CAP_SYS_ADMIN), so no claim is made here"
        );
        r.finish()
    }

    // ---- The init's two rules, measured into a task of its own ----
    //
    // The rules: the task a namespace is *spoken for by* cannot be ended by the
    // signals its own members send - only an ancestor namespace's SIGKILL
    // reaches it, and a signal it has a **handler** for is delivered like any
    // other, which is what makes this a disposition test rather than a blanket
    // refusal - while its own death takes the whole namespace with it, nested
    // namespaces included. All of it measured on the host rather than read off a
    // page (`.workbuddy/probe_pid1*.c`; `docs/notes/` §6.3).
    //
    // Forked **here**, before this process unshares, and that order is itself a
    // measured rule: a pid namespace whose init has exited is a namespace
    // nothing may be created in again (`fork` there answers `ENOMEM`), and the
    // namespace this process's children already go to had an init that has since
    // exited - so the section's task must be created while its own namespace is
    // still the machine's, and it makes the namespace it needs itself. Its
    // claims come back as words at the end of the program.
    let verdict = pipe();
    let stage = pipe();
    r.check(
        "the init section's report pipe is made",
        verdict.is_ok() && stage.is_ok(),
    );
    let mut section = -1;
    let mut verdict_r: Option<OwnedFd> = None;
    let mut stage_r: Option<OwnedFd> = None;
    if let (Ok((reader, writer)), Ok((stage_read, stage_write))) = (verdict, stage) {
        section = unsafe { libc::fork() };
        if section == 0 {
            drop(reader);
            drop(stage_read);
            run_init_section(&writer, &stage_write);
            raw_exit(0);
        }
        drop(writer);
        drop(stage_write);
        verdict_r = Some(reader);
        stage_r = Some(stage_read);
    }
    r.check("the init section was forked", section > 0);

    // ---- The capability gate, and the order it is asked in ----
    //
    // It runs **before** the `unshare` below, because it forks: a task whose
    // children go to a namespace whose init has exited cannot create one at
    // all (`ENOMEM`, the third claim of the section below), and this section's
    // child is not the thing under test here.
    //
    // A lowered caller hears Linux's `EPERM` for a *served* kind, which is what
    // tells the two refusals apart: `EOPNOTSUPP` above means "present, cannot do
    // that", and this means "present, and you may not". The drop happens in a
    // child so the effective id comes back with it.
    let gate = pipe();
    if let Ok((gate_r, gate_w)) = gate {
        let lowered = unsafe { libc::fork() };
        if lowered == 0 {
            drop(gate_r);
            unsafe { libc::syscall(libc::SYS_close, gate_w.as_raw_fd() as libc::c_long) };
            let rc = unsafe { libc::syscall(libc::SYS_setresuid, -1i64, 65534i64, -1i64) };
            // What a lowered caller learns is the *errno*: a served kind they
            // may not have is `EPERM`, where an unserved one is `EOPNOTSUPP`
            // (claimed above, as root) - which is how the two refusals are told
            // apart. "Unshares nothing" is not observable from the caller's own
            // pid (this call only ever moves children), so the claim is the
            // refusal itself. (The `EOPNOTSUPP` half is claimed further down,
            // because that word mixes in an unserved kind and this section now
            // runs first.)
            let ok = rc == 0 && unshare(UnshareFlags::NEWPID) == Err(Errno::PERM);
            raw_exit(if ok { 0 } else { 1 });
        }
        drop(gate_w);
        if lowered > 0 {
            let mut word = [0u8; 1];
            let _ = read(&gate_r, &mut word);
            drop(gate_r);
            let status = rustix::process::waitpid(Pid::from_raw(lowered), WaitOptions::empty());
            r.check(
                "an unprivileged caller is refused EPERM, and unshares nothing",
                matches!(&status, Ok(Some((_, st))) if st.exited() && st.exit_status() == Some(0)),
            );
        }
    }

    // ---- A word that mixes the served kind with an unserved one unshares
    //      nothing, and PID is the served half ----
    r.check(
        // The unserved half is `IPC` because the word used to be `NET`, which
        // became a served kind the moment the fourth rung landed - the drift
        // §6.3 records over and over (a served set written down as a constant).
        "unshare(CLONE_NEWPID|CLONE_NEWCGROUP) is EOPNOTSUPP",
        unshare(UnshareFlags::NEWPID | UnshareFlags::NEWCGROUP) == Err(Errno::OPNOTSUPP),
    );

    // ---- The caller does not move ----
    let mine = getpid();
    r.check(
        "unshare(CLONE_NEWPID) succeeds for root",
        unshare(UnshareFlags::NEWPID).is_ok(),
    );
    r.check(
        "the caller that unshares keeps the pid it had",
        getpid() == mine,
    );
    let mine_number = mine.as_raw_nonzero().get() as i64;
    r.check(
        "and /proc still names it by that number where it stands",
        stat_first_field(&format!("/proc/{mine_number}/stat")) == Some(mine_number),
    );

    // ---- The first child is pid 1 to itself, and 0's parent is the parent ----
    //
    // Two pipes: the parent's own number goes *down* so the child can prove a
    // foreign number does not reach it, and the child's observations come back
    // up. The child parks until the parent has read its `/proc` entry, so the
    // parent's look at a live child and the child's own report describe the same
    // moment.
    let to_child = pipe();
    let to_parent = pipe();
    r.check(
        "the two report pipes are made",
        to_child.is_ok() && to_parent.is_ok(),
    );
    if let (Ok((down_r, down_w)), Ok((up_r, up_w))) = (to_child, to_parent) {
        // A socketpair made *here*, while this task's namespace is still the
        // machine's: the child born inside the namespace inherits it, and the
        // peer it names - this task - is in that namespace's **ancestor**, so
        // the pid the child reads has no number there (0). Its own pair, made
        // after the fork, names a task *inside* and reads the number that
        // namespace gives it (1). Both are `SO_PEERCRED`'s pid field, which
        // Linux renders with `pid_vnr` in the reader's active pid namespace
        // (`.workbuddy/probe-ucredpid.c`; `docs/notes/` §6.3).
        let outer_sv = socketpair();
        r.check("the outer socketpair is made", outer_sv.is_some());
        let outer = outer_sv.unwrap_or([-1, -1]);
        let child = unsafe { libc::fork() };
        if child == 0 {
            // Only the ends this half must not hold: the write end of the pipe
            // the parent reads (so its `read` sees end-of-file when this process
            // leaves) is closed again at the end, and the parent has already
            // dropped its own copy of the read end it wrote into.
            for fd in [down_w.as_raw_fd(), up_r.as_raw_fd()] {
                if fd >= 0 {
                    unsafe { libc::syscall(libc::SYS_close, fd as libc::c_long) };
                }
            }
            // Wait for the parent's own number before making any claim about
            // what a foreign number does.
            let mut word = [0u8; 32];
            let n = match read(&down_r, &mut word) {
                Ok(n) => n,
                Err(_) => raw_exit(9),
            };
            let foreign: i32 = core::str::from_utf8(&word[..n])
                .ok()
                .and_then(|s| s.trim().parse().ok())
                .unwrap_or(-1);
            let me = getpid().as_raw_nonzero().get() as i64;
            // `rustix`'s `getppid` answers `Option<Pid>`, and pid 0 is not a
            // `Pid` - which *is* the claim here: the parent stayed outside this
            // namespace, so the namespace has no number for it and the kernel
            // answers 0. A `None` is therefore reported as 0 rather than as an
            // error.
            let parent = getppid().map_or(0, |p| p.as_raw_nonzero().get() as i64);
            let proc_one = stat_first_field("/proc/1/stat").unwrap_or(-1);
            let proc_self = stat_first_field("/proc/self/stat").unwrap_or(-1);
            let own_number = probe_number(me as i32);
            let foreign_number = probe_number(foreign);
            // The socket surface's `pid`: a peer in the **ancestor** namespace
            // (the task that made the outer pair) has no number here, and the
            // child's own pair names a task *inside* - itself - by the number
            // this namespace gives it.
            let outer_pid = peer_pid(outer[0]).map_or(-1, i64::from);
            let inner_pid = socketpair()
                .and_then(|sv| peer_pid(sv[0]))
                .map_or(-1, i64::from);
            let line = format!(
                "{me} {parent} {proc_one} {proc_self} {own_number} {foreign_number} {outer_pid} {inner_pid}\n"
            );
            let _ = write(&up_w, line.as_bytes());
            unsafe { libc::syscall(libc::SYS_close, up_w.as_raw_fd() as libc::c_long) };
            raw_exit(0);
        }
        drop(down_r);
        drop(up_w);
        r.check("the child that will be pid 1 was forked", child > 0);
        if child > 0 {
            // The parent's half first, while the child is parked: what the
            // *parent* calls it is the number `fork` answered with.
            let real = child as i64;
            r.check(
                "the child is still visible to the parent under its own number",
                stat_first_field(&format!("/proc/{real}/stat")) == Some(real),
            );
            // Tell the child which number must not reach it, then collect.
            let tell = format!("{mine_number}\n");
            let _ = write(&down_w, tell.as_bytes());
            drop(down_w);
            let mut report = [0u8; REPORT_MAX];
            let n = drain(&up_r, &mut report, 30_000);
            drop(up_r);
            let text = String::from_utf8_lossy(&report[..n]).to_string();
            let fields: Vec<i64> = text
                .split_whitespace()
                .filter_map(|f| f.parse().ok())
                .collect();
            r.check("the child reports eight words", fields.len() == 8);
            if fields.len() == 8 {
                r.check("the first child is pid 1 to itself", fields[0] == 1);
                r.check(
                    "its parent is outside its namespace, so getppid is 0",
                    fields[1] == 0,
                );
                r.check("/proc/1 names that child", fields[2] == 1);
                r.check("/proc/self names it by the same number", fields[3] == 1);
                r.check(
                    "and kill(1, 0) inside the namespace reaches the child itself",
                    fields[4] == 0,
                );
                r.check(
                    "while the parent's number does not name anybody there (ESRCH)",
                    fields[5] == -(Errno::SRCH.raw_os_error() as i64),
                );
                r.check(
                    "a socket's peer in the ancestor namespace has no pid here (SO_PEERCRED reads 0)",
                    fields[6] == 0,
                );
                r.check(
                    "and a peer inside the namespace is named by its number there (1)",
                    fields[7] == 1,
                );
            }
            let status = rustix::process::waitpid(Pid::from_raw(child), WaitOptions::empty());
            r.check(
                "the parent reaps the child, and the answer is a number of the parent's namespace",
                matches!(&status, Ok(Some((pid, st))) if st.exited() && st.exit_status() == Some(0) && Pid::from_raw(child) == Some(*pid)),
            );
            r.check(
                "the parent's own number did not move either",
                getpid() == mine,
            );
        }
    }

    // ---- A pid namespace is made once, and a dead one takes no tasks ----
    //
    // The three faces of one rule, and the ones this program's own structure ran
    // into: Linux's `copy_pid_ns` asks that the caller's **own** pid namespace be
    // the one its children would go to, so the call that makes one can only
    // succeed while that holds - and after it has, it does not. Everything here
    // is measured on the host (`.workbuddy/probe_pid_ns_again.c`,
    // `.workbuddy/probe_fork_after_init.c`; `docs/notes/` §6.3), and the state
    // this section asks from is the one the section above leaves behind: this
    // task's children went to a namespace whose init has since exited.
    //
    // It is also why the init section above was forked at the *top* of the
    // program rather than here - the shape a program that respects the rules has,
    // which is the pin's own way of being about them.
    r.check(
        "a second unshare(CLONE_NEWPID) in one process is EINVAL",
        unshare(UnshareFlags::NEWPID) == Err(Errno::INVAL),
    );
    let fresh = unsafe {
        libc::syscall(
            libc::SYS_clone,
            (libc::SIGCHLD | libc::CLONE_NEWPID) as libc::c_long,
            0 as libc::c_long,
            0 as libc::c_long,
            0 as libc::c_long,
            0 as libc::c_long,
        )
    };
    r.check(
        "and clone(CLONE_NEWPID) after one is EINVAL too",
        fresh < 0 && std::io::Error::last_os_error().raw_os_error() == Some(libc::EINVAL),
    );
    let sibling = unsafe { libc::fork() };
    r.check(
        "while a fork into the namespace whose init has exited is ENOMEM",
        sibling < 0 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ENOMEM),
    );

    // ---- The init section's verdict ----
    //
    // Seventeen words: the namespace was made; a sibling's SIGKILL and its
    // SIGTERM both answered 0; the init was alive after both; the signal it had
    // a handler for was delivered; the ancestor's SIGKILL landed; the
    // namespace's other task died with it; the nested one died too; the blocked
    // signal arrived; the three `NSpid` shapes (one level in, the machine's read
    // of it, the machine's own); the two locks; and the depth-2 chain, read from
    // both sides. The pair in the middle is what a "namespace" that is really a
    // shared global fails.
    if let Some(verdict_r) = verdict_r {
        let mut report = [0u8; REPORT_MAX];
        let n = drain(&verdict_r, &mut report, 60_000);
        // How far the section got, if it stopped: the last stage mark it wrote.
        let mut marks = [0u8; 32];
        let reached = match &stage_r {
            Some(ro) => {
                let mut seen = 0usize;
                while seen < marks.len() {
                    match read(ro, &mut marks[seen..]) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => seen += n,
                    }
                }
                marks[..seen].last().copied().unwrap_or(0)
            }
            None => 0,
        };
        r.check(
            &format!("the init section ran to its end (last stage {reached})"),
            reached == 6,
        );
        let words: Vec<isize> = String::from_utf8_lossy(&report[..n])
            .split_whitespace()
            .filter_map(|f| f.parse().ok())
            .collect();
        r.check(
            "the init section reports seventeen words",
            words.len() == 17,
        );
        if words.len() == 17 {
            r.check(
                "a task in its namespace may make one for its own children",
                words[0] == 1,
            );
            r.check("a sibling's kill(1, SIGKILL) answers 0", words[1] == 1);
            r.check("and its kill(1, SIGTERM) answers 0", words[2] == 1);
            r.check(
                "while the init is alive after both: its members cannot end it",
                words[3] == 1,
            );
            r.check(
                "a signal the init has a *handler* for is delivered, though",
                words[4] == 1,
            );
            r.check(
                "and an ancestor's SIGKILL lands where the members' could not",
                words[5] == 1,
            );
            r.check(
                "and its death took the namespace's other task with it",
                words[6] == 1,
            );
            r.check(
                "and the task in the namespace nested beneath it, where a member's kill could not reach",
                words[7] == 1,
            );
            r.check(
                "and one it *blocks* arrives, because a blocked signal is never ignored",
                words[8] == 3,
            );
            r.check(
                "the init reading its own status sees the chain: two numbers, ending in 1",
                words[9] == 1,
            );
            r.check(
                "and the machine reads the same line for it, its own number first",
                words[10] == 1,
            );
            r.check(
                "while a task of the machine's namespace shows the one number it answers to",
                words[11] == 1,
            );
            r.check(
                "a lock read from inside the namespace names its holder as that namespace does",
                words[12] == 1,
            );
            r.check(
                "and one held from outside the namespace names nobody there",
                words[13] == 1,
            );
            r.check(
                "while a reader outside is handed the holder's number out there",
                words[14] == 1,
            );
            r.check(
                "a task two namespaces in reads a chain of three, its own number last and 1",
                words[15] == 1,
            );
            r.check(
                "and the machine reading that same task by that chain's first number agrees",
                words[16] == 1,
            );
        }
        let status = rustix::process::waitpid(
            Pid::from_raw(section),
            rustix::process::WaitOptions::empty(),
        );
        r.check(
            "and the section's own task is reaped",
            matches!(&status, Ok(Some((_, st))) if st.exited() && st.exit_status() == Some(0)),
        );
    }

    r.finish()
}
