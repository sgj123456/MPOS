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

#![no_std]
#![no_main]

#[path = "../../../lib/lin.rs"]
mod lin;

use core::panic::PanicInfo;

static SOCK: &[u8] = b"/tmp/t-unix.sock\0";
static SOCK_TRIM: &[u8] = b"/tmp/t-unix.sock";
static SOCK2: &[u8] = b"/tmp/t-unix-2.sock\0";

#[unsafe(no_mangle)]
extern "C" fn mpos_user_main(_argc: usize, _argv: *const *const u8) -> ! {
    lin::puts(b"t-unix\n");
    let mut failed = false;

    // The family opens: AF_UNIX SOCK_STREAM, the same channel a socketpair
    // end is made of.
    let fd = lin::raw_sys(lin::nr::SOCKET, lin::AF_UNIX, 1 /* SOCK_STREAM */, 0) as i32;
    lin::check_true(
        &mut failed,
        b"socket(AF_UNIX, SOCK_STREAM) succeeds",
        fd >= 0,
    );

    // Leftovers from an earlier run cannot haunt this one.
    let _ = lin::unlinkat(lin::AT_FDCWD, SOCK, 0);
    let _ = lin::unlinkat(lin::AT_FDCWD, SOCK2, 0);

    // bind plants the name.
    let (addr, len) = lin::SockAddrUn::new(SOCK_TRIM);
    lin::check_eq(
        &mut failed,
        b"bind succeeds on a free name",
        lin::bind_un(fd, &addr, len),
        0,
    );

    // The name reports as a socket to `stat`.
    let mut st: lin::Stat = unsafe { core::mem::zeroed() };
    lin::check_eq(
        &mut failed,
        b"stat on the bound name succeeds",
        lin::stat(SOCK, &mut st),
        0,
    );
    lin::check_true(
        &mut failed,
        b"st_mode carries S_IFSOCK",
        st.st_mode as usize & 0o170_000 == 0o140_000,
    );

    // A second bind of the same socket is EINVAL; a second socket on the
    // same name is EADDRINUSE - the two refusals Linux tells apart.
    let (addr2, len2) = lin::SockAddrUn::new(&SOCK2[..SOCK2.len() - 1]);
    lin::check_eq(
        &mut failed,
        b"a socket cannot bind twice",
        lin::bind_un(fd, &addr2, len2),
        lin::EINVAL,
    );
    let fd2 = lin::raw_sys(lin::nr::SOCKET, lin::AF_UNIX, 1, 0) as i32;
    lin::check_eq(
        &mut failed,
        b"a second socket on a taken name is EADDRINUSE",
        lin::bind_un(fd2, &addr, len),
        lin::EADDRINUSE,
    );

    // getsockname echoes the bound path, and reports the length used.
    let mut back = lin::SockAddrUn {
        sun_family: 0,
        sun_path: [0; 108],
    };
    let mut blen = 110u32;
    lin::check_eq(
        &mut failed,
        b"getsockname succeeds on a bound socket",
        lin::raw_sys(
            lin::nr::GETSOCKNAME,
            fd as usize,
            &raw mut back as usize,
            &raw mut blen as usize,
        ),
        0,
    );
    lin::check_true(
        &mut failed,
        b"the echoed family is AF_UNIX",
        back.sun_family == 1,
    );
    lin::check_true(
        &mut failed,
        b"the echoed path is the bound path",
        &back.sun_path[..SOCK_TRIM.len()] == SOCK_TRIM,
    );
    lin::check_eq(
        &mut failed,
        b"the reported addrlen is family+path+NUL",
        blen as isize,
        (3 + SOCK_TRIM.len()) as isize,
    );

    // Opening the socket's name is Linux's ENXIO: the file doors do not
    // reach the queue behind the node.
    lin::check_eq(
        &mut failed,
        b"open on a socket path answers ENXIO",
        lin::openat(lin::AT_FDCWD, SOCK, 0, 0),
        lin::ENXIO,
    );

    // The parser's refusals, each before any bind could land.
    let (absent, alen) = lin::SockAddrUn::raw(1, b"\0abstract");
    lin::check_eq(
        &mut failed,
        b"the abstract namespace answers EINVAL",
        lin::bind_un(fd2, &absent, alen),
        lin::EINVAL,
    );
    let (empty, elen) = lin::SockAddrUn::raw(1, b"\0");
    lin::check_eq(
        &mut failed,
        b"an empty path answers EINVAL",
        lin::bind_un(fd2, &empty, elen),
        lin::EINVAL,
    );
    let (wrong, wlen) = lin::SockAddrUn::raw(2, b"x\0");
    lin::check_eq(
        &mut failed,
        b"a wrong family answers EINVAL",
        lin::bind_un(fd2, &wrong, wlen),
        lin::EINVAL,
    );
    let (nowhere, nlen) = lin::SockAddrUn::new(b"/no/such/dir/s.sock");
    lin::check_eq(
        &mut failed,
        b"bind under a missing directory answers ENOENT",
        lin::bind_un(fd2, &nowhere, nlen),
        lin::ENOENT,
    );

    // A bind whose parent is a plain file answers ENOTDIR.
    let parent = lin::openat(
        lin::AT_FDCWD,
        b"/tmp/t-unix-notdir\0",
        lin::O_CREAT | lin::O_WRONLY,
        0o644,
    ) as i32;
    lin::check_true(
        &mut failed,
        b"the not-a-directory parent was made",
        parent >= 0,
    );
    lin::close(parent);
    let (notdir, dlen) = lin::SockAddrUn::new(b"/tmp/t-unix-notdir/s.sock");
    lin::check_eq(
        &mut failed,
        b"bind under a plain file answers ENOTDIR",
        lin::bind_un(fd2, &notdir, dlen),
        lin::ENOTDIR,
    );
    // The path walk's own rule, pinned without the socket layer in the
    // way: a missing middle component is refused as the directory it is
    // not, before the deeper name could be missing at all.
    let (deep, dlen) = lin::SockAddrUn::new(b"/tmp/t-unix-no-dir-at-all/deeper.sock");
    let got = lin::bind_un(fd2, &deep, dlen);
    lin::check_true(
        &mut failed,
        b"bind two levels under a missing directory is refused",
        got == lin::ENOENT || got == lin::ENOTDIR,
    );
    let _ = lin::unlinkat(lin::AT_FDCWD, b"/tmp/t-unix-notdir\0", 0);

    // Unlinking retires the name; a fresh bind plants it again. (The old
    // socket keeps listening if it was - the survival is the next stage's
    // connect test.)
    lin::check_eq(
        &mut failed,
        b"unlinking the socket name succeeds",
        lin::unlinkat(lin::AT_FDCWD, SOCK, 0),
        0,
    );
    lin::check_eq(
        &mut failed,
        b"a freed name accepts a new bind",
        lin::bind_un(fd2, &addr, len),
        0,
    );

    // ------------------------------------------------------------------
    // The connected act: listen, connect, accept, and the byte channel
    // both ends can read and write.
    // ------------------------------------------------------------------
    // No cleanup unlinks here: fd2 still holds its re-bind of SOCK from
    // the unlink test above, and that binding is what the connect below
    // reaches by name.
    let _ = lin::unlinkat(lin::AT_FDCWD, SOCK2, 0);
    lin::check_eq(
        &mut failed,
        b"listen opens the queue",
        lin::raw_sys2(lin::nr::LISTEN, fd2 as usize, 4),
        0,
    );
    let client = lin::raw_sys(lin::nr::SOCKET, lin::AF_UNIX, 1, 0) as i32;
    lin::check_eq(
        &mut failed,
        b"connect reaches the listener by name",
        lin::connect_un(client, &addr, len),
        0,
    );
    lin::check_eq(
        &mut failed,
        b"a connected socket cannot connect again",
        lin::connect_un(client, &addr, len),
        lin::EISCONN,
    );
    let server = lin::accept4_un(fd2, core::ptr::null_mut(), core::ptr::null_mut(), 0) as i32;
    lin::check_true(
        &mut failed,
        b"accept hands out the queued connection",
        server >= 0,
    );

    // The byte channel, both ways.
    let _ = lin::write_all(client, b"ping");
    let mut rbuf = [0u8; 8];
    let n = lin::read(server, &mut rbuf);
    lin::check_eq(&mut failed, b"the server reads what the client wrote", n, 4);
    lin::check_true(
        &mut failed,
        b"the bytes are the bytes",
        &rbuf[..4] == b"ping",
    );
    let _ = lin::write_all(server, b"pong!");
    let mut cbuf = [0u8; 8];
    lin::check_eq(
        &mut failed,
        b"the client reads the reply",
        lin::read(client, &mut cbuf),
        5,
    );
    lin::check_true(
        &mut failed,
        b"the reply bytes arrived whole",
        &cbuf[..5] == b"pong!",
    );

    // getpeername on a connected end: the client carries no name and the
    // accepted half is anonymous too, so the answer is `AF_UNIX` with the
    // empty path - family, one NUL, and the length that spells.
    let mut paddr = lin::SockAddrUn {
        sun_family: 0,
        sun_path: [0; 108],
    };
    let mut plen: u32 = 110;
    lin::check_eq(
        &mut failed,
        b"getpeername on the connected end answers",
        lin::raw_sys(
            lin::nr::GETPEERNAME,
            client as usize,
            &mut paddr as *mut _ as usize,
            &mut plen as *mut _ as usize,
        ),
        0,
    );
    lin::check_true(
        &mut failed,
        b"the peer is AF_UNIX with the empty name",
        paddr.sun_family as usize == lin::AF_UNIX && paddr.sun_path[0] == 0 && plen == 3,
    );

    // Half-close: the writer's EOF is the reader's drain-then-zero.
    lin::check_eq(
        &mut failed,
        b"shutdown(SHUT_WR) succeeds",
        lin::raw_sys2(lin::nr::SHUTDOWN, client as usize, 1),
        0,
    );
    lin::check_eq(
        &mut failed,
        b"the server sees end-of-stream",
        lin::read(server, &mut rbuf),
        0,
    );

    // Backlog and nonblocking accept: an empty queue answers EAGAIN.
    lin::check_eq(
        &mut failed,
        b"accept on an empty nonblocking queue answers EAGAIN",
        lin::accept4_un(
            fd2,
            core::ptr::null_mut(),
            core::ptr::null_mut(),
            0x800, /* SOCK_NONBLOCK */
        ),
        lin::EAGAIN,
    );

    // Backlog's other face: a live queue is what makes the listener
    // readable, and the connection `poll` announces is the same one
    // `accept` then hands out - queue and descriptor agreeing on one
    // fact. (The EISCONN probe above may have left a half-dead
    // connection standing in the queue, so the drain makes "empty" a
    // fact first, and the poll's timeout proof runs against it.)
    loop {
        match lin::accept4_un(fd2, core::ptr::null_mut(), core::ptr::null_mut(), 0x800) {
            e if e == lin::EAGAIN => break,
            extra if extra >= 0 => {
                let _ = lin::close(extra as i32);
            }
            _ => break,
        }
    }
    let mut pfd = lin::PollFd {
        fd: fd2,
        events: 1, /* POLLIN */
        revents: 0,
    };
    lin::check_eq(
        &mut failed,
        b"poll on an empty listener times out",
        lin::poll(core::slice::from_mut(&mut pfd), 0),
        0,
    );
    let client2 = lin::raw_sys(lin::nr::SOCKET, lin::AF_UNIX, 1, 0) as i32;
    lin::check_eq(
        &mut failed,
        b"a second connection lands in the queue",
        lin::connect_un(client2, &addr, len),
        0,
    );
    let mut pfd = lin::PollFd {
        fd: fd2,
        events: 1, /* POLLIN */
        revents: 0,
    };
    lin::check_eq(
        &mut failed,
        b"poll says the listener is readable",
        lin::poll(core::slice::from_mut(&mut pfd), 100),
        1,
    );
    lin::check_true(
        &mut failed,
        b"POLLIN is the reported event",
        pfd.revents & 1 != 0,
    );
    let server2 = lin::accept4_un(fd2, core::ptr::null_mut(), core::ptr::null_mut(), 0) as i32;
    lin::check_true(&mut failed, b"and the queued socket is there", server2 >= 0);
    // The accepted descriptor is the connected half of that very handshake:
    // what the client writes, it reads - which is the queue and the
    // descriptor telling one story.
    let _ = lin::write_all(client2, b"x");
    let mut abuf = [0u8; 4];
    lin::check_eq(
        &mut failed,
        b"the accepted socket reads its own client",
        lin::read(server2, &mut abuf),
        1,
    );
    lin::close(server2);

    // `listen(2)`'s backlog is one deeper than the number it names, and the
    // number that makes that visible is zero - which is not a curiosity: it is
    // what eudev's control socket is created with (`listen(uctrl->sock, 0)` in
    // `udev-ctrl.c`), so a kernel that read the man page as "a zero backlog
    // never admits" left `udevadm` unable to reach the daemon at all. Linux's
    // admission test is `queue_len > backlog`, and measured against the host
    // kernel backlogs 0, 1, 2 and 3 admitted 1, 2, 3 and 4 connections before
    // the next `connect` was refused `EAGAIN`.
    let shallow = lin::raw_sys(lin::nr::SOCKET, lin::AF_UNIX, 1, 0) as i32;
    let (ba, bl) = lin::SockAddrUn::new(b"/tmp/t-unix-backlog.sock");
    lin::check_eq(
        &mut failed,
        b"a listener with a zero backlog binds",
        lin::bind_un(shallow, &ba, bl),
        0,
    );
    lin::check_eq(
        &mut failed,
        b"listen(0) opens a queue of one, not a closed door",
        lin::listen(shallow, 0),
        0,
    );
    // The connector has to be nonblocking for a full queue to answer rather
    // than park: `0o4000` is both `SOCK_NONBLOCK`'s bit and `O_NONBLOCK`.
    let eager = lin::raw_sys(lin::nr::SOCKET, lin::AF_UNIX, 1, 0) as i32;
    let _ = lin::fcntl(eager, lin::F_SETFL, lin::O_NONBLOCK);
    lin::check_eq(
        &mut failed,
        b"the one connection a zero backlog admits gets in",
        lin::connect_un(eager, &ba, bl),
        0,
    );
    let too_late = lin::raw_sys(lin::nr::SOCKET, lin::AF_UNIX, 1, 0) as i32;
    let _ = lin::fcntl(too_late, lin::F_SETFL, lin::O_NONBLOCK);
    lin::check_eq(
        &mut failed,
        b"and the next one is EAGAIN while the queue is full",
        lin::connect_un(too_late, &ba, bl),
        lin::EAGAIN,
    );
    let taken = lin::accept4_un(shallow, core::ptr::null_mut(), core::ptr::null_mut(), 0) as i32;
    lin::check_true(
        &mut failed,
        b"accept drains the connection the backlog was holding",
        taken >= 0,
    );
    lin::check_eq(
        &mut failed,
        b"and room for a new one appears with it",
        lin::connect_un(too_late, &ba, bl),
        0,
    );
    let _ = lin::close(taken);
    let _ = lin::close(eager);
    let _ = lin::close(too_late);
    lin::close(shallow);
    let _ = lin::unlinkat(lin::AT_FDCWD, b"/tmp/t-unix-backlog.sock", 0);

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
    let deep = lin::raw_sys(lin::nr::SOCKET, lin::AF_UNIX, 1, 0) as i32;
    let (da, dl) = lin::SockAddrUn::new(b"/tmp/t-unix-negative-backlog.sock");
    lin::check_eq(
        &mut failed,
        b"a listener for the negative backlog binds",
        lin::bind_un(deep, &da, dl),
        0,
    );
    lin::check_eq(
        &mut failed,
        b"listen(-1) opens the queue instead of refusing it",
        lin::raw_sys2(lin::nr::LISTEN, deep as usize, (-1isize) as usize),
        0,
    );
    // Two connections waiting at once is what tells the two readings of "-1"
    // apart: on the ceiling both are admitted, where a queue of one would take
    // the first and answer the second `EAGAIN` - the arithmetic the zero-backlog
    // stage above has just pinned from the other side.
    let wait1 = lin::raw_sys(lin::nr::SOCKET, lin::AF_UNIX, 1, 0) as i32;
    let wait2 = lin::raw_sys(lin::nr::SOCKET, lin::AF_UNIX, 1, 0) as i32;
    let _ = lin::fcntl(wait1, lin::F_SETFL, lin::O_NONBLOCK);
    let _ = lin::fcntl(wait2, lin::F_SETFL, lin::O_NONBLOCK);
    lin::check_eq(
        &mut failed,
        b"the first connection a negative backlog admits gets in",
        lin::connect_un(wait1, &da, dl),
        0,
    );
    lin::check_eq(
        &mut failed,
        b"and so does the second, the queue being the ceiling",
        lin::connect_un(wait2, &da, dl),
        0,
    );
    let held1 = lin::accept4_un(deep, core::ptr::null_mut(), core::ptr::null_mut(), 0) as i32;
    let held2 = lin::accept4_un(deep, core::ptr::null_mut(), core::ptr::null_mut(), 0) as i32;
    lin::check_true(
        &mut failed,
        b"both queued connections are there to accept",
        held1 >= 0 && held2 >= 0,
    );
    let _ = lin::close(held1);
    let _ = lin::close(held2);
    let _ = lin::close(wait1);
    let _ = lin::close(wait2);
    lin::close(deep);
    let _ = lin::unlinkat(lin::AT_FDCWD, b"/tmp/t-unix-negative-backlog.sock", 0);

    // A bound socket that never listened refuses streams (the kernel's
    // documented divergence from Linux's backlog-zero rendezvous).
    let nosock = lin::raw_sys(lin::nr::SOCKET, lin::AF_UNIX, 1, 0) as i32;
    let (na, nl) = lin::SockAddrUn::new(b"/tmp/t-unix-nolisten.sock");
    lin::check_eq(
        &mut failed,
        b"binding a never-listened socket succeeds",
        lin::bind_un(nosock, &na, nl),
        0,
    );
    let reacher = lin::raw_sys(lin::nr::SOCKET, lin::AF_UNIX, 1, 0) as i32;
    lin::check_eq(
        &mut failed,
        b"connect to a bound, unlistened name is ECONNREFUSED",
        lin::connect_un(reacher, &na, nl),
        lin::ECONNREFUSED,
    );

    // An unconnected unix socket reads as ENOTCONN - the answer the file
    // doors owe a socket whose channel is not there. getpeername owes the
    // same: there is no peer to name, which is a different fact from a
    // peer that is merely anonymous.
    let mut sink = [0u8; 4];
    lin::check_eq(
        &mut failed,
        b"read on an unconnected socket answers ENOTCONN",
        lin::read(reacher, &mut sink),
        lin::ENOTCONN,
    );
    let mut pname = lin::SockAddrUn {
        sun_family: 0,
        sun_path: [0; 108],
    };
    let mut plen: u32 = 110;
    lin::check_eq(
        &mut failed,
        b"getpeername on an unconnected socket answers ENOTCONN",
        lin::raw_sys(
            lin::nr::GETPEERNAME,
            reacher as usize,
            &mut pname as *mut _ as usize,
            &mut plen as *mut _ as usize,
        ),
        lin::ENOTCONN,
    );

    // The listener survives its own unlink; the name stops answering.
    lin::check_eq(
        &mut failed,
        b"unlinking a live listener's name succeeds",
        lin::unlinkat(lin::AT_FDCWD, SOCK, 0),
        0,
    );
    let after = lin::raw_sys(lin::nr::SOCKET, lin::AF_UNIX, 1, 0) as i32;
    lin::check_eq(
        &mut failed,
        b"connect by the retired name answers ENOENT",
        lin::connect_un(after, &addr, len),
        lin::ENOENT,
    );
    lin::close(after);
    lin::close(reacher);
    lin::close(nosock);
    let _ = lin::unlinkat(lin::AT_FDCWD, b"/tmp/t-unix-nolisten.sock\0", 0);
    lin::close(client);
    lin::close(client2);
    lin::close(server);
    lin::close(server2);
    lin::close(fd);
    lin::close(fd2);
    lin::done(failed)
}

#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    lin::die(b"t-unix: panicked\n")
}
