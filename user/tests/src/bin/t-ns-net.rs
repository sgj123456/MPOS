//! t-ns-net - a network stack that belongs to a *task*, not to the machine.
//!
//! `CLONE_NEWNET` is §6.3's fourth rung, and this program holds the pair the
//! plan fixed for it - **the child's view differs and the machine's does not**,
//! which is the assertion a "namespace" that is really the shared stack fails:
//!
//! * a child given `CLONE_NEWNET` has a stack of its own: it brings its own
//!   `lo` up through rtnetlink, gives it `127.0.0.1/8`, and a listener bound
//!   there is reachable from inside - which pins two facts at once, because the
//!   netlink write has to land in *the writer's* namespace for the socket to
//!   work afterwards;
//! * and it has **no wire**: there is no `eth0` to bring up, and the link dump
//!   has no entry for one, because a namespace cannot take the machine's only
//!   NIC with it (`ip link set eth0 netns N` is the thing this kernel has no
//!   place for - the first named gap of the rung);
//! * while the machine's stack is untouched: the very port the child holds is
//!   **free** in the parent's namespace, and a connect to it there is refused -
//!   the two port spaces are two;
//! * the identity differs: `/proc/self/ns/net` names a different number in the
//!   child than in the parent.
//!
//! Three refusals come with it, and each is a decision rather than a gap:
//! `unshare(CLONE_NEWNET)`/`setns(fd, CLONE_NEWNET)` are `EOPNOTSUPP` - moving a
//! *live* task into another stack needs each descriptor's socket to remember the
//! namespace it was made in, and that registry step is named in `docs/notes/`
//! §6.3; a word mixing the served kind with an unserved one is refused whole;
//! and a caller that already holds a **socket** is refused the clone rather than
//! handed a namespace in which that descriptor would name one of the new
//! namespace's sockets instead of its own (Linux has no such precondition,
//! because a `struct sock` remembers its `sk_net` - the same registry step).
//!
//! # The doors
//!
//! `rustix::thread::unshare_unsafe`, `rustix::net::{socket, bind, listen,
//! connect, accept, getsockname}` and `libc::syscall(SYS_clone)` carry the calls
//! (`rustix` binds no `fork` on either machine - `t-fork`'s reason - and a
//! process-shaped clone is what `t-ns-mnt`/`t-ns-pid` use too). The **route
//! netlink** requests the child needs are spelled here by hand, the way
//! `t-netlink` spells them and on the same layouts from `rtnetlink(7)`: this
//! file needs exactly two of that family (`RTM_SETLINK` up, `RTM_NEWADDR`) plus
//! the acknowledgement reader, and a `t-*` program is a self-contained program
//! here. The day a third test needs the codec it belongs in the crate's library
//! beside `Report`; duplicating the whole of `t-netlink` would not.
//!
//! A host run makes no claim: creating a network namespace needs
//! `CAP_SYS_ADMIN`, which this container's root does not hold (measured:
//! `.workbuddy/probe_netns.c` gets `EPERM` for `unshare(CLONE_NEWNET)`, and
//! `unshare --net` fails the same way), so the semantics here rest on Linux's
//! rules and this kernel's design record rather than on an observation - the
//! *pin* is what turns them into a claim about this one.

use mpos_user_tests::{Report, host_run};
use rustix::fd::{AsRawFd, OwnedFd};
use rustix::io::{Errno, read, write};
use rustix::net::{
    AddressFamily, SocketAddrV4, SocketType, accept, bind, connect, getsockname, listen, send,
    socket,
};
use rustix::pipe::pipe;
use rustix::thread::{UnshareFlags, unshare_unsafe};
use std::net::Ipv4Addr;

/// The address a fresh namespace's loopback gets, and the one the machine's
/// carries - `127.0.0.1`, from the std type because `rustix`'s own is the same
/// value behind a different name.
const LOOPBACK: Ipv4Addr = Ipv4Addr::new(127, 0, 0, 1);

const REPORT_MAX: usize = 128;

/// Where the two ends of the descriptor handover meet. A pathname, because the
/// unix registry is machine-wide (`docs/notes/` §6.3): a task in a fresh network
/// namespace can still reach it, which is exactly what makes the handover below
/// possible at all.
const PATH: &str = "/tmp/t-ns-net.sock";

