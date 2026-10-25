//! t-pid-numbers - the pid a **process door** answers with, for a caller inside
//! a pid namespace: `fork`/`clone`/`clone3`'s return value, `wait4`'s return
//! value, `waitid`'s `si_pid`, and the two `CLONE_*_SETTID` words.
//!
//! All of them answer the child's number in some namespace, and which one is
//! not a matter of taste - it was measured on a current host first
//! (`.workbuddy/probe_pid_numbers.c`, Linux 7.2.11, the probe standing as pid 1
//! of a fresh pid namespace so every number is small enough to read):
//!
//! * `fork` answers the child's number **in the caller's namespace** (`2` for a
//!   child whose own `getpid` is `2`), and `wait4(-1)` reports that same number;
//! * `clone(2)`/`clone3` answer it too;
//! * `CLONE_PARENT_SETTID` writes that same number into the **parent's** word
//!   (for a `CLONE_NEWPID` child: the parent's number, not the child's own `1`);
//! * `CLONE_CHILD_SETTID` writes the child's number **in its own** namespace
//!   (for a `CLONE_NEWPID` child: `1`, its own `getpid`);
//! * `waitid(P_PID, n)` reports `n` in `si_pid`.
//!
//! This kernel answered the scheduler's **task slot** at every one of those
//! doors for a caller inside a namespace, which is a number no other call
//! accepts - the child could not be killed, waited for, or found in `/proc` by
//! the number `fork` had just handed out. A parent *outside* the namespace
//! cannot see the difference (its number and the slot coincide), which is why
//! every existing pin was green: `t-ns-pid`'s parent-side claims are all that
//! outside shape. The shape that caught it is the one below - a parent **inside**
//! the namespace, which is the container's own process.
//!
//! Every claim runs inside a fresh user+pid namespace, made by the program
//! itself: nothing outside it is touched, so this is safe to run on a host too.

use mpos_user_tests::Report;

/// `CLONE_PARENT_SETTID` and `CLONE_CHILD_SETTID`, as the uapi writes them.
const CLONE_PARENT_SETTID: usize = 0x0010_0000;
const CLONE_CHILD_SETTID: usize = 0x0100_0000;
const CLONE_NEWPID: usize = 0x2000_0000;

/// Write one line of a `uid_map`-shaped file.
fn write_proc(path: &str, text: &str) {
    use std::io::Write;
    if let Ok(mut file) = std::fs::File::create(path) {
        let _ = file.write_all(text.as_bytes());
    }
}

/// Tell the parent this process's own number, down a pipe.
fn announce(write_end: libc::c_int) {
    let me = unsafe { libc::getpid() };
    let bytes = me.to_ne_bytes();
    unsafe {
        libc::write(
            write_end,
            bytes.as_ptr() as *const libc::c_void,
            bytes.len(),
        );
        libc::close(write_end);
    }
}

/// A number a child announced.
fn learn(read_end: libc::c_int) -> Option<i32> {
    let mut bytes = [0u8; 4];
    let got = unsafe { libc::read(read_end, bytes.as_mut_ptr() as *mut libc::c_void, 4) };
    unsafe { libc::close(read_end) };
    if got != 4 {
        return None;
    }
    Some(i32::from_ne_bytes(bytes))
}

fn number_pipe() -> Option<(libc::c_int, libc::c_int)> {
    let mut fds = [0 as libc::c_int; 2];
    if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
        return None;
    }
    Some((fds[0], fds[1]))
}

/// The exit code of one child, named or any (`which` may be `-1`).
fn reap(which: libc::pid_t) -> Option<i32> {
    let mut status: libc::c_int = 0;
    let got = unsafe { libc::waitpid(which, &mut status, 0) };
    if got < 0 || (which > 0 && got != which) {
        return None;
    }
    if libc::WIFEXITED(status) {
        Some(libc::WEXITSTATUS(status))
    } else {
        None
    }
}

/// `wait4(pid, &status, 0, NULL)`: the number it reports, or `-1`.
fn wait_reporting(pid: libc::pid_t) -> isize {
    let mut status: libc::c_int = 0;
    unsafe { libc::wait4(pid, &mut status, 0, core::ptr::null_mut()) as isize }
}

