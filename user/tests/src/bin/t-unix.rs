//! t-unix - the AF_UNIX path cycle, spoken through `rustix`.
//!
//! The `t-unix` of the abitest suite drives the same feature with raw
//! syscalls and hand-built addresses; this one drives it the way a real
//! daemon would - `rustix`, whose errno constants *are* Linux's - because
//! a dbus or systemd client will never see the kernel's own vocabulary,
//! only its errnos. What is checked here is the cycle every local service
//! runs: create, bind a name, hear the name in the directory, accept a
//! connection by that name, exchange bytes, and watch the refusals a
//! careless caller meets on the way (a taken name, a bound-but-never-listened
//! one, a socket path opened as a file, a name unlinked under the walk).
//!
//! Where this kernel answers differently from Linux the assertion says so
//! in its own name: connecting to a bound socket that never called
//! `listen` is `ECONNREFUSED` here, where Linux would complete an implicit
//! rendezvous - one of the recorded divergences of the milestone.

use rustix::fs::{Mode, OFlags, stat, unlink};
use rustix::io::{Errno, read, write};
use rustix::net::{
    AddressFamily, SocketAddrUnix, SocketType, accept, bind, connect, getpeername, getsockname,
    listen, socket,
};
use std::os::fd::AsFd;

/// The file-type bits of `struct stat`, and the socket shape among them.
const S_IFMT: u32 = 0o170_000;
const S_IFSOCK: u32 = 0o140_000;

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

fn unix_stream() -> Result<rustix::fd::OwnedFd, Errno> {
    socket(AddressFamily::UNIX, SocketType::STREAM, None)
}

