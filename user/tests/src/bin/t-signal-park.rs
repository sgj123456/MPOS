//! t-signal-park - a handled signal ends a parked wait with `EINTR`.
//!
//! Pinned here, per signal(7) and measured on Linux 7.2.11
//! (`build/scratch/sigpark/{e,p}.c`): a wait that is *interruptible* - the
//! `epoll_wait(2)` of an empty set with `-1`, a `read(2)` of a pipe with no
//! writer activity - returns `-1`/`EINTR` when a caught signal arrives, and the
//! handler has run by the time the call returns. A signal whose disposition is
//! `SIG_IGN` is not pending in Linux's sense and must **not** end the wait.
//!
//! Why it exists: weston on this machine was recorded parked after
//! `caught signal 15` (`park 1 block_until_on`, `docs/notes/` §7.4g), and the
//! first question about any such stall is whether the signal path itself can
//! end a park. It can - measured on the machine, `build/scratch/wsig/`: the
//! same probe that Linux answers `-1 errno=4` prints the same line there - and
//! the shape is a claim now so a later park rewrite cannot quietly lose it.
//!
//! # Whose park a process-directed signal ends
//!
//! The third shape is a *futex*, and it asks whose park the signal ends rather
//! than whether one can be ended: a leader that blocks `SIGTERM` while its only
//! worker unblocks it and parks in `FUTEX_WAIT`. Linux hands the copy to the
//! thread that may take it (`handler ran on the worker`, the worker's call
//! answering `EINTR`); this machine used to hand it to the leader's slot, where
//! the mask swallowed it and the parked worker was never woken
//! (`docs/notes/` §2.12).
//!
//! # The bounded child
//!
//! A claim whose failure mode is "never returns" cannot be written as a call
//! that waits: the program would be killed at its budget and the claim would go
//! unnamed. Every parked call runs in a forked child; the parent waits with a
//! deadline, kills the child if its answer never comes, and reports `FAIL`
//! under a name - the shape `t-sock-timeout`'s `bounded_call` established.

use mpos_user_tests::Report;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::time::{Duration, Instant};

/// The signal arrives well after the fork and the call: microseconds of work
/// against a third of a second.
const TERM_DELAY: Duration = Duration::from_millis(300);
/// How long the parent waits for the child's answer before deciding the wait
/// did not end. Wide enough for a loaded lane, short enough that a lost
/// interrupt reads as a failure in the same second.
const ANSWER_BUDGET: Duration = Duration::from_millis(4000);
const EINTR: i32 = 4;

/// Which door parks.
#[derive(Clone, Copy)]
enum Park {
    Epoll,
    PipeRead,
}

/// The child's own report: the call's return, its errno, and whether the
/// handler had run.
struct Answer {
    rc: i64,
    errno: i32,
    handler_ran: bool,
}

extern "C" fn on_term(_sig: libc::c_int) {
    // The handler's only job is to leave a mark the child reads back.
    unsafe { HANDLED = true };
}

static mut HANDLED: bool = false;

