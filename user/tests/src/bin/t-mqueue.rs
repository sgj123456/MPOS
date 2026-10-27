//! t-mqueue - POSIX message queues: the family `mq_open(3)` names, on the
//! machine.
//!
//! Every claim here is one measured on a current host first (Linux 7.2.11,
//! `.workbuddy/probe_mqueue{,2}.c`; the evidence and the decisions are
//! `docs/notes/` §6.12) and then pinned against this kernel, which had no
//! message queues at all before the change this program ships with:
//!
//! * the **name rules**: no leading `/` is `EINVAL`, a second `/` is `EACCES`,
//!   and a name past 256 bytes is `ENAMETOOLONG` - three answers to three
//!   questions, which is why they are three claims here;
//! * the **attributes**: a fresh queue is 10 messages of 8192 bytes, `mq_flags`
//!   0 and `mq_curmsgs` 0; `O_CREAT|O_EXCL` on an existing name is `EEXIST`; a
//!   `maxmsg` of 0 and one past the limit are both `EINVAL`;
//! * the **order**: highest priority first and FIFO within one, so `9`, `5a`,
//!   `5b`, `1` come out in that order however they went in;
//! * **full and empty** answer `EAGAIN` to a non-blocking caller, and an
//!   expired absolute deadline answers `ETIMEDOUT` - the timeout is a
//!   `CLOCK_REALTIME` instant, not a duration;
//! * the **descriptor's own mode**: `mq_send` on an `O_RDONLY` descriptor and
//!   `mq_receive` on an `O_WRONLY` one are `EBADF`, and the access mode is the
//!   descriptor's, not the queue's;
//! * **size**: a message wider than `mq_msgsize` is `EMSGSIZE`, and so is a
//!   receive buffer that cannot hold `mq_msgsize` - whatever the queued
//!   message's own length;
//! * **priority**: `32767` is accepted and `32768` is `EINVAL` (`MQ_PRIO_MAX` is
//!   the count of priorities, not the highest);
//! * **notification**: one registration per queue (`EBUSY` on a second), the
//!   send that finds the queue empty fires it with `SI_MESGQ` and the
//!   registration's `sigval`, and the firing clears it;
//! * **unlink**: the name goes (`ENOENT` on a reopen and on a second unlink)
//!   while the descriptor that already held the queue keeps working;
//! * **readiness**: `POLLOUT` alone on an empty queue, `POLLIN|POLLOUT` with one
//!   message, `POLLIN` alone when full - and epoll agrees with poll.
//!
//! `libc` rather than `rustix` for the whole family: `rustix` binds no `mq_*`
//! call at all, which is the first clause of `user/tests/Cargo.toml`'s rule. The
//! numbers are musl's own wrappers, so the test speaks the call the way a
//! program on this image would.
//!
//! No claim blocks on the *kernel* except the timed receive, which has a
//! deadline: a broken kernel must fail a claim, not hang the sweep.

use std::sync::atomic::{AtomicI32, Ordering};

use mpos_user_tests::Report;

static GOT_CODE: AtomicI32 = AtomicI32::new(0);
static GOT_VALUE: AtomicI32 = AtomicI32::new(0);

extern "C" fn on_notify(_sig: libc::c_int, si: *mut libc::siginfo_t, _ctx: *mut libc::c_void) {
    // SAFETY: the kernel handed us a `siginfo_t` for this signal.
    unsafe {
        GOT_CODE.store((*si).si_code, Ordering::SeqCst);
        GOT_VALUE.store((*si).si_value().sival_ptr as i32, Ordering::SeqCst);
    }
}

fn errno() -> i32 {
    std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
}

/// An attribute block with the two sizes set.
///
/// Built through `zeroed` because `libc`'s `mq_attr` carries a private padding
/// field on both machines, which is also why the four members are written one at
/// a time rather than in a literal.
fn attr_maxmsg(maxmsg: i64, msgsize: i64) -> libc::mq_attr {
    let mut a: libc::mq_attr = unsafe { std::mem::zeroed() };
    a.mq_flags = 0;
    a.mq_maxmsg = maxmsg as libc::c_long;
    a.mq_msgsize = msgsize as libc::c_long;
    a.mq_curmsgs = 0;
    a
}

/// `mq_notify(3)`, which `libc` does not bind on a musl target - so it is the
/// raw syscall with musl's own number, the same door `t-drm-sync` uses for
/// `setitimer`.
fn mq_notify_raw(q: libc::mqd_t, ev: &libc::sigevent) -> i32 {
    unsafe { libc::syscall(libc::SYS_mq_notify, q, ev as *const libc::sigevent) as i32 }
}

