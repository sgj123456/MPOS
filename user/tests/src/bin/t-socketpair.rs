//! t-socketpair - the channel `wget` hands its TLS helper.
//!
//! `socketpair(2)` makes no network connection: it makes two descriptors that
//! are already connected to each other. That is exactly what a program needs
//! when it wants to hand a private channel to a helper process - and it is not
//! a convenience, it is the whole mechanism behind busybox's HTTPS: `wget`
//! makes a pair, forks, gives one end to `ssl_client`, and from then on the
//! TLS is somebody else's problem. A kernel that refuses the call means
//! `https` cannot work at all, no matter how good the TLS library is.
//!
//! What is pinned here is the contract such a program relies on:
//!
//! * the pair carries bytes in *both* directions over the same two
//!   descriptors - a pipe, which is one-way, is not a substitute;
//! * a half-close (`shutdown` with `SHUT_WR`) delivers what is already in
//!   flight and then tells the peer the stream has ended, while leaving the
//!   other direction running - which is how a helper is told "the request is
//!   finished" without also being told to stop answering;
//! * the descriptors survive `execve`, so the helper really can use one as
//!   its standard input;
//! * a `SOCK_SEQPACKET` pair - udevd's worker notification channel - opens
//!   too, and its writes arrive as whole messages, boundaries kept;
//! * the refusals name the right thing: Linux answers a *family* it does not
//!   know with `EAFNOSUPPORT`, a family it knows but has no pair operation
//!   for with `EOPNOTSUPP`, and a protocol the pair cannot carry with
//!   `EPROTONOSUPPORT`. All three were checked against a real Linux rather
//!   than recalled.
//!
//! Written with `rustix`, so every errno compared below is the one Linux
//! defines rather than one this test remembered.

use rustix::io::{Errno, read, write};
use rustix::net::{
    AddressFamily, Shutdown, SocketFlags, SocketType, ipproto, shutdown, socketpair,
};
use std::process::{Command, Stdio};

/// What the test reports: a name, and whether the claim held.
struct Report {
    failed: bool,
}

impl Report {
    fn check(&mut self, name: &str, ok: bool) {
        println!("{}  {}", if ok { "ok:  " } else { "FAIL:" }, name);
        self.failed |= !ok;
    }

    fn check_errno(&mut self, name: &str, got: Errno, want: Errno) {
        self.check(&format!("{name} (got {got}, want {want})"), got == want);
    }
}