/// Run one parked wait in a child. `None` means the call never returned inside
/// [`ANSWER_BUDGET`]: the child was killed, and the caller reports the claim as
/// failed under its name.
fn parked_wait(park: Park, disposition: libc::sighandler_t) -> Option<Answer> {
    let mut answer = [0i32; 2];
    if unsafe { libc::pipe(answer.as_mut_ptr()) } != 0 {
        return None;
    }
    // The pipe a `read(2)` parks on, and the end the parent keeps open so the
    // read has a live writer and blocks instead of seeing EOF.
    let mut data = [0i32; 2];
    if unsafe { libc::pipe(data.as_mut_ptr()) } != 0 {
        return None;
    }
    let (answer_r, answer_w, data_r, data_w) = (answer[0], answer[1], data[0], data[1]);

    let pid = unsafe { libc::fork() };
    if pid < 0 {
        return None;
    }
    if pid == 0 {
        unsafe {
            libc::close(answer_r);
            libc::close(data_w);
            let mut sa: libc::sigaction = core::mem::zeroed();
            sa.sa_sigaction = if disposition == libc::SIG_IGN {
                libc::SIG_IGN
            } else {
                on_term as *const () as usize
            };
            sa.sa_flags = 0;
            libc::sigemptyset(&mut sa.sa_mask);
            libc::sigaction(libc::SIGTERM, &sa, core::ptr::null_mut());

            let mut errno: i32 = 0;
            let rc: i64 = match park {
                Park::Epoll => {
                    let ep = libc::epoll_create1(0);
                    let mut ev: libc::epoll_event = core::mem::zeroed();
                    let r = libc::epoll_wait(ep, &mut ev, 1, -1);
                    if r < 0 {
                        errno = *libc::__errno_location();
                    }
                    r as i64
                }
                Park::PipeRead => {
                    let mut byte = 0u8;
                    let r = libc::read(data_r, &mut byte as *mut u8 as *mut libc::c_void, 1);
                    if r < 0 {
                        errno = *libc::__errno_location();
                    }
                    r as i64
                }
            };
            let mut report = [0i32; 3];
            report[0] = rc as i32;
            report[1] = errno;
            report[2] = i32::from(HANDLED);
            libc::write(
                answer_w,
                report.as_ptr() as *const libc::c_void,
                core::mem::size_of_val(&report),
            );
            libc::_exit(0);
        }
    }
    unsafe {
        libc::close(answer_w);
        libc::close(data_r);
    }

    std::thread::sleep(TERM_DELAY);
    unsafe { libc::kill(pid, libc::SIGTERM) };

    let deadline = Instant::now() + ANSWER_BUDGET;
    let mut status = 0i32;
    loop {
        if unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) } == pid {
            break;
        }
        if Instant::now() >= deadline {
            unsafe {
                libc::kill(pid, libc::SIGKILL);
                libc::waitpid(pid, &mut status, 0);
                libc::close(answer_r);
                libc::close(data_w);
            }
            return None;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    unsafe { libc::close(data_w) };

    // The child answers before it exits, so the bytes are already there.
    let mut report = [0i32; 3];
    let got = unsafe {
        libc::read(
            answer_r,
            report.as_mut_ptr() as *mut libc::c_void,
            core::mem::size_of_val(&report),
        )
    };
    unsafe { libc::close(answer_r) };
    if got as usize != core::mem::size_of_val(&report) {
        return None;
    }
    Some(Answer {
        rc: report[0] as i64,
        errno: report[1],
        handler_ran: report[2] != 0,
    })
}

fn claim_park(r: &mut Report, park: Park, listed: &str, what: &str) {
    match parked_wait(park, libc::SIG_DFL) {
        Some(a) => {
            r.check(listed, a.rc == -1);
            r.check(
                &format!("and {what}'s call answers EINTR"),
                a.errno == EINTR,
            );
            r.check("and the handler had run by then", a.handler_ran);
        }
        None => {
            r.check(listed, false);
            r.check(&format!("and {what}'s call answers EINTR"), false);
            r.check("and the handler had run by then", false);
        }
    }
}

/// The third park: a `FUTEX_WAIT`, behind a sibling that blocks the signal.
///
/// This one asks a different question than the two above - not "can the signal
/// path end a park" but "**whose** park does a process-directed signal end".
/// Linux's answer, measured on 7.2.11 (`.workbuddy/probe_futex_signal.c`): the
/// copy goes on the thread group's shared pending set, where the first thread
/// whose mask lets it through takes it - so a leader that blocks `SIGTERM`
/// while its only worker unblocks it and parks in `FUTEX_WAIT` has the handler
/// run **on the worker**, and that worker's call returns `-1`/`EINTR`.
///
/// This machine answered neither: the copy stayed on the masked leader and the
/// parked worker was never woken, so a compositor's `SIGTERM` reached a locked
/// door (`docs/notes/` §2.12, and §7.4h's fourth and fifth readings, both with
/// `retired=false`). The claims are the worker's call, its errno, and *which*
/// thread ran the handler - the last is what tells a fixed kernel from one that
/// merely woke somebody.
static FUTEX_WORD: AtomicI32 = AtomicI32::new(0);
static FUTEX_READY: AtomicBool = AtomicBool::new(false);
static FUTEX_DONE: AtomicBool = AtomicBool::new(false);
static FUTEX_RC: AtomicI32 = AtomicI32::new(0);
static FUTEX_ERRNO: AtomicI32 = AtomicI32::new(0);
static FUTEX_HANDLED: AtomicBool = AtomicBool::new(false);
static FUTEX_HANDLER_TID: AtomicI32 = AtomicI32::new(0);
static FUTEX_WORKER_TID: AtomicI32 = AtomicI32::new(0);

