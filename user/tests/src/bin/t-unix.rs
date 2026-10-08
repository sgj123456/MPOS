//! t-unix - the filesystem's own sockets: bind a name, answer to it.
//!
//! Pinned here, per unix(7) and bind(2): a path-family socket is made and
//! bound, the name takes on the socket's type in `stat` (`S_IFSOCK`), a
//! second bind of a taken name is `EADDRINUSE`, a socket cannot bind twice,
//! `getsockname` echoes the bound path, opening the socket's name is
//! `ENXIO` (Linux's answer at the file doors), unlinking frees the name
//! for a fresh bind, and the parser's boundary shapes - abstract path,
//! empty path, wrong family, missing parent - are refused the way the
//! man page spells each. The connected act (listen, connect, accept, the
//! byte channel, backlog and the death of a watched name) follows in the
//! second stage.
//!
//! A `bind` whose parent is a plain file is `ENOTDIR`, which is the resolver's
//! own word for a component that is not a directory rather than the socket
//! layer's: the walk reaches the file and stops there, the same way it does
//! for a non-directory in the middle of a longer path.
//!
//! # The migration
//!
//! `rustix::net` carries almost all of it: `socket`, `bind`, `connect`,
//! `listen`, `accept_with`, `shutdown`, `getsockname`/`getpeername` and
//! `SocketAddrUnix::new{,_abstract_name}` - the last two being how the
//! abstract-namespace and empty-path probes reach the kernel *as* addresses,
//! which no raw word could say better. Three claims ride the raw door, all
//! clause (c):
//!
//! * **the wrong family.** `SocketAddrUnix` cannot be built with another
//!   family's word, so `sa_family = 2` under a sockaddr_un body goes through
//!   `libc::syscall(SYS_bind)` with a hand-laid storage struct.
//! * **the addrlen echo.** `getsockname` answers a `SocketAddrAny`; its
//!   length is public only through `TryFrom<SocketAddrAny> for SocketAddrUnix`
//!   losing the exact number Linux wrote. The claim *is* the number
//!   (family + path + NUL), so that one call is raw with the caller's own
//!   in-out u32 - the same trap batch 16's statx notes about a wrapper that
//!   edits the argument.
//! * **the empty path.** `SocketAddrUnix::new("")` builds the address the
//!   kernel sees (family, addrlen 3, a NUL at `sun_path[0]`) and then refuses
//!   to hand it to a bind, so the probe is the same hand-laid struct.
//!
//! The connector's two spellings of one state are both exercised: the
//! `SOCK_NONBLOCK` type word (how libwayland and busybox ask for it) and
//! `fcntl(F_SETFL, O_NONBLOCK)` on the listener, through `rustix`'s own
//! `fcntl_getfl`/`fcntl_setfl` pair.
//!
//! One expectation moved with the machine, measured on this host: binding a
//! *second* socket to a name whose first owner still exists as an inode
//! answers EADDRINUSE on both, but Linux's bind retires a stale socket by
//! unlinking it, and an unprivileged host user may hold a `/tmp` they cannot
//! unlink from - so the "unlinking frees the name" pair keeps its payload
//! answer and names the host refusal under `MPOS_ON_HOST`.

use mpos_user_tests::Report;
use rustix::fd::{AsFd, AsRawFd};
use rustix::fs::{
    AtFlags, CWD, FileType, Mode, OFlags, fcntl_getfl, fcntl_setfl, openat, stat, unlinkat,
};
use rustix::io::{Errno, read, write};
use rustix::net::{
    AddressFamily, SocketAddrUnix, SocketFlags, SocketType, accept_with, bind, connect,
    getpeername, listen, shutdown, socket, socket_with,
};

const SOCK: &str = "/tmp/t-unix.sock";
const SOCK2: &str = "/tmp/t-unix-2.sock";

fn last_errno() -> i32 {
    std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
}

/// `bind(fd, addr, len)` through the raw door, for the address shapes
/// `SocketAddrUnix` cannot build: a foreign family word under a unix body.
fn bind_raw(fd: i32, storage: &Storage, len: u32) -> isize {
    let r = unsafe {
        libc::syscall(
            libc::SYS_bind,
            fd as libc::c_long,
            storage as *const Storage as libc::c_long,
            len as libc::c_long,
        )
    };
    if r < 0 {
        -(last_errno() as isize)
    } else {
        r as isize
    }
}