fn main() {
    println!("t-unix");
    let mut r = Report { failed: false };

    // A name of our own, so two runs of the test cannot collide on it.
    let path = format!("/tmp/t-unix-{}.sock", std::process::id());
    let other = format!("{path}.other");
    let addr = SocketAddrUnix::new(&path).expect("socket path fits in sockaddr_un");
    let other_addr = SocketAddrUnix::new(&other).expect("second path fits");

    let server = match unix_stream() {
        Ok(fd) => {
            r.check("socket(AF_UNIX, SOCK_STREAM) opens through rustix", true);
            fd
        }
        Err(err) => {
            r.check(
                &format!("socket(AF_UNIX, SOCK_STREAM) opens through rustix (got {err})"),
                false,
            );
            std::process::exit(1);
        }
    };
    if let Err(err) = bind(&server, &addr) {
        r.check(&format!("bind creates the name (got {err})"), false);
        std::process::exit(1);
    }
    r.check("bind creates the name", true);

    // The directory carries the name as a socket, which is how `ls -l`
    // shows it as `s` and how `file` decides what it is looking at.
    match stat(&path) {
        Ok(st) => r.check(
            &format!("stat says the name is S_IFSOCK (mode {:#o})", st.st_mode),
            (st.st_mode as u32 & S_IFMT) == S_IFSOCK,
        ),
        Err(err) => r.check(&format!("stat of the name answers (got {err})"), false),
    }

    // The name read back from the socket itself, through rustix's own
    // address type - the conversion that any getaddrinfo-shaped caller does.
    match getsockname(&server).and_then(|any| SocketAddrUnix::try_from(any)) {
        Ok(back) => r.check(
            "getsockname echoes the bound path",
            back.path().is_some_and(|p| p.to_str() == Ok(path.as_str())),
        ),
        Err(err) => r.check(&format!("getsockname answers (got {err})"), false),
    }

    // A taken name is a refusal, not a second owner of the same listener.
    let also = unix_stream().expect("second socket");
    let err = bind(&also, &addr).unwrap_err();
    r.check_errno("binding a taken name is EADDRINUSE", err, Errno::ADDRINUSE);
    drop(also);

    if let Err(err) = listen(&server, 4) {
        r.check(&format!("listen opens the queue (got {err})"), false);
    } else {
        r.check("listen opens the queue", true);
    }

    // A bound, never-listened name: Linux would meet the caller in an
    // implicit backlog-0 rendezvous; this kernel says refused - the
    // divergence the milestone records, asserted rather than discovered.
    let lone = unix_stream().expect("lone socket");
    bind(&lone, &other_addr).expect("bind the second name");
    let probe = unix_stream().expect("probe socket");
    let err = connect(&probe, &other_addr).unwrap_err();
    r.check_errno(
        "connect to a bound, unlistened name is ECONNREFUSED (divergence: Linux joins)",
        err,
        Errno::CONNREFUSED,
    );
    drop(probe);
    drop(lone);
    let _ = unlink(&other);

    // The cycle: a client connects by name, the listener accepts, and the
    // two halves speak.
    let client = match unix_stream() {
        Ok(fd) => fd,
        Err(err) => {
            r.check(&format!("client socket opens (got {err})"), false);
            std::process::exit(1);
        }
    };
    if let Err(err) = connect(&client, &addr) {
        r.check(
            &format!("connect reaches the listener by name (got {err})"),
            false,
        );
        std::process::exit(1);
    }
    r.check("connect reaches the listener by name", true);

    let accepted = match accept(&server) {
        Ok(fd) => {
            r.check("accept hands out the connection", true);
            fd
        }
        Err(err) => {
            r.check(
                &format!("accept hands out the connection (got {err})"),
                false,
            );
            std::process::exit(1);
        }
    };

    // One exchange each way, byte-for-byte.
    const REQUEST: &[u8] = b"hello unix domain";
    const REPLY: &[u8] = b"and back again";
    let mut buf = [0u8; 64];
    if write(&client, REQUEST).is_err() {
        r.check("the client writes to the socket", false);
    } else {
        let got = read(accepted.as_fd(), &mut buf).unwrap_or(0);
        r.check(
            "the accepted socket reads what the client wrote",
            &buf[..got] == REQUEST,
        );
    }
    if write(&accepted, REPLY).is_err() {
        r.check("the server writes back", false);
    } else {
        buf = [0u8; 64];
        let got = read(&client, &mut buf).unwrap_or(0);
        r.check("the client reads the reply", &buf[..got] == REPLY);
    }

    // The anonymous peer: the client carries no name and the accepted
    // half is anonymous too, so getpeername answers an `AF_UNIX` with an
    // empty path - the empty name Linux gives for an unnamed socket.
    match getpeername(&client).map(|any| any.and_then(|a| SocketAddrUnix::try_from(a).ok())) {
        Ok(Some(back)) => r.check(
            "getpeername on the connected end is the empty unix name",
            back.path().is_none_or(|p| p.to_str() == Ok("")),
        ),
        Ok(None) => r.check("getpeername answers a unix address", false),
        Err(err) => r.check(
            &format!("getpeername on the connected end answers (got {err})"),
            false,
        ),
    }

    // A socket path is not a file: opening it is ENXIO, the answer the
    // kernel gives for a name that addresses a device that answers nothing.
    let err = rustix::fs::open(&path, OFlags::RDONLY, Mode::empty()).unwrap_err();
    r.check_errno("open on the socket path is ENXIO", err, Errno::NXIO);

    // An unconnected unix stream has no bytes to read - ENOTCONN, not EOF,
    // because "nobody is there" and "everybody left" are different facts.
    let fresh = unix_stream().expect("fresh socket");
    let err = read(&fresh, &mut buf).unwrap_err();
    r.check_errno(
        "read on an unconnected socket is ENOTCONN",
        err,
        Errno::NOTCONN,
    );
    drop(fresh);

    // The name goes when it is unlinked; the listener keeps living on its
    // descriptor, and a connect by the retired name is ENOENT.
    if let Err(err) = unlink(&path) {
        r.check(&format!("unlinking the name succeeds (got {err})"), false);
    } else {
        r.check("unlinking the name succeeds", true);
    }
    // An already-connected socket is not a way to look up a name - the
    // connect would be refused for its own state before resolution - so
    // the retired name is probed with a caller that has no state yet.
    let later = unix_stream().expect("later socket");
    let err = connect(&later, &addr).unwrap_err();
    r.check_errno("connect by the retired name is ENOENT", err, Errno::NOENT);
    drop(later);
    drop(client);
    drop(accepted);

    if r.failed {
        println!("FAILED");
        std::process::exit(1);
    }
    println!("PASS");
}