extern "C" fn on_term_futex(_sig: libc::c_int) {
    let tid = unsafe { libc::syscall(libc::SYS_gettid) } as i32;
    FUTEX_HANDLER_TID.store(tid, Ordering::SeqCst);
    FUTEX_HANDLED.store(true, Ordering::SeqCst);
}

/// What the futex shape's child reports: the parked call's answer, whether the
/// handler ran, whether it ran on the parked thread, and whether the call ever
/// came back at all (a park nothing interrupts reports `returned: false` rather
/// than hanging the lane, which is the same bounded-child rule the other shapes
/// follow).
struct FutexAnswer {
    rc: i32,
    errno: i32,
    handler_ran: bool,
    on_parked_thread: bool,
    returned: bool,
}

fn parked_futex_behind_a_masked_leader() -> Option<FutexAnswer> {
    let mut answer = [0i32; 2];
    if unsafe { libc::pipe(answer.as_mut_ptr()) } != 0 {
        return None;
    }
    let (answer_r, answer_w) = (answer[0], answer[1]);

    let pid = unsafe { libc::fork() };
    if pid < 0 {
        return None;
    }
    if pid == 0 {
        unsafe {
            libc::close(answer_r);
            let mut sa: libc::sigaction = core::mem::zeroed();
            sa.sa_sigaction = on_term_futex as *const () as usize;
            sa.sa_flags = 0;
            libc::sigemptyset(&mut sa.sa_mask);
            libc::sigaction(libc::SIGTERM, &sa, core::ptr::null_mut());

            // The leader blocks it, and keeps it blocked: on Linux this copy is
            // not his to take - it is the group's, and the worker below is the
            // only thread whose mask lets it through.
            let mut set: libc::sigset_t = core::mem::zeroed();
            libc::sigemptyset(&mut set);
            libc::sigaddset(&mut set, libc::SIGTERM);
            libc::pthread_sigmask(libc::SIG_BLOCK, &set, core::ptr::null_mut());

            FUTEX_WORD.store(0, Ordering::SeqCst);
            FUTEX_READY.store(false, Ordering::SeqCst);
            FUTEX_DONE.store(false, Ordering::SeqCst);
            FUTEX_HANDLED.store(false, Ordering::SeqCst);
            FUTEX_HANDLER_TID.store(0, Ordering::SeqCst);
            let _worker = std::thread::spawn(|| {
                let mut set: libc::sigset_t = unsafe { core::mem::zeroed() };
                unsafe {
                    libc::sigemptyset(&mut set);
                    libc::sigaddset(&mut set, libc::SIGTERM);
                    libc::pthread_sigmask(libc::SIG_UNBLOCK, &set, core::ptr::null_mut());
                }
                let tid = unsafe { libc::syscall(libc::SYS_gettid) } as i32;
                FUTEX_WORKER_TID.store(tid, Ordering::SeqCst);
                FUTEX_READY.store(true, Ordering::SeqCst);
                // `FUTEX_WAIT | FUTEX_PRIVATE_FLAG`: sleep while the word is 0.
                let rc = unsafe {
                    libc::syscall(
                        libc::SYS_futex,
                        core::ptr::addr_of!(FUTEX_WORD) as *const u32,
                        128usize,
                        0u32,
                        0usize,
                        0usize,
                        0usize,
                    )
                } as i32;
                let errno = if rc < 0 {
                    unsafe { *libc::__errno_location() }
                } else {
                    0
                };
                FUTEX_RC.store(rc, Ordering::SeqCst);
                FUTEX_ERRNO.store(errno, Ordering::SeqCst);
                FUTEX_DONE.store(true, Ordering::SeqCst);
            });
            while !FUTEX_READY.load(Ordering::SeqCst) {
                std::thread::sleep(Duration::from_millis(1));
            }
            let deadline = Instant::now() + Duration::from_millis(3500);
            while !FUTEX_DONE.load(Ordering::SeqCst) && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(2));
            }
            let returned = FUTEX_DONE.load(Ordering::SeqCst);
            let handler_ran = FUTEX_HANDLED.load(Ordering::SeqCst);
            let handler_tid = FUTEX_HANDLER_TID.load(Ordering::SeqCst);
            let worker_tid = FUTEX_WORKER_TID.load(Ordering::SeqCst);
            let report = [
                if returned {
                    FUTEX_RC.load(Ordering::SeqCst)
                } else {
                    0
                },
                if returned {
                    FUTEX_ERRNO.load(Ordering::SeqCst)
                } else {
                    0
                },
                i32::from(handler_ran),
                i32::from(handler_ran && handler_tid == worker_tid),
                i32::from(returned),
            ];
            libc::write(
                answer_w,
                report.as_ptr() as *const libc::c_void,
                core::mem::size_of_val(&report),
            );
            libc::_exit(0);
        }
    }
    unsafe { libc::close(answer_w) };

    std::thread::sleep(TERM_DELAY);
    unsafe { libc::kill(pid, libc::SIGTERM) };

    let deadline = Instant::now() + ANSWER_BUDGET;
    let mut status = 0i32;
    loop {
        if unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) } == pid {
            break;
        }
        if Instant::now() >= deadline {
            unsafe {
                libc::kill(pid, libc::SIGKILL);
                libc::waitpid(pid, &mut status, 0);
                libc::close(answer_r);
            }
            return None;
        }
        std::thread::sleep(Duration::from_millis(10));
    }

    let mut report = [0i32; 5];
    let got = unsafe {
        libc::read(
            answer_r,
            report.as_mut_ptr() as *mut libc::c_void,
            core::mem::size_of_val(&report),
        )
    };
    unsafe { libc::close(answer_r) };
    if got as usize != core::mem::size_of_val(&report) {
        return None;
    }
    Some(FutexAnswer {
        rc: report[0],
        errno: report[1],
        handler_ran: report[2] != 0,
        on_parked_thread: report[3] != 0,
        returned: report[4] != 0,
    })
}

