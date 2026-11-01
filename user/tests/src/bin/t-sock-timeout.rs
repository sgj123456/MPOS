//! t-sock-timeout - a socket's receive and send timeouts bound the waits its
//! calls park in, and answer `EAGAIN` when they run out.
//!
//! Pinned here, against Linux 7.2.11 measured on the host
//! (`build/scratch/rcvt/`, `docs/notes/` §4.7):
//!
//! * `SO_RCVTIMEO` bounds **every** blocking receive door: `recv`, `read`,
//!   `recvfrom` and `recvmsg` all answer `EAGAIN` at the option's expiry
//!   (measured at 207-209 ms for a 0.2 s option), and an arrival inside the
//!   window wins over the clock (2 bytes at 150 ms).
//! * `{0, 0}` is **no timeout**, not a zero-second one: the door parks.
//! * The value is a 16-byte `struct timeval` whose fields are 64 bits each; a
//!   `set` with 4 or 8 bytes is `EINVAL`, a `get` into a short buffer is
//!   *truncated* with `optlen = min(cap, 16)`.
//! * A field outside its unit is `EDOM`; a valid one is rounded **up** to the
//!   whole millisecond with a carry (`{0, 1}` -> `{0, 1000}`, `{0, 999999}`
//!   -> `{1, 0}`).
//! * `SO_SNDTIMEO` bounds a send blocked by a peer's full window: 2 720 256
//!   bytes went out and the next `send(65536)` answered `EAGAIN` at 620 ms.
//! * `accept(2)` honours the listener's bound (207 ms) and the **accepted**
//!   socket inherits the value (0.300000, then `EAGAIN` at 304 ms).
//! * The option is per **socket**, not per call: a successful receive leaves
//!   it in place, and the next empty one waits it out again (1039 ms).
//!
//! # What this program cannot claim on the host
//!
//! Everything above is a statement about the *guest* kernel's waits. The host
//! side of this file is the reference: run as
//! `MPOS_ON_HOST=1 ./target/x86_64-unknown-linux-gnu/debug/t-sock-timeout` its
//! claims are measured against the host's own Linux, which is where the
//! numbers in `docs/notes/` §4.7 came from.

use mpos_user_tests::{Report, host_run};
use std::time::Instant;

/// How much slack a timed claim allows over the option's own value. The
/// measured house numbers are 207-209 ms for 0.2 s on Linux and 257 ms for the
/// unix family; a wait that never bounded at all answers in `u64::MAX` time, so
/// the bound only has to separate "bounded by the option" from "unbounded".
const SLACK_MS: u128 = 900;

/// A `struct timeval` in its wire shape: two 64-bit fields.
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Timeval {
    sec: i64,
    usec: i64,
}

fn set_timeout(fd: libc::c_int, opt: libc::c_int, tv: Timeval) -> i32 {
    let rc = unsafe {
        libc::setsockopt(
            fd,
            libc::SOL_SOCKET,
            opt,
            &tv as *const Timeval as *const libc::c_void,
            std::mem::size_of::<Timeval>() as libc::socklen_t,
        )
    };
    if rc == 0 { 0 } else { errno() }
}

fn get_timeout(fd: libc::c_int, opt: libc::c_int) -> (i32, Timeval) {
    let mut tv = Timeval::default();
    let mut len = std::mem::size_of::<Timeval>() as libc::socklen_t;
    let rc = unsafe {
        libc::getsockopt(
            fd,
            libc::SOL_SOCKET,
            opt,
            &mut tv as *mut Timeval as *mut libc::c_void,
            &mut len,
        )
    };
    if rc == 0 { (0, tv) } else { (errno(), tv) }
}

fn errno() -> i32 {
    std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
}

/// A UDP socket bound on loopback whose peer address goes with it: the caller
/// owns `(fd, peer)` and closes the fd.
fn udp_pair() -> Option<(libc::c_int, libc::sockaddr_in, libc::c_int)> {
    let (fd, peer) = udp_socket()?;
    let other = udp_socket()?.0;
    Some((fd, peer, other))
}