/// `unshare(2)`, safe wrapper: single-threaded program, flags are the subject.
fn unshare(flags: UnshareFlags) -> Result<(), Errno> {
    unsafe { unshare_unsafe(flags) }
}

/// `clone(flags, 0, NULL, NULL, 0)`: a process-shaped clone whose child keeps
/// running in the same function (a copied address space, its own stack), which
/// is what `t-ns-mnt`'s and `t-ns-pid`'s probes do. Answers the child's pid, 0
/// in the child, or the negative errno.
fn clone_word(flags: usize) -> isize {
    let rc = unsafe { libc::syscall(libc::SYS_clone, flags as libc::c_long, 0, 0, 0, 0) };
    if rc < 0 {
        -(std::io::Error::last_os_error().raw_os_error().unwrap_or(0) as isize)
    } else {
        rc as isize
    }
}

fn raw_exit(code: i32) -> ! {
    unsafe { libc::syscall(libc::SYS_exit, code as libc::c_long) };
    loop {
        core::hint::spin_loop();
    }
}

/// Read one newline-terminated line (or to end-of-file), answering its length.
/// The child writes its reports a line at a time and the parent reads them in
/// order, so a byte-at-a-time reader is what keeps the two in step.
fn read_line(ro: &OwnedFd, buf: &mut [u8]) -> usize {
    let mut total = 0;
    while total < buf.len() {
        let mut byte = [0u8; 1];
        match read(ro, &mut byte) {
            Ok(0) | Err(_) => break,
            Ok(_) => {
                buf[total] = byte[0];
                total += 1;
                if byte[0] == b'\n' {
                    break;
                }
            }
        }
    }
    total
}

// ---------------------------------------------------------------------------
// The two route-netlink requests this test needs
// ---------------------------------------------------------------------------

const NLM_F_REQUEST: u16 = 0x01;
const NLM_F_ACK: u16 = 0x04;
const NLM_F_CREATE: u16 = 0x400;
const NLM_F_EXCL: u16 = 0x200;
const RTM_SETLINK: u16 = 19;
const RTM_NEWADDR: u16 = 20;
const NLMSG_ERROR: u16 = 2;
const IFF_UP: u32 = 0x1;
const IFA_ADDRESS: u16 = 1;
const LO_INDEX: i32 = 1;
const ETH_INDEX: i32 = 2;

/// A netlink message: the 16-byte header, then the payload and its attributes,
/// every piece 4-byte aligned (`rtnetlink(7)`'s layouts, spelled by hand).
struct Msg {
    body: Vec<u8>,
    ty: u16,
    flags: u16,
}

impl Msg {
    fn new(ty: u16, flags: u16, payload: &[u8]) -> Msg {
        Msg {
            body: payload.to_vec(),
            ty,
            flags,
        }
    }

    fn attr(&mut self, ty: u16, value: &[u8]) {
        let len = 4 + value.len();
        self.body.extend_from_slice(&(len as u16).to_ne_bytes());
        self.body.extend_from_slice(&ty.to_ne_bytes());
        self.body.extend_from_slice(value);
        while !self.body.len().is_multiple_of(4) {
            self.body.push(0);
        }
    }

    fn wire(&self) -> Vec<u8> {
        let total = 16 + self.body.len();
        let mut out = Vec::with_capacity(total);
        out.extend_from_slice(&(total as u32).to_ne_bytes());
        out.extend_from_slice(&self.ty.to_ne_bytes());
        out.extend_from_slice(&self.flags.to_ne_bytes());
        out.extend_from_slice(&1u32.to_ne_bytes()); // seq
        out.extend_from_slice(&0u32.to_ne_bytes()); // pid: the kernel
        out.extend_from_slice(&self.body);
        out
    }
}