/// A **fork-shaped** raw `clone`, whose return is the kernel's answer. Stack and
/// pointer order follow the machine: the two ABIs swap `parent_tid`/`tls`
/// (`t-clone` reconciles the same pair).
fn clone_raw(flags: usize, parent_tid: &mut i32, child_tid: &mut i32) -> isize {
    #[cfg(target_arch = "x86_64")]
    let ret = unsafe {
        libc::syscall(
            libc::SYS_clone,
            flags as libc::c_long,
            0 as libc::c_long,
            parent_tid as *mut i32 as libc::c_long,
            child_tid as *mut i32 as libc::c_long,
            0 as libc::c_long,
        )
    };
    #[cfg(target_arch = "aarch64")]
    let ret = unsafe {
        libc::syscall(
            libc::SYS_clone,
            flags as libc::c_long,
            0 as libc::c_long,
            parent_tid as *mut i32 as libc::c_long,
            0 as libc::c_long,
            child_tid as *mut i32 as libc::c_long,
        )
    };
    ret as isize
}

/// Run `body` as **pid 1 of a fresh pid namespace**, answering the code it
/// chose (`None` when the namespace itself could not be made).
fn in_fresh_pid_ns(body: fn() -> i32) -> Option<i32> {
    match unsafe { libc::fork() } {
        -1 => None,
        0 => {
            if unsafe { libc::unshare(libc::CLONE_NEWUSER) } != 0 {
                unsafe { libc::_exit(90) }
            }
            let uid = unsafe { libc::getuid() };
            let gid = unsafe { libc::getgid() };
            write_proc("/proc/self/setgroups", "deny\n");
            write_proc("/proc/self/uid_map", &format!("0 {uid} 1\n"));
            write_proc("/proc/self/gid_map", &format!("0 {gid} 1\n"));
            if unsafe { libc::unshare(libc::CLONE_NEWPID) } != 0 {
                unsafe { libc::_exit(91) }
            }
            match unsafe { libc::fork() } {
                -1 => unsafe { libc::_exit(92) },
                0 => unsafe { libc::_exit(body()) },
                _kid => {
                    let code = reap(-1).unwrap_or(93);
                    unsafe { libc::_exit(code) }
                }
            }
        }
        kid => reap(kid),
    }
}

/// `fork`, its answer, and the number a `wait4` reports for it.
fn body_fork_answer() -> i32 {
    if unsafe { libc::getpid() } != 1 {
        return 10;
    }
    let Some((read_end, write_end)) = number_pipe() else {
        return 11;
    };
    let fork_answer = unsafe { libc::fork() };
    if fork_answer == 0 {
        unsafe { libc::close(read_end) };
        announce(write_end);
        unsafe { libc::_exit(0) }
    }
    if fork_answer < 0 {
        return 12;
    }
    unsafe { libc::close(write_end) };
    let Some(number) = learn(read_end) else {
        return 13;
    };
    // The child's own number, the answer `fork` gave, and the number a wait
    // reports: three doors, one number.
    let answered = unsafe { libc::wait4(-1, core::ptr::null_mut(), 0, core::ptr::null_mut()) };
    if fork_answer as i32 != number {
        return 14;
    }
    if answered as i32 != number {
        return 15;
    }
    // The named form answers the number it was given.
    let Some((read_end, write_end)) = number_pipe() else {
        return 16;
    };
    let second = unsafe { libc::fork() };
    if second == 0 {
        unsafe { libc::close(read_end) };
        announce(write_end);
        unsafe { libc::_exit(0) }
    }
    if second < 0 {
        return 17;
    }
    unsafe { libc::close(write_end) };
    let Some(second_number) = learn(read_end) else {
        return 18;
    };
    if wait_reporting(second_number) as i32 != second_number {
        return 19;
    }
    0
}

/// `CLONE_PARENT_SETTID`'s word, for a child in the caller's own namespace:
/// the child's number here.
fn body_parent_settid() -> i32 {
    if unsafe { libc::getpid() } != 1 {
        return 20;
    }
    let Some((read_end, write_end)) = number_pipe() else {
        return 21;
    };
    let mut parent_word: i32 = -1;
    let mut unused: i32 = 0;
    let ret = clone_raw(
        libc::SIGCHLD as usize | CLONE_PARENT_SETTID,
        &mut parent_word,
        &mut unused,
    );
    if ret == 0 {
        unsafe { libc::close(read_end) };
        announce(write_end);
        unsafe { libc::_exit(0) }
    }
    if ret < 0 {
        return 22;
    }
    unsafe { libc::close(write_end) };
    let Some(number) = learn(read_end) else {
        return 23;
    };
    let answered = wait_reporting(-1);
    if parent_word != number || answered as i32 != number {
        return 24;
    }
    0
}