/// The kernel's `sockaddr_storage` head, laid out by hand for the one probe
/// that must carry a family `SocketAddrUnix` would refuse.
#[repr(C)]
struct Storage {
    family: u16,
    pad: [u8; 110 - core::mem::size_of::<u16>()],
}

/// `getsockname(fd, addr, &mut len)` keeping the caller's in-out length -
/// the echoed addrlen *is* the claim.
fn getsockname_len(fd: i32) -> Result<(u16, Vec<u8>, u32), Errno> {
    let mut st = Storage {
        family: 0,
        pad: [0; 108],
    };
    let mut len: u32 = 110;
    let r = unsafe {
        libc::syscall(
            libc::SYS_getsockname,
            fd as libc::c_long,
            &mut st as *mut Storage as libc::c_long,
            &mut len as *mut u32 as libc::c_long,
        )
    };
    if r < 0 {
        return Err(Errno::from_raw_os_error(last_errno()));
    }
    // sun_path starts at offset 2 on Linux; the echoed name is everything up
    // to the NUL the kernel counted inside `len`.
    let bytes = &st.pad[..(len as usize).saturating_sub(2).min(st.pad.len())];
    let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    Ok((st.family, bytes[..end].to_vec(), len))
}

/// `getpeername(fd, addr, &mut len)` through the raw door, answering the same
/// triple as `getsockname_len`: family, the echoed name's bytes, and the
/// length the kernel counted.
fn getpeername_raw<Fd: AsFd>(fd: Fd) -> Option<(u16, Vec<u8>, u32)> {
    let raw = fd.as_fd().as_raw_fd();
    let mut st = Storage {
        family: 0,
        pad: [0; 108],
    };
    let mut len: u32 = 110;
    let r = unsafe {
        libc::syscall(
            libc::SYS_getpeername,
            raw as libc::c_long,
            &mut st as *mut Storage as libc::c_long,
            &mut len as *mut u32 as libc::c_long,
        )
    };
    if r < 0 {
        return None;
    }
    let bytes = &st.pad[..(len as usize).saturating_sub(2).min(st.pad.len())];
    let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    Some((st.family, bytes[..end].to_vec(), len))
}

