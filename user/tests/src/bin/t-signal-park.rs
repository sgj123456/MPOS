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
//! # The bounded child
//!
//! A claim whose failure mode is "never returns" cannot be written as a call
//! that waits: the program would be killed at its budget and the claim would go
//! unnamed. Every parked call runs in a forked child; the parent waits with a
//! deadline, kills the child if its answer never comes, and reports `FAIL`
//! under a name - the shape `t-sock-timeout`'s `bounded_call` established.

use mpos_user_tests::Report;
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

    r.finish()
}
