//! t-poll - one test for poll: does it report what is actually ready, and
//! does it wait when there is nothing to report?
//!
//! The strace's shape starts it off: Rust std sanitizes fds 0-2 at startup
//! with `events=0, timeout=0`, asking nothing, so nothing can be ready and
//! the answer is 0 with every revents cleared. The test seeds revents with a
//! sentinel, so a kernel that scribbles on the array cannot hide.
//!
//! Beyond that, this pins what a *working* poll does, because a shell is built
//! on it: `ash` polls stdin and reads what the poll says arrived, so a poll
//! that always answers "nothing" is a shell that decides its terminal hung up.
//!
//! * a writable descriptor asked for POLLOUT is ready, and says so;
//! * a pipe with nothing in it and a writer still attached is not ready, and
//!   with a timeout of 0 the call says so instead of waiting;
//! * once a byte lands in it, the same request is ready;
//! * a negative fd is a placeholder, not a dead descriptor: skipped whole,
//!   so a poll array shaped like musl's resolver waits on its real sockets;
//! * and the skip holds for the *wait* too, not only the report: a blocking
//!   poll of quiet descriptors beside a placeholder has to park until another
//!   task makes one ready, which is the shape `apk add` polls its trigger
//!   child's pipes in.
//!
//! # The migration
//!
//! The no-std version handed the kernel a caller-built `struct pollfd` array
//! and read `revents` back raw. Three claims live outside `rustix::event::poll`:
//!
//! * **the seeded revents.** `PollFd::new` zero-fills the slot, so "the
//!   sentinel came back cleared, not answered" cannot be stated through it -
//!   clause (c), the buffer-ownership shape again. The array here is the
//!   caller's own `libc::pollfd`s, and the events words come from
//!   `rustix::event::PollFlags`' numbers (`IN`/`OUT` are named variants).
//! * **the negative fd.** `BorrowedFd::borrow_raw` refuses `-1` by contract,
//!   so the placeholder slots cannot exist inside a rustix array at all. That
//!   is the resolver's shape and the whole subject of two claims.
//! * **the timeout word.** `rustix::event::poll` takes an
//!   `Option<&Timespec>` and reaches Linux through `ppoll`; the ABI promise is
//!   poll(2)'s millisecond integer, and `timeout = -1` (block forever) is a
//!   word the Timespec spelling cannot carry. So the call is
//!   `libc::syscall(SYS_poll, ...)` - 7 here, 73 on the second machine - with
//!   nfds and timeout as Linux passes them.
//!
//! The trigger-shaped child keeps the no-std program's fork/pipe/wait4
//! sequence exactly (`libc::fork`, raw `SYS_exit`, `libc::wait4`): std's
//! Command would change the call sequence the claim is about, and musl's
//! exit_group would flush the parent's stdout buffer a second time.

use mpos_user_tests::Report;
use rustix::event::PollFlags;
use std::mem::size_of;

/// The two event words, taken from rustix's maintained table and narrowed to
/// the `short` the kernel's array carries.
const POLL_IN: i16 = PollFlags::IN.bits() as i16;
const POLL_OUT: i16 = PollFlags::OUT.bits() as i16;

/// The `struct pollfd` as Linux defines it - the caller's array, so the
/// sentinel seed and the negative fd are both expressible.
#[repr(C)]
#[derive(Clone, Copy)]
struct Sock {
    fd: i32,
    events: i16,
    revents: i16,
}

/// `poll(fds, nfds, timeout_ms)` through the raw door. Returns the answer, or
/// the negative errno the way the no-std harness reported it.
#[cfg(target_arch = "x86_64")]
fn poll(fds: &mut [Sock], timeout: i32) -> isize {
    let r = unsafe {
        libc::syscall(
            libc::SYS_poll,
            fds.as_mut_ptr() as libc::c_long,
            fds.len() as libc::c_long,
            timeout as libc::c_long,
        )
    };
    fix(r)
}