/// The asymmetric pair: a `CLONE_NEWPID` child is `1` to itself, its parent's
/// `CLONE_PARENT_SETTID` word is the parent's number for it, and its own
/// `CLONE_CHILD_SETTID` word is its own number.
fn body_nested_numbers() -> i32 {
    if unsafe { libc::getpid() } != 1 {
        return 30;
    }
    let Some((read_end, write_end)) = number_pipe() else {
        return 31;
    };
    let mut parent_word: i32 = -1;
    let mut unused: i32 = 0;
    let ret = clone_raw(
        libc::SIGCHLD as usize | CLONE_PARENT_SETTID | CLONE_NEWPID,
        &mut parent_word,
        &mut unused,
    );
    if ret == 0 {
        unsafe { libc::close(read_end) };
        announce(write_end);
        unsafe { libc::_exit(0) }
    }
    if ret < 0 {
        return 32;
    }
    unsafe { libc::close(write_end) };
    let Some(own_number) = learn(read_end) else {
        return 33;
    };
    if own_number != 1 {
        return 34;
    }
    let answered = wait_reporting(-1);
    // The parent's word and the wait's answer are the parent's number, and not
    // the child's own `1`.
    if parent_word == 1 || parent_word as isize != answered {
        return 35;
    }

    // The child's own word, read in the child: its own number.
    let Some((read_end, write_end)) = number_pipe() else {
        return 36;
    };
    let mut unused2: i32 = 0;
    let mut child_word: i32 = -1;
    let ret = clone_raw(
        libc::SIGCHLD as usize | CLONE_CHILD_SETTID | CLONE_NEWPID,
        &mut unused2,
        &mut child_word,
    );
    if ret == 0 {
        unsafe { libc::close(read_end) };
        let seen = child_word;
        let bytes = seen.to_ne_bytes();
        unsafe {
            libc::write(
                write_end,
                bytes.as_ptr() as *const libc::c_void,
                bytes.len(),
            );
            libc::close(write_end);
            libc::_exit(0)
        }
    }
    if ret < 0 {
        return 37;
    }
    unsafe { libc::close(write_end) };
    let Some(word) = learn(read_end) else {
        return 38;
    };
    let _ = wait_reporting(-1);
    if word != 1 {
        return 39;
    }
    0
}

/// `waitid(P_PID, n)`'s `si_pid` is the number it was given.
fn body_waitid_si_pid() -> i32 {
    if unsafe { libc::getpid() } != 1 {
        return 40;
    }
    let Some((read_end, write_end)) = number_pipe() else {
        return 41;
    };
    let kid = unsafe { libc::fork() };
    if kid == 0 {
        unsafe { libc::close(read_end) };
        announce(write_end);
        unsafe { libc::_exit(0) }
    }
    if kid < 0 {
        return 42;
    }
    unsafe { libc::close(write_end) };
    let Some(number) = learn(read_end) else {
        return 43;
    };
    let mut info: libc::siginfo_t = unsafe { core::mem::zeroed() };
    let ret = unsafe { libc::waitid(libc::P_PID, number as libc::id_t, &mut info, libc::WEXITED) };
    if ret != 0 {
        return 44;
    }
    if unsafe { info.si_pid() } != number {
        return 45;
    }
    0
}

fn main() {
    println!("t-pid-numbers");
    let mut r = Report::new();

    let fork_answer = in_fresh_pid_ns(body_fork_answer);
    r.check(
        &format!("fork's answer, the child's own number and the wait's report are one number (got {fork_answer:?})"),
        fork_answer == Some(0),
    );

    let settid = in_fresh_pid_ns(body_parent_settid);
    r.check(
        &format!("CLONE_PARENT_SETTID carries the child's number in the caller's namespace (got {settid:?})"),
        settid == Some(0),
    );

    let nested = in_fresh_pid_ns(body_nested_numbers);
    r.check(
        &format!("a CLONE_NEWPID child is 1 to itself, its parent's SETTID word and the wait are the parent's number, and its own SETTID word is 1 (got {nested:?})"),
        nested == Some(0),
    );

    let waitid = in_fresh_pid_ns(body_waitid_si_pid);
    r.check(
        &format!("waitid(P_PID) reports the number it was given in si_pid (got {waitid:?})"),
        waitid == Some(0),
    );

    r.finish()
}
