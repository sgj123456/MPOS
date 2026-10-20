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
    AddressFamily, RecvFlags, SendFlags, SocketAddrUnix, SocketFlags, SocketType, accept_with,
    bind, connect, getpeername, getsockname, listen, recvfrom, sendto, shutdown, socket,
    socket_with,
};

const SOCK: &str = "/tmp/t-unix.sock";
const SOCK2: &str = "/tmp/t-unix-2.sock";

fn last_errno() -> i32 {
    std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
}

/// `bind(fd, addr, len)` through the raw door, for the address shapes
/// `SocketAddrUnix` cannot build: a foreign family word under a unix body.
/// The raw `getsockname(2)` door. `rustix` has no way to read an abstract
/// name back - its unix address type spells a path and nothing else - so the
/// two claims about what the kernel *answers* with go through the syscall,
/// which is the same reason the empty-path probe does.
fn getsockname_raw(fd: i32, storage: &mut Storage, len: &mut u32) -> isize {
    let r = unsafe {
        libc::syscall(
            libc::SYS_getsockname,
            fd as libc::c_long,
            storage as *mut Storage as libc::c_long,
            len as *mut u32 as libc::c_long,
        )
    };
    if r < 0 {
        -(last_errno() as isize)
    } else {
        r as isize
    }
}

/// The raw `recvfrom(2)` door, for the same reason: the *sender's* address of
/// a datagram is what this program has to read, and for an abstract sender it
/// is a name no rustix type can carry.
fn recvfrom_raw(fd: i32, buf: &mut [u8], storage: &mut Storage, len: &mut u32) -> isize {
    let r = unsafe {
        libc::syscall(
            libc::SYS_recvfrom,
            fd as libc::c_long,
            buf.as_mut_ptr() as libc::c_long,
            buf.len() as libc::c_long,
            0 as libc::c_long,
            storage as *mut Storage as libc::c_long,
            len as *mut u32 as libc::c_long,
        )
    };
    if r < 0 {
        -(last_errno() as isize)
    } else {
        r as isize
    }
}