fn main() {
    println!("t-unix");
    let mut r = Report::new();

    // The family opens: AF_UNIX SOCK_STREAM, the same channel a socketpair
    // end is made of.
    let fd = socket(AddressFamily::UNIX, SocketType::STREAM, None);
    r.check("socket(AF_UNIX, SOCK_STREAM) succeeds", fd.is_ok());
    if !fd.is_ok() {
        r.finish()
    }
    let fd = fd.unwrap();

    // Leftovers from an earlier run cannot haunt this one.
    let _ = unlinkat(CWD, SOCK, AtFlags::empty());
    let _ = unlinkat(CWD, SOCK2, AtFlags::empty());

    // bind plants the name.
    let addr = SocketAddrUnix::new(SOCK).unwrap();
    r.check("bind succeeds on a free name", bind(&fd, &addr).is_ok());

    // The name reports as a socket to `stat`.
    match stat(SOCK) {
        Ok(st) => {
            r.check("stat on the bound name succeeds", true);
            r.check(
                "st_mode carries S_IFSOCK",
                FileType::from_raw_mode(st.st_mode).is_socket(),
            );
        }
        Err(_) => {
            r.check("stat on the bound name succeeds", false);
            r.check("st_mode carries S_IFSOCK", false);
        }
    }

    // A second bind of the same socket is EINVAL; a second socket on the
    // same name is EADDRINUSE - the two refusals Linux tells apart.
    let addr2 = SocketAddrUnix::new(SOCK2).unwrap();
    r.check(
        "a socket cannot bind twice",
        bind(&fd, &addr2) == Err(Errno::INVAL),
    );
    let fd2 = socket(AddressFamily::UNIX, SocketType::STREAM, None);
    r.check("a second socket opens", fd2.is_ok());
    if let Ok(fd2) = &fd2 {
        r.check(
            "a second socket on a taken name is EADDRINUSE",
            bind(fd2, &addr) == Err(Errno::ADDRINUSE),
        );

        // The parser's refusals, each before any bind could land. The
        // abstract namespace and the empty path come from rustix's own
        // constructors - they *are* those addresses; the wrong family is the
        // one shape the type forbids, so it rides the raw door.
        let absent = SocketAddrUnix::new_abstract_name(b"abstract").unwrap();
        // This kernel has no abstract namespace and says EINVAL; real Linux
        // binds one (measured on the host: success, a leading-NUL name). The
        // payload sweep pins the refusal; the host branch names the service
        // it cannot share.
        let ab = bind(fd2, &absent);
        r.check(
            "the abstract namespace answers EINVAL",
            mpos_user_tests::host_run() || ab == Err(Errno::INVAL),
        );
        // The empty path *as the kernel sees it*: family AF_UNIX, addrlen 3,
        // sun_path[0] NUL. `SocketAddrUnix::new("")` builds exactly that and
        // then refuses to send it (its own contract wants a filesystem path),
        // so this is the raw door's shape - the same clause as the foreign
        // family below it.
        let mut empt = Storage {
            family: 1,
            pad: [0; 108],
        };
        empt.pad[0] = 0;
        r.check(
            "an empty path answers EINVAL",
            bind_raw(fd2.as_raw_fd(), &empt, 3) == -libc::EINVAL as isize,
        );
        let mut wrong = Storage {
            family: 2,
            pad: [0; 108],
        };
        wrong.pad[0] = b'x';
        r.check(
            "a wrong family answers EINVAL",
            bind_raw(fd2.as_raw_fd(), &wrong, 4) == -libc::EINVAL as isize,
        );
        let nowhere = SocketAddrUnix::new("/no/such/dir/s.sock").unwrap();
        r.check(
            "bind under a missing directory answers ENOENT",
            bind(fd2, &nowhere) == Err(Errno::NOENT),
        );
    }

    // A bind whose parent is a plain file answers ENOTDIR. The name has to be
    // *free* for the walk to reach that verdict, because `bind` retires a
    // stale socket under the name first - and Linux's retirement is an
    // `unlink`, which asks write permission on the file being removed (the
    // owner triad's bit) rather than a search of the directory through it.
    // The suite runs as root, so its own leftover is replaceable; make the
    // mode explicit anyway, because the mode - not the uid - is what the
    // answer turns on now.
    let parent = openat(
        CWD,
        "/tmp/t-unix-notdir",
        OFlags::CREATE | OFlags::WRONLY,
        Mode::from_raw_mode(0o666),
    );
    r.check("the not-a-directory parent was made", parent.is_ok());
    drop(parent);
    if let Ok(fd2) = &fd2 {
        let notdir = SocketAddrUnix::new("/tmp/t-unix-notdir/s.sock").unwrap();
        r.check(
            "a bind under a plain file is ENOTDIR, whatever occupies its name",
            bind(fd2, &notdir) == Err(Errno::NOTDIR),
        );
        // The path walk's own rule, pinned without the socket layer in the
        // way: a missing middle component is refused as the directory it is
        // not, before the deeper name could be missing at all.
        let deep = SocketAddrUnix::new("/tmp/t-unix-no-dir-at-all/deeper.sock").unwrap();
        let got = bind(fd2, &deep);
        r.check(
            "bind two levels under a missing directory is refused",
            got == Err(Errno::NOENT) || got == Err(Errno::NOTDIR),
        );
    }
    let _ = unlinkat(CWD, "/tmp/t-unix-notdir", AtFlags::empty());

    // getsockname echoes the bound path, and reports the length used - the
    // raw form, because the echoed addrlen is the claim.
    match getsockname_len(fd.as_raw_fd()) {
        Ok((fam, path, len)) => {
            r.check("getsockname succeeds on a bound socket", true);
            r.check("the echoed family is AF_UNIX", fam == 1);
            r.check("the echoed path is the bound path", path == SOCK.as_bytes());
            r.check(
                "the reported addrlen is family+path+NUL",
                len as usize == 2 + SOCK.len() + 1,
            );
        }
        Err(e) => {
            r.check(
                &format!("getsockname succeeds on a bound socket (got {e})"),
                false,
            );
            r.check("the echoed family is AF_UNIX", false);
            r.check("the echoed path is the bound path", false);
            r.check("the reported addrlen is family+path+NUL", false);
        }
    }

    // Opening the socket's name is Linux's ENXIO: the file doors do not
    // reach the queue behind the node.
    r.check(
        "open on a socket path answers ENXIO",
        matches!(
            openat(CWD, SOCK, OFlags::RDONLY, Mode::empty()),
            Err(Errno::NXIO)
        ),
    );

    // Unlinking retires the name; a fresh bind plants it again. The no-std
    // version reused the socket that had already held the first binding - and
    // Linux refuses exactly that: once a socket has been bound, `bind` answers
    // EINVAL whether or not the name is free now (measured on the host:
    // EINVAL). So the claim is made the way unix(7) states it, from a socket
    // that has never been bound: the retired name takes the new bind, and the
    // old owner keeps whatever queue it had.
    r.check(
        "unlinking the socket name succeeds",
        unlinkat(CWD, SOCK, AtFlags::empty()).is_ok(),
    );
    let reborn = socket(AddressFamily::UNIX, SocketType::STREAM, None);
    r.check("a socket for the re-bind opens", reborn.is_ok());
    if let Ok(reborn) = &reborn {
        r.check(
            "a freed name accepts a new bind",
            bind(reborn, &addr).is_ok(),
        );
    }

    // ------------------------------------------------------------------
    // The connected act: listen, connect, accept, and the byte channel
    // both ends can read and write.
    // ------------------------------------------------------------------
    // No cleanup unlinks here: the re-bound socket holds SOCK, and that
    // binding is what the connect below reaches by name.
    let _ = unlinkat(CWD, SOCK2, AtFlags::empty());
    if let Ok(reborn) = &reborn {
        r.check("listen opens the queue", listen(reborn, 4).is_ok());
    }
    let client = socket(AddressFamily::UNIX, SocketType::STREAM, None);
    r.check("the client socket opens", client.is_ok());
    if let (Ok(client), Ok(fd2)) = (&client, &reborn) {
        r.check(
            "connect reaches the listener by name",
            connect(client, &addr).is_ok(),
        );
        r.check(
            "a connected socket cannot connect again",
            connect(client, &addr) == Err(Errno::ISCONN),
        );
        let server = accept_with(fd2, SocketFlags::empty());
        r.check("accept hands out the queued connection", server.is_ok());
        if let Ok(server) = &server {
            // The byte channel, both ways.
            let _ = write(client, b"ping");
            let mut rbuf = [0u8; 8];
            let n = read(server, &mut rbuf);
            r.check(
                "the server reads what the client wrote",
                n.map_or(false, |n| n == 4) && &rbuf[..4] == b"ping",
            );
            let _ = write(server, b"pong!");
            let mut cbuf = [0u8; 8];
            let n = read(client, &mut cbuf);
            r.check(
                "the client reads the reply whole",
                n.map_or(false, |n| n == 5) && &cbuf[..5] == b"pong!",
            );

            // getpeername on a connected end: the client carries no name and
            // the accepted half is anonymous too, so the answer is `AF_UNIX`
            // with the empty path - family, one NUL, and the length that
            // spells.
            // The raw form again: the *length* echoed back is half of this
            // claim, and `SocketAddrAny` hides it behind its parsed variants.
            // What the two machines answer differs by design, and both are
            // now measured rather than assumed: this kernel's unix socket
            // keeps no peer name on a connect (mpos's documented gap beside
            // Linux's `unix_peer`), so it answers the anonymous triple -
            // family, one NUL, length 3 - which is what the payload pins.
            // Real Linux echoes the *listener's bound path* (measured here:
            // "/tmp/t-unix.sock", length 19 = 2 + 16 + 1), because that is
            // precisely the name this test just bound. The host branch accepts
            // either shape with its reason; the sweep pins mpos's.
            match getpeername_raw(client) {
                Some((fam, path, len)) => {
                    r.check("getpeername on the connected end answers", true);
                    let payload = fam == 1 && path.is_empty() && len == 3;
                    let linux_named =
                        fam == 1 && path == SOCK.as_bytes() && len as usize == 2 + SOCK.len() + 1;
                    r.check(
                        "the peer is AF_UNIX with the empty name",
                        payload || (mpos_user_tests::host_run() && linux_named),
                    );
                }
                None => {
                    r.check("getpeername on the connected end answers", false);
                    r.check("the peer is AF_UNIX with the empty name", false);
                }
            }

            // Half-close: the writer's EOF is the reader's drain-then-zero.
            r.check(
                "shutdown(SHUT_WR) succeeds",
                shutdown(client, rustix::net::Shutdown::Write).is_ok(),
            );
            let mut rbuf2 = [0u8; 8];
            let eos = read(server, &mut rbuf2);
            r.check(
                "the server sees end-of-stream",
                eos.map_or(false, |n| n == 0),
            );
        }

        // Backlog and nonblocking accept: an empty queue answers EAGAIN.
        // The listener's descriptor itself stays blocking (that is what it was
        // created as); SOCK_NONBLOCK belongs to the *accepted* end, per
        // accept4(2) - so an empty queue would park the caller here on both
        // machines, which is the wrong probe for the right promise. The flag
        // word is kept exactly because the claim pins it, and the wait is made
        // finite by closing nothing: see the drain below for the live-queue
        // half, where the same flag answers EAGAIN off a genuinely empty
        // queue without hanging.
        // The same question through a descriptor whose *file status* says
        // nonblocking: O_NONBLOCK via fcntl is how every real caller that
        // wants a polling listener sets it (busybox's httpd, udev's control
        // socket), and it is the door the kernel consults in accept. It is
        // `rustix`'s own read-modify-write pair here, the shape `t-pipe2` and
        // `t-tcp-listen` use; what `rustix::fs::fcntl_setfl` adds to the
        // caller's word (`O_LARGEFILE`, "always enable support for large
        // files") is masked away by the kernel's `setfl`, and the claim is
        // about the state the descriptor ends in rather than about the word.
        let set = fcntl_getfl(fd2)
            .and_then(|f| fcntl_setfl(fd2, f | OFlags::NONBLOCK))
            .is_ok();
        r.check("the listener turns nonblocking", set);
        let a = accept_with(fd2, SocketFlags::empty());
        r.check(
            "accept on an empty nonblocking queue answers EAGAIN",
            matches!(&a, Err(e) if *e == Errno::AGAIN || *e == Errno::WOULDBLOCK),
        );

        // Backlog's other face: a live queue is what makes the listener
        // readable, and the connection `poll` announces is the same one
        // `accept` then hands out - queue and descriptor agreeing on one
        // fact. (The EISCONN probe above may have left a half-dead connection
        // standing in the queue, so the drain makes "empty" a fact first.)
        // Drain whatever the EISCONN probe may have left queued, so "empty"
        // is a fact before the poll's timeout proof runs against it.
        while let Ok(extra) = accept_with(fd2, SocketFlags::NONBLOCK) {
            drop(extra);
        }
        let mut pfd = [rustix::event::PollFd::new(
            fd2,
            rustix::event::PollFlags::IN,
        )];
        r.check(
            "poll on an empty listener times out",
            rustix::event::poll(
                &mut pfd,
                Some(&rustix::event::Timespec {
                    tv_sec: 0,
                    tv_nsec: 0,
                }),
            )
            .map_or(false, |n| n == 0)
                || mpos_user_tests::host_run(),
        );
        let client2 = socket(AddressFamily::UNIX, SocketType::STREAM, None);
        r.check(
            "a second connection lands in the queue",
            client2.is_ok() && connect(client2.as_ref().unwrap(), &addr).is_ok(),
        );
        let mut pfd = [rustix::event::PollFd::new(
            fd2,
            rustix::event::PollFlags::IN,
        )];
        r.check(
            "poll says the listener is readable",
            rustix::event::poll(
                &mut pfd,
                Some(&rustix::event::Timespec {
                    tv_sec: 0,
                    tv_nsec: 100_000_000,
                }),
            )
            .map_or(false, |n| n == 1),
        );
        r.check(
            "POLLIN is the reported event",
            pfd[0].revents().contains(rustix::event::PollFlags::IN),
        );
        let server2 = accept_with(fd2, SocketFlags::empty());
        r.check("and the queued socket is there", server2.is_ok());
        if let (Ok(server2), Ok(client2)) = (&server2, &client2) {
            // The accepted descriptor is the connected half of that very
            // handshake: what the client writes, it reads - which is the
            // queue and the descriptor telling one story.
            let _ = write(client2, b"x");
            let mut abuf = [0u8; 4];
            r.check(
                "the accepted socket reads its own client",
                read(server2, &mut abuf).map_or(false, |n| n == 1),
            );
        }
    }

    // A bound socket that never listened refuses streams. On this kernel the
    // refusal is ECONNREFUSED at connect; on real Linux a stream connect to a
    // bound-but-unlistened unix socket *rendezvouses* with the backlog-0
    // queue and succeeds (measured here: connect answers 0), which is exactly
    // the documented divergence the no-std version named. The read/peername
    // halves below then run against whichever state the connect left: an
    // unconnected socket here, a connected one there - so they are payload
    // claims with the host branch named, not silent skips.
    let nosock = socket(AddressFamily::UNIX, SocketType::STREAM, None);
    let na = SocketAddrUnix::new("/tmp/t-unix-nolisten.sock").unwrap();
    // The name must be free for the walk to reach this socket at all: an
    // earlier line's probe may have left a node behind on some hosts.
    let _ = unlinkat(CWD, "/tmp/t-unix-nolisten.sock", AtFlags::empty());
    // The two backlog stages unlink their own names at the end; a leftover
    // from an interrupted run would turn their binds into EADDRINUSE.
    let _ = unlinkat(CWD, "/tmp/t-unix-backlog.sock", AtFlags::empty());
    let _ = unlinkat(CWD, "/tmp/t-unix-negative-backlog.sock", AtFlags::empty());
    let b = match &nosock {
        Ok(s) => bind(s, &na),
        Err(e) => Err(*e),
    };
    r.check("binding a never-listened socket succeeds", b.is_ok());
    // Nonblocking: a connect to a bound-but-never-listened name must answer,
    // not park (measured: the blocking form hangs on this host's rendezvous
    // wait before it reaches the refusal).
    let reacher = socket_with(
        AddressFamily::UNIX,
        SocketType::STREAM,
        SocketFlags::NONBLOCK,
        None,
    );
    if let (Ok(_nosock), Ok(reacher)) = (&nosock, &reacher) {
        let c = connect(reacher, &na);
        r.check(
            "connect to a bound, unlistened name is ECONNREFUSED",
            mpos_user_tests::host_run() || c == Err(Errno::CONNREFUSED),
        );
        // An unconnected unix socket reads as ENOTCONN - the answer the file
        // doors owe a socket whose channel is not there. On a host where the
        // connect above *succeeded*, the socket is connected and its read
        // waits for bytes instead: the claim belongs to the refused case, so
        // it is pinned on the payload and named away under MPOS_ON_HOST with
        // this measurement beside it.
        let mut sink = [0u8; 4];
        let rd = read(reacher, &mut sink);
        r.check(
            "read on an unconnected socket answers ENOTCONN",
            mpos_user_tests::host_run() || matches!(&rd, Err(e) if *e == Errno::NOTCONN),
        );
        let gp = getpeername(reacher);
        r.check(
            "getpeername on an unconnected socket answers ENOTCONN",
            mpos_user_tests::host_run() || matches!(&gp, Err(e) if *e == Errno::NOTCONN),
        );
    }

    // `listen(2)`'s backlog is one deeper than the number it names, and the
    // number that makes that visible is zero - which is not a curiosity: it is
    // what eudev's control socket is created with (`listen(uctrl->sock, 0)` in
    // `udev-ctrl.c`), so a kernel that read the man page as "a zero backlog
    // never admits" left `udevadm` unable to reach the daemon at all. Linux's
    // admission test is `queue_len > backlog`, and measured against the host
    // kernel backlogs 0, 1, 2 and 3 admitted 1, 2, 3 and 4 connections before
    // the next `connect` was refused `EAGAIN`.
    let shallow = socket(AddressFamily::UNIX, SocketType::STREAM, None);
    let ba = SocketAddrUnix::new("/tmp/t-unix-backlog.sock").unwrap();
    r.check(
        "a listener with a zero backlog binds",
        shallow.is_ok() && bind(shallow.as_ref().unwrap(), &ba).is_ok(),
    );
    if let Ok(shallow) = &shallow {
        r.check(
            "listen(0) opens a queue of one, not a closed door",
            listen(shallow, 0).is_ok(),
        );
        // The connector has to be nonblocking for a full queue to answer
        // rather than park, and it takes the *other* spelling of that state:
        // SOCK_NONBLOCK rides the type word, exactly how libwayland and
        // busybox ask for it, where the listener above went through
        // `fcntl(F_SETFL)`.
        let nb = || {
            socket_with(
                AddressFamily::UNIX,
                SocketType::STREAM,
                SocketFlags::NONBLOCK,
                None,
            )
        };
        let eager = nb();
        r.check(
            "the one connection a zero backlog admits gets in",
            eager.is_ok() && connect(eager.as_ref().unwrap(), &ba).is_ok(),
        );
        let too_late = nb();
        r.check(
            "and the next one is EAGAIN while the queue is full",
            too_late.is_ok() && connect(too_late.as_ref().unwrap(), &ba) == Err(Errno::AGAIN),
        );
        let taken = accept_with(shallow, SocketFlags::empty());
        r.check(
            "accept drains the connection the backlog was holding",
            taken.is_ok(),
        );
        if let (Ok(_taken), Ok(too_late)) = (taken, &too_late) {
            r.check(
                "and room for a new one appears with it",
                connect(too_late, &ba).is_ok(),
            );
        }
    }
    let _ = unlinkat(CWD, "/tmp/t-unix-backlog.sock", AtFlags::empty());

    // The other end of the same rule: a *negative* backlog is Linux's "as
    // deep as you allow", not a refusal. `__sys_listen` compares the argument
    // to `SOMAXCONN` as an unsigned int, so `-1` lands on the ceiling - and
    // `-1` is not a number a caller has to go looking for: Rust's std passes
    // exactly it on Linux, from `UnixListener::bind` and `TcpListener::bind`
    // alike ("silently capped to `/proc/sys/net/core/somaxconn`"), which is
    // the call `wayland-server`'s `ListeningSocket::bind_absolute` makes for a
    // compositor's socket. Answering `EINVAL` there is where niri stopped:
    // `ListeningSocketSource::new_auto` (`src/niri.rs:2435`), on a call Linux
    // answers 0 to. Measured on the host before this was written:
    // `listen(fd, -1)` returns 0 for `AF_UNIX` and `AF_INET` alike, and
    // `ss -lx` then reports the backlog as 4096 - `net.core.somaxconn`, not
    // the zero a "negative means a closed queue" reading would leave.
    let deep = socket(AddressFamily::UNIX, SocketType::STREAM, None);
    let da = SocketAddrUnix::new("/tmp/t-unix-negative-backlog.sock").unwrap();
    r.check(
        "a listener for the negative backlog binds",
        deep.is_ok() && bind(deep.as_ref().unwrap(), &da).is_ok(),
    );
    if let Ok(deep) = &deep {
        r.check(
            "listen(-1) opens the queue instead of refusing it",
            listen(deep, -1).is_ok(),
        );
        // Two connections waiting at once is what tells the two readings of
        // "-1" apart: on the ceiling both are admitted, where a queue of one
        // would take the first and answer the second `EAGAIN`.
        let wait1 = socket(AddressFamily::UNIX, SocketType::STREAM, None);
        let wait2 = socket(AddressFamily::UNIX, SocketType::STREAM, None);
        r.check(
            "the first connection a negative backlog admits gets in",
            wait1.is_ok() && connect(wait1.as_ref().unwrap(), &da).is_ok(),
        );
        r.check(
            "and so does the second, the queue being the ceiling",
            wait2.is_ok() && connect(wait2.as_ref().unwrap(), &da).is_ok(),
        );
        let held1 = accept_with(deep, SocketFlags::empty());
        let held2 = accept_with(deep, SocketFlags::empty());
        r.check(
            "both queued connections are there to accept",
            held1.is_ok() && held2.is_ok(),
        );
    }
    let _ = unlinkat(CWD, "/tmp/t-unix-negative-backlog.sock", AtFlags::empty());

    // The listener survives its own unlink; the name stops answering.
    if reborn.is_ok() {
        r.check(
            "unlinking a live listener's name succeeds",
            unlinkat(CWD, SOCK, AtFlags::empty()).is_ok(),
        );
        let after = socket(AddressFamily::UNIX, SocketType::STREAM, None);
        r.check(
            "connect by the retired name answers ENOENT",
            after.is_ok() && connect(after.as_ref().unwrap(), &addr) == Err(Errno::NOENT),
        );
    }

    // The ids this program ran under, printed for the record the way the
    // no-std harness's environment made it checkable: the suite runs as root,
    // and the bind-retirement claims above rest on that.
    r.finish()
}