/// The same call on the machine with no `poll` number: `ppoll(fds, nfds,
/// &timespec, NULL, 0)`, which is how musl spells poll there and which reaches
/// the kernel's one `do_sys_poll`. arm64 has no `SYS_poll` at all
/// (`nr_aarch64.rs`: `POLL = 64003; // no arm64 number; unreachable on
/// aarch64`, where `PPOLL = 73` carries the family), exactly the shape batch
/// 4 measured for `dup2`. The millisecond word becomes a timespec; `-1` stays
/// block-forever, which ppoll spells as a null timeout pointer.
#[cfg(not(target_arch = "x86_64"))]
fn poll(fds: &mut [Sock], timeout: i32) -> isize {
    let ts = libc::timespec {
        tv_sec: (timeout / 1000) as _,
        tv_nsec: ((timeout % 1000) * 1_000_000) as libc::c_long,
    };
    let tsp = if timeout < 0 { core::ptr::null() } else { &ts };
    let r = unsafe {
        libc::syscall(
            libc::SYS_ppoll,
            fds.as_mut_ptr() as libc::c_long,
            fds.len() as libc::c_long,
            tsp as libc::c_long,
            0 as libc::c_long,
            0 as libc::c_long,
        )
    };
    fix(r)
}

fn fix(r: libc::c_long) -> isize {
    if r < 0 {
        -(std::io::Error::last_os_error().raw_os_error().unwrap_or(0) as isize)
    } else {
        r as isize
    }
}

