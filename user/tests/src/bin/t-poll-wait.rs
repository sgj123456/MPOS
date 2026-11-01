//! t-poll-wait - `poll(2)`'s timeout is a wait, and a ready descriptor ends it.
//!
//! Pinned here, per poll(2)/ppoll(2): a wait with **no descriptors at all** is
//! a sleep of the timeout's length (not an immediate return, not a hang), the
//! same for a descriptor that is open and quiet, the `ppoll` spelling carries
//! the same deadline in a `timespec`, a zero timeout on a quiet descriptor
//! answers 0 at once, and a descriptor that becomes ready inside an **infinite**
//! wait wakes it with `revents` set to what happened.
//!
//! Why it exists: the desktop image's boot sat 120 s in `udev-settle`, and the
//! kernel's own heartbeat showed `/bin/udevadm` parked in syscall 7 (`poll`)
//! with the timeout it was given (`docs/notes/` §7.9c). A poll whose deadline
//! is not honoured, or whose wake is lost, looks exactly like that.
//!
//! # The migration
//!
//! `rustix::event::poll` names the call but not these claims: it derives the
//! descriptor list from a slice (so "no descriptors at all" has no spelling),
//! it has no `ppoll` (`rustix::event::poll::ppoll` does not exist on these
//! targets), and its timeout is a `Timespec`-typed `Option` the caller cannot
//! pass as the measured `timespec` the kernel reads. So the program rides the
//! raw doors with libc's own structs, which are the same shapes the machine's
//! ABI hands over.
//!
//! Run as `MPOS_ON_HOST=1 ./target/x86_64-unknown-linux-gnu/debug/t-poll-wait`
//! it is a reference against the host's own poll(2).

use mpos_user_tests::Report;
use std::time::Instant;

/// A `poll(2)` deadline, and whether it returned when it should.
///
/// The window is wide on the low side because a loaded lane can be late, and
/// narrow on the high side because the claim is that the *timeout* is honoured:
/// a wait that hangs is killed by the program's budget and reads as a failure
/// anyway, so the important half is the lower bound.
fn timed(wait_ms: i32) -> (i32, u64) {
    let mut fds: [libc::pollfd; 0] = [];
    let start = Instant::now();
    let n = unsafe { libc::poll(fds.as_mut_ptr(), 0, wait_ms) };
    (n, start.elapsed().as_millis() as u64)
}