/// `ip link set <index> up` - the `ifinfomsg` of `rtnetlink(7)`.
fn set_link_up(index: i32) -> Msg {
    let mut head = Vec::new();
    head.push(0); // ifi_family: AF_UNSPEC
    head.push(0);
    head.extend_from_slice(&0u16.to_ne_bytes()); // ifi_type
    head.extend_from_slice(&index.to_ne_bytes());
    head.extend_from_slice(&IFF_UP.to_ne_bytes()); // ifi_flags
    head.extend_from_slice(&IFF_UP.to_ne_bytes()); // ifi_change
    Msg::new(RTM_SETLINK, NLM_F_REQUEST | NLM_F_ACK, &head)
}

/// `ip addr add <ip>/<prefix> dev <index>` - the `ifaddrmsg`, then the address
/// the way this kernel's own handler reads it (`IFA_ADDRESS`; `IFA_LOCAL` is the
/// other spelling it accepts).
fn add_addr(index: i32, prefix: u8, ip: [u8; 4]) -> Msg {
    let head = [2u8, prefix, 0, 254, index as u8, 0, 0, 0];
    let mut m = Msg::new(
        RTM_NEWADDR,
        NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE | NLM_F_EXCL,
        &head,
    );
    m.attr(IFA_ADDRESS, &ip);
    m
}

/// Open a route-netlink socket.
fn route_socket() -> Option<OwnedFd> {
    let fd = socket(AddressFamily::NETLINK, SocketType::RAW, None).ok()?;
    // An unbound netlink socket cannot receive: the kernel needs the port id the
    // bind assigns to route its answers back (`t-netlink`'s measurement).
    rustix::net::bind(&fd, &rustix::net::netlink::SocketAddrNetlink::new(0, 0)).ok()?;
    Some(fd)
}

/// Send one request and read its acknowledgement: 0 on success, the negative
/// errno otherwise (the `nlmsgerr`'s own word, which is what `ip` prints).
fn ask(fd: &OwnedFd, msg: &Msg) -> isize {
    let wire = msg.wire();
    if send(fd, &wire, rustix::net::SendFlags::empty()).ok() != Some(wire.len()) {
        return -5; // EIO
    }
    let mut buf = [0u8; 512];
    match rustix::net::recv(fd, &mut buf, rustix::net::RecvFlags::empty()) {
        Ok((n, _)) => {
            if n < 20 {
                return -5;
            }
            // `nlmsghdr`: len@0, type@4, flags@6, seq@8, pid@12 - and an
            // `NLMSG_ERROR` carries its `error` word at 16, which is the errno
            // (zero *is* the success report).
            let ty = u16::from_ne_bytes([buf[4], buf[5]]);
            if ty == NLMSG_ERROR {
                i32::from_ne_bytes([buf[16], buf[17], buf[18], buf[19]]) as isize
            } else {
                0
            }
        }
        Err(_) => -5,
    }
}

/// One line of `/proc/self/ns/net`'s `net:[<n>]`, as the number.
fn ns_number() -> Option<u64> {
    let text = std::fs::read_link("/proc/self/ns/net").ok()?;
    let text = text.to_string_lossy().into_owned();
    text.strip_prefix("net:[")?.strip_suffix(']')?.parse().ok()
}

