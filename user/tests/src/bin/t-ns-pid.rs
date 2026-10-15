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
//!   is what a program does with its own pid - would signal the machine's init.
//!
//! # The doors
//!
//! `rustix::thread::unshare(UnshareFlags::NEWPID)` makes the namespace,
//! `rustix::process::{getpid, getppid}` read the numbers, and `libc::fork` makes
//! the child (`rustix` binds no `fork` on either machine - the reason
//! `t-fork`/`t-wait4` give). Two claims cannot come from a wrapper at all:
//!
//! * **`kill(pid, 0)`** - the "is this number there at all" probe, which is the
//!   whole of the safety claim - has no spelling in `rustix`: its `Signal` type
//!   has no zero, and zero *is* the request. So the raw `SYS_kill` door, with
//!   the errno read back the way `musl`'s `syscall(2)` leaves it.
//! * **`/proc/<n>/stat`'s first field** is read as *text*, because the claim is
//!   about the name a reader is given rather than about a struct.
//!
//! A host run cannot make a pid namespace at all - `CLONE_NEWPID` needs
//! `CAP_SYS_ADMIN`, which this container's root does not hold (measured:
//! `.workbuddy/probe_pidns.c` gets `EPERM`, and `unshare --pid` fails the same
//! way) - so the host branch says so and makes no claim. That is also why the
//! semantics here come from §6.3's design record and Linux's rules rather than
//! from an observation on a real kernel: the *pin* is what turns them into a
//! claim about this one.

use mpos_user_tests::{Report, host_run};
use rustix::fd::AsRawFd;
use rustix::io::{Errno, read, write};
use rustix::pipe::pipe;
use rustix::process::{Pid, WaitOptions, getpid, getppid};
use rustix::thread::{UnshareFlags, unshare_unsafe};

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

fn main() {
    let mut r = Report::new();

    if host_run() {
        println!(
            "note: a host cannot create a pid namespace (CLONE_NEWPID needs CAP_SYS_ADMIN), so no claim is made here"
        );
        r.finish()
    }

    // ---- A word that mixes the served kind with an unserved one unshares
    //      nothing, and PID is the served half ----
    r.check(
        "unshare(CLONE_NEWPID|CLONE_NEWNET) is EOPNOTSUPP",
        unshare(UnshareFlags::NEWPID | UnshareFlags::NEWNET) == Err(Errno::OPNOTSUPP),
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

    r.finish()
}