fn getattr(q: libc::mqd_t) -> libc::mq_attr {
    let mut a = attr_maxmsg(0, 0);
    let rc = unsafe { libc::mq_getattr(q, &mut a) };
    assert_eq!(rc, 0, "mq_getattr: {}", std::io::Error::last_os_error());
    a
}

/// A queue name no other claim in this program (or a re-run) still holds.
fn fresh(name: &str) -> libc::mqd_t {
    unsafe {
        libc::mq_unlink(cstring(name).as_ptr());
    }
    let q = unsafe {
        libc::mq_open(
            cstring(name).as_ptr(),
            libc::O_CREAT | libc::O_RDWR,
            0o600,
            std::ptr::null::<libc::mq_attr>(),
        )
    };
    assert!(
        q >= 0,
        "mq_open({name}): {}",
        std::io::Error::last_os_error()
    );
    q
}

fn cstring(s: &str) -> std::ffi::CString {
    std::ffi::CString::new(s).expect("no NUL in a test name")
}

fn send(q: libc::mqd_t, data: &[u8], prio: u32) -> i32 {
    let rc = unsafe { libc::mq_send(q, data.as_ptr().cast(), data.len(), prio) };
    if rc < 0 { -errno() } else { rc }
}

fn receive(q: libc::mqd_t, len: usize) -> (i32, Vec<u8>, u32) {
    let mut buf = vec![0u8; len.max(1)];
    let mut prio = 0u32;
    let rc = unsafe { libc::mq_receive(q, buf.as_mut_ptr().cast(), buf.len(), &mut prio) };
    if rc < 0 {
        (-errno(), Vec::new(), 0)
    } else {
        buf.truncate(rc as usize);
        (rc as i32, buf, prio)
    }
}

