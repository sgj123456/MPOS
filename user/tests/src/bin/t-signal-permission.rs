//! t-signal-permission - who may signal whom.
//!
//! Linux's `kill_ok_by_cred` is a rule about *credentials*, not about the
//! signal: the sender's real or effective uid must be the target's real or
//! saved-set uid, or the sender must hold `CAP_KILL` in the **target's** user
//! namespace. This kernel had no such rule at all - `kill`/`tgkill` delivered
//! whatever they were asked to - so a process that had dropped to another user
//! could end a root process.
//!
//! Measured on a current host first (`.workbuddy/probe_killperm.c`, Linux
//! 7.2.11, uid 1000): `kill(1, 0)` is `EPERM`, `kill(self, 0)` is 0 and
//! `kill(child, 0)` is 0. The guest pins the other half of the same table:
//! a child that dropped to uid 65534 is refused a signal to its root parent,
//! while the root parent's signal to that child is served - the parent's uid
//! matches nowhere, so that answer can only be `CAP_KILL`. A third child
//! stays uid 0 but empties its capability sets first: the same signal to the
//! same 65534 target is then `EPERM`, which is the capability's own
//! counterfactual and not a second uid rule.
//!
//! `SIGCONT` has one door beside the credential rule: Linux's
//! `check_kill_permission` lets a caller whose ids and capabilities both refuse
//! continue a process it shares a **session** with, and no other signal
//! crosses that line. Measured on a current host with the shape this program
//! already builds - a 65534 child and its root parent
//! (`.workbuddy/probe_sigcont_session.c`, Linux 7.2.11): the same-session
//! child reads `EPERM` for `kill(0)` and `SIGUSR1` but `0` for `SIGCONT`
//! through `kill`, `tgkill` and `rt_sigqueueinfo`, while a `setsid` child reads
//! `EPERM` for all four; a caller the ids already allow reads `0` whatever the
//! sessions are. So the guest pins both sides of the session and the signal
//! that is not `SIGCONT` as the control.
//!
//! `tgkill` and `rt_sigqueueinfo` are the same question at two more doors, and
//! the record door additionally refuses `SI_TKILL` and a non-negative
//! `si_code` to somebody else - measured in `.workbuddy/probe_qinfo.c`, where
//! `-1`, `-2`, `-7`, `-8` and `-100` arrive unchanged but `0`, `+1` and `-6`
//! are `EPERM`. This program pins those two refusals as well.
//!
//! # The host run
//!
//! The host's user cannot lower itself to 65534, so the uid-drop half is named
//! and not made there; the two claims the host *can* make are the two that
//! measure the same rule from its own side (uid 1000 against root's pid 1).

use mpos_user_tests::{Report, host_run};

/// A uid the guest has no account for but the kernel will accept: `nobody`.
/// The account's existence is not what the rule reads - the number is.
const NOBODY: u32 = 65534;

/// The v3 capability wire (`struct __user_cap_data_struct`, two records).
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct CapRecord {
    effective: u32,
    permitted: u32,
    inheritable: u32,
}

#[repr(C)]
struct CapHeader {
    version: u32,
    pid: libc::c_int,
}

/// The version word `libcap`'s v3 uses: `_LINUX_CAPABILITY_VERSION_3`.
const CAP_VERSION_3: u32 = 0x2008_0522;

/// The errno a call answered, or 0.
fn errno_of(r: libc::c_int) -> i32 {
    if r == 0 {
        0
    } else {
        std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
    }
}

fn syscall_errno(r: libc::c_long) -> i32 {
    if r >= 0 {
        0
    } else {
        std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
    }
}

/// A caller-owned `siginfo_t` (`128` bytes), seeded only with the two fields
/// `rt_sigqueueinfo` validates before the permission question: `si_signo` at
/// 0 and `si_code` at 8. The record's identity words stay zero, which is what
/// makes the door's answer about permission rather than about the record.
#[repr(align(8))]
struct RawInfo([u8; 128]);

