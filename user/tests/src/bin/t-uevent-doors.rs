//! t-uevent-doors - every receive door on a device-event socket answers the
//! event, not just `recvmsg(2)`.
//!
//! The shape this pins was measured on a NIC-less boot of the current tree
//! (`docs/notes/` §7.9r): on a `NETLINK_KOBJECT_UEVENT` socket that had just
//! been bound, `recvmsg(2)` answered the announcement and `recv(2)`/`read(2)`
//! answered **`EBADF`** on the same descriptor. The kernel had a branch for
//! these handles in its `recvmsg` door only, so the other two fell through to
//! the IPv4 registry - which has never heard of a uevent handle - and read the
//! process's own bound socket as a descriptor that names nothing.
//!
//! That is not a cosmetic gap. `read(2)` and `recv(2)` are the doors a
//! program uses when it does not care who sent the message, and udev's older
//! workers and many small initramfs listeners read the bus that way: which
//! door a listener happens to pick must not decide whether it hears anything
//! at all. Linux answers the family on all three (measured, root,
//! `build/scratch/uevdoor/`: 156 bytes from `read`, `recv` and `recvmsg`
//! alike, `EAGAIN` when the queue is empty and the caller asked not to wait,
//! and a blocking door woken by an event 300 ms later).
//!
//! **Host runs make no claims.** Announcing a device means writing a sysfs
//! `uevent` attribute, which an ordinary host process is not allowed to do -
//! and a claim that has no announcement behind it would be a claim about a
//! machine that never made one. The guest is where the announcement can be
//! made, so that is where these run.

use std::io::Write;

use mpos_user_tests::{Report, host_run};

const NETLINK_KOBJECT_UEVENT: libc::c_int = 15;
/// `MSG_DONTWAIT`: look at the queue without parking on it.
const MSG_DONTWAIT: libc::c_int = 0x40;
/// The one group kernel-originated events go to.
const UEVENT_GROUP_KERNEL: u32 = 1;
/// How long a parked door is given to be woken.
const BUDGET_MS: u32 = 2000;

/// How long the parent waits for a blocking child before killing it: the
/// claims below give a door 150-600 ms, so a second is generous.
const BOUND_MS: i32 = 1000;

/// A `struct sockaddr_nl`: family, pad, port id, groups.
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct SockAddrNl {
    family: u16,
    _pad: u16,
    pid: u32,
    groups: u32,
}

fn nl_addr(pid: u32, groups: u32) -> SockAddrNl {
    SockAddrNl {
        family: libc::AF_NETLINK as u16,
        _pad: 0,
        pid,
        groups,
    }
}

fn nl_socket() -> libc::c_int {
    unsafe { libc::socket(libc::AF_NETLINK, libc::SOCK_RAW, NETLINK_KOBJECT_UEVENT) }
}

fn nl_bind(fd: libc::c_int, addr: &SockAddrNl) -> libc::c_int {
    unsafe {
        libc::bind(
            fd,
            addr as *const SockAddrNl as *const libc::sockaddr,
            std::mem::size_of::<SockAddrNl>() as libc::socklen_t,
        )
    }
}

/// The errno `call` answered, or `None` when it answered something else.
fn failed_with(call: impl FnOnce() -> isize) -> Option<i32> {
    let n = call();
    if n < 0 {
        Some(std::io::Error::last_os_error().raw_os_error().unwrap_or(0))
    } else {
        None
    }
}

/// The card's **devpath** - the name its uevent announces it under - read out
/// of the index link `/sys/dev/char/226:0`. Asking rather than spelling it
/// keeps the machine's bus shape out of the test.
fn card_devpath() -> Option<String> {
    let link = std::fs::read_link("/sys/dev/char/226:0").ok()?;
    let devpath = link.to_string_lossy();
    let devpath = devpath.trim_start_matches("../");
    if devpath.is_empty() {
        return None;
    }
    Some(format!("/{devpath}"))
}

/// Announce the card the way `udevadm trigger` does: a write to its `uevent`
/// attribute, from a child, so that a read in this process has to wait for it.
fn announce_in_child(trigger: &str) -> libc::pid_t {
    let trigger = trigger.to_string();
    let pid = unsafe { libc::fork() };
    if pid == 0 {
        let nap = libc::timespec {
            tv_sec: 0,
            tv_nsec: 50_000_000,
        };
        let mut rem = nap;
        unsafe {
            libc::nanosleep(&nap, &mut rem);
        }
        if let Ok(mut f) = std::fs::OpenOptions::new().write(true).open(&trigger) {
            let _ = f.write_all(b"add");
        }
        unsafe { libc::_exit(0) };
    }
    pid
}

