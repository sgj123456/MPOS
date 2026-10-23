//! t-sigpipe - a write to a broken channel answers `EPIPE` and raises
//! `SIGPIPE`, and the two measured exemptions stay silent.
//!
//! Measured whole on a current host first (`.workbuddy/probe_sigpipe{,2,3,4}.c`,
//! Linux 7.2.11):
//!
//! * a pipe with no reader, a `SOCK_STREAM` pair whose peer closed, a reset
//!   stream and a send on a listener all answer `EPIPE` **and** run a
//!   `SIGPIPE` handler before the call returns; the record's `si_code` is
//!   `SI_USER` (0). `writev` raises once, `splice` and `sendfile` to a broken
//!   pipe raise too.
//! * `MSG_NOSIGNAL` keeps the `EPIPE` and suppresses the signal.
//! * a **`SOCK_SEQPACKET`** send to a dead peer answers `EPIPE` with **no**
//!   signal at all, and keeps answering `EPIPE`: the connection is remembered.
//! * a **connected `SOCK_DGRAM`** socket whose peer closed answers
//!   `ECONNREFUSED` **once**, forgets the connection, and answers `ENOTCONN`
//!   from the second call on - `send` and `write` alike, no signal either way.
//! * a socket that was never connected is `ENOTCONN`, and raises nothing.
//! * with the default disposition the writer **dies of SIGPIPE** (13); a
//!   blocked signal stays pending while the call answers `EPIPE`, and the
//!   handler runs when it is unblocked.
//!
//! The partial-write edge is measured in the notes rather than pinned here: a
//! pipe whose reader closes after this call has already landed bytes answers
//! their count *and* raises the signal, where a stream's same race answers the
//! count silently - a race this test cannot make deterministic.
//!
//! `libc` carries the dispositions, the masks and the two copy calls, because
//! `rustix` binds no handler-installing `sigaction`; everything else is
//! `rustix`, so each errno compared below is the one Linux defines.

use mpos_user_tests::Report;
use rustix::io::Errno;
use rustix::net::{AddressFamily, SendFlags, SocketFlags, SocketType, send, socket, socketpair};
use rustix::pipe::pipe;
use std::io::IoSlice;
use std::os::fd::{AsRawFd, OwnedFd};
use std::sync::atomic::{AtomicI32, AtomicUsize, Ordering};

/// How many times the `SIGPIPE` handler ran, and the last record's `si_code`.
static RUNS: AtomicUsize = AtomicUsize::new(0);
static LAST_CODE: AtomicI32 = AtomicI32::new(-999);

extern "C" fn on_pipe(_sig: libc::c_int, info: *mut libc::siginfo_t, _ctx: *mut libc::c_void) {
    RUNS.fetch_add(1, Ordering::SeqCst);
    if !info.is_null() {
        // SAFETY: the kernel handed us the record the handler was entered
        // with; reading its `si_code` is what `SA_SIGINFO` is for.
        LAST_CODE.store(unsafe { (*info).si_code }, Ordering::SeqCst);
    }
}

/// Install a caught `SIGPIPE`, resetting the counters the claims read.
fn catch_pipe() {
    let mut sa: libc::sigaction = unsafe { std::mem::zeroed() };
    sa.sa_sigaction = on_pipe as *const () as usize;
    sa.sa_flags = libc::SA_SIGINFO;
    unsafe {
        libc::sigemptyset(&mut sa.sa_mask);
        libc::sigaction(libc::SIGPIPE, &sa, std::ptr::null_mut());
    }
    RUNS.store(0, Ordering::SeqCst);
    LAST_CODE.store(-999, Ordering::SeqCst);
}

/// Go back to the `SIG_IGN` the Rust startup installed, so a claim that raises
/// the signal cannot end the program.
fn ignore_pipe() {
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_IGN);
    }
}

fn runs() -> usize {
    RUNS.load(Ordering::SeqCst)
}

fn code() -> i32 {
    LAST_CODE.load(Ordering::SeqCst)
}

/// `socketpair` with the type the claim is about, returning raw ends rather
/// than `OwnedFd` so a peer can be dropped by value.
fn pair(typ: SocketType) -> (OwnedFd, OwnedFd) {
    socketpair(AddressFamily::UNIX, typ, SocketFlags::empty(), None)
        .expect("an AF_UNIX pair is made")
}