impl RawInfo {
    fn queue(sig: i32) -> RawInfo {
        let mut bytes = [0u8; 128];
        bytes[0..4].copy_from_slice(&sig.to_ne_bytes());
        bytes[8..12].copy_from_slice(&(-1i32).to_ne_bytes()); // SI_QUEUE
        RawInfo(bytes)
    }
}

fn write_all(fd: libc::c_int, mut bytes: &[u8]) {
    while !bytes.is_empty() {
        let n = unsafe { libc::write(fd, bytes.as_ptr() as *const libc::c_void, bytes.len()) };
        if n < 0 && std::io::Error::last_os_error().raw_os_error() == Some(libc::EINTR) {
            continue;
        }
        if n <= 0 {
            return;
        }
        bytes = &bytes[n as usize..];
    }
}

fn read_all(fd: libc::c_int, bytes: &mut [u8]) {
    let mut seen = 0;
    while seen < bytes.len() {
        let n = unsafe {
            libc::read(
                fd,
                bytes[seen..].as_mut_ptr() as *mut libc::c_void,
                bytes.len() - seen,
            )
        };
        // The `SIGCONT` this program sends its parent is *delivered* now, and
        // a signal wakes a parking read: without the retry the collector sees
        // an empty answer and every claim it owns reads against `-1`.
        if n < 0 && std::io::Error::last_os_error().raw_os_error() == Some(libc::EINTR) {
            continue;
        }
        if n <= 0 {
            return;
        }
        seen += n as usize;
    }
}

/// A target that has dropped to [`NOBODY`] and asks the questions the doors
/// put to the permission rule - the credential ones, the `SIGCONT` session
/// door, and its `SIGUSR1` control. It writes its answers as nine `i32`s and
/// exits; the parent owns the verdict, because the parent is the one that can
/// still print.
fn nobody_child(parent: libc::pid_t, out: libc::c_int, release: libc::c_int, detach: bool) -> ! {
    let mut answer = [-1i32; 9];
    // A session of its own is what makes the SIGCONT question *cross* one.
    // Child A answers 1 (it inherited the parent's session); child B has
    // moved, which is the only difference between the two shapes below.
    // `None` is a `setsid` that failed, and then there is no session question
    // to ask - the claim reads `-1` rather than a comparison of two answers
    // that were never about sessions.
    let mut detached: Option<bool> = Some(false);
    if detach && unsafe { libc::setsid() } <= 0 {
        detached = None;
    }
    // The drop is all three ids: a saved root id would let the child raise
    // itself back, and the rule asks about the target's *saved* uid as well.
    let dropped = unsafe {
        libc::setresgid(NOBODY, NOBODY, NOBODY) == 0 && libc::setresuid(NOBODY, NOBODY, NOBODY) == 0
    };
    answer[0] =
        if dropped && unsafe { libc::getuid() } == NOBODY && unsafe { libc::geteuid() } == NOBODY {
            0
        } else {
            -1
        };
    if detached.is_some() {
        answer[7] = (unsafe { libc::getsid(0) } == unsafe { libc::getsid(parent) }) as i32;
    }
    answer[1] = errno_of(unsafe { libc::kill(parent, 0) });
    answer[2] = errno_of(unsafe { libc::kill(libc::getpid(), 0) });
    answer[3] = syscall_errno(unsafe { libc::syscall(libc::SYS_tgkill, parent, parent, 0) });
    // The one signal the credential rule has a door beside: a continuation
    // crosses the session, and nothing else does. `SIGUSR1` is the control
    // (the parent swallows it, so a wrong answer cannot kill the program).
    answer[4] = errno_of(unsafe { libc::kill(parent, libc::SIGCONT) });
    answer[5] = errno_of(unsafe { libc::kill(parent, libc::SIGUSR1) });
    answer[8] =
        syscall_errno(unsafe { libc::syscall(libc::SYS_tgkill, parent, parent, libc::SIGCONT) });
    let info = RawInfo::queue(libc::SIGCONT);
    answer[6] = syscall_errno(unsafe {
        libc::syscall(
            libc::SYS_rt_sigqueueinfo,
            parent,
            libc::SIGCONT,
            info.0.as_ptr(),
        )
    });
    write_all(out, unsafe {
        std::slice::from_raw_parts(answer.as_ptr() as *const u8, std::mem::size_of_val(&answer))
    });
    // Wait for the parent's own check before exiting: `kill(zombie, 0)` is a
    // different question from `kill(live target, 0)`, and this claim is about
    // the live one. A byte is the parent's "I have looked".
    let mut byte = [0u8; 1];
    read_all(release, &mut byte);
    unsafe { libc::_exit(0) }
}