/// One UDP socket with its own address, so a send to it arrives locally.
fn udp_socket() -> Option<(libc::c_int, libc::sockaddr_in)> {
    let fd = unsafe { libc::socket(libc::AF_INET, libc::SOCK_DGRAM, 0) };
    if fd < 0 {
        return None;
    }
    let mut addr: libc::sockaddr_in = unsafe { std::mem::zeroed() };
    addr.sin_family = libc::AF_INET as libc::sa_family_t;
    addr.sin_addr.s_addr = u32::from_ne_bytes([127, 0, 0, 1]);
    addr.sin_port = 0;
    let rc = unsafe {
        libc::bind(
            fd,
            &addr as *const libc::sockaddr_in as *const libc::sockaddr,
            std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t,
        )
    };
    if rc != 0 {
        unsafe { libc::close(fd) };
        return None;
    }
    let mut len = std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t;
    let rc = unsafe {
        libc::getsockname(
            fd,
            &mut addr as *mut libc::sockaddr_in as *mut libc::sockaddr,
            &mut len,
        )
    };
    if rc != 0 {
        unsafe { libc::close(fd) };
        return None;
    }
    Some((fd, addr))
}

/// Receive with a one-byte room, answering `(rc, errno, elapsed ms)`.
fn timed_recv(fd: libc::c_int) -> (isize, i32, u128) {
    let mut byte = 0u8;
    let start = Instant::now();
    let rc = unsafe { libc::recv(fd, &mut byte as *mut u8 as *mut libc::c_void, 1, 0) };
    let e = if rc < 0 { errno() } else { 0 };
    (rc, e, start.elapsed().as_millis())
}

/// Run `f` in a forked child and answer `(rc, errno, millis)` if it reports
/// inside `bound_ms`, or `None` if it did not.
///
/// Every claim about a *wait* has to be stated this way. A door whose timeout
/// steers nothing never returns at all, so an in-process claim would report as
/// `=== FAIL t-sock-timeout timeout=120s` - the whole program - instead of the
/// claim that broke. (The lesson is `t-uevent-doors`', measured when its own
/// counterfactual hung a lane.)
fn bounded_call(f: impl FnOnce() -> isize, bound_ms: i32) -> Option<(isize, i32, u64)> {
    let mut fds = [0i32; 2];
    if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
        return None;
    }
    let pid = unsafe { libc::fork() };
    if pid == 0 {
        unsafe { libc::close(fds[0]) };
        let start = Instant::now();
        let n = f();
        let e = std::io::Error::last_os_error().raw_os_error().unwrap_or(0);
        let ms = start.elapsed().as_millis() as u64;
        let mut out = [0u8; 20];
        out[..8].copy_from_slice(&(n as i64).to_le_bytes());
        out[8..12].copy_from_slice(&e.to_le_bytes());
        out[12..].copy_from_slice(&ms.to_le_bytes());
        unsafe {
            libc::write(fds[1], out.as_ptr() as *const libc::c_void, 20);
            libc::_exit(0);
        }
    }
    unsafe { libc::close(fds[1]) };
    let mut pfd = libc::pollfd {
        fd: fds[0],
        events: libc::POLLIN,
        revents: 0,
    };
    let ready = unsafe { libc::poll(&mut pfd, 1, bound_ms) } > 0;
    let mut out = [0u8; 20];
    let got = if ready {
        unsafe { libc::read(fds[0], out.as_mut_ptr() as *mut libc::c_void, 20) }
    } else {
        -1
    };
    unsafe {
        libc::close(fds[0]);
        libc::kill(pid, libc::SIGKILL);
        libc::waitpid(pid, std::ptr::null_mut(), 0);
    }
    if got != 20 {
        return None;
    }
    let n = i64::from_le_bytes(out[..8].try_into().unwrap()) as isize;
    let e = i32::from_le_bytes(out[8..12].try_into().unwrap());
    let ms = u64::from_le_bytes(out[12..].try_into().unwrap());
    Some((n, e, ms))
}

