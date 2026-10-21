//! t-exit-group - one test for exit_group, how musl terminates: the strace
//! closes with `exit_group(0)`.
//!
//! The code a process leaves with cannot be read from inside the dying
//! process, so it is read from just outside it: the test forks, the child
//! calls `exit_group(42)`, and the parent reaps it and checks the status the
//! kernel handed back. That is what "the code survives the scheduler to the
//! parent's wait" means, and it is observable - a test that simply exited 42
//! would leave the harness holding the code, reading it as a failure of the
//! very test that produced it.
//!
//! # The migration
//!
//! The child leaves through the raw `SYS_exit_group` rather than through
//! `libc::_exit`, because the syscall is this program's whole subject - and the
//! number comes from libc's per-machine table (231 here, 94 on the second
//! machine) rather than from this repository's memory of it. The raw door is
//! also the only one that *cannot flush*: musl's `_exit` is the same call
//! (measured from the crate's own static `libc.a`: `__syscall(SYS_exit_group,
//! status)`, with a loop of `SYS_exit` after it for the return that cannot
//! happen), but a forked std child also holds a copy of the parent's buffered
//! stdout, and an exit that flushed would print every line above it twice.
//!
//! The reaping end is `rustix::process::waitpid`, which *is* `wait4` with a null
//! `rusage` - the call the no-std version made by hand - and its `WaitStatus` is
//! where this program's `WIFEXITED`/`WEXITSTATUS` went: `exited()` and
//! `exit_status()` are those two rules, taken from the maintained table instead
//! of spelled out here.
//!
//! # The same call from a thread
//!
//! `exit_group` is not the leader's call: a libc calls it from whichever thread
//! reached `exit`, and Linux ends the **group** either way. What the parent
//! hears is the **process's** death, which is the group's **last thread**
//! leaving - the record names the leader, the status is the dying thread's own
//! code, and the signal is the leader's `clone` byte - and a leader that leaves
//! first on the thread door is a zombie the parent cannot reap until its group
//! ends. All four shapes are measured on a current host first
//! (`.workbuddy/probe_exit_group_thread.c`, five scenarios) and pinned below:
//! a worker's `exit_group(7)` reaped as the leader with 7; the leader's
//! `WNOHANG` over a zombie leader answering 0 while the worker runs; a
//! `clone(SIGUSR1)` group invisible to a plain wait and delivering SIGUSR1; and
//! a thread's child handed to a live task outside the dead group.
//!
//! The fixtures are two shapes of thread creation, and the split is measured
//! rather than stylistic: a forked child may use `std::thread`, while the
//! `clone(SIGUSR1)` leader cannot use libc's `clone` wrapper at all - musl
//! refuses `CLONE_THREAD` there by design (`flags & 0x290000` is `EINVAL`) - so
//! its worker is a `pthread` too, and the bare-syscall alternative is written
//! down as the failure it was (a child continuing on a stack no frame was built
//! for, SIGSEGV `0x8b`).

use mpos_user_tests::Report;
use rustix::process::{Pid, WaitOptions, waitpid};

/// The exit code the parent must observe.
const CODE: i32 = 42;

/// The code the worker in the thread-group section leaves with - distinct from
/// [`CODE`] so a wait that answered the leader's own byte could not pass.
const THREAD_CODE: i32 = 7;

/// `__WCLONE`, Linux's private `wait` bit: collect a "clone" child, one whose
/// `clone` byte is not `SIGCHLD`. Spelled the way `t-wait4` and `t-clone-exit`
/// spell it.
const WCLONE: i32 = 0x8000_0000u32 as i32;

/// Leave through `exit_group`, the call this program is about.
fn leave(code: i32) -> ! {
    unsafe {
        libc::syscall(libc::SYS_exit_group, code as libc::c_long);
        // The door does not return; `_exit` is the maintained fallback.
        libc::_exit(code)
    }
}

/// `wait4(pid, &status, options, NULL)`: the answer and the status word.
fn wait4_opts(pid: i32, options: i32) -> (isize, i32) {
    let mut status: i32 = 0;
    let answer = unsafe {
        libc::syscall(
            libc::SYS_wait4,
            pid as libc::c_long,
            &mut status as *mut i32 as libc::c_long,
            options as libc::c_long,
            0 as libc::c_long,
        )
    };
    (answer as isize, status)
}