/// The control's handler: a `SIGUSR1` that the credential rule wrongly lets
/// through must not end the program before the red claim is printed.
extern "C" fn swallow_usr1(_: libc::c_int) {}

/// A uid-0 child that empties its capability sets after proving it can signal
/// a [`NOBODY`] grandchild with them.
///
/// The first answer is the capability's positive half and the second is the
/// same call with the sets emptied: only the capability can explain the pair,
/// because the uid comparison is false in both.
fn no_cap_child(out: libc::c_int) -> ! {
    // The grandchild announces its drop through a pipe, so the two kill calls
    // cannot race the identity move.
    let mut ready = [0 as libc::c_int; 2];
    unsafe { libc::pipe(ready.as_mut_ptr()) };
    let grandchild = unsafe { libc::fork() };
    if grandchild == 0 {
        unsafe { libc::close(ready[0]) };
        let dropped = unsafe {
            libc::setresgid(NOBODY, NOBODY, NOBODY) == 0
                && libc::setresuid(NOBODY, NOBODY, NOBODY) == 0
        };
        write_all(ready[1], &[dropped as u8]);
        // Long enough for both kills to land, short enough not to hold the
        // suite open; init reaps this one after its parent is gone.
        unsafe {
            libc::usleep(150_000);
            libc::_exit(0)
        }
    }
    unsafe { libc::close(ready[1]) };
    let mut dropped = [0u8; 1];
    read_all(ready[0], &mut dropped);
    let mut answer = [-1i32; 4];
    answer[0] = errno_of(unsafe { libc::kill(grandchild, 0) });
    let mut header = CapHeader {
        version: CAP_VERSION_3,
        pid: 0,
    };
    let mut sets = [CapRecord::default(); 2];
    let read = unsafe {
        libc::syscall(
            libc::SYS_capget,
            &mut header as *mut CapHeader as libc::c_long,
            sets.as_mut_ptr() as libc::c_long,
        )
    };
    let emptied = if read == 0 {
        sets = [CapRecord::default(); 2];
        let answer = unsafe {
            libc::syscall(
                libc::SYS_capset,
                &mut header as *mut CapHeader as libc::c_long,
                sets.as_mut_ptr() as libc::c_long,
            )
        };
        answer == 0
    } else {
        false
    };
    answer[1] = dropped[0] as i32 - 1; // 0 when the grandchild announced its drop
    answer[2] = if emptied { 0 } else { -1 };
    answer[3] = errno_of(unsafe { libc::kill(grandchild, 0) });
    write_all(out, unsafe {
        std::slice::from_raw_parts(answer.as_ptr() as *const u8, std::mem::size_of_val(&answer))
    });
    unsafe { libc::_exit(0) }
}