fn main() {
    println!("t-socketpair");
    let mut r = Report { failed: false };

    let pair = socketpair(
        AddressFamily::UNIX,
        SocketType::STREAM,
        SocketFlags::empty(),
        None,
    );
    let (a, b) = match pair {
        Ok(pair) => pair,
        Err(err) => {
            r.check(
                &format!("an AF_UNIX stream pair is created (got {err})"),
                false,
            );
            std::process::exit(1);
        }
    };
    r.check("an AF_UNIX stream pair is created", true);

    // Both directions, over those two descriptors and no others. A one-way
    // channel would pass the first half of this and fail the second.
    let mut buf = [0u8; 64];
    r.check("the first end takes a write", write(&a, b"ping").is_ok());
    match read(&b, &mut buf[..]) {
        Ok(n) => r.check(
            &format!("the second end reads it back ({n} bytes)"),
            &buf[..n] == b"ping",
        ),
        Err(err) => r.check(&format!("the second end reads it back (got {err})"), false),
    }
    r.check(
        "and the second end takes a write too",
        write(&b, b"pong").is_ok(),
    );
    match read(&a, &mut buf[..]) {
        Ok(n) => r.check(
            &format!("which the first end reads back ({n} bytes)"),
            &buf[..n] == b"pong",
        ),
        Err(err) => r.check(
            &format!("which the first end reads back (got {err})"),
            false,
        ),
    }

    // The half-close. This is the operation `wget` needs to tell its helper
    // that the request is over: what was sent must still arrive, the far end
    // must then see the end of the stream, and neither end's *reading* may be
    // disturbed by it.
    r.check(
        "a write before the half-close is accepted",
        write(&a, b"last").is_ok(),
    );
    r.check(
        "the write half can be closed",
        shutdown(&a, Shutdown::Write).is_ok(),
    );
    match read(&b, &mut buf[..]) {
        Ok(n) => r.check(
            &format!("what was in flight still arrives ({n} bytes)"),
            &buf[..n] == b"last",
        ),
        Err(err) => r.check(
            &format!("what was in flight still arrives (got {err})"),
            false,
        ),
    }
    match read(&b, &mut buf[..]) {
        Ok(0) => r.check("and then the peer is told the stream has ended", true),
        Ok(n) => r.check(
            &format!("and then the peer is told the stream has ended (got {n} bytes)"),
            false,
        ),
        Err(err) => r.check(
            &format!("and then the peer is told the stream has ended (got {err})"),
            false,
        ),
    }
    r.check(
        "but the peer's own direction is untouched by it",
        write(&b, b"still").is_ok(),
    );
    match read(&a, &mut buf[..]) {
        Ok(n) => r.check(
            &format!("so it can still be heard ({n} bytes)"),
            &buf[..n] == b"still",
        ),
        Err(err) => r.check(&format!("so it can still be heard (got {err})"), false),
    }

    // The other half-close: closing the reading direction is how a program
    // says it has stopped listening, and the peer finds out by the write
    // failing. (Checked against a real Linux: `EPIPE`, not a silent drop.)
    match socketpair(
        AddressFamily::UNIX,
        SocketType::STREAM,
        SocketFlags::empty(),
        None,
    ) {
        Ok((c, d)) => {
            r.check(
                "the read half can be closed",
                shutdown(&c, Shutdown::Read).is_ok(),
            );
            match write(&d, b"anybody there") {
                Ok(n) => r.check(
                    &format!("a write to a peer that stopped listening fails (stored {n})"),
                    false,
                ),
                Err(err) => r.check_errno(
                    "a write to a peer that stopped listening is EPIPE",
                    err,
                    Errno::PIPE,
                ),
            }
        }
        Err(err) => r.check(&format!("a second pair is created (got {err})"), false),
    }

    // The refusals. Each one is a case a caller is expected to branch on.
    for (label, domain, want) in [
        (
            "a family with no pair operation is EOPNOTSUPP",
            AddressFamily::INET,
            Errno::OPNOTSUPP,
        ),
        (
            "a family this kernel does not speak is EAFNOSUPPORT",
            AddressFamily::APPLETALK,
            Errno::AFNOSUPPORT,
        ),
    ] {
        match socketpair(domain, SocketType::STREAM, SocketFlags::empty(), None) {
            Ok(_) => r.check(label, false),
            Err(err) => r.check_errno(label, err, want),
        }
    }
    match socketpair(
        AddressFamily::UNIX,
        SocketType::STREAM,
        SocketFlags::empty(),
        Some(ipproto::TCP),
    ) {
        Ok(_) => r.check(
            "a protocol a Unix pair cannot carry is EPROTONOSUPPORT",
            false,
        ),
        Err(err) => r.check_errno(
            "a protocol a Unix pair cannot carry is EPROTONOSUPPORT",
            err,
            Errno::PROTONOSUPPORT,
        ),
    }
    // A word that is not a socket type at all is the syscall layer's EINVAL,
    // before the family is asked. Measured on Linux 7.2.11: 99 is EINVAL;
    // `SOCK_RDM` is ESOCKTNOSUPPORT and `SOCK_RAW` is served by AF_UNIX (the
    // last is a gap here, not a shape this claim covers).
    match socketpair(
        AddressFamily::UNIX,
        SocketType::from_raw(99),
        SocketFlags::empty(),
        None,
    ) {
        Ok(_) => r.check("a word that is not a socket type is EINVAL", false),
        Err(err) => r.check_errno(
            "a word that is not a socket type is EINVAL",
            err,
            Errno::INVAL,
        ),
    }

    // The message pair: udevd's worker notification channel is a
    // `SOCK_SEQPACKET` socketpair, and what it carries is one-byte
    // announcements that must arrive as separate messages, not coalesce.
    // A pair of these opens, and each write is one whole message the
    // reading end takes one at a time.
    match socketpair(
        AddressFamily::UNIX,
        SocketType::SEQPACKET,
        SocketFlags::empty(),
        None,
    ) {
        Ok((m, n)) => {
            r.check("a SOCK_SEQPACKET pair is created", true);
            r.check("the first message goes in", write(&m, b"add").is_ok());
            r.check(
                "and the second goes in behind it",
                write(&m, b"del").is_ok(),
            );
            let mut buf = [0u8; 16];
            match read(&n, &mut buf) {
                Ok(got) => r.check(
                    &format!("the peer reads the first message whole ({got} bytes)"),
                    &buf[..got] == b"add",
                ),
                Err(err) => r.check(
                    &format!("the peer reads the first message whole (got {err})"),
                    false,
                ),
            }
            match read(&n, &mut buf) {
                Ok(got) => r.check(
                    &format!("then the second, still as its own message ({got} bytes)"),
                    &buf[..got] == b"del",
                ),
                Err(err) => r.check(
                    &format!("then the second, still as its own message (got {err})"),
                    false,
                ),
            }
        }
        Err(err) => r.check(
            &format!("a SOCK_SEQPACKET pair is created (got {err})"),
            false,
        ),
    }

    // And the shape it is actually used in: one end handed to a child process
    // as its standard input. `/bin/cat` copies what it reads to its own
    // standard output, so a line that made the trip across `execve`, through
    // the pair, and back out over the child's output is proof the descriptor
    // survived - which is the part `wget` depends on and nothing above tests.
    //
    // The write half is closed *before* the child starts, so the child sees
    // the end of the stream and exits on its own: the test cannot hang waiting
    // for a helper that is waiting for input.
    let out_path = "/tmp/socketpair-child.txt";
    r.check(
        "the helper's output file is created",
        std::fs::write(out_path, b"").is_ok(),
    );
    let line = b"through the pair\n";
    let handed = write(&b, line);
    r.check(
        &format!("a line goes into the pair for the child ({handed:?})"),
        handed == Ok(line.len()),
    );
    r.check(
        "the write half is closed before the child starts",
        shutdown(&b, Shutdown::Write).is_ok(),
    );
    match std::fs::File::create(out_path) {
        Ok(out) => {
            let status = Command::new("/bin/cat")
                .stdin(Stdio::from(a))
                .stdout(Stdio::from(out))
                .status();
            r.check(
                &format!("the child runs to completion ({status:?})"),
                matches!(status, Ok(s) if s.success()),
            );
            match std::fs::read(out_path) {
                Ok(bytes) => r.check(
                    &format!(
                        "and read back what the pair carried ({:?})",
                        String::from_utf8_lossy(&bytes)
                    ),
                    bytes.as_slice() == &line[..],
                ),
                Err(err) => r.check(&format!("and its output is readable (got {err})"), false),
            }
        }
        Err(err) => r.check(
            &format!("the helper's output file opens (got {err})"),
            false,
        ),
    }

    if r.failed {
        println!("t-socketpair: FAILED");
        std::process::exit(1);
    }
    println!("t-socketpair: all passed");
}