/// The errno the last syscall left, if it failed.
fn errno() -> Option<i32> {
    std::io::Error::last_os_error().raw_os_error()
}

/// One `i32` through a pipe.
fn write_i32(fd: i32, value: i32) {
    unsafe { libc::write(fd, (&value as *const i32).cast(), 4) };
}

fn read_i32(fd: i32) -> i32 {
    let mut value: i32 = 0;
    let got = unsafe { libc::read(fd, (&mut value as *mut i32).cast(), 4) };
    if got == 4 { value } else { 0 }
}

/// [`read_i32`], but bounded: a fixture that broke would otherwise park the
/// parent on a pipe the child never writes, and a claim that never prints
/// reads as a hang - worse than one that fails.
fn read_i32_within(fd: i32, millis: i32) -> i32 {
    let mut poll = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    let ready = unsafe { libc::poll(&mut poll, 1, millis) };
    if ready > 0 { read_i32(fd) } else { 0 }
}

/// A signal handler that does nothing: the point is the interruption.
extern "C" fn wake_me(_: libc::c_int) {}

/// Arm a one-shot timer that ends a parked syscall.
///
/// The claims that make a **blocking** wait park must not hang the sweep when
/// the kernel is wrong - a claim that never prints reads as a hang, which is
/// worse than one that fails (`t-tid-dir`'s rule) - so the wait is made
/// interruptible: no `SA_RESTART`, and the signal is caught rather than fatal.
fn interrupt_after(seconds: u32) {
    unsafe {
        let mut act: libc::sigaction = core::mem::zeroed();
        act.sa_sigaction = wake_me as *const () as usize;
        act.sa_flags = 0;
        libc::sigemptyset(&mut act.sa_mask);
        libc::sigaction(libc::SIGALRM, &act, core::ptr::null_mut());
        libc::alarm(seconds);
    }
}

fn cancel_interrupt() {
    unsafe { libc::alarm(0) };
}

/// Park the calling task, but not for ever: a broken fixture must not hang the
/// sweep. The deadline ends the whole group with `0`, which makes the parent's
/// wait answer - with the wrong code, which is the point.
fn park_bounded() -> ! {
    let nap = libc::timespec {
        tv_sec: 0,
        tv_nsec: 10_000_000,
    };
    for _ in 0..300 {
        unsafe { libc::nanosleep(&nap, core::ptr::null_mut()) };
    }
    unsafe { libc::syscall(libc::SYS_exit_group, 0 as libc::c_long) };
    loop {}
}

/// Field 3 (the state letter) of a `stat`-shaped file.
fn stat_state(path: &str) -> Option<char> {
    let text = std::fs::read_to_string(path).ok()?;
    let rest = &text[text.rfind(')')? + 1..];
    rest.split_whitespace().next()?.chars().next()
}

/// One line of a `status`-shaped file's value, trimmed.
fn status_value(path: &str, key: &str) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    let prefix = format!("{key}:");
    text.lines().find_map(|line| {
        line.strip_prefix(&prefix)
            .map(|rest| rest.trim().to_string())
    })
}

/// A fork of this process that leads a group ending in `exit_group`, and the
/// worker of that group.
///
/// `leader_leaves` picks what the leader does once the worker has reported its
/// tid: park (the worker is the one that ends the group), or leave on
/// `SYS_exit` - Linux's `pthread_exit`, which ends only the calling thread and
/// leaves the leader a zombie. The worker reports its own kernel tid through
/// the pipe, then sleeps `delay_ms` and calls `exit_group(THREAD_CODE)`.
///
/// Answers `(leader, worker_tid)` in the parent; the child never returns.
fn spawn_group(leader_leaves: bool, delay_ms: u64) -> Option<(i32, i32)> {
    let mut report = [0i32; 2];
    if unsafe { libc::pipe(report.as_mut_ptr()) } != 0 {
        return None;
    }
    let pid = unsafe { libc::fork() };
    if pid == 0 {
        unsafe { libc::close(report[0]) };
        let fd = report[1];
        let (tid_tx, tid_rx) = std::sync::mpsc::channel::<i32>();
        let _worker = std::thread::spawn(move || {
            let _ = tid_tx.send(unsafe { libc::syscall(libc::SYS_gettid) } as i32);
            std::thread::sleep(std::time::Duration::from_millis(delay_ms));
            unsafe { libc::syscall(libc::SYS_exit_group, THREAD_CODE as libc::c_long) };
            loop {}
        });
        let tid = tid_rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .unwrap_or(0);
        write_i32(fd, tid);
        unsafe { libc::close(fd) };
        if tid == 0 {
            // The worker never named itself: end the group so the parent's
            // wait answers (with the wrong code) instead of hanging.
            unsafe { libc::_exit(1) };
        }
        if leader_leaves {
            // The thread door, not the group's: the process stays up on its
            // worker and the leader becomes the zombie Linux keeps for it.
            unsafe { libc::syscall(libc::SYS_exit, 0 as libc::c_long) };
        }
        park_bounded()
    }
    unsafe { libc::close(report[1]) };
    let tid = read_i32_within(report[0], 2000);
    unsafe { libc::close(report[0]) };
    Some((pid, tid))
}