fn main() {
    println!("t-uevent-doors");
    let mut r = Report::new();

    if host_run() {
        println!(
            "note: announcing a device means writing a sysfs `uevent` attribute, which this host \
             will not let an ordinary process do - so no claim is made here (the reference \
             measurement is Linux's own, quoted in this program's header)"
        );
        r.finish();
    }
    let Some(devpath) = card_devpath() else {
        println!(
            "note: this machine has no /sys/dev/char/226:0, so there is no card to announce and \
             nothing is claimed"
        );
        r.finish();
    };
    let trigger = format!("/sys{devpath}/uevent");
    if std::fs::metadata(&trigger).is_err() {
        println!("note: {trigger} does not exist on this machine, so nothing is claimed");
        r.finish();
    }

    let fd = nl_socket();
    r.check("a kernel-uevent listener opens", fd >= 0);
    if fd < 0 {
        r.finish();
    }
    r.check(
        "and binds to the kernel's group",
        nl_bind(fd, &nl_addr(0, UEVENT_GROUP_KERNEL)) == 0,
    );

    // Nothing has been announced since this bound, so the queue is empty and
    // the three non-blocking doors say so with `EAGAIN` - never `EBADF`, which
    // is a descriptor this process never asked for.
    let mut buf = [0u8; 512];
    r.check(
        "recv(MSG_DONTWAIT) on an empty queue answers EAGAIN, not EBADF",
        failed_with(|| unsafe {
            libc::recv(fd, buf.as_mut_ptr() as *mut _, buf.len(), MSG_DONTWAIT)
        }) == Some(libc::EAGAIN),
    );
    r.check(
        "recvmsg(MSG_DONTWAIT) on an empty queue answers EAGAIN",
        failed_with(|| {
            let mut iov = [libc::iovec {
                iov_base: buf.as_mut_ptr() as *mut libc::c_void,
                iov_len: buf.len(),
            }];
            let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
            msg.msg_iov = iov.as_mut_ptr();
            msg.msg_iovlen = 1;
            unsafe { libc::recvmsg(fd, &mut msg, MSG_DONTWAIT) }
        }) == Some(libc::EAGAIN),
    );
    // `read(2)`'s non-blocking answer needs the descriptor's own flag.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) };
    r.check(
        "read(2) on an empty queue answers EAGAIN",
        failed_with(|| unsafe { libc::read(fd, buf.as_mut_ptr() as *mut _, buf.len()) })
            == Some(libc::EAGAIN),
    );
    unsafe { libc::fcntl(fd, libc::F_SETFL, flags) };

    // One announcement, then the same event through `read(2)`.
    let announcer = announce_in_child(&trigger);
    r.check("the announcing child started", announcer > 0);
    let mut got = [0u8; 512];
    let read_n = loop {
        let n = unsafe { libc::read(fd, got.as_mut_ptr() as *mut _, got.len()) };
        if n > 0 || !n_was_eagain() {
            break n;
        }
        std::thread::sleep(std::time::Duration::from_millis(2));
    };
    let text = String::from_utf8_lossy(&got[..read_n.max(0) as usize]).to_string();
    r.check(
        "read(2) answers the announcement a bound listener is owed",
        read_n > 0 && text.starts_with("add@"),
    );
    r.check(
        "and its bytes name the device that announced itself",
        text.contains(&devpath),
    );
    unsafe { libc::waitpid(announcer, std::ptr::null_mut(), 0) };

    // `recv(2)` is the third door, with the same event.
    let announcer = announce_in_child(&trigger);
    let mut got = [0u8; 512];
    let recv_n = loop {
        let n = unsafe { libc::recv(fd, got.as_mut_ptr() as *mut _, got.len(), MSG_DONTWAIT) };
        if n > 0 || !n_was_eagain() {
            break n;
        }
        std::thread::sleep(std::time::Duration::from_millis(2));
    };
    let text = String::from_utf8_lossy(&got[..recv_n.max(0) as usize]).to_string();
    r.check(
        "recv(2) answers the announcement too",
        recv_n > 0 && text.starts_with("add@"),
    );
    unsafe { libc::waitpid(announcer, std::ptr::null_mut(), 0) };

    // The source slots a `recvfrom(2)` caller leaves room for: this event
    // comes from the kernel, which is port 0 on the kernel's group.
    let announcer = announce_in_child(&trigger);
    let mut from = SockAddrNl::default();
    let mut from_len = std::mem::size_of::<SockAddrNl>() as libc::socklen_t;
    let mut got = [0u8; 512];
    let mut n;
    let mut left = BUDGET_MS;
    loop {
        n = unsafe {
            libc::recvfrom(
                fd,
                got.as_mut_ptr() as *mut _,
                got.len(),
                MSG_DONTWAIT,
                &mut from as *mut SockAddrNl as *mut libc::sockaddr,
                &mut from_len,
            )
        };
        if n >= 0 || left == 0 {
            break;
        }
        left -= 2;
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
    r.check("recvfrom(2) answers the announcement", n > 0);
    r.check(
        "and the source it reports is the kernel's own port on its group",
        from.family == libc::AF_NETLINK as u16
            && from.pid == 0
            && from.groups == UEVENT_GROUP_KERNEL,
    );
    unsafe { libc::waitpid(announcer, std::ptr::null_mut(), 0) };

    // A blocking door must *wait* for the event rather than spin or refuse: the
    // announcement is 300 ms away, so an answer before the queue is filled is
    // the bug a caller with no timeout cannot survive. The `poll` below bounds
    // the claim, so a door that never wakes fails the sweep instead of hanging
    // it.
    let announcer = {
        let trigger = trigger.clone();
        let pid = unsafe { libc::fork() };
        if pid == 0 {
            let nap = libc::timespec {
                tv_sec: 0,
                tv_nsec: 300_000_000,
            };
            let mut rem = nap;
            unsafe {
                libc::nanosleep(&nap, &mut rem);
            }
            if let Ok(mut f) = std::fs::OpenOptions::new().write(true).open(&trigger) {
                let _ = f.write_all(b"add");
            }
            unsafe { libc::_exit(0) };
        }
        pid
    };
    let mut pfd = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    let ready = unsafe { libc::poll(&mut pfd, 1, BUDGET_MS as libc::c_int) };
    let mut got = [0u8; 512];
    let blocked = if ready > 0 {
        unsafe { libc::recv(fd, got.as_mut_ptr() as *mut _, got.len(), 0) }
    } else {
        -1
    };
    r.check(
        "a blocking recv(2) holds the event once one is announced",
        blocked > 0,
    );
    unsafe { libc::waitpid(announcer, std::ptr::null_mut(), 0) };

    // ---- the receive timeout, which bounds a door that waits ----
    //
    // Linux's contract, measured (`build/scratch/rt/rt`, `docs/notes/` §7.9t):
    // `SO_RCVTIMEO` of 200 ms on a blocking receive over an empty queue answers
    // `EAGAIN` at 202-208 ms on `recv(2)`, `read(2)` and `recvmsg(2)` alike;
    // the event wins when it arrives inside the window (156 bytes at 150 ms);
    // `MSG_DONTWAIT` still answers at once; and `{0, 0}` means *no* timeout.
    // Before this was served the option was stored and read back but steered
    // nothing, so every one of those doors waited forever.
    let timeouts = nl_socket();
    if timeouts < 0 || nl_bind(timeouts, &nl_addr(0, UEVENT_GROUP_KERNEL)) != 0 {
        r.check(
            "a second listener opens and binds for the timeout claims",
            false,
        );
        r.finish();
    }
    // Drain whatever the parking claim above left behind, so the timeout
    // claims start from an empty queue.
    let mut sink = [0u8; 512];
    while unsafe {
        libc::recv(
            timeouts,
            sink.as_mut_ptr() as *mut _,
            sink.len(),
            MSG_DONTWAIT,
        )
    } > 0
    {}
    r.check(
        "SO_RCVTIMEO of 200 ms is accepted",
        set_timeval(timeouts, 0, 200_000).is_ok(),
    );
    let mut tv = [0i64; 2];
    r.check(
        "and it reads back as the half a caller set",
        get_timeval(timeouts, &mut tv).is_ok() && tv == [0, 200_000],
    );
    // Each blocking door runs in its own child, bounded by the parent: a door
    // that waits forever is then a named failure instead of a lane timeout.
    let buf = sink.as_mut_ptr();
    let len = sink.len();
    let answered = blocking_answer(
        move || unsafe { libc::recv(timeouts, buf as *mut _, len, 0) },
        BOUND_MS,
    );
    r.check(
        "a blocking recv(2) gives up at its socket's timeout with EAGAIN",
        matches!(answered, Some((n, e, ms)) if n < 0 && e == libc::EAGAIN && (150..=600).contains(&ms)),
    );
    let buf = sink.as_mut_ptr();
    let len = sink.len();
    let answered = blocking_answer(
        move || {
            let mut iov = [libc::iovec {
                iov_base: buf as *mut libc::c_void,
                iov_len: len,
            }];
            let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
            msg.msg_iov = iov.as_mut_ptr();
            msg.msg_iovlen = 1;
            unsafe { libc::recvmsg(timeouts, &mut msg, 0) }
        },
        BOUND_MS,
    );
    r.check(
        "and so does a blocking recvmsg(2)",
        matches!(answered, Some((n, e, ms)) if n < 0 && e == libc::EAGAIN && (150..=600).contains(&ms)),
    );
    let buf = sink.as_mut_ptr();
    let len = sink.len();
    let answered = blocking_answer(
        move || unsafe { libc::read(timeouts, buf as *mut _, len) },
        BOUND_MS,
    );
    r.check(
        "and a blocking read(2)",
        matches!(answered, Some((n, e, ms)) if n < 0 && e == libc::EAGAIN && (150..=600).contains(&ms)),
    );
    // `MSG_DONTWAIT` answers at once regardless of the timeout.
    let (n, ms) = timed(|| unsafe {
        libc::recv(
            timeouts,
            sink.as_mut_ptr() as *mut _,
            sink.len(),
            MSG_DONTWAIT,
        )
    });
    r.check(
        "MSG_DONTWAIT still answers at once under a timeout",
        n < 0 && n_was_eagain() && ms < 100,
    );
    // An event inside the window wins: a 500 ms timeout, announced at 150 ms.
    r.check(
        "a 500 ms timeout is accepted",
        set_timeval(timeouts, 0, 500_000).is_ok(),
    );
    let announcer = {
        let trigger = trigger.clone();
        let pid = unsafe { libc::fork() };
        if pid == 0 {
            let nap = libc::timespec {
                tv_sec: 0,
                tv_nsec: 150_000_000,
            };
            let mut rem = nap;
            unsafe {
                libc::nanosleep(&nap, &mut rem);
            }
            if let Ok(mut f) = std::fs::OpenOptions::new().write(true).open(&trigger) {
                let _ = f.write_all(b"add");
            }
            unsafe { libc::_exit(0) };
        }
        pid
    };
    let buf = sink.as_mut_ptr();
    let len = sink.len();
    let answered = blocking_answer(
        move || unsafe { libc::recv(timeouts, buf as *mut _, len, 0) },
        BOUND_MS,
    );
    r.check(
        "an event inside the window is returned, not the timeout",
        matches!(answered, Some((n, _, ms)) if n > 0 && ms < 500),
    );
    unsafe { libc::waitpid(announcer, std::ptr::null_mut(), 0) };
    // `{0, 0}` means "no timeout": the door parks again rather than answering
    // at once. Claimed as "it did not answer inside 300 ms", which is the only
    // thing a bounded probe can say about an unbounded wait.
    r.check(
        "a zero timeout is stored as no timeout",
        set_timeval(timeouts, 0, 0).is_ok()
            && get_timeval(timeouts, &mut tv).is_ok()
            && tv == [0, 0],
    );
    // ...and with no timeout a waiting door parks: the probe's bound is what
    // ends it here, and the claim it can make is that the door did *not*
    // answer by itself inside 300 ms (a timeout of zero is not a zero-second
    // timeout).
    let buf = sink.as_mut_ptr();
    let len = sink.len();
    let answered = blocking_answer(
        move || unsafe { libc::recv(timeouts, buf as *mut _, len, 0) },
        300,
    );
    r.check(
        "a zero timeout is not a zero-second timeout",
        answered.is_none(),
    );

    // ---- the range Linux refuses ----
    //
    // `tv_usec` outside `0..=999_999` is `EDOM`, measured; and a valid one is
    // rounded up to the next whole millisecond, carrying into the seconds
    // (`{0, 5001}` reads back `{0, 6000}`, `{0, 999_999}` reads back `{1, 0}`).
    r.check(
        "a tv_usec of exactly one second is refused with EDOM",
        failed_with(|| {
            let mut tv = libc::timeval {
                tv_sec: 0,
                tv_usec: 1_000_000,
            };
            unsafe {
                libc::setsockopt(
                    timeouts,
                    libc::SOL_SOCKET,
                    libc::SO_RCVTIMEO,
                    &mut tv as *mut libc::timeval as *const libc::c_void,
                    std::mem::size_of::<libc::timeval>() as libc::socklen_t,
                ) as isize
            }
        }) == Some(libc::EDOM),
    );
    let mut tv8 = [0i64; 2];
    r.check(
        "and the microseconds are stored rounded up to a whole millisecond",
        set_timeval(timeouts, 0, 5_001).is_ok()
            && get_timeval(timeouts, &mut tv8).is_ok()
            && tv8 == [0, 6_000],
    );
    let mut tv9 = [0i64; 2];
    r.check(
        "with a carry into the seconds at the top of the range",
        set_timeval(timeouts, 0, 999_999).is_ok()
            && get_timeval(timeouts, &mut tv9).is_ok()
            && tv9 == [1, 0],
    );

    r.finish();
}

/// Set `SO_RCVTIMEO` from a microseconds count.
fn set_timeval(fd: libc::c_int, secs: i64, usecs: i64) -> Result<(), std::io::Error> {
    let mut tv = libc::timeval {
        tv_sec: secs,
        tv_usec: usecs,
    };
    let rc = unsafe {
        libc::setsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_RCVTIMEO,
            &mut tv as *mut libc::timeval as *const libc::c_void,
            std::mem::size_of::<libc::timeval>() as libc::socklen_t,
        )
    };
    if rc == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

/// Read `SO_RCVTIMEO` back as `[seconds, microseconds]`.
fn get_timeval(fd: libc::c_int, out: &mut [i64; 2]) -> Result<(), std::io::Error> {
    let mut tv = libc::timeval {
        tv_sec: 0,
        tv_usec: 0,
    };
    let mut len = std::mem::size_of::<libc::timeval>() as libc::socklen_t;
    let rc = unsafe {
        libc::getsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_RCVTIMEO,
            &mut tv as *mut libc::timeval as *mut libc::c_void,
            &mut len,
        )
    };
    out[0] = tv.tv_sec;
    out[1] = tv.tv_usec;
    if rc == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

/// Run `f` and report what it answered and how long it took in milliseconds.
fn timed(f: impl FnOnce() -> isize) -> (isize, u64) {
    let start = std::time::Instant::now();
    let n = f();
    (n, start.elapsed().as_millis() as u64)
}

/// Run a blocking call in a child, and give it a bounded wait.
///
/// A blocking receive at a socket whose timeout steers no wait never returns,
/// and a pin that hangs reports as a lane timeout rather than a named claim -
/// the counterfactual for this section measured exactly that
/// (`=== FAIL t-uevent-doors timeout=120s`). A signal does not end the park
/// either (measured: an `alarm(1)` handler left the child parked), so the bound
/// is the parent's: the child does the call and reports `(answer, millis)`
/// through a pipe, and the parent waits `BOUND_MS` for it. A child still silent
/// then is killed and the claim fails.
fn blocking_answer(f: impl FnOnce() -> isize, bound_ms: i32) -> Option<(isize, i32, u64)> {
    let mut fds = [0i32; 2];
    if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
        return None;
    }
    let pid = unsafe { libc::fork() };
    if pid == 0 {
        unsafe { libc::close(fds[0]) };
        let start = std::time::Instant::now();
        let n = f();
        // The errno is this child's, so it has to travel with the answer.
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
    let mut n = [0u8; 8];
    n.copy_from_slice(&out[..8]);
    let mut e = [0u8; 4];
    e.copy_from_slice(&out[8..12]);
    let mut ms = [0u8; 8];
    ms.copy_from_slice(&out[12..]);
    Some((
        i64::from_le_bytes(n) as isize,
        i32::from_le_bytes(e),
        u64::from_le_bytes(ms),
    ))
}

/// `EAGAIN` in this call's errno, read the same way [`failed_with`] does.
fn n_was_eagain() -> bool {
    std::io::Error::last_os_error().raw_os_error() == Some(libc::EAGAIN)
}
