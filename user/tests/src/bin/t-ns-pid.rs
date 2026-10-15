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
//!   task it speaks for, one in a namespace *nested* beneath it included.
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
//! (Linux answers a second `unshare(CLONE_NEWPID)` with `EINVAL`), and a `fork`
//! into a namespace whose init has exited is `ENOMEM` - both named as gaps in
//! `docs/notes/` §6.3, because this kernel does not refuse them yet. So the
//! section runs in a task whose own namespace is still the machine's, and raises
//! the namespaces it needs itself.
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

/// The first field of `/proc/<n>/stat`, which is the pid that file is about -
/// as text, because the claim is about the name a reader is handed.
fn stat_first_field(path: &str) -> Option<i64> {
    let text = std::fs::read_to_string(path).ok()?;
    text.split_whitespace().next()?.parse().ok()
}

/// Read a pipe to end-of-file, answering the byte count.
fn drain(ro: &rustix::fd::OwnedFd, buf: &mut [u8]) -> usize {
    let mut total = 0;
    while total < buf.len() {
        match read(ro, &mut buf[total..]) {
            Ok(0) | Err(_) => break,
            Ok(n) => total += n,
        }
    }
    total
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
fn run_init_section(verdict: &OwnedFd) {
    let mut words = [0isize; 9];
    words[0] = if unshare(UnshareFlags::NEWPID).is_ok() {
        1
    } else {
        0
    };
    let life = pipe();
    let said = pipe();
    let crossed = pipe();
    let deeper = pipe();
    let waited = pipe();
    if let (
        Ok((life_r, life_w)),
        Ok((said_r, said_w)),
        Ok((crossed_r, crossed_w)),
        Ok((deeper_r, deeper_w)),
        Ok((waited_r, waited_w)),
    ) = (life, said, crossed, deeper, waited)
    {
        // The nested namespace's liveness, read the same way and with the same
        // non-blocking retry: the flag belongs to the pipe, so setting it here
        // serves every end of it.
        let _ = unsafe { libc::fcntl(deeper_r.as_raw_fd(), libc::F_SETFL, libc::O_NONBLOCK) };
        let _ = unsafe { libc::fcntl(waited_r.as_raw_fd(), libc::F_SETFL, libc::O_NONBLOCK) };
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
                let killed = send(1, libc::SIGKILL);
                let termed = send(1, libc::SIGTERM);
                let blocked = send(1, libc::SIGUSR1);
                let line = format!("{killed} {termed} {blocked}\n");
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
        if init > 0 {
            // The worker's two answers: one line, written in one call.
            let mut report = [0u8; REPORT_MAX];
            let n = read(&said_r, &mut report).unwrap_or(0);
            let attempts: Vec<isize> = String::from_utf8_lossy(&report[..n])
                .split_whitespace()
                .filter_map(|f| f.parse().ok())
                .collect();
            if attempts.len() >= 3 {
                words[1] = if attempts[0] == 0 { 1 } else { 0 };
                words[2] = if attempts[1] == 0 { 1 } else { 0 };
                words[8] = if attempts[2] == 0 { 1 } else { 0 };
            }
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
    r.check("the init section's report pipe is made", verdict.is_ok());
    let mut section = -1;
    let mut verdict_r: Option<OwnedFd> = None;
    if let Ok((reader, writer)) = verdict {
        section = unsafe { libc::fork() };
        if section == 0 {
            drop(reader);
            run_init_section(&writer);
            raw_exit(0);
        }
        drop(writer);
        verdict_r = Some(reader);
    }
    r.check("the init section was forked", section > 0);

    // ---- A word that mixes the served kind with an unserved one unshares
    //      nothing, and PID is the served half ----
    r.check(
        // The unserved half is `IPC` because the word used to be `NET`, which
        // became a served kind the moment the fourth rung landed - the drift
        // §6.3 records over and over (a served set written down as a constant).
        "unshare(CLONE_NEWPID|CLONE_NEWIPC) is EOPNOTSUPP",
        unshare(UnshareFlags::NEWPID | UnshareFlags::NEWIPC) == Err(Errno::OPNOTSUPP),
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
            let line =
                format!("{me} {parent} {proc_one} {proc_self} {own_number} {foreign_number}\n");
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
            let n = drain(&up_r, &mut report);
            drop(up_r);
            let text = String::from_utf8_lossy(&report[..n]).to_string();
            let fields: Vec<i64> = text
                .split_whitespace()
                .filter_map(|f| f.parse().ok())
                .collect();
            r.check("the child reports six words", fields.len() == 6);
            if fields.len() == 6 {
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

    // ---- The capability gate, and the order it is asked in ----
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
            // refusal itself.
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

    // ---- The init section's verdict ----
    //
    // Seven words: the namespace was made; a sibling's SIGKILL and its SIGTERM
    // both answered 0; the init was alive after both; the signal it had a
    // handler for was delivered; the ancestor's SIGKILL landed; and the
    // namespace's other task died with it. The pair in the middle is what a
    // "namespace" that is really a shared global fails.
    if let Some(verdict_r) = verdict_r {
        let mut report = [0u8; REPORT_MAX];
        let n = drain(&verdict_r, &mut report);
        let words: Vec<isize> = String::from_utf8_lossy(&report[..n])
            .split_whitespace()
            .filter_map(|f| f.parse().ok())
            .collect();
        r.check("the init section reports nine words", words.len() == 9);
        if words.len() == 9 {
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