/// The same, with the worker forking a child of its own first: the child is the
/// one a group's death must hand to init. Answers `(leader, child)`.
fn spawn_group_with_child() -> Option<(i32, i32)> {
    let mut report = [0i32; 2];
    if unsafe { libc::pipe(report.as_mut_ptr()) } != 0 {
        return None;
    }
    let pid = unsafe { libc::fork() };
    if pid == 0 {
        unsafe { libc::close(report[0]) };
        let fd = report[1];
        let _worker = std::thread::spawn(move || {
            let child = unsafe { libc::fork() };
            if child == 0 {
                // The thread's child: it parks, so the reparenting can be read
                // after the group is gone. It owns no copy of the report pipe
                // by the time the parent reads, because it closes it.
                unsafe { libc::close(fd) };
                loop {
                    unsafe { libc::pause() };
                }
            }
            write_i32(fd, child);
            unsafe { libc::close(fd) };
            std::thread::sleep(std::time::Duration::from_millis(150));
            unsafe { libc::syscall(libc::SYS_exit_group, THREAD_CODE as libc::c_long) };
            loop {}
        });
        park_bounded()
    }
    unsafe { libc::close(report[1]) };
    let child = read_i32_within(report[0], 2000);
    unsafe { libc::close(report[0]) };
    Some((pid, child))
}

/// The same, but the child belongs to a **sibling** of the thread that ends the
/// group: the sibling never runs the exit path, so only `exit_group` can hand
/// its child on. Answers `(leader, child)`.
fn spawn_group_with_sibling_child() -> Option<(i32, i32)> {
    let mut report = [0i32; 2];
    if unsafe { libc::pipe(report.as_mut_ptr()) } != 0 {
        return None;
    }
    let pid = unsafe { libc::fork() };
    if pid == 0 {
        let read_fd = report[0];
        let write_fd = report[1];
        let _sibling = std::thread::spawn(move || {
            let child = unsafe { libc::fork() };
            if child == 0 {
                unsafe { libc::close(write_fd) };
                loop {
                    unsafe { libc::pause() };
                }
            }
            write_i32(write_fd, child);
            unsafe { libc::close(write_fd) };
            // The leader ends the group; this thread never runs its own exit
            // path, which is exactly what the claim below is about.
            loop {
                unsafe { libc::pause() };
            }
        });
        // The leader, not the forking thread, is the one that calls
        // `exit_group`: the sibling is ended by the group's death, so its
        // child has no exit path of its own to be handed on by.
        let child = read_i32_within(read_fd, 2000);
        unsafe { libc::close(read_fd) };
        if child == 0 {
            unsafe { libc::_exit(1) };
        }
        unsafe { libc::syscall(libc::SYS_exit_group, THREAD_CODE as libc::c_long) };
        loop {}
    }
    unsafe { libc::close(report[1]) };
    let child = read_i32_within(report[0], 2000);
    unsafe { libc::close(report[0]) };
    Some((pid, child))
}