fn main() {
    let mut r = Report::new();

    if host_run() {
        println!(
            "note: a host cannot create a network namespace (CLONE_NEWNET needs CAP_SYS_ADMIN), so no claim is made here"
        );
        r.finish()
    }

    // ---- The two words this kernel refuses, and refuses whole ----
    r.check(
        "unshare(CLONE_NEWNET) is EOPNOTSUPP: a live task cannot be moved into a stack yet",
        unshare(UnshareFlags::NEWNET) == Err(Errno::OPNOTSUPP),
    );
    r.check(
        "unshare(CLONE_NEWNET|CLONE_NEWUTS) is refused whole",
        unshare(UnshareFlags::NEWNET | UnshareFlags::NEWUTS) == Err(Errno::OPNOTSUPP),
    );

    // ---- The precondition: a caller holding a socket is refused, not aliased ----
    //
    // A socket here lives in its namespace's stack, so a child handed a fresh
    // stack while an inherited descriptor names a socket would let that fd see
    // one of the *new* namespace's sockets. Linux has no such precondition (a
    // `struct sock` remembers its `sk_net`); this kernel says so by refusing.
    let held = socket(AddressFamily::INET, SocketType::STREAM, None);
    r.check("a socket for the precondition is opened", held.is_ok());
    let with_socket = clone_word(libc::SIGCHLD as usize | libc::CLONE_NEWNET as usize);
    r.check(
        "clone(CLONE_NEWNET) with a socket open is EOPNOTSUPP",
        with_socket == -(Errno::OPNOTSUPP.raw_os_error() as isize),
    );
    drop(held);

    // ---- A port the machine's stack has free, for the child to take ----
    let port = {
        match socket(AddressFamily::INET, SocketType::STREAM, None) {
            Ok(probe) => {
                let addr = SocketAddrV4::new(LOOPBACK, 0);
                match bind(&probe, &addr).and_then(|()| getsockname(&probe)) {
                    Ok(any) => SocketAddrV4::try_from(any).map(|ip| ip.port()).unwrap_or(0),
                    Err(_) => 0,
                }
            }
            Err(_) => 0,
        }
    };
    r.check("the machine's stack offers a free port", port != 0);

    let to_child = pipe();
    let to_parent = pipe();
    r.check(
        "the two report pipes are made",
        to_child.is_ok() && to_parent.is_ok(),
    );
    if let (Ok((down_r, down_w)), Ok((up_r, up_w))) = (to_child, to_parent) {
        let child = clone_word(libc::SIGCHLD as usize | libc::CLONE_NEWNET as usize);
        if child == 0 {
            for fd in [down_w.as_raw_fd(), up_r.as_raw_fd()] {
                unsafe { libc::syscall(libc::SYS_close, fd as libc::c_long) };
            }
            let mut word = [0u8; 16];
            let n = match read(&down_r, &mut word) {
                Ok(n) => n,
                Err(_) => raw_exit(9),
            };
            let port: u16 = core::str::from_utf8(&word[..n])
                .ok()
                .and_then(|s| s.trim().parse().ok())
                .unwrap_or(0);

            // Its own loopback, through *its own* namespace's netlink: `lo` is
            // already up and addressed (born that way - see the reason in the
            // claims below), the request that would add the address to it is
            // refused by the kernel, and the eth0 request pins the absent wire.
            let netlink = route_socket();
            let (lo_up, addr, eth_up) = match &netlink {
                Some(fd) => (
                    ask(fd, &set_link_up(LO_INDEX)),
                    ask(fd, &add_addr(LO_INDEX, 8, [127, 0, 0, 1])),
                    ask(fd, &set_link_up(ETH_INDEX)),
                ),
                None => (-5, -5, -5),
            };

            let listener = socket(AddressFamily::INET, SocketType::STREAM, None);
            let mut listen_ok = -1;
            let mut connect_ok = -1;
            let mut accept_ok = -1;
            if let Ok(listener) = &listener {
                let here = SocketAddrV4::new(LOOPBACK, port);
                if bind(listener, &here).is_ok() && listen(listener, 1).is_ok() {
                    listen_ok = 0;
                    if let Ok(client) = socket(AddressFamily::INET, SocketType::STREAM, None) {
                        if connect(&client, &here).is_ok() {
                            connect_ok = 0;
                            if accept(listener).is_ok() {
                                accept_ok = 0;
                            }
                        }
                    }
                }
            }
            let line = format!(
                "{} {} {} {} {} {}\n",
                lo_up, addr, eth_up, listen_ok, connect_ok, accept_ok
            );
            let _ = write(&up_w, line.as_bytes());

            // ---- A descriptor handed over from *another* namespace ----
            //
            // The socket the parent sends was made in the machine's stack, so
            // there is no entry for it *here*, and the index it carries names one
            // of this namespace's own sockets - which is worse than losing it. The
            // handover is therefore refused the way Linux refuses control data
            // that did not fit: `MSG_CTRUNC`, with the descriptors closed rather
            // than queued (the registry step in `docs/notes/` §6.3 is what lifts
            // this). A pathname unix socket is the meeting point, and it works
            // across the boundary because the unix registry is machine-wide.
            let mut trunc = -1isize;
            let mut kept = -1isize;
            // A leftover from an earlier run would make the bind fail for a
            // reason that is not this claim's, so the name is cleared first.
            let _ = std::fs::remove_file(PATH);
            if let Ok(listener) = socket(AddressFamily::UNIX, SocketType::STREAM, None)
                && let Ok(path) = rustix::net::SocketAddrUnix::new(PATH)
                && bind(&listener, &path).is_ok()
                && listen(&listener, 4).is_ok()
            {
                // Announce the listener, then wait for the parent's call.
                let _ = write(&up_w, b"ready\n");
                if let Ok(conn) = accept(&listener) {
                    let mut bytes = [0u8; 8];
                    let mut control = [0u8; 64];
                    let mut iov = libc::iovec {
                        iov_base: bytes.as_mut_ptr() as *mut libc::c_void,
                        iov_len: bytes.len(),
                    };
                    let mut msg: libc::msghdr = unsafe { core::mem::zeroed() };
                    msg.msg_iov = &mut iov;
                    msg.msg_iovlen = 1;
                    msg.msg_control = control.as_mut_ptr() as *mut libc::c_void;
                    // musl's `msghdr` keeps the control length in a `socklen_t`.
                    msg.msg_controllen = control.len() as u32;
                    let rc = unsafe { libc::recvmsg(conn.as_raw_fd(), &mut msg, 0) };
                    if rc >= 0 {
                        trunc = if msg.msg_flags & libc::MSG_CTRUNC != 0 {
                            1
                        } else {
                            0
                        };
                        // A descriptor that *did* arrive would show up as control
                        // data, and its absence as a zero length.
                        kept = if msg.msg_controllen == 0 { 0 } else { 1 };
                    }
                }
            }
            let second = format!("{trunc} {kept}\n");
            let _ = write(&up_w, second.as_bytes());
            unsafe { libc::syscall(libc::SYS_close, up_w.as_raw_fd() as libc::c_long) };
            raw_exit(0);
        }

        drop(down_r);
        drop(up_w);
        r.check("the child that gets its own stack was cloned", child > 0);
        if child > 0 {
            let mine = ns_number();
            r.check(
                "the parent reads its own net namespace number",
                mine.is_some(),
            );
            let tell = format!("{port}\n");
            let _ = write(&down_w, tell.as_bytes());
            drop(down_w);

            let mut report = [0u8; REPORT_MAX];
            let n = read_line(&up_r, &mut report);
            let text = String::from_utf8_lossy(&report[..n]).to_string();
            let fields: Vec<isize> = text
                .split_whitespace()
                .filter_map(|f| f.parse().ok())
                .collect();
            r.check("the child reports six words", fields.len() == 6);
            if fields.len() == 6 {
                r.check(
                    &format!(
                        "the child's own loopback answers `ip link set lo up` (got {})",
                        fields[0]
                    ),
                    fields[0] == 0,
                );
                // This one is a *divergence*, and it is the reason the
                // namespace's loopback is born carrying `127.0.0.1/8`: this
                // kernel answers `EOPNOTSUPP` for an address request naming
                // `lo`, because `lo`'s one address is the kernel's own, so a
                // namespace that started unaddressed could never be given one.
                r.check(
                    &format!(
                        "and adding an address to lo is refused, EOPNOTSUPP (got {})",
                        fields[1]
                    ),
                    fields[1] == -(Errno::OPNOTSUPP.raw_os_error() as isize),
                );
                r.check(
                    &format!(
                        "while there is no eth0 to bring up: a namespace has no wire (got {})",
                        fields[2]
                    ),
                    fields[2] == -(Errno::NODEV.raw_os_error() as isize),
                );
                r.check(
                    &format!(
                        "a listener on the child's own loopback is connectable from inside it (got {} {} {})",
                        fields[3], fields[4], fields[5]
                    ),
                    fields[3] == 0 && fields[4] == 0 && fields[5] == 0,
                );
            }

            // The machine's stack, untouched: the *same* port is free here, and
            // a connect to it is refused - the two port spaces are two.
            let here = SocketAddrV4::new(LOOPBACK, port);
            let refused = match socket(AddressFamily::INET, SocketType::STREAM, None) {
                Ok(probe) => connect(&probe, &here).err(),
                Err(e) => Some(e),
            };
            r.check(
                "a connect to the child's port from here is refused",
                refused == Some(Errno::CONNREFUSED) || refused == Some(Errno::NETUNREACH),
            );
            let bindable = match socket(AddressFamily::INET, SocketType::STREAM, None) {
                Ok(probe) => bind(&probe, &here).is_ok(),
                Err(_) => false,
            };
            r.check(
                "and the very port the child holds is free in the machine's namespace",
                bindable,
            );

            // ---- And the descriptor the child must refuse ----
            //
            // A socket made *here*, handed over the machine-wide unix registry to
            // a task in its own namespace: the child has no entry for it, so the
            // receive answers `MSG_CTRUNC` and closes it rather than delivering a
            // descriptor that would name one of the child's own sockets.
            let gift = socket(AddressFamily::INET, SocketType::STREAM, None);
            r.check("the parent's gift socket is opened", gift.is_ok());
            let mut handover = None;
            if let Ok(gift) = &gift
                && let Ok(path) = rustix::net::SocketAddrUnix::new(PATH)
                && let Ok(conn) = socket(AddressFamily::UNIX, SocketType::STREAM, None)
                    .and_then(|c| connect(&c, &path).map(|()| c))
            {
                let mut ready = [0u8; 16];
                if read_line(&up_r, &mut ready) > 0 {
                    let mut control = [0u8; 24];
                    unsafe {
                        let hdr = control.as_mut_ptr() as *mut libc::cmsghdr;
                        (*hdr).cmsg_len = 20; // CMSG_LEN(sizeof(int))
                        (*hdr).cmsg_level = libc::SOL_SOCKET;
                        (*hdr).cmsg_type = libc::SCM_RIGHTS;
                        core::ptr::copy_nonoverlapping(
                            gift.as_raw_fd().to_ne_bytes().as_ptr(),
                            control.as_mut_ptr().add(16),
                            4,
                        );
                    }
                    let mut byte = *b"x";
                    let mut iov = libc::iovec {
                        iov_base: &mut byte as *mut u8 as *mut libc::c_void,
                        iov_len: 1,
                    };
                    let mut msg: libc::msghdr = unsafe { core::mem::zeroed() };
                    msg.msg_iov = &mut iov;
                    msg.msg_iovlen = 1;
                    msg.msg_control = control.as_mut_ptr() as *mut libc::c_void;
                    // musl's `msghdr` keeps the control length in a `socklen_t`.
                    msg.msg_controllen = control.len() as u32;
                    let rc = unsafe { libc::sendmsg(conn.as_raw_fd(), &msg, 0) };
                    handover = Some(rc == 1);
                }
            }
            r.check(
                "the parent hands a descriptor over the machine-wide unix registry",
                handover == Some(true),
            );
            let mut second = [0u8; REPORT_MAX];
            let n = read_line(&up_r, &mut second);
            let said: Vec<isize> = String::from_utf8_lossy(&second[..n])
                .split_whitespace()
                .filter_map(|f| f.parse().ok())
                .collect();
            drop(up_r);
            r.check("the child reports the handover's outcome", said.len() == 2);
            if said.len() == 2 {
                r.check(
                    "a descriptor from another namespace arrives as MSG_CTRUNC, not as a socket",
                    said[0] == 1 && said[1] == 0,
                );
            }

            let status = rustix::process::waitpid(
                rustix::process::Pid::from_raw(child as i32),
                rustix::process::WaitOptions::empty(),
            );
            r.check(
                "the child is reaped",
                matches!(&status, Ok(Some((_, st))) if st.exited() && st.exit_status() == Some(0)),
            );
        }
    }

    // ---- The identity: a namespace a descriptor can name ----
    //
    // `/proc/<pid>/ns/net` exists and reads back a number, and it is the
    // machine's own for a task that never asked for one - which is the half of
    // the kind that makes it nameable at all.
    let link = std::fs::read_link("/proc/self/ns/net").ok();
    let link = link.map(|p| p.to_string_lossy().into_owned());
    r.check(
        "the machine's own namespace is nameable: /proc/self/ns/net reads net:[<n>]",
        link.as_deref()
            .is_some_and(|s| s.starts_with("net:[") && s.ends_with(']')),
    );

    r.finish()
}