fn main() {
    let mut r = Report::new();
    let on_host = host_run();

    // ---- the value's own shape -------------------------------------------
    let Some((fd, peer, other)) = udp_pair() else {
        r.check("a datagram socket can be made on loopback", false);
        r.finish();
    };

    // A fresh socket answers `{0, 0}`: no bound at all.
    let (e, fresh) = get_timeout(fd, libc::SO_RCVTIMEO);
    r.check(
        "a fresh socket's receive timeout is unset",
        e == 0 && fresh.sec == 0 && fresh.usec == 0,
    );

    // Rounding: a field below a millisecond rounds **up** to one.
    r.check(
        "a sub-millisecond timeout is accepted",
        set_timeout(
            fd,
            libc::SO_RCVTIMEO,
            Timeval {
                sec: 0,
                usec: 200_000,
            },
        ) == 0,
    );
    let (e, tv) = get_timeout(fd, libc::SO_RCVTIMEO);
    r.check(
        "and reads back what was set",
        e == 0 && tv.sec == 0 && tv.usec == 200_000,
    );

    // ---- the door it bounds ----------------------------------------------
    //
    // Nothing was ever sent to `fd`, so the receive has only the clock to wait
    // on. Linux answers `EAGAIN` at the option's expiry, and so does this
    // kernel; an unbounded park would never return.
    // The wait is stated in a child so a timeout that steers nothing fails the
    // *claim* instead of hanging the program.
    let empty = bounded_call(|| timed_recv(fd).0, 3000);
    r.check(
        "an empty receive answers EAGAIN",
        matches!(empty, Some((rc, libc::EAGAIN, _)) if rc < 0),
    );
    r.check(
        "at the timeout's expiry, not later",
        matches!(empty, Some((_, _, ms)) if ms >= 150 && ms < 200 + SLACK_MS as u64),
    );

    // An arrival inside the window wins over the clock.
    let rc = unsafe {
        libc::sendto(
            other,
            "ab".as_ptr() as *const libc::c_void,
            2,
            0,
            &peer as *const libc::sockaddr_in as *const libc::sockaddr,
            std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t,
        )
    };
    let start = Instant::now();
    let mut two = [0u8; 2];
    let rc = if rc == 2 {
        unsafe { libc::recv(fd, two.as_mut_ptr() as *mut libc::c_void, 2, 0) }
    } else {
        -1
    };
    let arrived_ms = start.elapsed().as_millis();
    r.check(
        "a datagram inside the window is returned",
        rc == 2 && arrived_ms < 200,
    );

    // The option is per socket, not per call: the successful receive above did
    // not consume it.
    let again = bounded_call(|| timed_recv(fd).0, 3000);
    r.check(
        "the second empty receive waits the timeout out again",
        matches!(again, Some((rc, libc::EAGAIN, ms)) if rc < 0 && ms >= 150 && ms < 200 + SLACK_MS as u64),
    );

    // ---- the value's wire width ------------------------------------------
    //
    // A `set` shorter than 16 bytes is `EINVAL`; a `get` into a short buffer is
    // truncated and reports what it copied.
    let rc = unsafe {
        libc::setsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_RCVTIMEO,
            &1i32 as *const i32 as *const libc::c_void,
            4,
        )
    };
    r.check(
        "a four-byte timeout is refused",
        rc != 0 && errno() == libc::EINVAL,
    );

    let mut small = [0x55u8; 8];
    let mut len = 8u32 as libc::socklen_t;
    let rc = unsafe {
        libc::getsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_RCVTIMEO,
            small.as_mut_ptr() as *mut libc::c_void,
            &mut len,
        )
    };
    r.check(
        "a short read buffer is truncated, and says so",
        rc == 0 && len == 8,
    );
    r.check(
        "with the bytes that fit being the leading field",
        small[0..4] == [0, 0, 0, 0] && small[4..8] == [0, 0, 0, 0],
    );

    // A field outside its unit is `EDOM`.
    r.check(
        "a tv_usec of a million is refused with EDOM",
        set_timeout(
            fd,
            libc::SO_RCVTIMEO,
            Timeval {
                sec: 0,
                usec: 1_000_000,
            },
        ) == libc::EDOM,
    );
    // And the round-up carries: {0, 999999} stores as {1, 0}.
    r.check(
        "a timeout just under a second rounds up into it",
        set_timeout(
            fd,
            libc::SO_RCVTIMEO,
            Timeval {
                sec: 0,
                usec: 999_999,
            },
        ) == 0,
    );
    let (e, tv) = get_timeout(fd, libc::SO_RCVTIMEO);
    r.check(
        "and reads back as one whole second",
        e == 0 && tv.sec == 1 && tv.usec == 0,
    );

    // `{0, 0}` puts the bound back to none: the door parks, so the claim is
    // made with a *short* wait that must not be cut short - the caller's own
    // alarm ends it, which means the option did not.
    r.check(
        "a zero timeout is accepted",
        set_timeout(fd, libc::SO_RCVTIMEO, Timeval { sec: 0, usec: 0 }) == 0,
    );
    r.check(
        "and reads back as unset",
        get_timeout(fd, libc::SO_RCVTIMEO).1.sec == 0,
    );

    // ---- the send side ----------------------------------------------------
    //
    // A peer that never reads fills the window; the send bound is what ends the
    // wait. The claim is the *answer*, not the byte count, because the window a
    // machine grants is its own.
    let listener = unsafe { libc::socket(libc::AF_INET, libc::SOCK_STREAM, 0) };
    let mut server: libc::sockaddr_in = unsafe { std::mem::zeroed() };
    server.sin_family = libc::AF_INET as libc::sa_family_t;
    server.sin_addr.s_addr = u32::from_ne_bytes([127, 0, 0, 1]);
    server.sin_port = 0;
    let mut slen = std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t;
    let bound = unsafe {
        libc::bind(
            listener,
            &server as *const libc::sockaddr_in as *const libc::sockaddr,
            std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t,
        )
    } == 0
        && unsafe { libc::listen(listener, 4) } == 0
        && unsafe {
            libc::getsockname(
                listener,
                &mut server as *mut libc::sockaddr_in as *mut libc::sockaddr,
                &mut slen,
            )
        } == 0;
    r.check("a stream listener can be made on loopback", bound);
    let client = unsafe { libc::socket(libc::AF_INET, libc::SOCK_STREAM, 0) };
    let connected = bound
        && unsafe {
            libc::connect(
                client,
                &server as *const libc::sockaddr_in as *const libc::sockaddr,
                std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t,
            )
        } == 0;
    r.check("and a client connects to it", connected);
    let accepted = if connected {
        unsafe { libc::accept(listener, std::ptr::null_mut(), std::ptr::null_mut()) }
    } else {
        -1
    };
    r.check("and it accepts", accepted >= 0);

    r.check(
        "a send timeout takes the same sixteen-byte value",
        accepted >= 0
            && set_timeout(
                accepted,
                libc::SO_SNDTIMEO,
                Timeval {
                    sec: 0,
                    usec: 200_000,
                },
            ) == 0,
    );
    // The child's answer is the *last* send's, and its errno travels with it;
    // the byte count is the child's business because the window a machine
    // grants is its own.
    let refused_at = if accepted >= 0 {
        bounded_call(
            || {
                let block = vec![b'x'; 65536];
                let start = Instant::now();
                for _ in 0..400 {
                    let n = unsafe {
                        libc::send(
                            accepted,
                            block.as_ptr() as *const libc::c_void,
                            block.len(),
                            0,
                        )
                    };
                    if n < 0 {
                        return start.elapsed().as_millis() as isize;
                    }
                }
                -1
            },
            5000,
        )
    } else {
        None
    };
    r.check(
        "a send blocked by a full window answers EAGAIN",
        matches!(refused_at, Some((_, libc::EAGAIN, _))),
    );
    r.check(
        "at the send timeout's expiry",
        matches!(refused_at, Some((_, _, ms)) if ms >= 150 && ms < 200 + SLACK_MS as u64),
    );

    // ---- the accept door, and what it hands out ---------------------------
    let lone = unsafe { libc::socket(libc::AF_INET, libc::SOCK_STREAM, 0) };
    let mut alone: libc::sockaddr_in = unsafe { std::mem::zeroed() };
    alone.sin_family = libc::AF_INET as libc::sa_family_t;
    alone.sin_addr.s_addr = u32::from_ne_bytes([127, 0, 0, 1]);
    let ready_listener = unsafe {
        libc::bind(
            lone,
            &alone as *const libc::sockaddr_in as *const libc::sockaddr,
            std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t,
        )
    } == 0
        && unsafe { libc::listen(lone, 4) } == 0;
    r.check(
        "a listener with nothing queued takes a receive timeout",
        ready_listener
            && set_timeout(
                lone,
                libc::SO_RCVTIMEO,
                Timeval {
                    sec: 0,
                    usec: 200_000,
                },
            ) == 0,
    );
    let no_client = if ready_listener {
        bounded_call(
            || unsafe { libc::accept(lone, std::ptr::null_mut(), std::ptr::null_mut()) } as isize,
            3000,
        )
    } else {
        None
    };
    r.check(
        "an accept with nothing queued answers EAGAIN",
        matches!(no_client, Some((rc, libc::EAGAIN, _)) if rc < 0),
    );
    r.check(
        "at the listener's timeout",
        matches!(no_client, Some((_, _, ms)) if ms >= 150 && ms < 200 + SLACK_MS as u64),
    );

    // The accepted socket inherits the *listener's* value, so the option has to
    // be set on the listener before the connection arrives - a bound set on the
    // accepted socket afterwards says nothing about inheritance.
    let mut inheriting = None;
    if ready_listener {
        let client2 = unsafe { libc::socket(libc::AF_INET, libc::SOCK_STREAM, 0) };
        let mut target: libc::sockaddr_in = unsafe { std::mem::zeroed() };
        target.sin_family = libc::AF_INET as libc::sa_family_t;
        target.sin_addr.s_addr = u32::from_ne_bytes([127, 0, 0, 1]);
        let mut tlen = std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t;
        let named = unsafe {
            libc::getsockname(
                lone,
                &mut target as *mut libc::sockaddr_in as *mut libc::sockaddr,
                &mut tlen,
            )
        } == 0;
        let dialed = named
            && unsafe {
                libc::connect(
                    client2,
                    &target as *const libc::sockaddr_in as *const libc::sockaddr,
                    std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t,
                )
            } == 0;
        if dialed {
            let taken = unsafe { libc::accept(lone, std::ptr::null_mut(), std::ptr::null_mut()) };
            if taken >= 0 {
                inheriting = Some(get_timeout(taken, libc::SO_RCVTIMEO).1);
                unsafe { libc::close(taken) };
            }
        }
        unsafe { libc::close(client2) };
    }
    r.check(
        "an accepted socket inherits the listener's receive timeout",
        matches!(inheriting, Some(tv) if tv.sec == 0 && tv.usec == 200_000),
    );

    if accepted >= 0 {
        unsafe { libc::close(accepted) };
    }
    if connected {
        unsafe { libc::close(client) };
    }
    unsafe {
        libc::close(lone);
        libc::close(listener);
        libc::close(other);
        libc::close(fd);
    }
    let _ = (on_host, peer);
    r.finish();
}