fn main() {
    println!("t-signal-permission");
    let mut r = Report::new();

    if host_run() {
        r.check(
            "host: another user's init is EPERM",
            errno_of(unsafe { libc::kill(1, 0) }) == libc::EPERM,
        );
        r.check(
            "host: the caller may signal itself",
            errno_of(unsafe { libc::kill(libc::getpid(), 0) }) == 0,
        );
        println!(
            "note: a host run cannot drop to uid {NOBODY}; the guest's uid and capability claims are named and not made"
        );
        r.finish();
    }

    // ---- a nobody child against its root parent, in the same session ----
    unsafe {
        libc::signal(
            libc::SIGUSR1,
            swallow_usr1 as *const () as libc::sighandler_t,
        )
    };
    let mut pipe = [0 as libc::c_int; 2];
    assert_eq!(unsafe { libc::pipe(pipe.as_mut_ptr()) }, 0);
    let mut release = [0 as libc::c_int; 2];
    assert_eq!(unsafe { libc::pipe(release.as_mut_ptr()) }, 0);
    let child = unsafe { libc::fork() };
    assert!(child >= 0, "fork failed");
    if child == 0 {
        unsafe {
            libc::close(pipe[0]);
            libc::close(release[1]);
        }
        nobody_child(unsafe { libc::getppid() }, pipe[1], release[0], false);
    }
    unsafe {
        libc::close(pipe[1]);
        libc::close(release[0]);
    }

    let mut answer = [-1i32; 9];
    read_all(pipe[0], unsafe {
        std::slice::from_raw_parts_mut(
            answer.as_mut_ptr() as *mut u8,
            std::mem::size_of_val(&answer),
        )
    });
    unsafe { libc::close(pipe[0]) };

    r.check(
        "the child dropped its real, effective and saved ids",
        answer[0] == 0,
    );
    r.check("the child is still in its parent's session", answer[7] == 1);
    r.check(
        "a 65534 child's kill(root parent, 0) is EPERM",
        answer[1] == libc::EPERM,
    );
    r.check("the same child may signal itself", answer[2] == 0);
    r.check(
        "a 65534 child's tgkill(root parent) is EPERM",
        answer[3] == libc::EPERM,
    );
    r.check(
        "the same session lets a 65534 child continue its parent",
        answer[4] == 0,
    );
    r.check(
        "the session door is SIGCONT's alone: SIGUSR1 is still EPERM",
        answer[5] == libc::EPERM,
    );
    r.check(
        "the record door takes the same session door",
        answer[6] == 0,
    );
    r.check("the tgkill door takes it too", answer[8] == 0);
    let live = errno_of(unsafe { libc::kill(child, 0) });
    write_all(release[1], &[0u8]);
    unsafe { libc::close(release[1]) };
    r.check("root may signal the 65534 child (CAP_KILL)", live == 0);
    let mut status = 0;
    unsafe { libc::waitpid(child, &mut status, 0) };

    // ---- the same child, in a session of its own ------------------------
    let mut pipe = [0 as libc::c_int; 2];
    assert_eq!(unsafe { libc::pipe(pipe.as_mut_ptr()) }, 0);
    let mut release = [0 as libc::c_int; 2];
    assert_eq!(unsafe { libc::pipe(release.as_mut_ptr()) }, 0);
    let child = unsafe { libc::fork() };
    assert!(child >= 0, "fork failed");
    if child == 0 {
        unsafe {
            libc::close(pipe[0]);
            libc::close(release[1]);
        }
        nobody_child(unsafe { libc::getppid() }, pipe[1], release[0], true);
    }
    unsafe {
        libc::close(pipe[1]);
        libc::close(release[0]);
    }

    let mut answer = [-1i32; 9];
    read_all(pipe[0], unsafe {
        std::slice::from_raw_parts_mut(
            answer.as_mut_ptr() as *mut u8,
            std::mem::size_of_val(&answer),
        )
    });
    unsafe { libc::close(pipe[0]) };

    r.check("the detached child dropped its ids", answer[0] == 0);
    r.check(
        "the detached child left the parent's session",
        answer[7] == 0,
    );
    r.check(
        "outside the session a 65534 child's SIGCONT is EPERM",
        answer[4] == libc::EPERM,
    );
    r.check(
        "the same child's SIGUSR1 is refused too",
        answer[5] == libc::EPERM,
    );
    r.check(
        "the record door's SIGCONT is refused outside the session",
        answer[6] == libc::EPERM,
    );
    r.check("so is the tgkill door's", answer[8] == libc::EPERM);
    let live = errno_of(unsafe { libc::kill(child, 0) });
    write_all(release[1], &[0u8]);
    unsafe { libc::close(release[1]) };
    r.check("root may signal the detached 65534 child", live == 0);
    unsafe { libc::waitpid(child, &mut status, 0) };

    // ---- the capability's own counterfactual ----------------------------
    let mut pipe = [0 as libc::c_int; 2];
    assert_eq!(unsafe { libc::pipe(pipe.as_mut_ptr()) }, 0);
    let child = unsafe { libc::fork() };
    assert!(child >= 0, "fork failed");
    if child == 0 {
        unsafe { libc::close(pipe[0]) };
        no_cap_child(pipe[1]);
    }
    unsafe { libc::close(pipe[1]) };
    let mut answer = [-1i32; 4];
    read_all(pipe[0], unsafe {
        std::slice::from_raw_parts_mut(
            answer.as_mut_ptr() as *mut u8,
            std::mem::size_of_val(&answer),
        )
    });
    unsafe { libc::close(pipe[0]) };
    r.check(
        "root with CAP_KILL may signal a 65534 grandchild",
        answer[0] == 0,
    );
    r.check("the grandchild dropped its ids", answer[1] == 0);
    r.check("capset emptied the child's sets", answer[2] == 0);
    r.check(
        "without CAP_KILL the same signal is EPERM",
        answer[3] == libc::EPERM,
    );
    unsafe { libc::waitpid(child, &mut status, 0) };

    // ---- the record door's own two refusals -----------------------------
    let mut pipe = [0 as libc::c_int; 2];
    assert_eq!(unsafe { libc::pipe(pipe.as_mut_ptr()) }, 0);
    let child = unsafe { libc::fork() };
    assert!(child >= 0, "fork failed");
    if child == 0 {
        unsafe { libc::close(pipe[0]) };
        let mut answer = [0i32; 2];
        let mut info = RawInfo::queue(libc::SIGCONT);
        info.0[8..12].copy_from_slice(&(-6i32).to_ne_bytes()); // SI_TKILL
        answer[0] = syscall_errno(unsafe {
            libc::syscall(
                libc::SYS_rt_sigqueueinfo,
                libc::getppid(),
                libc::SIGCONT,
                info.0.as_ptr(),
            )
        });
        let mut info = RawInfo::queue(libc::SIGCONT);
        info.0[8..12].copy_from_slice(&0i32.to_ne_bytes()); // SI_USER to another process
        answer[1] = syscall_errno(unsafe {
            libc::syscall(
                libc::SYS_rt_sigqueueinfo,
                libc::getppid(),
                libc::SIGCONT,
                info.0.as_ptr(),
            )
        });
        write_all(pipe[1], unsafe {
            std::slice::from_raw_parts(answer.as_ptr() as *const u8, std::mem::size_of_val(&answer))
        });
        unsafe { libc::_exit(0) }
    }
    unsafe { libc::close(pipe[1]) };
    let mut answer = [0i32; 2];
    read_all(pipe[0], unsafe {
        std::slice::from_raw_parts_mut(
            answer.as_mut_ptr() as *mut u8,
            std::mem::size_of_val(&answer),
        )
    });
    unsafe { libc::close(pipe[0]) };
    r.check(
        "rt_sigqueueinfo with SI_TKILL to another process is EPERM",
        answer[0] == libc::EPERM,
    );
    r.check(
        "rt_sigqueueinfo with SI_USER to another process is EPERM",
        answer[1] == libc::EPERM,
    );
    unsafe { libc::waitpid(child, &mut status, 0) };

    r.finish();
}