fn main() {
    println!("t-sigpipe");
    let mut r = Report::new();

    // ---- the pipe: EPIPE, the signal, and its record -----------------------
    {
        ignore_pipe();
        let (read, write) = pipe().expect("pipe");
        drop(read);
        match rustix::io::write(&write, b"x") {
            Err(e) => r.check(
                &format!("a broken pipe answers EPIPE even when the signal is ignored (got {e})"),
                e == Errno::PIPE,
            ),
            Ok(n) => r.check(&format!("a broken pipe answers EPIPE (wrote {n})"), false),
        }

        catch_pipe();
        let (read, write) = pipe().expect("pipe");
        drop(read);
        let err = rustix::io::write(&write, b"x").unwrap_err();
        r.check(
            &format!("the caught case still answers EPIPE (got {err})"),
            err == Errno::PIPE,
        );
        r.check(
            &format!("the handler ran before `write` returned (runs={})", runs()),
            runs() == 1,
        );
        r.check(
            &format!("the record's si_code is SI_USER (got {})", code()),
            code() == 0,
        );

        // The other direction: a write that lands raises nothing.
        catch_pipe();
        let (read, write) = pipe().expect("pipe");
        let ok = rustix::io::write(&write, b"x").is_ok();
        drop(read);
        r.check(
            &format!(
                "a write that lands raises nothing (ok={ok}, runs={})",
                runs()
            ),
            ok && runs() == 0,
        );
    }

    // ---- a blocked SIGPIPE stays pending, then runs ------------------------
    {
        catch_pipe();
        let (read, write) = pipe().expect("pipe");
        drop(read);
        let mut set: libc::sigset_t = unsafe { std::mem::zeroed() };
        let mut old: libc::sigset_t = unsafe { std::mem::zeroed() };
        unsafe {
            libc::sigemptyset(&mut set);
            libc::sigaddset(&mut set, libc::SIGPIPE);
            libc::sigprocmask(libc::SIG_BLOCK, &set, &mut old);
        }
        let err = rustix::io::write(&write, b"x").unwrap_err();
        let mut pending: libc::sigset_t = unsafe { std::mem::zeroed() };
        unsafe { libc::sigpending(&mut pending) };
        let is_pending = unsafe { libc::sigismember(&pending, libc::SIGPIPE) } == 1;
        r.check(
            &format!("a blocked broken write still answers EPIPE (got {err})"),
            err == Errno::PIPE,
        );
        r.check(
            &format!("the signal waits in the pending set (pending={is_pending})"),
            is_pending,
        );
        r.check(
            &format!("and has not run yet (runs={})", runs()),
            runs() == 0,
        );
        unsafe { libc::sigprocmask(libc::SIG_UNBLOCK, &set, std::ptr::null_mut()) };
        r.check(
            &format!("unblocking runs it (runs={})", runs()),
            runs() == 1,
        );
    }

    // ---- a stream whose peer closed ---------------------------------------
    {
        catch_pipe();
        let (a, b) = pair(SocketType::STREAM);
        drop(b);
        let err = rustix::io::write(&a, b"x").unwrap_err();
        r.check(
            &format!("a stream write to a closed peer is EPIPE (got {err})"),
            err == Errno::PIPE,
        );
        r.check(
            &format!("and raised the signal (runs={})", runs()),
            runs() == 1,
        );

        catch_pipe();
        let (a, b) = pair(SocketType::STREAM);
        drop(b);
        let err = send(&a, b"x", SendFlags::empty()).unwrap_err();
        r.check(
            &format!("a stream `send` to a closed peer is EPIPE (got {err})"),
            err == Errno::PIPE,
        );
        r.check(
            &format!("and raised the signal too (runs={})", runs()),
            runs() == 1,
        );

        catch_pipe();
        let (a, b) = pair(SocketType::STREAM);
        drop(b);
        let err = send(&a, b"x", SendFlags::NOSIGNAL).unwrap_err();
        r.check(
            &format!("MSG_NOSIGNAL keeps the EPIPE (got {err})"),
            err == Errno::PIPE,
        );
        r.check(
            &format!("and suppresses the signal (runs={})", runs()),
            runs() == 0,
        );
    }

    // ---- the sequenced-packet exemption -----------------------------------
    {
        catch_pipe();
        let (a, b) = pair(SocketType::SEQPACKET);
        drop(b);
        let first = send(&a, b"x", SendFlags::empty()).unwrap_err();
        r.check(
            &format!("a seqpacket send to a closed peer is EPIPE (got {first})"),
            first == Errno::PIPE,
        );
        r.check(
            &format!("and raises no signal (runs={})", runs()),
            runs() == 0,
        );
        let second = send(&a, b"x", SendFlags::empty()).unwrap_err();
        r.check(
            &format!(
                "the connection is remembered, so the second send is EPIPE too (got {second})"
            ),
            second == Errno::PIPE,
        );
        let w = rustix::io::write(&a, b"x").unwrap_err();
        r.check(
            &format!("a plain write on the same end is EPIPE as well (got {w})"),
            w == Errno::PIPE && runs() == 0,
        );
    }

    // ---- the datagram refusal, once ---------------------------------------
    {
        catch_pipe();
        let (a, b) = pair(SocketType::DGRAM);
        drop(b);
        let first = send(&a, b"x", SendFlags::empty()).unwrap_err();
        r.check(
            &format!("a datagram send to a closed peer is ECONNREFUSED (got {first})"),
            first == Errno::CONNREFUSED,
        );
        r.check(
            &format!("and raises no signal (runs={})", runs()),
            runs() == 0,
        );
        let second = send(&a, b"x", SendFlags::empty()).unwrap_err();
        r.check(
            &format!("the connection is forgotten, so the second send is ENOTCONN (got {second})"),
            second == Errno::NOTCONN,
        );
        let w = rustix::io::write(&a, b"x").unwrap_err();
        r.check(
            &format!("and a write after it is ENOTCONN too (got {w})"),
            w == Errno::NOTCONN,
        );
    }

    // ---- a socket that never connected ------------------------------------
    {
        catch_pipe();
        let s = socket(AddressFamily::UNIX, SocketType::STREAM, None).expect("stream socket");
        let err = rustix::io::write(&s, b"x").unwrap_err();
        r.check(
            &format!("an unconnected stream write is ENOTCONN (got {err})"),
            err == Errno::NOTCONN,
        );
        let q = socket(AddressFamily::UNIX, SocketType::SEQPACKET, None).expect("seqpacket socket");
        let err = send(&q, b"x", SendFlags::empty()).unwrap_err();
        r.check(
            &format!("an unconnected seqpacket send is ENOTCONN (got {err})"),
            err == Errno::NOTCONN && runs() == 0,
        );
    }

    // ---- a scattered write and the two copy calls -------------------------
    {
        catch_pipe();
        let (read, write) = pipe().expect("pipe");
        drop(read);
        let iov = [IoSlice::new(b"ab"), IoSlice::new(b"cd")];
        let err = rustix::io::writev(&write, &iov).unwrap_err();
        r.check(
            &format!("a writev to a broken pipe is EPIPE (got {err})"),
            err == Errno::PIPE,
        );
        r.check(
            &format!("and raised the signal once (runs={})", runs()),
            runs() == 1,
        );

        catch_pipe();
        let (input, input_write) = pipe().expect("pipe");
        rustix::io::write(&input_write, b"hello").expect("fill the input pipe");
        drop(input_write);
        let (output, output_write) = pipe().expect("pipe");
        drop(output);
        let n = unsafe {
            libc::splice(
                input.as_raw_fd(),
                std::ptr::null_mut(),
                output_write.as_raw_fd(),
                std::ptr::null_mut(),
                5,
                0,
            )
        };
        let e = std::io::Error::last_os_error().raw_os_error().unwrap_or(0);
        r.check(
            &format!("splice to a broken pipe is EPIPE (got {n}/{e})"),
            n == -1 && e == libc::EPIPE,
        );
        r.check(
            &format!("and raised the signal (runs={})", runs()),
            runs() == 1,
        );

        catch_pipe();
        let path = "/tmp/t-sigpipe-src";
        std::fs::write(path, b"hello").expect("write the source file");
        let src = std::fs::File::open(path).expect("open the source file");
        let (output, output_write) = pipe().expect("pipe");
        drop(output);
        let n = unsafe {
            libc::sendfile(
                output_write.as_raw_fd(),
                src.as_raw_fd(),
                std::ptr::null_mut(),
                5,
            )
        };
        let e = std::io::Error::last_os_error().raw_os_error().unwrap_or(0);
        r.check(
            &format!("sendfile to a broken pipe is EPIPE (got {n}/{e})"),
            n == -1 && e == libc::EPIPE,
        );
        r.check(
            &format!("and raised the signal (runs={})", runs()),
            runs() == 1,
        );
        let _ = std::fs::remove_file(path);
    }

    // ---- the default disposition: the writer dies -------------------------
    {
        let (read, write) = pipe().expect("pipe");
        drop(read);
        let pid = unsafe { libc::fork() };
        if pid == 0 {
            unsafe {
                libc::signal(libc::SIGPIPE, libc::SIG_DFL);
                let _ = rustix::io::write(&write, b"x");
                libc::_exit(0);
            }
        }
        let mut status: libc::c_int = 0;
        unsafe { libc::waitpid(pid, &mut status, 0) };
        let signaled = libc::WIFSIGNALED(status);
        let sig = if signaled { libc::WTERMSIG(status) } else { 0 };
        r.check(
            &format!("with SIG_DFL the writer dies of SIGPIPE (status={status}, sig={sig})"),
            signaled && sig == libc::SIGPIPE,
        );
    }

    ignore_pipe();
    r.finish();
}
