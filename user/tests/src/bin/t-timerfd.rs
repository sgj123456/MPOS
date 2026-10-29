//! t-timerfd - the timer that reports through a descriptor.
//!
//! Pinned here, per timerfd_create(2): the five clocks are the ones the
//! call takes (and a CPU-time clock is EINVAL), the create flags are
//! TFD_NONBLOCK and TFD_CLOEXEC alone, a read shorter than eight bytes is
//! the man page's own EINVAL, an empty nonblocking read is EAGAIN, a
//! disarmed timer gettimes as zeros, an arm is visible in the remaining
//! time, a one-shot's read hands back exactly one expiration, an interval
//! timer's read hands back the pile that built up, and the disarm a zero
//! `it_value` spells is real.
//!
//! rustix binds the whole family (`timerfd_create`/`timerfd_settime`/
//! `timerfd_gettime`, `ClockId`-typed) except the two out-of-vocabulary
//! words this program claims: the unknown create flag 0xFFFF rides the raw
//! door (clause (c) against rustix's typed flags), and the CPU-time clock -
//! a `ClockId` variant rustix *does* name but timerfd refuses - rides it
//! too, because the claim is the refusal at the syscall level.

use mpos_user_tests::Report;
use rustix::fs::{AtFlags, CWD};
use rustix::io::{Errno, read};
use rustix::pipe::{PipeFlags, pipe_with};
use rustix::time::{
    Itimerspec, TimerfdClockId, TimerfdFlags, TimerfdTimerFlags, Timespec, timerfd_create,
    timerfd_gettime, timerfd_settime,
};
use std::os::fd::AsRawFd;

/// The machine's `struct epoll_event`, x86_64-only packing and all - the same
/// shape `t-epoll` spells, because the wait writes it back.
#[cfg(target_arch = "x86_64")]
#[repr(C, packed(4))]
#[derive(Clone, Copy)]
struct Ev {
    events: u32,
    data: u64,
}
#[cfg(target_arch = "aarch64")]
#[repr(C)]
#[derive(Clone, Copy)]
struct Ev {
    events: u32,
    data: u64,
}

/// The door returns `-1` and leaves the refusal in `errno` (batch 12's
/// measurement); the claim is about what the kernel answered.
fn refused(ret: isize, want: i32) -> bool {
    ret == -1 && std::io::Error::last_os_error().raw_os_error() == Some(want)
}

fn sleep_ms(ms: i64) {
    let req = Timespec {
        tv_sec: ms / 1000,
        tv_nsec: (ms % 1000) * 1_000_000,
    };
    let _ = rustix::thread::nanosleep(&req);
}