/// A group born through raw `clone(SIGUSR1)`, whose worker ends it. The child
/// is the leader - its word holds `SIGUSR1`, so a wait without `__WCLONE` must
/// not see it and its death must deliver that number, not `SIGCHLD`. Answers
/// the leader's pid (or a negative number).
///
/// The worker is a raw `CLONE_THREAD` task rather than a `pthread`, for the
/// reason `t-clone-exit`'s `spawn` gives: the leader is a raw `clone` child
/// whose libc was never told it is one, and its thread bookkeeping is not this
/// program's to touch. The child side of the `clone` continues on the stack it
/// was handed and does nothing but two syscalls, so no frame of the leader's is
/// read through a stack pointer that moved.
fn spawn_sigusr1_group() -> i32 {
    let pid = unsafe {
        libc::syscall(
            libc::SYS_clone,
            libc::SIGUSR1 as libc::c_long,
            0 as libc::c_long,
            0 as libc::c_long,
            0 as libc::c_long,
            0 as libc::c_long,
        )
    };
    if pid != 0 {
        return pid as i32;
    }
    // The leader. A plain `pthread`, not a raw `CLONE_THREAD`: musl's `clone`
    // wrapper refuses that flag by design (`flags & 0x290000` is `EINVAL`,
    // measured on the host - `CLONE_THREAD` belongs to `pthread_create`), and
    // a bare syscall that continues on a stack the compiler never set up read
    // its own frame out of fresh memory and died of SIGSEGV (status `0x8b`).
    let _worker = std::thread::spawn(|| {
        unsafe { libc::syscall(libc::SYS_exit_group, THREAD_CODE as libc::c_long) };
        loop {}
    });
    park_bounded()
}