fn main() {
    println!("t-poll");
    let mut r = Report::new();

    // The layout claim the migrated struct rests on: the caller's array is
    // byte-for-byte what the kernel reads.
    r.check(
        "the pollfd array is the size Linux reads",
        size_of::<Sock>() == size_of::<libc::pollfd>(),
    );

    // The strace's call: nothing asked for, nothing ready. std owns fds 0-2,
    // and the point is that the *kernel* clears the seeded revents even for a
    // slot nobody asked about.
    let mut fds = [
        Sock {
            fd: 0,
            events: 0,
            revents: 0x5a5a_u16 as i16,
        },
        Sock {
            fd: 1,
            events: 0,
            revents: 0x5a5a_u16 as i16,
        },
        Sock {
            fd: 2,
            events: 0,
            revents: 0x5a5a_u16 as i16,
        },
    ];
    r.check(
        "poll over fds 0-2 with events=0, timeout=0 reports nothing ready",
        poll(&mut fds, 0) == 0,
    );
    r.check(
        "no revents come back for events=0",
        fds[0].revents == 0 && fds[1].revents == 0 && fds[2].revents == 0,
    );

    // nfds = 0: the call is trivially over before it starts.
    r.check("poll with an nfds of 0 answers 0", poll(&mut [], 0) == 0);

    // A descriptor that can always be written to is ready, and the reason is
    // reported - not just "something happened".
    let mut one = [Sock {
        fd: 1,
        events: POLL_OUT,
        revents: 0x5555_u16 as i16,
    }];
    r.check("stdout asked for POLLOUT is ready", poll(&mut one, 0) == 1);
    r.check(
        "the readiness comes back as POLLOUT, not the sentinel",
        one[0].revents == POLL_OUT,
    );

    // A pipe with no data and a writer still attached: not readable, and a
    // timeout of 0 means "say so" rather than "wait".
    let mut pair = [-1i32; 2];
    let made = unsafe { libc::pipe2(pair.as_mut_ptr(), 0) } == 0;
    r.check("pipe2 for the poll test succeeds", made);
    if !made {
        r.finish()
    }
    let mut one = [Sock {
        fd: pair[0],
        events: POLL_IN,
        revents: 0x5555_u16 as i16,
    }];
    r.check(
        "an empty pipe with a live writer is not ready",
        poll(&mut one, 0) == 0,
    );
    r.check(
        "and the revents sentinel is cleared rather than answered",
        one[0].revents == 0,
    );

    // Once something is in it, the same request is ready. This is the whole
    // point: a shell waits on this and reads what the poll named.
    let wrote = unsafe { libc::write(pair[1], b"x".as_ptr() as *const libc::c_void, 1) } == 1;
    r.check("the byte lands in the pipe", wrote);
    let mut one = [Sock {
        fd: pair[0],
        events: POLL_IN,
        revents: 0x5555_u16 as i16,
    }];
    r.check(
        "a pipe with a byte in it is ready to read",
        poll(&mut one, 0) == 1,
    );
    r.check("and says POLLIN", one[0].revents == POLL_IN);

    // The resolver's shape: placeholder slots at negative fds beside a live
    // one, with revents the caller never initialized. poll(2) skips a
    // negative fd whole - events ignored, revents zeroed, never counted as
    // ready. Answering POLLNVAL for them instead reports "ready" for slots
    // nobody asked about, and musl's name lookups - which poll exactly this
    // array, TCP-fallback slots parked at -1 beside the UDP socket - spin to
    // their timeout and come back to busybox as `bad address`.
    let mut mixed = [
        Sock {
            fd: -1,
            events: 0,
            revents: 0x5a5a_u16 as i16,
        },
        Sock {
            fd: pair[0],
            events: POLL_IN,
            revents: 0x5a5a_u16 as i16,
        },
        Sock {
            fd: -2,
            events: 0,
            revents: 0x5a5a_u16 as i16,
        },
    ];
    r.check(
        "negative fds are skipped: only the live pipe counts as ready",
        poll(&mut mixed, 0) == 1,
    );
    r.check(
        "the live pipe still says POLLIN",
        mixed[1].revents == POLL_IN,
    );
    r.check(
        "and the placeholder slots come back cleared, not NVAL",
        mixed[0].revents == 0 && mixed[2].revents == 0,
    );

    unsafe {
        libc::close(pair[0]);
        libc::close(pair[1]);
    }

    // The same array, but *blocking*. Everything above asks with a timeout of
    // 0, so the call answers before it ever considers waiting and cannot tell
    // a skipped placeholder from a ready one. `apk add` does ask that way: it
    // parks in `poll({pipe}, {pipe}, {-1}, -1)` for its trigger child's two
    // pipes, and a wait loop that reads the placeholder as "ready" but the
    // report as "nothing" neither breaks nor sleeps - it spins there, and on
    // one core the child that was going to fill the pipe never runs.
    //
    // So the placeholder is kept beside a pipe that is quiet when the wait
    // starts, and a child makes it ready. The answer this pins is that the
    // wait *ends*, on the event rather than on a clock.
    let mut q = [-1i32; 2];
    let made = unsafe { libc::pipe2(q.as_mut_ptr(), 0) } == 0;
    r.check("pipe2 for the blocking test succeeds", made);
    if !made {
        r.finish()
    }
    let pid = unsafe { libc::fork() };
    if pid == 0 {
        unsafe {
            libc::close(q[0]);
            let nap = libc::timespec {
                tv_sec: 0,
                tv_nsec: 40_000_000,
            };
            libc::nanosleep(&nap, core::ptr::null_mut());
            libc::write(q[1], b"y".as_ptr() as *const libc::c_void, 1);
            // The door does not return; `_exit` is the maintained fallback.
            libc::_exit(0)
        }
    }
    r.check("the trigger-shaped child started", pid > 0);
    if pid > 0 {
        let mut waiting = [
            Sock {
                fd: -1,
                events: POLL_IN,
                revents: 0x5a5a_u16 as i16,
            },
            Sock {
                fd: q[0],
                events: POLL_IN,
                revents: 0x5a5a_u16 as i16,
            },
        ];
        r.check(
            "a blocking poll of a quiet pipe beside a placeholder waits for the byte",
            poll(&mut waiting, -1) == 1,
        );
        r.check(
            "and names the pipe as the ready one",
            waiting[1].revents == POLL_IN,
        );
        r.check(
            "while the placeholder is still skipped",
            waiting[0].revents == 0,
        );
        let mut status = 0i32;
        unsafe { libc::wait4(pid, &mut status, 0, core::ptr::null_mut()) };
    }
    unsafe {
        libc::close(q[0]);
        libc::close(q[1]);
    }

    r.finish()
}