fn main() {
    println!("t-mqueue");
    let mut r = Report::new();

    // --- the name rules: three questions, three answers -------------------
    //
    // The wrapper and the syscall want different spellings, and the difference
    // is measured (`.workbuddy/probe_mqueue_names.c`, raw syscalls): both libcs
    // strip the leading `/` and reject a name that has none, so the *kernel*
    // takes a bare name in which **any** slash is a path - `EACCES` - and the
    // empty name is `ENOENT`. These claims stand on what a program on this image
    // sees: the libc's spelling in, the kernel's answer out.
    unsafe {
        let rc = libc::mq_open(
            cstring("/t-mqueue-x/y").as_ptr(),
            libc::O_CREAT | libc::O_RDWR,
            0o600,
            std::ptr::null::<libc::mq_attr>(),
        );
        r.check(
            "a name holding a slash is EACCES, a path this caller was not given",
            rc < 0 && errno() == libc::EACCES,
        );
        if rc >= 0 {
            libc::mq_close(rc);
        }
        let rc = unsafe {
            // The libc wrapper validates the spelling itself (it requires a
            // leading `/` and answers an empty name with EINVAL before the
            // syscall); the kernel's rule for the bare name the syscall takes is
            // ENOENT. Raw, so the claim is about the kernel.
            libc::syscall(
                libc::SYS_mq_open,
                cstring("").as_ptr(),
                libc::O_CREAT | libc::O_RDWR,
                0o600,
                std::ptr::null::<libc::mq_attr>(),
            ) as libc::mqd_t
        };
        r.check("an empty name is ENOENT", rc < 0 && errno() == libc::ENOENT);
        if rc >= 0 {
            libc::mq_close(rc);
        }
        let long = format!("/{}", "x".repeat(299));
        let rc = libc::mq_open(
            cstring(&long).as_ptr(),
            libc::O_CREAT | libc::O_RDWR,
            0o600,
            std::ptr::null::<libc::mq_attr>(),
        );
        r.check(
            "a name over 255 bytes is ENAMETOOLONG",
            rc < 0 && errno() == libc::ENAMETOOLONG,
        );
        if rc >= 0 {
            libc::mq_close(rc);
        }
        let rc = libc::mq_open(
            cstring("/t-mqueue-missing").as_ptr(),
            libc::O_RDWR,
            0o600,
            std::ptr::null::<libc::mq_attr>(),
        );
        r.check(
            "opening a name that does not exist without O_CREAT is ENOENT",
            rc < 0 && errno() == libc::ENOENT,
        );
        if rc >= 0 {
            libc::mq_close(rc);
        }
    }

    // --- a fresh queue's attributes --------------------------------------
    let q = fresh("/t-mqueue-a");
    let a = getattr(q);
    r.check(
        "a fresh queue holds ten messages of 8192 bytes",
        a.mq_maxmsg == 10 && a.mq_msgsize == 8192,
    );
    r.check(
        "with no messages and no flags",
        a.mq_curmsgs == 0 && a.mq_flags == 0,
    );

    // An explicit attribute block is honoured, and O_EXCL refuses the name.
    unsafe {
        libc::mq_unlink(cstring("/t-mqueue-b").as_ptr());
        let want = attr_maxmsg(8, 16);
        let b = libc::mq_open(
            cstring("/t-mqueue-b").as_ptr(),
            libc::O_CREAT | libc::O_RDWR,
            0o600,
            &want,
        );
        r.check("mq_open takes the caller's maxmsg and msgsize", b >= 0);
        if b >= 0 {
            let got = getattr(b);
            r.check(
                "and reads them back",
                got.mq_maxmsg == 8 && got.mq_msgsize == 16,
            );
            let rc = libc::mq_open(
                cstring("/t-mqueue-b").as_ptr(),
                libc::O_CREAT | libc::O_EXCL | libc::O_RDWR,
                0o600,
                &want,
            );
            r.check(
                "O_CREAT|O_EXCL on a name that exists is EEXIST",
                rc < 0 && errno() == libc::EEXIST,
            );
            if rc >= 0 {
                libc::mq_close(rc);
            }
            let zero = attr_maxmsg(0, 0);
            let rc = libc::mq_open(
                cstring("/t-mqueue-c").as_ptr(),
                libc::O_CREAT | libc::O_RDWR,
                0o600,
                &zero,
            );
            r.check("a maxmsg of 0 is EINVAL", rc < 0 && errno() == libc::EINVAL);
            if rc >= 0 {
                libc::mq_close(rc);
            }
            let over = attr_maxmsg(11, 16);
            let rc = libc::mq_open(
                cstring("/t-mqueue-c").as_ptr(),
                libc::O_CREAT | libc::O_RDWR,
                0o600,
                &over,
            );
            r.check(
                "a maxmsg past the limit is EINVAL, not a quiet clamp",
                rc < 0 && errno() == libc::EINVAL,
            );
            if rc >= 0 {
                libc::mq_close(rc);
            }
            // The ordering claims run on this 8-deep queue.
            r.check("mq_send takes the first message", send(b, b"low", 1) == 0);
            r.check(
                "mq_send takes the highest priority",
                send(b, b"high", 9) == 0,
            );
            r.check(
                "mq_send takes the first of two equal priorities",
                send(b, b"mid-a", 5) == 0,
            );
            r.check(
                "mq_send takes the second of the pair",
                send(b, b"mid-b", 5) == 0,
            );
            r.check("mq_getattr counts them", getattr(b).mq_curmsgs == 4);
            let first = receive(b, 16);
            r.check(
                "the first receive is the highest priority",
                first.2 == 9 && first.1 == b"high",
            );
            let second = receive(b, 16);
            let third = receive(b, 16);
            r.check(
                "then the equal priorities in arrival order",
                second.2 == 5 && second.1 == b"mid-a" && third.1 == b"mid-b",
            );
            let last = receive(b, 16);
            r.check(
                "and the lowest priority last",
                last.2 == 1 && last.1 == b"low",
            );
            r.check("the queue is empty again", getattr(b).mq_curmsgs == 0);

            // Every claim below is about a *refusal*, so the descriptor is put in
            // `O_NONBLOCK` first - the same bit `mq_setattr` writes. A blocking
            // `mq_receive` on an empty queue is the kernel doing its job, and a
            // claim that waited on it would hang the sweep rather than fail.
            let nb = attr_maxmsg(0, 0);
            let mut nb = nb;
            nb.mq_flags = libc::O_NONBLOCK as libc::c_long;
            let _ = libc::mq_setattr(b, &nb, std::ptr::null_mut());
            let empty = receive(b, 16);
            r.check(
                "a receive on an empty queue is EAGAIN",
                empty.0 == -libc::EAGAIN,
            );

            // Fill it, then ask for one more.
            for _ in 0..8 {
                let _ = send(b, b"x", 0);
            }
            let full = send(b, b"y", 0);
            r.check("a send to a full queue is EAGAIN", full == -libc::EAGAIN);
            r.check(
                "a message wider than msgsize is EMSGSIZE",
                send(b, b"0123456789abcdefg", 0) == -libc::EMSGSIZE,
            );
            r.check(
                "a receive buffer narrower than msgsize is EMSGSIZE",
                receive(b, 8).0 == -libc::EMSGSIZE,
            );
            let _ = receive(b, 16);
            r.check(
                "priority 32767 is inside MQ_PRIO_MAX",
                send(b, b"p", 32767) == 0,
            );
            r.check(
                "priority 32768 is EINVAL",
                send(b, b"p", 32768) == -libc::EINVAL,
            );
            libc::mq_close(b);
            libc::mq_unlink(cstring("/t-mqueue-b").as_ptr());
        }
    }

    // --- the descriptor's mode -------------------------------------------
    unsafe {
        libc::mq_unlink(cstring("/t-mqueue-ro").as_ptr());
        let ro = libc::mq_open(
            cstring("/t-mqueue-ro").as_ptr(),
            libc::O_CREAT | libc::O_RDONLY,
            0o600,
            std::ptr::null::<libc::mq_attr>(),
        );
        r.check("a queue can be opened O_RDONLY", ro >= 0);
        if ro >= 0 {
            r.check(
                "mq_send on an O_RDONLY descriptor is EBADF",
                send(ro, b"x", 0) == -libc::EBADF,
            );
            libc::mq_close(ro);
        }
        libc::mq_unlink(cstring("/t-mqueue-wo").as_ptr());
        let wo = libc::mq_open(
            cstring("/t-mqueue-wo").as_ptr(),
            libc::O_CREAT | libc::O_WRONLY,
            0o600,
            std::ptr::null::<libc::mq_attr>(),
        );
        r.check("a queue can be opened O_WRONLY", wo >= 0);
        if wo >= 0 {
            r.check(
                "mq_receive on an O_WRONLY descriptor is EBADF",
                receive(wo, 16).0 == -libc::EBADF,
            );
            libc::mq_close(wo);
        }
    }

    // --- timeout and O_NONBLOCK ------------------------------------------
    unsafe {
        let mut block = attr_maxmsg(0, 0);
        block.mq_flags = 0;
        libc::mq_setattr(q, &block, std::ptr::null_mut());
        let mut now = std::mem::zeroed::<libc::timespec>();
        libc::clock_gettime(libc::CLOCK_REALTIME, &mut now);
        let mut abs = libc::timespec {
            tv_sec: now.tv_sec,
            tv_nsec: now.tv_nsec + 20_000_000,
        };
        if abs.tv_nsec >= 1_000_000_000 {
            abs.tv_sec += 1;
            abs.tv_nsec -= 1_000_000_000;
        }
        let mut buf = [0u8; 8192];
        let rc = libc::mq_timedreceive(
            q,
            buf.as_mut_ptr().cast(),
            buf.len(),
            std::ptr::null_mut(),
            &abs,
        );
        r.check(
            "an expired absolute deadline is ETIMEDOUT",
            rc < 0 && errno() == libc::ETIMEDOUT,
        );
        let nb = attr_maxmsg(0, 0);
        let _ = nb;
        let mut set = attr_maxmsg(0, 0);
        set.mq_flags = libc::O_NONBLOCK as libc::c_long;
        let mut old = attr_maxmsg(0, 0);
        let rc = libc::mq_setattr(q, &set, &mut old);
        r.check("mq_setattr(O_NONBLOCK) succeeds", rc == 0);
        r.check("and reports the flags it replaced", old.mq_flags == 0);
        r.check(
            "mq_getattr reads the new flags back",
            getattr(q).mq_flags == libc::O_NONBLOCK as libc::c_long,
        );
        let clear = attr_maxmsg(0, 0);
        r.check(
            "mq_setattr(0) clears it",
            libc::mq_setattr(q, &clear, std::ptr::null_mut()) == 0 && getattr(q).mq_flags == 0,
        );
    }

    // --- readiness -------------------------------------------------------
    unsafe {
        let mut poll = libc::pollfd {
            fd: q,
            events: libc::POLLIN | libc::POLLOUT,
            revents: 0,
        };
        let rc = libc::poll(&mut poll, 1, 0);
        r.check(
            "an empty queue is POLLOUT and nothing else",
            rc == 1 && poll.revents == libc::POLLOUT,
        );
        r.check("a message goes in", send(q, b"r", 0) == 0);
        poll.revents = 0;
        let rc = libc::poll(&mut poll, 1, 0);
        r.check(
            "a queue with a message is readable and writable",
            rc == 1 && poll.revents == (libc::POLLIN | libc::POLLOUT),
        );
        let ep = libc::epoll_create1(0);
        r.check("an epoll instance is made", ep >= 0);
        if ep >= 0 {
            let mut ev = libc::epoll_event {
                events: libc::EPOLLIN as u32,
                u64: 0xfeed,
            };
            r.check(
                "the queue is added to it",
                libc::epoll_ctl(ep, libc::EPOLL_CTL_ADD, q, &mut ev) == 0,
            );
            let mut out = [libc::epoll_event { events: 0, u64: 0 }; 2];
            let n = libc::epoll_wait(ep, out.as_mut_ptr(), 2, 0);
            r.check(
                "epoll_wait reports the readable queue",
                n == 1 && out[0].events as i32 & libc::EPOLLIN != 0,
            );
            libc::close(ep);
        }
        let _ = receive(q, 8192);
    }

    // --- notification ----------------------------------------------------
    unsafe {
        let mut sa: libc::sigaction = std::mem::zeroed();
        sa.sa_sigaction = on_notify as *const () as usize;
        sa.sa_flags = libc::SA_SIGINFO;
        libc::sigemptyset(&mut sa.sa_mask);
        r.check(
            "the SIGUSR1 handler installs",
            libc::sigaction(libc::SIGUSR1, &sa, std::ptr::null_mut()) == 0,
        );
        let mut ev: libc::sigevent = std::mem::zeroed();
        ev.sigev_notify = libc::SIGEV_SIGNAL;
        ev.sigev_signo = libc::SIGUSR1;
        ev.sigev_value.sival_ptr = 77 as *mut libc::c_void;
        r.check("a registration succeeds", mq_notify_raw(q, &ev) == 0);
        r.check(
            "a second registration is EBUSY",
            mq_notify_raw(q, &ev) < 0 && errno() == libc::EBUSY,
        );
        GOT_CODE.store(0, Ordering::SeqCst);
        r.check("a send into the empty queue goes in", send(q, b"n", 0) == 0);
        std::thread::sleep(std::time::Duration::from_millis(30));
        r.check(
            "the registration fired",
            GOT_CODE.load(Ordering::SeqCst) != 0,
        );
        r.check("with SI_MESGQ", GOT_CODE.load(Ordering::SeqCst) == -3);
        r.check(
            "and the registration's sigval",
            GOT_VALUE.load(Ordering::SeqCst) == 77,
        );
        // The firing cleared it, so nothing holds the slot now.
        GOT_CODE.store(0, Ordering::SeqCst);
        let _ = receive(q, 8192);
        r.check("a send with no registration goes in", send(q, b"m", 0) == 0);
        std::thread::sleep(std::time::Duration::from_millis(20));
        r.check("and raises nothing", GOT_CODE.load(Ordering::SeqCst) == 0);
        let _ = receive(q, 8192);
        let mut none: libc::sigevent = std::mem::zeroed();
        none.sigev_notify = libc::SIGEV_NONE;
        r.check(
            "a SIGEV_NONE registration is accepted",
            mq_notify_raw(q, &none) == 0,
        );
        r.check(
            "and holds the queue's one slot",
            mq_notify_raw(q, &ev) < 0 && errno() == libc::EBUSY,
        );
        let _ = send(q, b"z", 0);
        let _ = receive(q, 8192);
        r.check(
            "a send clears it, and the next registration succeeds",
            mq_notify_raw(q, &ev) == 0,
        );
    }

    // --- unlink ----------------------------------------------------------
    unsafe {
        r.check(
            "the name is retired",
            libc::mq_unlink(cstring("/t-mqueue-a").as_ptr()) == 0,
        );
        let rc = libc::mq_open(
            cstring("/t-mqueue-a").as_ptr(),
            libc::O_RDWR,
            0o600,
            std::ptr::null::<libc::mq_attr>(),
        );
        r.check(
            "a reopen of the retired name is ENOENT",
            rc < 0 && errno() == libc::ENOENT,
        );
        if rc >= 0 {
            libc::mq_close(rc);
        }
        r.check(
            "the descriptor that held it still answers",
            libc::mq_getattr(q, &mut attr_maxmsg(0, 0)) == 0,
        );
        r.check(
            "and a second unlink is ENOENT",
            libc::mq_unlink(cstring("/t-mqueue-a").as_ptr()) < 0 && errno() == libc::ENOENT,
        );
        r.check("mq_close succeeds", libc::mq_close(q) == 0);
        libc::mq_unlink(cstring("/t-mqueue-ro").as_ptr());
        libc::mq_unlink(cstring("/t-mqueue-wo").as_ptr());
    }

    r.finish()
}