fn main() {
    println!("t-timerfd");
    let mut r = Report::new();

    // The clockids the call takes - and the CPU-time one it does not.
    let cpu = unsafe {
        libc::syscall(
            libc::SYS_timerfd_create,
            libc::CLOCK_PROCESS_CPUTIME_ID,
            0u64,
        ) as isize
    };
    r.check(
        "a CPU-time clock answers EINVAL",
        refused(cpu, libc::EINVAL),
    );
    let bad_flags = unsafe {
        libc::syscall(libc::SYS_timerfd_create, libc::CLOCK_MONOTONIC, 0xFFFFu64) as isize
    };
    r.check(
        "unknown create flags answer EINVAL",
        refused(bad_flags, libc::EINVAL),
    );
    let fd = match timerfd_create(TimerfdClockId::Monotonic, TimerfdFlags::empty()) {
        Ok(fd) => {
            r.check("create hands back a descriptor", true);
            fd
        }
        Err(_) => {
            r.check("create hands back a descriptor", false);
            r.finish();
        }
    };

    // The read's own validation: fewer than eight bytes is EINVAL, and an
    // empty nonblocking timer is EAGAIN rather than a park.
    let mut four = [0u8; 4];
    r.check(
        "a 4-byte read answers EINVAL",
        read(&fd, &mut four) == Err(Errno::INVAL),
    );

    let nb = match timerfd_create(TimerfdClockId::Monotonic, TimerfdFlags::NONBLOCK) {
        Ok(fd) => {
            r.check("the nonblocking create succeeds", true);
            fd
        }
        Err(_) => {
            r.check("the nonblocking create succeeds", false);
            r.finish();
        }
    };
    let mut eight = [0u8; 8];
    r.check(
        "an empty nonblocking read answers EAGAIN",
        read(&nb, &mut eight) == Err(Errno::AGAIN),
    );

    // A disarmed timer gettimes as zeros throughout.
    match timerfd_gettime(&fd) {
        Ok(curr) => {
            r.check("gettime on a fresh timer succeeds", true);
            r.check(
                "a disarmed timer reports zeros",
                curr.it_value.tv_sec == 0
                    && curr.it_value.tv_nsec == 0
                    && curr.it_interval.tv_sec == 0,
            );
        }
        Err(_) => {
            r.check("gettime on a fresh timer succeeds", false);
            r.check("a disarmed timer reports zeros", false);
        }
    }

    // Arm 200ms, once. The remaining time is somewhere inside the span,
    // and the read after it passes hands back exactly one expiration.
    let arm = Itimerspec {
        it_interval: Timespec {
            tv_sec: 0,
            tv_nsec: 0,
        },
        it_value: Timespec {
            tv_sec: 0,
            tv_nsec: 50_000_000,
        },
    };
    match timerfd_settime(&fd, TimerfdTimerFlags::empty(), &arm) {
        Ok(old) => {
            r.check("settime arming 50ms succeeds", true);
            r.check(
                "the old setting of a fresh disarm is zeros",
                old.it_value.tv_sec == 0 && old.it_value.tv_nsec == 0,
            );
        }
        Err(_) => {
            r.check("settime arming 50ms succeeds", false);
            r.check("the old setting of a fresh disarm is zeros", false);
        }
    }
    match timerfd_gettime(&fd) {
        Ok(curr) => {
            r.check("gettime on the armed timer succeeds", true);
            r.check(
                "the remaining time is inside the armed span",
                // The arm is rounded *up* to the machine's tick - 250 Hz, so
                // 50 ms is 13 ticks and reads back as 52 - which is why the
                // bound carries one tick of slack rather than the bare span.
                // (The 200ms this used to arm was exactly 50 ticks, so the
                // rounding was invisible; the remeasurement caught it.)
                curr.it_value.tv_sec == 0
                    && curr.it_value.tv_nsec > 0
                    && curr.it_value.tv_nsec <= 60_000_000,
            );
        }
        Err(_) => {
            r.check("gettime on the armed timer succeeds", false);
            r.check("the remaining time is inside the armed span", false);
        }
    }

    // A gettime on something that is not a timer: EINVAL.
    let pipes = pipe_with(PipeFlags::empty());
    match &pipes {
        Ok(p) => {
            r.check("the pipe pair is made", true);
            let e = timerfd_gettime(&p.0).err();
            r.check("gettime on a pipe answers EINVAL", e == Some(Errno::INVAL));
        }
        Err(_) => {
            r.check("the pipe pair is made", false);
            r.check("gettime on a pipe answers EINVAL", false);
        }
    }

    // A budget: the timer fires 50ms after it was armed, and this only has to
    // be after that (`docs/notes/` §15.93).
    sleep_ms(80);
    let mut eight = [0u8; 8];
    let n = read(&fd, &mut eight);
    r.check("the one-shot's read takes 8 bytes", n == Ok(8));
    r.check(
        "the count is exactly one expiration",
        n == Ok(8) && u64::from_ne_bytes(eight) == 1,
    );
    // Spent: what is left of a fired one-shot is nothing.
    match timerfd_gettime(&fd) {
        Ok(curr) => {
            r.check("gettime after the read succeeds", true);
            r.check(
                "a spent one-shot reports zeros",
                curr.it_value.tv_sec == 0 && curr.it_value.tv_nsec == 0,
            );
        }
        Err(_) => {
            r.check("gettime after the read succeeds", false);
            r.check("a spent one-shot reports zeros", false);
        }
    }

    // An interval timer: the expirations pile up while nobody reads, and
    // the read takes the whole pile in one u64.
    let arm = Itimerspec {
        it_interval: Timespec {
            tv_sec: 0,
            tv_nsec: 10_000_000,
        },
        it_value: Timespec {
            tv_sec: 0,
            tv_nsec: 10_000_000,
        },
    };
    r.check(
        "settime arming the interval succeeds",
        timerfd_settime(&fd, TimerfdTimerFlags::empty(), &arm).is_ok(),
    );
    sleep_ms(40);
    let mut eight = [0u8; 8];
    let n = read(&fd, &mut eight);
    r.check("the interval read takes 8 bytes", n == Ok(8));
    let count = u64::from_ne_bytes(eight);
    r.check("the pile is several expirations", (2..=12).contains(&count));

    // The disarm: a zero it_value is the man page's own spelling, and the
    // gettime after it agrees.
    let disarm = Itimerspec {
        it_interval: Timespec {
            tv_sec: 0,
            tv_nsec: 0,
        },
        it_value: Timespec {
            tv_sec: 0,
            tv_nsec: 0,
        },
    };
    r.check(
        "settime with a zero value disarms",
        timerfd_settime(&fd, TimerfdTimerFlags::empty(), &disarm).is_ok(),
    );
    match timerfd_gettime(&fd) {
        Ok(curr) => {
            r.check("gettime after the disarm succeeds", true);
            r.check(
                "the disarmed timer reports zeros",
                curr.it_value.tv_sec == 0
                    && curr.it_value.tv_nsec == 0
                    && curr.it_interval.tv_sec == 0,
            );
        }
        Err(_) => {
            r.check("gettime after the disarm succeeds", false);
            r.check("the disarmed timer reports zeros", false);
        }
    }

    // ---- epoll: the expiration reaches a *park*, not a later look --------
    //
    // A timerfd's readiness is the clock's own work: no task calls a wake for
    // it, so a park that watches one must re-ask at least once a tick - the
    // one-tick floor `poll(2)` has always kept under every timeout. Before the
    // floor existed (`docs/notes/` §3.13, item 125) a 150 ms timer was
    // answered by `epoll_wait` at its own 3000 ms timeout, which is what this
    // claim is the counterfactual against: it demands the *arrival* inside a
    // second, so a wait that only notices at the timeout fails.
    {
        let t = timerfd_create(TimerfdClockId::Monotonic, TimerfdFlags::empty());
        r.check(
            "a monotonic timerfd for the epoll wake claim exists",
            t.is_ok(),
        );
        if let Ok(t) = t {
            let ep = unsafe { libc::epoll_create1(0) };
            r.check("an epoll instance watches it", ep >= 0);
            let mut armed_ok = false;
            if ep >= 0 {
                let mut ev = Ev {
                    events: libc::EPOLLIN as u32,
                    data: 0x5eed_u64,
                };
                let added = unsafe {
                    libc::epoll_ctl(
                        ep,
                        libc::EPOLL_CTL_ADD,
                        t.as_raw_fd(),
                        &mut ev as *mut Ev as *mut libc::epoll_event,
                    )
                };
                r.check("the timerfd is in the interest list", added == 0);
                let its = Itimerspec {
                    it_interval: Timespec {
                        tv_sec: 0,
                        tv_nsec: 0,
                    },
                    it_value: Timespec {
                        tv_sec: 0,
                        tv_nsec: 100_000_000,
                    },
                };
                armed_ok = timerfd_settime(&t, TimerfdTimerFlags::empty(), &its).is_ok();
            }
            r.check("it is armed for 100 ms", armed_ok);
            if ep >= 0 && armed_ok {
                let mut out = [0u8; 16];
                let started = std::time::Instant::now();
                let got = unsafe {
                    libc::epoll_wait(ep, out.as_mut_ptr() as *mut libc::epoll_event, 1, 2000)
                };
                let waited = started.elapsed();
                r.check(
                    "the expiration wakes the park inside a second",
                    got == 1 && waited < std::time::Duration::from_millis(1000),
                );
                r.check(
                    "the report is EPOLLIN",
                    got == 1
                        && u32::from_ne_bytes([out[0], out[1], out[2], out[3]])
                            & (libc::EPOLLIN as u32)
                            != 0,
                );
            }
            if ep >= 0 {
                unsafe {
                    libc::close(ep);
                }
            }
        }
    }

    drop((pipes, nb, fd));
    let _ = AtFlags::empty();
    let _ = CWD;
    r.finish()
}