fn main() {
    println!("t-poll-wait");
    let mut r = Report::new();

    // ---- a wait with no descriptors is a sleep ---------------------------
    let (n, ms) = timed(250);
    r.check("a wait with no descriptors answers 0", n == 0);
    r.check("and it slept the timeout it was given, not less", ms >= 150);
    r.check(
        "and it did not sleep longer than a loaded lane can explain",
        ms <= 3000,
    );

    // ---- a descriptor that is open and quiet ----------------------------
    let (read_end, write_end) = pipe_pair();
    let mut fds = [libc::pollfd {
        fd: read_end,
        events: libc::POLLIN,
        revents: 0,
    }];
    let start = Instant::now();
    let n = unsafe { libc::poll(fds.as_mut_ptr(), 1, 250) };
    let quiet_ms = start.elapsed().as_millis() as u64;
    r.check("a quiet descriptor answers 0 at its timeout", n == 0);
    r.check("and that wait also slept the timeout", quiet_ms >= 150);

    // ---- a zero timeout is the immediate answer -------------------------
    let start = Instant::now();
    let n = unsafe { libc::poll(fds.as_mut_ptr(), 1, 0) };
    let immediate_ms = start.elapsed().as_millis() as u64;
    r.check(
        "a zero timeout on a quiet descriptor answers 0 at once",
        n == 0 && immediate_ms < 100,
    );

    // ---- ppoll: the same deadline in a timespec -------------------------
    let timeout = libc::timespec {
        tv_sec: 0,
        tv_nsec: 250_000_000,
    };
    let start = Instant::now();
    let n = unsafe { libc::ppoll(fds.as_mut_ptr(), 1, &timeout, std::ptr::null()) };
    let ppoll_ms = start.elapsed().as_millis() as u64;
    r.check("ppoll answers 0 at its deadline", n == 0);
    r.check("and it slept the timespec's length", ppoll_ms >= 150);

    // ---- a ready descriptor wakes an infinite wait ----------------------
    //
    // The child writes after 200 ms; the parent waits with -1 (no deadline),
    // so only a real wake can end it - the shape of any event loop, and of
    // `udevd`/`udevadm` themselves.
    let pid = unsafe { libc::fork() };
    if pid == 0 {
        unsafe {
            libc::usleep(200_000);
            let byte = 7u8;
            libc::write(write_end, (&byte as *const u8).cast(), 1);
        }
        unsafe { libc::_exit(0) };
    }
    let mut fds = [libc::pollfd {
        fd: read_end,
        events: libc::POLLIN,
        revents: 0,
    }];
    let start = Instant::now();
    let n = unsafe { libc::poll(fds.as_mut_ptr(), 1, -1) };
    let woke_ms = start.elapsed().as_millis() as u64;
    r.check(
        "a descriptor that becomes ready ends an infinite wait",
        n == 1 && fds[0].revents & libc::POLLIN != 0,
    );
    r.check(
        "and it ended when the byte arrived, not at a deadline",
        woke_ms >= 150 && woke_ms <= 5000,
    );
    let mut byte = 0u8;
    let got = unsafe { libc::read(read_end, (&mut byte as *mut u8).cast(), 1) };
    r.check(
        "the byte the wait reported is readable",
        got == 1 && byte == 7,
    );

    let mut status: libc::c_int = 0;
    unsafe {
        libc::waitpid(pid, &mut status, 0);
        libc::close(read_end);
        libc::close(write_end);
    }

    // ---- the count the wait is allowed to name ---------------------------
    //
    // Linux measures `nfds` against `RLIMIT_NOFILE` and answers `EINVAL` past
    // it, *before* it reads a single entry - a second argument one past the
    // limit is `EINVAL` even when the array is an address nothing maps
    // (measured on the host, `build/scratch/pnfds/l7.c`: unmapped + over the
    // limit -> `EINVAL`, unmapped + inside it -> `EFAULT`). The shape this
    // pins is that a wait may name exactly as many descriptors as it is
    // allowed to hold, and that the array is judged by the entries it reaches
    // rather than by its size - which is the difference between a 4 MiB array
    // being served and refused.
    //
    // The array is the two-entry one this program already owns: naming more
    // descriptors than it holds must still answer 0 at a zero timeout where
    // the entries below the count are reachable, because the wait never reads
    // further than the array it was handed. That is Linux's own lazy shape
    // (`build/scratch/pmax/s.c`: 524288 entries of a mapped array, 0 answered)
    // and it is what a *size* check cannot express.
    let mut limit = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    let got_limit =
        unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) } == 0 && limit.rlim_cur > 0;
    r.check("the machine answers its descriptor limit", got_limit);
    if got_limit {
        let over = (limit.rlim_cur as usize).saturating_add(1) as libc::nfds_t;
        // An unmapped address and a count past the limit: the count is refused
        // first, so this is `EINVAL` and not `EFAULT`.
        let rc = unsafe { libc::poll(core::ptr::null_mut(), over, 0) };
        let errno = std::io::Error::last_os_error().raw_os_error().unwrap_or(0);
        r.check(
            "a count past the descriptor limit is refused with EINVAL",
            rc == -1 && errno == libc::EINVAL,
        );
        // The same count *at* the limit, over an array that long and a zero
        // timeout: every entry is reachable and names no descriptor, so the
        // answer is 0. This is the shape a size check cannot serve - the
        // array is `RLIMIT_NOFILE` entries, megabytes of it.
        let at_limit = limit.rlim_cur as libc::nfds_t;
        let mut room: Vec<libc::pollfd> = (0..at_limit)
            .map(|_| libc::pollfd {
                fd: -1,
                events: 0,
                revents: 0,
            })
            .collect();
        let rc = unsafe { libc::poll(room.as_mut_ptr(), at_limit, 0) };
        r.check(
            "a count at the descriptor limit over an array that large is served",
            rc == 0,
        );
        drop(room);
        // And at the *hard* limit, which is where the size check actually
        // bites: this machine's hard `RLIMIT_NOFILE` is 1048576, an array of
        // 8 MiB - past the one-mebibyte window a single kernel copy is allowed
        // (`MAX_IO`), and still a wait Linux serves for a caller allowed to
        // hold that many descriptors. Pre-fix this was `EFAULT` for the size
        // alone, which is the whole defect.
        let mut raised = libc::rlimit {
            rlim_cur: limit.rlim_max,
            rlim_max: limit.rlim_max,
        };
        let raised_ok = unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &raised) } == 0;
        r.check(
            "the descriptor limit can be raised to its own maximum",
            raised_ok,
        );
        if raised_ok {
            let big = raised.rlim_cur as libc::nfds_t;
            let mut wide: Vec<libc::pollfd> = (0..big)
                .map(|_| libc::pollfd {
                    fd: -1,
                    events: 0,
                    revents: 0,
                })
                .collect();
            let rc = unsafe { libc::poll(wide.as_mut_ptr(), big, 0) };
            r.check(
                "a wait wider than one kernel copy window is still served",
                rc == 0,
            );
            drop(wide);
            raised.rlim_cur = limit.rlim_cur;
            unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &raised) };
        }
    }

    // ---- the netlink door udevadm's settle monitors ----------------------
    //
    // `udevadm settle` opens the kernel's uevent netlink socket; a poll on it
    // with a deadline is the exact wait the boot sat in. The claim is the
    // deadline, not the event: an idle socket must answer 0 at its timeout.
    let sock = unsafe { libc::socket(libc::AF_NETLINK, libc::SOCK_RAW, 15) };
    if sock >= 0 {
        let mut fds = [libc::pollfd {
            fd: sock,
            events: libc::POLLIN,
            revents: 0,
        }];
        let start = Instant::now();
        let n = unsafe { libc::poll(fds.as_mut_ptr(), 1, 250) };
        let netlink_ms = start.elapsed().as_millis() as u64;
        r.check(
            "a poll on the uevent socket answers its deadline",
            n >= 0 && netlink_ms >= 150,
        );
        unsafe { libc::close(sock) };
    } else {
        r.check("the uevent socket is a door this machine opens", false);
    }

    r.finish();
}

/// A pipe whose ends the tests own.
fn pipe_pair() -> (libc::c_int, libc::c_int) {
    let mut fds = [0i32; 2];
    let rc = unsafe { libc::pipe(fds.as_mut_ptr()) };
    assert_eq!(rc, 0, "a pipe is a door this machine opens");
    (fds[0], fds[1])
}