fn claim_futex_park(r: &mut Report) {
    match parked_futex_behind_a_masked_leader() {
        Some(a) => {
            r.check(
                "a process-directed signal reaches a futex waiter whose sibling blocks it",
                a.returned && a.rc == -1,
            );
            r.check("and that futex_wait call answers EINTR", a.errno == EINTR);
            r.check(
                "and the handler ran on the parked thread, not the masked leader",
                a.handler_ran && a.on_parked_thread,
            );
        }
        None => {
            r.check(
                "a process-directed signal reaches a futex waiter whose sibling blocks it",
                false,
            );
            r.check("and that futex_wait call answers EINTR", false);
            r.check(
                "and the handler ran on the parked thread, not the masked leader",
                false,
            );
        }
    }
}

fn main() {
    let mut r = Report::new();

    claim_park(
        &mut r,
        Park::Epoll,
        "a parked epoll_wait is ended by a handled signal",
        "epoll_wait",
    );
    claim_park(
        &mut r,
        Park::PipeRead,
        "a parked read of a pipe is ended by a handled signal",
        "read",
    );

    // An ignored signal is not an interruption: the child stays parked, the
    // parent's deadline expires, and the child is killed.
    r.check(
        "an ignored signal does not end the park",
        parked_wait(Park::Epoll, libc::SIG_IGN).is_none(),
    );

    // And the shape a masked leader makes of the same question.
    claim_futex_park(&mut r);

    r.finish()
}