fn main() {
    println!("t-exit-group");
    let mut r = Report::new();

    let child = unsafe { libc::fork() };
    if child == 0 {
        // Nothing after this runs: the group ends here, with `CODE`.
        leave(CODE);
    }
    r.check("fork answered a child", child > 0);

    let want = Pid::from_raw(child);
    let reaped = waitpid(want, WaitOptions::empty());
    r.check(
        "the exit_group'ing child is reaped by its pid",
        matches!(reaped, Ok(Some((got, _))) if Some(got) == want),
    );
    match reaped {
        Ok(Some((_, status))) => {
            r.check("the child left a status, not a signal", status.exited());
            r.check(
                "the code passed to exit_group is the one the parent waits for",
                status.exit_status() == Some(CODE),
            );
        }
        _ => {
            r.check("the child left a status, not a signal", false);
            r.check(
                "the code passed to exit_group is the one the parent waits for",
                false,
            );
        }
    }

    // ---- the same call from a thread --------------------------------------
    //
    // Linux's `do_group_exit` ends every thread of the group, and the parent
    // hears about the **process** - the leader's pid, the last thread's code,
    // the leader's exit byte - when the group's last thread leaves. Measured
    // whole on a current host first (`.workbuddy/probe_exit_group_thread.c`,
    // five scenarios); each fixture below is one of its shapes.

    // A. a worker's `exit_group(7)` is the process's death, and the worker's
    // tid is nobody's child.
    match spawn_group(false, 60) {
        Some((leader, worker)) => {
            r.check(
                "a group with a worker was started",
                leader > 0 && worker > 0,
            );
            let mut status = 0;
            // The wait starts while the leader is still alive, so it is the
            // *parked* half of the rule - and the timer is what keeps a wrong
            // kernel from hanging the sweep instead of failing the claim.
            interrupt_after(2);
            let answer = unsafe { libc::waitpid(leader, &mut status, 0) };
            cancel_interrupt();
            r.check(
                "a worker's exit_group is the process's death: the leader is reaped",
                answer == leader,
            );
            r.check(
                "with the code the worker passed",
                libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == THREAD_CODE,
            );
            let (worker_answer, _) = wait4_opts(worker, libc::WNOHANG);
            r.check(
                "and the worker's own tid is not a child (ECHILD)",
                worker_answer == -1 && errno() == Some(libc::ECHILD),
            );
        }
        None => {
            r.check("a group with a worker was started", false);
            r.check(
                "a worker's exit_group is the process's death: the leader is reaped",
                false,
            );
            r.check("with the code the worker passed", false);
            r.check("and the worker's own tid is not a child (ECHILD)", false);
        }
    }

    // B. a leader that leaves on the thread door first is a zombie, not a
    // corpse the parent can take: `WNOHANG` answers 0 until the group dies,
    // and then the wait answers with the *last thread's* code.
    match spawn_group(true, 100) {
        Some((leader, worker)) => {
            let path = format!("/proc/{leader}/stat");
            let mut zombie = false;
            for _ in 0..400 {
                if stat_state(&path) == Some('Z') {
                    zombie = true;
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            r.check(
                "a leader that leaves under a live worker is a zombie",
                zombie,
            );
            let (early, _) = wait4_opts(leader, libc::WNOHANG);
            r.check(
                "but not reapable yet: WNOHANG answers 0, not ECHILD",
                early == 0,
            );
            let mut status = 0;
            interrupt_after(2);
            let answer = unsafe { libc::waitpid(leader, &mut status, 0) };
            cancel_interrupt();
            r.check(
                "the group's death then answers for it, as the leader",
                answer == leader,
            );
            r.check(
                "with the last thread's code",
                libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == THREAD_CODE,
            );
            let (worker_answer, _) = wait4_opts(worker, libc::WNOHANG);
            r.check(
                "and the worker is still nobody's child",
                worker_answer == -1 && errno() == Some(libc::ECHILD),
            );
        }
        None => {
            r.check(
                "a leader that leaves under a live worker is a zombie",
                false,
            );
            r.check("but not reapable yet: WNOHANG answers 0, not ECHILD", false);
            r.check(
                "the group's death then answers for it, as the leader",
                false,
            );
            r.check("with the last thread's code", false);
            r.check("and the worker is still nobody's child", false);
        }
    }

    // C. a group born through `clone(SIGUSR1)`: a plain wait must not see it,
    // `__WCLONE` must, and its death must deliver the **leader's** byte.
    let old = {
        let mut both: libc::sigset_t = unsafe { core::mem::zeroed() };
        let mut old: libc::sigset_t = unsafe { core::mem::zeroed() };
        unsafe {
            libc::sigemptyset(&mut both);
            libc::sigaddset(&mut both, libc::SIGCHLD);
            libc::sigaddset(&mut both, libc::SIGUSR1);
            libc::sigprocmask(libc::SIG_BLOCK, &both, &mut old);
        }
        old
    };
    let sigusr1_group = spawn_sigusr1_group();
    if sigusr1_group > 0 {
        // The **wildcard** door, because it is the one that reads the byte the
        // *record* carries: a named wait asks the live task's (the leader's
        // SIGUSR1 either way), so only `wait4(-1, ..)` tells the record's byte
        // from the dying thread's.
        // A plain wait cannot see this group at all: its byte is the leader's
        // SIGUSR1, which is what makes it a "clone" child, and the live task's
        // byte is the one this door asks. (The record's own byte is only read
        // once the slot is gone, which `exit_code_pending` prevents while a
        // record waits - so this claim pins the live half, and the notification
        // below pins the delivery.)
        let (plain, _) = wait4_opts(-1, libc::WNOHANG);
        r.check(
            "a clone(SIGUSR1) group's death is invisible to a plain wait (ECHILD)",
            plain == -1 && errno() == Some(libc::ECHILD),
        );
        // Polled rather than parked for the same reason the two `waitpid`s are
        // bounded: a wrong kernel must fail the claim, not hang the sweep.
        let mut answer = 0;
        let mut status = 0;
        for _ in 0..400 {
            let (got, got_status) = wait4_opts(-1, WCLONE | libc::WNOHANG);
            if got != 0 {
                answer = got;
                status = got_status;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        r.check(
            "__WCLONE collects it as the leader",
            answer == sigusr1_group as isize,
        );
        r.check(
            "with the last thread's code",
            libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == THREAD_CODE,
        );
        let mut pending: libc::sigset_t = unsafe { core::mem::zeroed() };
        unsafe { libc::sigpending(&mut pending) };
        r.check(
            "and the death carries the leader's byte: SIGUSR1 is pending",
            unsafe { libc::sigismember(&pending, libc::SIGUSR1) } == 1,
        );
        r.check(
            "not SIGCHLD, which no task in that group was born with",
            unsafe { libc::sigismember(&pending, libc::SIGCHLD) } == 0,
        );
        // Drain the record before the mask goes back, or restoring it would
        // deliver the SIGUSR1 and kill this program.
        let mut one: libc::sigset_t = unsafe { core::mem::zeroed() };
        let zero = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        unsafe {
            libc::sigemptyset(&mut one);
            libc::sigaddset(&mut one, libc::SIGUSR1);
            // A zero timeout, not `sigwaitinfo`: a record that never arrived
            // must fail the claim above rather than park here. The raw door is
            // `t-clone-exit`'s: this libc defines no `siginfo_t` for linux, so
            // the named wrapper has no type to hand it.
            libc::syscall(
                libc::SYS_rt_sigtimedwait,
                &one as *const libc::sigset_t as libc::c_long,
                0 as libc::c_long,
                &zero as *const libc::timespec as libc::c_long,
                8 as libc::c_long,
            );
            libc::sigprocmask(libc::SIG_SETMASK, &old, core::ptr::null_mut());
        }
    } else {
        r.check(
            "a clone(SIGUSR1) group's death is invisible to a plain wait (ECHILD)",
            false,
        );
        r.check("__WCLONE collects it as the leader", false);
        r.check("with the last thread's code", false);
        r.check(
            "and the death carries the leader's byte: SIGUSR1 is pending",
            false,
        );
        r.check(
            "not SIGCHLD, which no task in that group was born with",
            false,
        );
        unsafe { libc::sigprocmask(libc::SIG_SETMASK, &old, core::ptr::null_mut()) };
    }

    // D. a thread's child is handed out of the group when it ends: no task of
    // the group is left to adopt it, so init is the parent.
    match spawn_group_with_child() {
        Some((leader, child)) => {
            // Polled: the wait starts while the group is alive, so a wrong
            // record would park it, and the sweep must not hang on a claim.
            let mut status = 0;
            let mut answer = 0;
            for _ in 0..400 {
                answer = unsafe { libc::waitpid(leader, &mut status, libc::WNOHANG) };
                if answer != 0 {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            r.check(
                "a thread's forked child was started under the group",
                child > 0 && answer == leader,
            );
            if child > 0 {
                let path = format!("/proc/{child}/status");
                let mut ppid = None;
                for _ in 0..400 {
                    ppid = status_value(&path, "PPid").and_then(|v| v.parse::<i32>().ok());
                    if ppid != Some(leader) {
                        break;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
                // Linux hands the child to the nearest **child subreaper** - pid
                // 1 only when nothing nearer exists - while this kernel has no
                // subreaper and names init. What both must show is that the dead
                // group let it go: a live task outside it, not the leader and not
                // nothing. The host's own reaper is named in that program's
                // output rather than assumed to be init.
                let ok = if mpos_user_tests::host_run() {
                    ppid.is_some_and(|p| {
                        p > 0
                            && Some(p) != Some(leader)
                            && std::path::Path::new(&format!("/proc/{p}")).exists()
                    })
                } else {
                    ppid == Some(1)
                };
                r.check(
                    "when the group ends, the thread's child names a live task outside it",
                    ok,
                );
                unsafe { libc::kill(child, libc::SIGKILL) };
            } else {
                r.check(
                    "when the group ends, the thread's child names a live task outside it",
                    false,
                );
            }
        }
        None => {
            r.check("a thread's forked child was started under the group", false);
            r.check(
                "when the group ends, the thread's child names a live task outside it",
                false,
            );
        }
    }

    // E. a **sibling's** child: the thread that forked it is ended by the
    // group, never runs the exit path, and so has no chance to hand the child
    // on itself. `exit_group`'s own walk is the only door - counterfactual 4.
    match spawn_group_with_sibling_child() {
        Some((leader, child)) => {
            let mut status = 0;
            let mut answer = 0;
            for _ in 0..400 {
                answer = unsafe { libc::waitpid(leader, &mut status, libc::WNOHANG) };
                if answer != 0 {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            r.check(
                "a sibling's forked child was started under the group",
                child > 0 && answer == leader,
            );
            if child > 0 {
                let path = format!("/proc/{child}/status");
                let mut ppid = None;
                for _ in 0..400 {
                    ppid = status_value(&path, "PPid").and_then(|v| v.parse::<i32>().ok());
                    if ppid != Some(leader) {
                        break;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
                // Same rule as D: Linux hands the child to its nearest child
                // subreaper, which this kernel spells as init. What the claim
                // pins is that the dead group let it go at all.
                let ok = if mpos_user_tests::host_run() {
                    ppid.is_some_and(|p| {
                        p > 0
                            && Some(p) != Some(leader)
                            && std::path::Path::new(&format!("/proc/{p}")).exists()
                    })
                } else {
                    ppid == Some(1)
                };
                r.check(
                    "the sibling's child names a live task outside the dead group",
                    ok,
                );
                unsafe { libc::kill(child, libc::SIGKILL) };
            } else {
                r.check(
                    "the sibling's child names a live task outside the dead group",
                    false,
                );
            }
        }
        None => {
            r.check(
                "a sibling's forked child was started under the group",
                false,
            );
            r.check(
                "the sibling's child names a live task outside the dead group",
                false,
            );
        }
    }

    r.finish()
}