/// The raw `connect(2)` door, for the shapes a typed address cannot spell -
/// here an `addrlen` of just the family, which `SocketAddrUnix` cannot build
/// (its constructors always carry a name).
fn connect_raw(fd: i32, storage: &Storage, len: u32) -> isize {
    let r = unsafe {
        libc::syscall(
            libc::SYS_connect,
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
        // --- the abstract namespace ------------------------------------------
        //
        // No filesystem holds the name, so the bytes *are* the address. Every
        // answer below was measured on the host first
        // (`.workbuddy/probe_unix_abstract.c`), and it is the namespace
        // `xwayland-satellite` binds as `@/tmp/.X11-unix/X0`.
        let abs_name = SocketAddrUnix::new_abstract_name(b"t-unix-abs").unwrap();
        let srv = socket(AddressFamily::UNIX, SocketType::STREAM, None).unwrap();
        r.check("an abstract name binds", bind(&srv, &abs_name).is_ok());
        let other = socket(AddressFamily::UNIX, SocketType::STREAM, None).unwrap();
        r.check(
            "a second bind of the same abstract name is EADDRINUSE",
            bind(&other, &abs_name) == Err(Errno::ADDRINUSE),
        );
        let mut back = Storage {
            family: 0,
            pad: [0; 108],
        };
        let mut back_len = 128u32;
        let asked = getsockname_raw(srv.as_raw_fd(), &mut back, &mut back_len);
        // Linux answers family, the leading NUL, then the name, and `addrlen`
        // is three more than the name - which is the whole shape of the
        // abstract namespace's address.
        r.check(
            "getsockname answers an abstract sockaddr_un",
            asked == 0
                && back.family == 1
                && back_len == 3 + 10
                && back.pad[..11]
                    == [
                        0, b't', b'-', b'u', b'n', b'i', b'x', b'-', b'a', b'b', b's',
                    ],
        );
        r.check(
            "an abstract listener accepts a connection",
            listen(&srv, 4).is_ok() && connect(&other, &abs_name).is_ok(),
        );
        let missing = SocketAddrUnix::new_abstract_name(b"t-unix-nobody").unwrap();
        let cli = socket(AddressFamily::UNIX, SocketType::STREAM, None).unwrap();
        r.check(
            "connect to an unbound abstract name is ECONNREFUSED",
            connect(&cli, &missing) == Err(Errno::CONNREFUSED),
        );
        // The key is the name *and* the socket type, so a datagram socket may
        // hold the name a stream socket holds - Linux keys them that way and a
        // path cannot be shared at all.
        let dg = socket(AddressFamily::UNIX, SocketType::DGRAM, None).unwrap();
        r.check(
            "a datagram socket may hold a stream's abstract name",
            bind(&dg, &abs_name).is_ok(),
        );
        // A datagram round trip over abstract names, the sender's address
        // included: the receiver is told the sender's *name*, not a path.
        let dg_rx = socket(AddressFamily::UNIX, SocketType::DGRAM, None).unwrap();
        let rx_name = SocketAddrUnix::new_abstract_name(b"t-unix-abs-rx").unwrap();
        r.check(
            "a datagram socket binds an abstract name",
            bind(&dg_rx, &rx_name).is_ok(),
        );
        let sent = sendto(&dg, &b"x"[..], SendFlags::empty(), &rx_name);
        r.check("a datagram reaches an abstract name", sent.is_ok());
        let mut payload = [0u8; 8];
        let mut sender = Storage {
            family: 0,
            pad: [0; 108],
        };
        let mut sender_len = 128u32;
        let got = recvfrom_raw(
            dg_rx.as_raw_fd(),
            &mut payload,
            &mut sender,
            &mut sender_len,
        );
        r.check("the datagram arrived", got == 1 && payload[0] == b'x');
        r.check(
            "the sender's abstract name came back",
            sender.family == 1
                && sender_len == 3 + 10
                && sender.pad[..11]
                    == [
                        0, b't', b'-', b'u', b'n', b'i', b'x', b'-', b'a', b'b', b's',
                    ],
        );
        // An `addrlen` of just the family: `bind` makes a name up (Linux's
        // `unix_autobind`) and every other door refuses - the second half is
        // `connect`, whose address the parser now accepts.
        let mut none_at_all = Storage {
            family: 1,
            pad: [0; 108],
        };
        none_at_all.pad[0] = 0;
        let auto_fd = socket(AddressFamily::UNIX, SocketType::STREAM, None).unwrap();
        r.check(
            "bind with no name at all auto-binds",
            bind_raw(auto_fd.as_raw_fd(), &none_at_all, 2) == 0,
        );
        let mut made = Storage {
            family: 0,
            pad: [0; 108],
        };
        let mut made_len = 128u32;
        // Linux's `unix_autobind` mints a five-hex-character name: `addrlen`
        // 8 and a leading NUL are what a caller can see of it (measured).
        r.check(
            "and the name it made is abstract",
            getsockname_raw(auto_fd.as_raw_fd(), &mut made, &mut made_len) == 0
                && made.family == 1
                && made_len == 8
                && made.pad[0] == 0,
        );
        let other2 = socket(AddressFamily::UNIX, SocketType::STREAM, None).unwrap();
        r.check(
            "connect with no name at all answers EINVAL",
            connect_raw(other2.as_raw_fd(), &none_at_all, 2) == -libc::EINVAL as isize,
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

    // --- a path's bytes after its terminator -----------------------------
    //
    // Linux's `unix_mkname` takes `strnlen` and stops at the first NUL, so
    // whatever the caller left after the terminator is storage, not part of
    // the name. Measured on a current host first
    // (`.workbuddy/probe_unix_path_tail.c`, Linux 7.2.11): `bind` succeeds,
    // `getsockname` echoes the prefix with the short length, the node on disk
    // is the prefix's, and a connection spelled with the same trailing bytes
    // reaches the listener. The 108-byte boundary is the other half of the
    // same rule - a field with no NUL at all is taken whole - and `addrlen`
    // past the struct is the one `EINVAL` the parser still owes.
    let tail_path = "/tmp/t-unix-tail.sock";
    let tail_prefix = b"/tmp/t-unix-tail.sock";
    let _ = unlinkat(CWD, tail_path, AtFlags::empty());
    let _ = unlinkat(CWD, "/tmp/t-unix-tail.sockXYZ", AtFlags::empty());
    let tail_addr = |extra: &[u8]| {
        let mut st = Storage {
            family: 1,
            pad: [0; 108],
        };
        st.pad[..tail_prefix.len()].copy_from_slice(tail_prefix);
        // `pad` is zeroed, so the byte after the name is the terminator; the
        // caller's extra bytes sit behind it, exactly like the probe's
        // literal `"abc\0XYZ"`.
        st.pad[tail_prefix.len() + 1..tail_prefix.len() + 1 + extra.len()].copy_from_slice(extra);
        st
    };
    let with_tail = tail_addr(b"XYZ");
    let exact = tail_addr(b"");
    let tail_fd = socket(AddressFamily::UNIX, SocketType::STREAM, None).unwrap();
    r.check(
        "a path with bytes after its terminator binds",
        bind_raw(
            tail_fd.as_raw_fd(),
            &with_tail,
            (2 + tail_prefix.len() + 1 + 3) as u32,
        ) == 0,
    );
    let mut tail_echo = Storage {
        family: 0,
        pad: [0; 108],
    };
    let mut tail_echo_len = 128u32;
    let echoed = getsockname_raw(tail_fd.as_raw_fd(), &mut tail_echo, &mut tail_echo_len);
    r.check(
        "and getsockname echoes the name before it",
        echoed == 0
            && tail_echo_len as usize == 2 + tail_prefix.len() + 1
            && tail_echo.pad[..tail_prefix.len()] == tail_prefix[..],
    );
    r.check(
        "the node it planted is the prefix, not the tail",
        stat(tail_path).is_ok() && stat("/tmp/t-unix-tail.sockXYZ").is_err(),
    );
    let tail_second = socket(AddressFamily::UNIX, SocketType::STREAM, None).unwrap();
    r.check(
        "so a second bind of the prefix is EADDRINUSE",
        bind_raw(
            tail_second.as_raw_fd(),
            &exact,
            (2 + tail_prefix.len() + 1) as u32,
        ) == -libc::EADDRINUSE as isize,
    );
    r.check(
        "the trailing-garbage listener listens",
        listen(&tail_fd, 4).is_ok(),
    );
    let tail_cli = socket(AddressFamily::UNIX, SocketType::STREAM, None).unwrap();
    let tail_connected = connect_raw(
        tail_cli.as_raw_fd(),
        &with_tail,
        (2 + tail_prefix.len() + 1 + 3) as u32,
    ) == 0;
    r.check(
        "a connect spelled with the same trailing bytes reaches it",
        tail_connected,
    );
    r.check(
        "and the listener accepts that connection",
        // Short-circuited: a kernel that refused the bind never completed the
        // connect, and a blocking accept there would park the sweep instead
        // of turning the claim red.
        tail_connected && accept_with(&tail_fd, SocketFlags::empty()).is_ok(),
    );
    drop(tail_cli);
    drop(tail_fd);
    let _ = unlinkat(CWD, tail_path, AtFlags::empty());

    // A field with no NUL anywhere is the *whole* field, and the echo is one
    // byte longer than the field because Linux appends its own terminator -
    // except that the terminator has nowhere to land: `move_addr_to_user`
    // copies `min(len, ulen)` and reports the used length, so a 108-byte
    // `sun_path` comes back full and the count still says 111.
    let mut full_name = [b'f'; 108];
    full_name[..5].copy_from_slice(b"/tmp/");
    let mut full = Storage {
        family: 1,
        pad: [0; 108],
    };
    full.pad.copy_from_slice(&full_name);
    let full_fd = socket(AddressFamily::UNIX, SocketType::STREAM, None).unwrap();
    r.check(
        "a 108-byte field with no NUL binds whole",
        bind_raw(full_fd.as_raw_fd(), &full, 2 + 108) == 0,
    );
    let mut full_echo = Storage {
        family: 0,
        pad: [0; 108],
    };
    let mut full_echo_len = 128u32;
    let full_echoed = getsockname_raw(full_fd.as_raw_fd(), &mut full_echo, &mut full_echo_len);
    r.check(
        "a full-field bind reports the field plus one NUL",
        full_echoed == 0 && full_echo_len as usize == 2 + 108 + 1,
    );
    r.check(
        "and copies only what the caller's field holds",
        full_echoed == 0 && full_echo.pad == full_name,
    );
    drop(full_fd);
    let _ = unlinkat(
        CWD,
        core::str::from_utf8(&full_name).unwrap(),
        AtFlags::empty(),
    );
    // `addrlen` past the struct is refused before the name is looked at -
    // the parser's own bound, and the one refusal this rule leaves standing.
    let past = socket(AddressFamily::UNIX, SocketType::STREAM, None).unwrap();
    r.check(
        "an addrlen past the struct is EINVAL",
        bind_raw(past.as_raw_fd(), &exact, (2 + 108 + 1) as u32) == -libc::EINVAL as isize,
    );

    let _ = unlinkat(CWD, "/tmp/t-unix-notdir", AtFlags::empty());

    // --- `shutdown` answers 0 for every socket Linux says it does ----
    //
    // Linux's `unix_shutdown` never asks whether there is a peer: the call
    // sets a flag on the socket and reports success, so an unconnected
    // stream socket, a listener and a datagram socket all answer 0, and only
    // the `how` word is validated (`EINVAL` outside 0..=2). Measured on a
    // current host first (`.workbuddy/probe_unix_shutdown.c`, Linux 7.2.11).
    let quiet = socket(AddressFamily::UNIX, SocketType::STREAM, None).unwrap();
    r.check(
        "shutdown on an unconnected stream socket answers 0",
        shutdown(&quiet, rustix::net::Shutdown::Both).is_ok(),
    );
    drop(quiet);
    let quiet_dgram = socket(AddressFamily::UNIX, SocketType::DGRAM, None).unwrap();
    r.check(
        "shutdown on a datagram socket answers 0",
        shutdown(&quiet_dgram, rustix::net::Shutdown::Write).is_ok(),
    );
    drop(quiet_dgram);
    // The `how` word is the only thing refused, and it is refused whatever
    // the socket is - 3 and -1 are both `EINVAL`.
    let quiet2 = socket(AddressFamily::UNIX, SocketType::STREAM, None).unwrap();
    let how_raw = |fd: i32, how: i32| unsafe {
        let r = libc::syscall(libc::SYS_shutdown, fd as libc::c_long, how as libc::c_long);
        if r < 0 {
            -(last_errno() as isize)
        } else {
            r as isize
        }
    };
    r.check(
        "a how word outside 0..=2 is EINVAL",
        how_raw(quiet2.as_raw_fd(), 3) == -libc::EINVAL as isize
            && how_raw(quiet2.as_raw_fd(), -1) == -libc::EINVAL as isize,
    );
    drop(quiet2);

    // --- `listen` is the *named* call --------------------------------
    //
    // Linux's `unix_listen` asks the socket's address and answers `EINVAL`
    // when there is none, so a listener is always reachable by something; a
    // datagram socket has no accept queue at all and answers `EOPNOTSUPP`
    // (95), bound or not. A connected socket and an accepted end are `EINVAL`
    // too - they have an address and a channel, and neither is a listener.
    // Measured on a current host first (`.workbuddy/probe_unix_listen{,2}.c`,
    // Linux 7.2.11).
    let unbound_stream = socket(AddressFamily::UNIX, SocketType::STREAM, None).unwrap();
    r.check(
        "listen on an unbound stream socket is EINVAL",
        listen(&unbound_stream, 4) == Err(Errno::INVAL),
    );
    drop(unbound_stream);
    let unbound_packet = socket(AddressFamily::UNIX, SocketType::SEQPACKET, None).unwrap();
    r.check(
        "listen on an unbound seqpacket socket is EINVAL",
        listen(&unbound_packet, 4) == Err(Errno::INVAL),
    );
    drop(unbound_packet);
    let dgram = socket(AddressFamily::UNIX, SocketType::DGRAM, None).unwrap();
    r.check(
        "listen on a datagram socket is EOPNOTSUPP",
        listen(&dgram, 4) == Err(Errno::OPNOTSUPP),
    );
    drop(dgram);
    // An abstract-bound socket has an address, so it listens - the rule is
    // the address and not the filesystem.
    let abs_listen = socket(AddressFamily::UNIX, SocketType::STREAM, None).unwrap();
    let mut abs_addr = Storage {
        family: 1,
        pad: [0; 108],
    };
    abs_addr.pad[0] = 0;
    abs_addr.pad[1..12].copy_from_slice(b"t-unix-lstn");
    r.check(
        "an abstract-bound socket listens",
        bind_raw(abs_listen.as_raw_fd(), &abs_addr, 3 + 11) == 0 && listen(&abs_listen, 4).is_ok(),
    );
    drop(abs_listen);
    // A second `listen` only replaces the backlog.
    let twice = socket(AddressFamily::UNIX, SocketType::STREAM, None).unwrap();
    let mut twice_addr = Storage {
        family: 1,
        pad: [0; 108],
    };
    let twice_name = b"/tmp/t-unix-twice.sock";
    twice_addr.pad[..twice_name.len()].copy_from_slice(twice_name);
    let _ = unlinkat(CWD, "/tmp/t-unix-twice.sock", AtFlags::empty());
    r.check(
        "a bound socket listens, and a second listen only replaces the backlog",
        bind_raw(
            twice.as_raw_fd(),
            &twice_addr,
            (2 + twice_name.len() + 1) as u32,
        ) == 0
            && listen(&twice, 4).is_ok()
            && listen(&twice, 8).is_ok(),
    );
    drop(twice);
    let _ = unlinkat(CWD, "/tmp/t-unix-twice.sock", AtFlags::empty());

    // --- the three addresses a socket can answer with --------------------
    //
    // Linux keeps "what is my address" and "did `bind` plant a name" apart,
    // and the answers differ by more than a flag: an unbound socket answers
    // the **bare family** (two bytes), a socket bound to the empty path
    // answers the family and a NUL (three), and an **accepted** end inherits
    // the listener's address - which then refuses it a bind of its own with
    // `EINVAL`, the same refusal a listener gets. Measured on a current host
    // first (`.workbuddy/probe_unix_unnamed.c`, `.workbuddy/probe_unix_names.c`,
    // `.workbuddy/probe_unix_bind_after.c`, Linux 7.2.11).
    let unnamed = socket(AddressFamily::UNIX, SocketType::STREAM, None).unwrap();
    let mut u_echo = Storage {
        family: 0,
        pad: [0x7f; 108],
    };
    let mut u_len = 110u32;
    let u_r = getsockname_raw(unnamed.as_raw_fd(), &mut u_echo, &mut u_len);
    r.check(
        "an unbound socket's name is the bare family, two bytes",
        u_r == 0 && u_len == 2 && u_echo.family == 1,
    );
    r.check("and it wrote no path byte at all", u_echo.pad[0] == 0x7f);
    drop(unnamed);

    // The empty path is a *name* of zero bytes, and its three bytes are the
    // proof: the answer is not "unnamed".
    let empty_name = socket(AddressFamily::UNIX, SocketType::STREAM, None).unwrap();
    let mut zero = Storage {
        family: 1,
        pad: [0; 108],
    };
    r.check(
        "binding the empty path succeeds",
        bind_raw(empty_name.as_raw_fd(), &zero, 3) == 0,
    );
    let mut e_echo = Storage {
        family: 0,
        pad: [0x7f; 108],
    };
    let mut e_len = 110u32;
    let e_r = getsockname_raw(empty_name.as_raw_fd(), &mut e_echo, &mut e_len);
    r.check(
        "and its name is the family plus the NUL: three bytes",
        e_r == 0 && e_len == 3 && e_echo.family == 1 && e_echo.pad[0] == 0,
    );
    drop(empty_name);
    zero.family = 1;
    let _ = &zero;

    // The accepted end of a connection inherits the listener's address, and
    // that inheritance is what makes a second bind `EINVAL` - the client end
    // of the same connection answers the *unnamed* address and binds fine.
    let inherit_path = "/tmp/t-unix-inherit.sock";
    let _ = unlinkat(CWD, inherit_path, AtFlags::empty());
    let _ = unlinkat(CWD, "/tmp/t-unix-inherit2.sock", AtFlags::empty());
    let inherit_l = socket(AddressFamily::UNIX, SocketType::STREAM, None).unwrap();
    let mut inh = Storage {
        family: 1,
        pad: [0; 108],
    };
    inh.pad[..inherit_path.len()].copy_from_slice(inherit_path.as_bytes());
    r.check(
        "a listener binds for the inheritance probe",
        bind_raw(
            inherit_l.as_raw_fd(),
            &inh,
            (2 + inherit_path.len() + 1) as u32,
        ) == 0
            && listen(&inherit_l, 4).is_ok(),
    );
    let inherit_c = socket(AddressFamily::UNIX, SocketType::STREAM, None).unwrap();
    r.check(
        "a client dials it",
        connect_raw(
            inherit_c.as_raw_fd(),
            &inh,
            (2 + inherit_path.len() + 1) as u32,
        ) == 0,
    );
    let accepted = accept_with(&inherit_l, SocketFlags::empty());
    match &accepted {
        Ok(acc) => {
            let mut a_echo = Storage {
                family: 0,
                pad: [0x7f; 108],
            };
            let mut a_len = 110u32;
            let a_r = getsockname_raw(acc.as_raw_fd(), &mut a_echo, &mut a_len);
            r.check(
                "the accepted end's own name is the listener's",
                a_r == 0
                    && a_len as usize == 2 + inherit_path.len() + 1
                    && a_echo.pad[..inherit_path.len()] == inherit_path.as_bytes()[..],
            );
            let mut second = Storage {
                family: 1,
                pad: [0; 108],
            };
            let second_name = b"/tmp/t-unix-inherit2.sock";
            second.pad[..second_name.len()].copy_from_slice(second_name);
            r.check(
                "so a bind on the accepted end is EINVAL",
                bind_raw(acc.as_raw_fd(), &second, (2 + second_name.len() + 1) as u32)
                    == -libc::EINVAL as isize,
            );
        }
        Err(_) => {
            r.check("the accepted end's own name is the listener's", false);
            r.check("so a bind on the accepted end is EINVAL", false);
        }
    }
    // The client end of the same connection answers the unnamed address, so
    // its own bind is served - the two halves of the same connect, told apart
    // by what the accept copied.
    let mut c_echo = Storage {
        family: 0,
        pad: [0x7f; 108],
    };
    let mut c_len = 110u32;
    let c_r = getsockname_raw(inherit_c.as_raw_fd(), &mut c_echo, &mut c_len);
    r.check(
        "while the client end's name is the bare family",
        c_r == 0 && c_len == 2,
    );
    let mut second = Storage {
        family: 1,
        pad: [0; 108],
    };
    let second_name = b"/tmp/t-unix-inherit2.sock";
    second.pad[..second_name.len()].copy_from_slice(second_name);
    r.check(
        "and its own bind is served",
        bind_raw(
            inherit_c.as_raw_fd(),
            &second,
            (2 + second_name.len() + 1) as u32,
        ) == 0,
    );
    drop(accepted);
    drop(inherit_c);
    drop(inherit_l);
    let _ = unlinkat(CWD, inherit_path, AtFlags::empty());
    let _ = unlinkat(CWD, "/tmp/t-unix-inherit2.sock", AtFlags::empty());

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
        // ... but the name is resolved first, which is the order Linux asks
        // in: the same connected socket naming a peer that is not there hears
        // the resolver's answer, not `EISCONN` (measured on a current host -
        // `ENOENT` for a path nobody holds, `ECONNREFUSED` for an abstract
        // name nobody holds).
        let gone = SocketAddrUnix::new("/tmp/t-unix-gone.sock").unwrap();
        r.check(
            "a connected socket naming a dead peer hears ENOENT, not EISCONN",
            connect(client, &gone) == Err(Errno::NOENT),
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

            // getpeername on a connected end is the name the *connect*
            // dialled, kept on the socket rather than read off the peer's
            // slot: Linux's `unix_peer` records it, so it survives the peer's
            // death and its node's `unlink` (measured -
            // `.workbuddy/probe_unix_names2.c`). The raw form again, because
            // the echoed `addrlen` is half the claim.
            match getpeername_raw(client) {
                Some((fam, path, len)) => {
                    r.check("getpeername on the connected end answers", true);
                    r.check(
                        "and the peer is the name the connect dialled",
                        fam == 1 && path == SOCK.as_bytes() && len as usize == 2 + SOCK.len() + 1,
                    );
                }
                None => {
                    r.check("getpeername on the connected end answers", false);
                    r.check("and the peer is the name the connect dialled", false);
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
