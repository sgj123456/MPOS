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
//!   has no entry for one, because a namespace is born with `lo` alone. It used
//!   to be *stuck* that way - `ip link set eth0 netns N` was the first named
//!   gap of the rung - but the move is served now, and the last claim below
//!   walks a whole round trip of it: the wire leaves the machine's namespace,
//!   arrives in the child's down and unconfigured, and comes back. What the
//!   parent pins in *this* section is what the request still refuses: `lo`,
//!   which is netns-local (`EINVAL`), and a pid that names no task (`ESRCH`).
//! * while the machine's stack is untouched: the very port the child holds is
//!   **free** in the parent's namespace, and a connect to it there is refused -
//!   the two port spaces are two;
//! * and `/proc/net/tcp` is the *reader's* table: the child's listener is a row
//!   of the file read inside the namespace, while the file read in the machine's
//!   namespace has no row for that same port - the collector filters on
//!   `current_net_ns()`, and no claim read the file before this one;
//! * the identity differs: `/proc/self/ns/net` names a different number in the
//!   child than in the parent;
//! * and the unix family's **abstract** names are the namespace's own: a socket
//!   made here and carried across still dials the name this namespace holds, a
//!   socket made inside the new one cannot see it, the same name binds again
//!   there, and a name planted inside is unknown here - while a *path* name
//!   crosses both ways, which is what the descriptor handover below needs.
//!
//! One refusal comes with it, and it is a decision rather than a gap: a word
//! mixing the served kind with an unserved one (`CLONE_NEWNET|CLONE_NEWIPC`) is
//! refused whole. Two more used to be here and are now claims instead, because
//! the registry step landed (`net`'s `HOMES`, the commit that made a socket
//! remember its stack): `unshare(CLONE_NEWNET)`/`setns(fd, CLONE_NEWNET)` are
//! served, and a caller that already holds a socket may clone a namespace -
//! inherited descriptors are no longer a reason to refuse, since each one goes
//! on working against the stack it was made in.
//!
//! The last section asks **whose** capability a packet socket is: `AF_PACKET`'s
//! `CAP_NET_RAW` is asked of the *network namespace's* owner, not of the
//! caller's own standing - measured on a current host
//! (`.workbuddy/probe_ns_owner.c`), where a caller holding the whole set inside
//! a bare user namespace is still `EPERM` in the machine's network namespace
//! and served in one it made itself. That is the same walk `setns` asks about
//! the namespace being joined, and `t-ns-setns` pins that half.
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
//! A host run makes no claim: `unshare(CLONE_NEWNET)` needs `CAP_SYS_ADMIN`,
//! which this container's own capabilities do not hold (measured:
//! `.workbuddy/probe_netns.c` gets `EPERM`, and `unshare --net` fails the same
//! way). That measurement is about *that* call and not about the host, though,
//! and the difference cost this rung a false "cannot be measured here" for two
//! steps: `unshare -Urn` (a user namespace first) creates the network namespace
//! fine, which is how the three `probe_unix_netns*.c` transcripts in §6.3 were
//! taken. So the *unix* half of the claims above is an observation, not a
//! reading - and what the pin adds is that this kernel matches it.

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
use std::time::Instant;

/// The address a fresh namespace's loopback gets, and the one the machine's
/// carries - `127.0.0.1`, from the std type because `rustix`'s own is the same
/// value behind a different name.
const LOOPBACK: Ipv4Addr = Ipv4Addr::new(127, 0, 0, 1);

const REPORT_MAX: usize = 128;

/// The sixth rung, which `rustix`'s `UnshareFlags` does not carry: the user
/// namespace the packet-socket question at the bottom of this file is asked
/// through.
const CLONE_NEWUSER: usize = 0x1000_0000;

/// Where the two ends of the descriptor handover meet. A **pathname**, because a
/// path is the half of the unix family that is *not* namespaced - a task in a
/// fresh network namespace reaches it by name, which is exactly what makes the
/// handover below possible at all (an abstract name would not cross; §6.3 has
/// the measurement of both halves).
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
const RTM_NEWROUTE: u16 = 24;
const RTA_GATEWAY: u16 = 5;
const IFA_ADDRESS: u16 = 1;
/// `IFLA_NET_NS_PID` (`linux/if_link.h`): the network namespace a `link set`
/// wants to move the device into, named by a pid in the caller's namespace.
const IFLA_NET_NS_PID: u16 = 19;
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

/// `ip link set <index> netns <pid>` - the same `ifinfomsg`, carrying
/// `IFLA_NET_NS_PID` instead of a flag change. This is the move request the
/// machine's namespace must refuse: there is nowhere to move a device *to*.
fn set_link_netns(index: i32, pid: i32) -> Msg {
    let mut head = Vec::new();
    head.push(0); // ifi_family: AF_UNSPEC
    head.push(0);
    head.extend_from_slice(&0u16.to_ne_bytes()); // ifi_type
    head.extend_from_slice(&index.to_ne_bytes());
    head.extend_from_slice(&0u32.to_ne_bytes()); // ifi_flags
    head.extend_from_slice(&0u32.to_ne_bytes()); // ifi_change
    let mut m = Msg::new(RTM_SETLINK, NLM_F_REQUEST | NLM_F_ACK, &head);
    m.attr(IFLA_NET_NS_PID, &pid.to_ne_bytes());
    m
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

/// `ip route add default via <gw>` - the `rtmsg` of `rtnetlink(7)`: family,
/// destination length (zero, which is what "default" means), then the rest of
/// the twelve bytes, then the router. No `RTA_DST` at all: the kernel reads a
/// missing destination as the zero prefix, and it requires `RTA_GATEWAY`.
fn add_route(gateway: [u8; 4]) -> Msg {
    let head = [2u8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
    let mut m = Msg::new(
        RTM_NEWROUTE,
        NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE,
        &head,
    );
    m.attr(RTA_GATEWAY, &gateway);
    m
}

/// The machine's wire as the boot configures it. The move pin has to put both
/// back after the wire comes home, for the reason Linux makes it necessary: an
/// address belongs to the namespace that holds it, not to the device, so a
/// device that arrives in a namespace has none.
const WIRE_IP: [u8; 4] = [10, 0, 2, 15];
const WIRE_PREFIX: u8 = 24;
const WIRE_GATEWAY: [u8; 4] = [10, 0, 2, 2];

/// A pid no task answers to. `ip` refuses this itself, so the kernel's answer
/// never reaches a script - the raw request here is what measures it.
const NO_SUCH_PID: i32 = 999_999;

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

/// Whether `/proc/net/tcp` as **this task** reads it has a row whose *local*
/// address is `port` and whose state byte is `state`. The file is the reader's
/// own namespace's table: the collector filters on `current_net_ns()`, so the
/// same call in two namespaces answers about two different stacks. That
/// per-namespace resolution is the claim below - the file is parsed here by
/// position, which is the column layout `ss`/`netstat` read, not a shape this
/// test invented: `sl`, then `local_address`, then `rem_address`, then `st`,
/// and the local address is the little-endian `%08X` word, a colon, and the
/// port as `%04X`. `None` when the file cannot be read at all, so a refusal and
/// an empty table stay apart.
fn proc_net_tcp_local(port: u16, state: u8) -> Option<bool> {
    let text = std::fs::read_to_string("/proc/net/tcp").ok()?;
    let want_port = format!(":{:04X}", port);
    let want_state = format!("{:02X}", state);
    Some(text.lines().skip(1).any(|line| {
        let mut field = line.split_whitespace();
        let _sl = field.next();
        let local = match field.next() {
            Some(l) => l,
            None => return false,
        };
        let _rem = field.next();
        local.ends_with(&want_port) && field.next() == Some(want_state.as_str())
    }))
}

/// An **abstract** `sockaddr_un`: the leading NUL and the name, which is the
/// address the namespace split below is about. `rustix` writes the field
/// (`new_abstract_name`), so this file carries no second copy of the encoding.
fn abstract_addr(name: &[u8]) -> rustix::net::SocketAddrUnix {
    rustix::net::SocketAddrUnix::new_abstract_name(name).expect("an abstract name fits its field")
}

/// The machine's wire can be moved into a namespace, and comes back.
///
/// Linux's shape, measured on the host before any of this was written
/// (`.workbuddy/probe-netns-move.sh` and `.workbuddy/probe-netns-errno2.py`,
/// transcript in `docs/notes/` §6.3): the device keeps its name, its MTU, its
/// hardware address and its interface index, and it arrives **down** and **with
/// nothing configured on it** - an address belongs to the namespace that holds
/// it, not to the device, so both sides of the move end up with none, and a
/// moved interface has to be configured again.
///
/// Three of those halves are observable without a dump parser:
///
/// * the namespace it left has no `eth0` at all afterwards - `ENODEV` where the
///   same request answered `0` a moment earlier;
/// * the namespace it entered has one, and can give it the very address the
///   machine had, which it could not if the address had travelled with the
///   device (`EEXIST`);
/// * and it comes back the same way it left, so the machine has to bring it up
///   and address it again before it has a network.
///
/// The last one is why **nothing is asserted until the wire is home and
/// configured**: a claim that failed halfway would leave the machine with no
/// network for every test that runs after this one, and this program is one of
/// ~180 sharing a boot.
fn wire_moves_into_a_namespace(r: &mut Report) {
    let to_child = pipe();
    let to_parent = pipe();
    r.check(
        "the move's two pipes are made",
        to_child.is_ok() && to_parent.is_ok(),
    );
    let (Ok((down_r, down_w)), Ok((up_r, up_w))) = (to_child, to_parent) else {
        return;
    };
    // The child names this pid to hand the wire back, and it has to be read
    // before the clone: the child inherits the number.
    let parent_pid = std::process::id() as i32;
    let child = clone_word(libc::SIGCHLD as usize | libc::CLONE_NEWNET as usize);
    if child == 0 {
        // SAFETY: the clone child, so nothing of `std`'s may run past it and
        // every way out is a raw `_exit`.
        unsafe {
            libc::syscall(libc::SYS_close, up_r.as_raw_fd() as libc::c_long);
            libc::syscall(libc::SYS_close, down_w.as_raw_fd() as libc::c_long);
        }
        let mut word = [0u8; 8];
        // Two rounds, and the second is not optional: a wire left in a
        // namespace whose last task is leaving it does not come home.
        loop {
            let n = match read(&down_r, &mut word) {
                Ok(n) => n,
                Err(_) => break,
            };
            if n == 0 {
                break;
            }
            let Some(fd) = route_socket() else {
                break;
            };
            if word[0] == b'1' {
                // This namespace holds the wire now: bringing it up and
                // addressing it both work, where both were `ENODEV` before.
                let here = ask(&fd, &set_link_up(ETH_INDEX));
                let addressed = ask(&fd, &add_addr(ETH_INDEX, WIRE_PREFIX, WIRE_IP));
                let line = format!("{here} {addressed}\n");
                let _ = write(&up_w, line.as_bytes());
            } else {
                // Hand it back by naming the parent, which is in the machine's
                // namespace. This is the caller Linux demands of the request:
                // the device is in *this* namespace now, and a request from
                // outside it has no standing over it.
                let back = ask(&fd, &set_link_netns(ETH_INDEX, parent_pid));
                let line = format!("{back}\n");
                let _ = write(&up_w, line.as_bytes());
                break;
            }
        }
        raw_exit(0);
    }
    drop(down_r);
    drop(up_w);
    r.check("the child that will take the wire was cloned", child > 0);
    if child <= 0 {
        return;
    }
    let Some(fd) = route_socket() else {
        r.check("the machine's route socket is made", false);
        return;
    };

    // ---- the whole round trip, before any claim is made ----
    let before = ask(&fd, &set_link_up(ETH_INDEX));
    let moved = ask(&fd, &set_link_netns(ETH_INDEX, child as i32));
    let gone = ask(&fd, &set_link_up(ETH_INDEX));
    let _ = write(&down_w, b"1\n");
    let mut line = [0u8; REPORT_MAX];
    let n = read_line(&up_r, &mut line);
    let seen: Vec<isize> = String::from_utf8_lossy(&line[..n])
        .split_whitespace()
        .filter_map(|f| f.parse().ok())
        .collect();
    let child_up = seen.first().copied().unwrap_or(-5);
    let child_addr = seen.get(1).copied().unwrap_or(-5);
    let _ = write(&down_w, b"2\n");
    let n = read_line(&up_r, &mut line);
    let back: isize = String::from_utf8_lossy(&line[..n])
        .split_whitespace()
        .filter_map(|f| f.parse().ok())
        .next()
        .unwrap_or(-5);
    // Restoring the machine, which is what Linux's shape makes necessary: the
    // wire came back down and unconfigured, and every test that runs after this
    // one needs the network.
    let up_again = ask(&fd, &set_link_up(ETH_INDEX));
    let addressed = ask(&fd, &add_addr(ETH_INDEX, WIRE_PREFIX, WIRE_IP));
    let routed = ask(&fd, &add_route(WIRE_GATEWAY));
    let status = rustix::process::waitpid(
        rustix::process::Pid::from_raw(child as i32),
        rustix::process::WaitOptions::empty(),
    );

    // ---- and only now the claims ----
    let enodev = -(Errno::NODEV.raw_os_error() as isize);
    r.check(
        &format!("the wire is up in the machine's namespace to start with (got {before})"),
        before == 0,
    );
    r.check(
        &format!("moving it into the child's namespace succeeds (got {moved})"),
        moved == 0,
    );
    r.check(
        &format!("and the namespace it left has no eth0 afterwards, ENODEV (got {gone})"),
        gone == enodev,
    );
    r.check(
        &format!("the child's namespace has the wire and can bring it up (got {child_up})"),
        child_up == 0,
    );
    r.check(
        &format!(
            "and can give it the machine's own address, so no address travelled \
             with the device (got {child_addr}; EEXIST would mean one had)"
        ),
        child_addr == 0,
    );
    r.check(
        &format!("the child hands it back by naming the parent (got {back})"),
        back == 0,
    );
    r.check(
        &format!("the machine has it again and brings it up (got {up_again})"),
        up_again == 0,
    );
    r.check(
        &format!(
            "and re-addresses it, because it came back unconfigured \
             (got {addressed}; EEXIST would mean it had not)"
        ),
        addressed == 0,
    );
    r.check(
        &format!("and restores its default route (got {routed})"),
        routed == 0,
    );
    r.check(
        "and the child that carried the wire is reaped",
        matches!(&status, Ok(Some((_, st))) if st.exited() && st.exit_status() == Some(0)),
    );
}

fn main() {
    let mut r = Report::new();

    if host_run() {
        println!(
            "note: a host cannot create a network namespace (CLONE_NEWNET needs CAP_SYS_ADMIN), so no claim is made here"
        );
        r.finish()
    }

    // ---- A word that mixes the served kinds with an unserved one is refused
    //      whole ----
    r.check(
        "unshare(CLONE_NEWNET|CLONE_NEWIPC) is EOPNOTSUPP, whole",
        unshare(UnshareFlags::NEWNET | UnshareFlags::NEWIPC) == Err(Errno::OPNOTSUPP),
    );

    // ---- A socket outlives the move, which is what makes `unshare` usable ----
    //
    // `unshare(CLONE_NEWNET)` moves the **caller** (Linux's rule for this kind,
    // and the opposite of PID's). The socket opened *before* the call therefore
    // still belongs to the stack it was made in, and it goes on answering its
    // own address afterwards - Linux's `sk_net`, which this kernel has as the
    // handle map (`net`'s `HOMES`). The move happens in a child so that the rest
    // of this program stays in the machine's namespace.
    let report = pipe();
    r.check("the move's report pipe is made", report.is_ok());
    if let Ok((moved_r, moved_w)) = report {
        let moved = unsafe { libc::fork() };
        if moved == 0 {
            drop(moved_r);
            let before = socket(AddressFamily::INET, SocketType::STREAM, None);
            let bound = match &before {
                Ok(pipe_sock) => {
                    let any = SocketAddrV4::new(LOOPBACK, 0);
                    match bind(pipe_sock, &any).and_then(|()| getsockname(pipe_sock)) {
                        Ok(addr) => SocketAddrV4::try_from(addr).map(|a| a.port()).unwrap_or(0),
                        Err(_) => 0,
                    }
                }
                Err(_) => 0,
            };
            let ok = unshare(UnshareFlags::NEWNET).is_ok();
            // A socket made *after* the move is the new namespace's, while the
            // one made before is still the old stack's.
            let after = match &before {
                Ok(pipe_sock) => getsockname(pipe_sock)
                    .ok()
                    .and_then(|a| SocketAddrV4::try_from(a).ok())
                    .map(|a| a.port())
                    .unwrap_or(0),
                Err(_) => 0,
            };
            let line = format!("{} {} {}\n", if ok { 1 } else { 0 }, bound, after);
            let _ = write(&moved_w, line.as_bytes());
            unsafe { libc::syscall(libc::SYS_close, moved_w.as_raw_fd() as libc::c_long) };
            raw_exit(0);
        }
        drop(moved_w);
        r.check("the task that moves was forked", moved > 0);
        if moved > 0 {
            let mut buf = [0u8; 64];
            let n = read_line(&moved_r, &mut buf);
            let said: Vec<isize> = String::from_utf8_lossy(&buf[..n])
                .split_whitespace()
                .filter_map(|f| f.parse().ok())
                .collect();
            r.check("the child reports the move", said.len() == 3);
            if said.len() == 3 {
                r.check("unshare(CLONE_NEWNET) is served", said[0] == 1);
                r.check("a socket bound before the move has a port", said[1] != 0);
                r.check(
                    "and it still answers that port after the move: the socket kept its stack",
                    said[1] == said[2],
                );
            }
            let status = rustix::process::waitpid(
                rustix::process::Pid::from_raw(moved as i32),
                rustix::process::WaitOptions::empty(),
            );
            r.check(
                "and the child exits cleanly",
                matches!(&status, Ok(Some((_, st))) if st.exited() && st.exit_status() == Some(0)),
            );
        }
    }

    // ---- A caller holding a socket may still clone a namespace ----
    //
    // Inherited descriptors are no problem any more: a socket remembers the
    // stack it was made in, so one the child inherits goes on working against
    // *that* stack - Linux's shape, and the reason the map exists.
    let held = socket(AddressFamily::INET, SocketType::STREAM, None);
    r.check("a socket for the clone is opened", held.is_ok());
    let with_socket = clone_word(libc::SIGCHLD as usize | libc::CLONE_NEWNET as usize);
    // The child of a *successful* clone lands here with 0, and it has to leave:
    // without this it ran the whole program a second time, and its copy of every
    // claim - including this one, which is false for it, since it is not the
    // caller and cloned nothing - interleaved with the real run's. The harness
    // waits for the original pid, which exits 0, so it read the doubled run as a
    // pass and the second copy's `FAILED` was never anyone's business. Measured
    // in the sweep's own serial log before this line existed (every claim of this
    // program printed twice, the interleaved half failing on exactly two of
    // them), which is what makes it a defect in the pin rather than in the
    // kernel: a pin whose body runs twice is not a pin.
    if with_socket == 0 {
        raw_exit(0);
    }
    r.check(
        "clone(CLONE_NEWNET) while holding a socket is served",
        with_socket > 0,
    );
    if with_socket > 0 {
        let status = rustix::process::waitpid(
            rustix::process::Pid::from_raw(with_socket as i32),
            rustix::process::WaitOptions::empty(),
        );
        r.check(
            "and that child is reaped",
            matches!(&status, Ok(Some((_, st))) if st.exited()),
        );
    }
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
            // How long the handshake took, as the eighth word: the other
            // seven say it *completed*, and completion was true even when
            // nothing was polling this namespace's stack and the wait only
            // ended when the connect deadline expired (`docs/notes/` §15.92).
            let mut handshake_ms = -1isize;
            if let Ok(listener) = &listener {
                let here = SocketAddrV4::new(LOOPBACK, port);
                if bind(listener, &here).is_ok() && listen(listener, 1).is_ok() {
                    listen_ok = 0;
                    if let Ok(client) = socket(AddressFamily::INET, SocketType::STREAM, None) {
                        let started = Instant::now();
                        if connect(&client, &here).is_ok() {
                            connect_ok = 0;
                            if accept(listener).is_ok() {
                                accept_ok = 0;
                            }
                        }
                        handshake_ms = started.elapsed().as_millis() as isize;
                    }
                }
            }
            // This namespace's own socket table, read here: the listener just
            // bound must be a row *of this namespace's* file. The parent picks
            // the port because it is free in the *machine's* stack, so the same
            // number is a row here and absent from the machine's table below -
            // the two answers about two stacks are the whole claim.
            let own_row = proc_net_tcp_local(port, 10)
                .map(|present| present as isize)
                .unwrap_or(-1);
            let line = format!(
                "{} {} {} {} {} {} {} {}\n",
                lo_up, addr, eth_up, listen_ok, connect_ok, accept_ok, own_row, handshake_ms
            );
            let _ = write(&up_w, line.as_bytes());

            // ---- A descriptor handed over from *another* namespace ----
            //
            // The socket the parent sends was made in the machine's stack. It is
            // delivered here like any other descriptor, because a socket carries
            // the stack it was made in (`net`'s `HOMES`) - so what arrives is a
            // *working* socket whose address is the sender's, not one of this
            // namespace's own sockets renumbered. Reading that address back is
            // the claim; a pathname unix socket is the meeting point, and it
            // works across the boundary because the unix registry is
            // machine-wide. A leftover from an earlier run would fail the bind
            // for a reason that is not this claim's, so the name is cleared.
            let _ = std::fs::remove_file(PATH);
            let mut trunc = -1isize;
            let mut kept = -1isize;
            let mut port = -1isize;
            if let Ok(listener) = socket(AddressFamily::UNIX, SocketType::STREAM, None)
                && let Ok(path) = rustix::net::SocketAddrUnix::new(PATH)
                && bind(&listener, &path).is_ok()
                && listen(&listener, 4).is_ok()
            {
                // Announce the listener, then wait for the parent's call and for
                // the address its gift is bound on.
                let _ = write(&up_w, b"ready\n");
                let mut tell = [0u8; 32];
                let told = read_line(&down_r, &mut tell);
                let want: i64 = String::from_utf8_lossy(&tell[..told])
                    .trim()
                    .parse()
                    .unwrap_or(0);
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
                    msg.msg_controllen = control.len() as u32;
                    let rc = unsafe { libc::recvmsg(conn.as_raw_fd(), &mut msg, 0) };
                    if rc >= 0 {
                        trunc = if msg.msg_flags & libc::MSG_CTRUNC != 0 {
                            1
                        } else {
                            0
                        };
                        // The descriptor, if it came, is at the chain's first
                        // record: `struct cmsghdr` is length/level/type and the
                        // fd follows it, which is where a libc's `CMSG_DATA`
                        // points on this target.
                        if msg.msg_controllen >= 20 {
                            kept = 1;
                            let fd = i32::from_ne_bytes([
                                control[16],
                                control[17],
                                control[18],
                                control[19],
                            ]);
                            let borrowed = unsafe { rustix::fd::BorrowedFd::borrow_raw(fd) };
                            if let Ok(addr) = getsockname(borrowed)
                                && let Ok(v4) = SocketAddrV4::try_from(addr)
                            {
                                // Bound by the sender, so a port here is the
                                // sender's own state reached from this namespace.
                                port = if v4.port() == want as u16 { 1 } else { 0 };
                            }
                        } else {
                            kept = 0;
                        }
                    }
                }
            }
            let second = format!("{trunc} {kept} {port}\n");
            let _ = write(&up_w, second.as_bytes());
            unsafe { libc::syscall(libc::SYS_close, up_w.as_raw_fd() as libc::c_long) };
            raw_exit(0);
        }

        drop(down_r);
        drop(up_w);
        r.check("the child that gets its own stack was cloned", child > 0);
        if child > 0 {
            // ---- What the move request still refuses ----
            //
            // `ip link set X netns N` is served now - the round trip below walks
            // a whole one - but two shapes of it are refusals, and both are
            // Linux's own answers, measured on the host with
            // `.workbuddy/probe-netns-errno2.py`:
            //
            //   * `lo` is netns-local: `EINVAL` (-22) whatever it is aimed at;
            //   * a pid that names no task is `ESRCH` (-3), which is what
            //     `get_net_ns_by_pid` answers. `ip` refuses that one in
            //     userland, so no script ever sees the kernel's word.
            //
            // Neither moves anything, which is the only reason they can be
            // asked here in mid-flow: the round trip needs the wire to itself.
            let route = route_socket();
            let (lo_moved, gone_pid) = match &route {
                Some(fd) => (
                    ask(fd, &set_link_netns(LO_INDEX, child as i32)),
                    ask(fd, &set_link_netns(ETH_INDEX, NO_SUCH_PID)),
                ),
                None => (0, 0),
            };
            r.check(
                &format!("lo cannot be moved into a namespace, EINVAL (got {lo_moved})"),
                lo_moved == -(Errno::INVAL.raw_os_error() as isize),
            );
            r.check(
                &format!(
                    "and a pid that names no task is ESRCH, not a silent success (got {gone_pid})"
                ),
                gone_pid == -(Errno::SRCH.raw_os_error() as isize),
            );

            let mine = ns_number();
            r.check(
                "the parent reads its own net namespace number",
                mine.is_some(),
            );
            let tell = format!("{port}\n");
            let _ = write(&down_w, tell.as_bytes());

            let mut report = [0u8; REPORT_MAX];
            let n = read_line(&up_r, &mut report);
            let text = String::from_utf8_lossy(&report[..n]).to_string();
            let fields: Vec<isize> = text
                .split_whitespace()
                .filter_map(|f| f.parse().ok())
                .collect();
            r.check("the child reports eight words", fields.len() == 8);
            if fields.len() == 8 {
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
                // The socket table, read *inside* the namespace: the listener
                // bound a moment ago is a row of `/proc/net/tcp` here, because
                // the collector resolves against this reader's namespace. That
                // is the half `t-ns-net` never exercised - the file existed and
                // the filter ran, but no claim read it.
                r.check(
                    &format!(
                        "and the child's own /proc/net/tcp carries the listener it just bound (got {})",
                        fields[6]
                    ),
                    fields[6] == 1,
                );
                // The handshake's latency, and the reason it is a claim: the
                // seven words above were all true when the wait was 29.997s,
                // because that is the connect deadline expiring and the
                // condition being re-evaluated - success, at the speed of a
                // timer. A loopback handshake is microseconds on Linux; a
                // second is three orders of magnitude of margin and still
                // fails the deadline-expiry path by thirty.
                r.check(
                    &format!(
                        "and the handshake did not wait for the connect deadline ({} ms)",
                        fields[7]
                    ),
                    fields[7] >= 0 && fields[7] < 1000,
                );
            }

            // The machine's stack, untouched: the *same* port is free here, so
            // the machine's `/proc/net/tcp` - read *here*, in the machine's
            // namespace - has no row for it. The same number, a table each: the
            // rows belong to the reader's namespace and not to the machine.
            r.check(
                "while the machine's /proc/net/tcp has no row for the child's port",
                proc_net_tcp_local(port, 10) == Some(false),
            );

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
            // Bound here, in the machine's namespace, so the child has something
            // to read back off it: the *sender's* address is the proof that the
            // descriptor resolved against the sender's stack.
            let gift_port = match &gift {
                Ok(sock) => {
                    let any = SocketAddrV4::new(LOOPBACK, 0);
                    bind(sock, &any)
                        .and_then(|()| getsockname(sock))
                        .ok()
                        .and_then(|a| SocketAddrV4::try_from(a).ok())
                        .map(|a| a.port())
                        .unwrap_or(0)
                }
                Err(_) => 0,
            };
            r.check(
                "and bound, so the child can read its address",
                gift_port != 0,
            );
            let mut handover = None;
            // The child announces its listener only after `bind`+`listen`, and
            // that announcement is what makes the connect below a rendezvous
            // rather than a race: connecting first was reading the child's
            // scheduling, and a run that lost the race failed the handover and
            // left the child parked in `accept`. Measured on the aarch64 sweep
            // when a socket-timing change moved the parent ahead of the child.
            let mut ready = [0u8; 16];
            if let Ok(gift) = &gift
                && read_line(&up_r, &mut ready) > 0
                && let Ok(path) = rustix::net::SocketAddrUnix::new(PATH)
                && let Ok(conn) = socket(AddressFamily::UNIX, SocketType::STREAM, None)
                    .and_then(|c| connect(&c, &path).map(|()| c))
            {
                // Tell the child the address to expect, then hand it over.
                let tell = format!("{gift_port}\n");
                let _ = write(&down_w, tell.as_bytes());
                drop(down_w);
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
            r.check("the child reports the handover's outcome", said.len() == 3);
            if said.len() == 3 {
                r.check(
                    &format!(
                        "a descriptor handed across namespaces arrives intact (got {} {})",
                        said[0], said[1]
                    ),
                    said[0] == 0 && said[1] == 1,
                );
                r.check(
                    &format!(
                        "and it is the sender's socket, address and all (got {})",
                        said[2]
                    ),
                    said[2] == 1,
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

    // ---- The abstract namespace belongs to the *socket's* namespace ----
    //
    // The unix family's two kinds of name are not namespaced the same way, and
    // neither is namespaced by the *task*: measured on the host with the three
    // probes beside this rung's plan (`.workbuddy/probe_unix_netns{,2,3}.c`,
    // transcript in `docs/notes/` §6.3). An abstract name is the namespace's of
    // the socket that holds it - a name planted in one is `ECONNREFUSED` from a
    // socket in another, the same name binds again there, and a socket carried
    // across an `unshare` goes on using the table it was made in. A path is not
    // namespaced at all, which is the *other* half and the reason the handover
    // above can meet at a pathname.
    //
    // One `fork` carries both of the parent's unix sockets into a fresh
    // namespace, and then: the carried dialer must still find the name the
    // machine holds, a socket *made here* must not, and the very name must be
    // free to take again. The child's own name has to be invisible in the
    // machine's namespace afterwards, so the child waits on the pipe until the
    // parent has asked.
    const HOME: &[u8] = b"t-ns-net-home";
    const INNER: &[u8] = b"t-ns-net-inner";

    let home = socket(AddressFamily::UNIX, SocketType::STREAM, None);
    let carried = socket(AddressFamily::UNIX, SocketType::STREAM, None);
    let home_ready = match (&home, &carried) {
        (Ok(home), Ok(_)) => bind(home, &abstract_addr(HOME)).is_ok() && listen(home, 8).is_ok(),
        _ => false,
    };
    r.check(
        "the machine's namespace binds an abstract name to carry across",
        home_ready,
    );
    if home_ready {
        let report = pipe();
        r.check("the abstract-name report pipe is made", report.is_ok());
        if let Ok((inner_r, inner_w)) = report {
            // A second pipe, so the child can be *held* open while this
            // namespace asks about the name it bound: an exited child would
            // answer `ECONNREFUSED` because its socket died, which is the same
            // errno for the wrong reason and would make the claim below pass
            // without measuring anything.
            let release = pipe();
            r.check("the release pipe is made", release.is_ok());
            if let Ok((release_r, release_w)) = release {
                let child = unsafe { libc::fork() };
                if child == 0 {
                    // The parent's end is the only writer, so the read below
                    // ends when *it* lets go.
                    drop(release_w);
                    // Leave for a fresh namespace with both inherited
                    // descriptors: the fork came first, so the sockets are the
                    // *machine's*.
                    let moved = unshare(UnshareFlags::NEWNET).is_ok();
                    let carried_ok = match &carried {
                        Ok(dialer) => connect(dialer, &abstract_addr(HOME)).is_ok(),
                        Err(_) => false,
                    };
                    let stranger_refused =
                        match socket(AddressFamily::UNIX, SocketType::STREAM, None) {
                            Ok(fresh) => {
                                connect(&fresh, &abstract_addr(HOME)) == Err(Errno::CONNREFUSED)
                            }
                            Err(_) => false,
                        };
                    // The same name, taken again here: a name is the namespace's.
                    let rebind_ok = match socket(AddressFamily::UNIX, SocketType::STREAM, None) {
                        Ok(same) => bind(&same, &abstract_addr(HOME)).is_ok(),
                        Err(_) => false,
                    };
                    // And a name of this namespace's own, planted and answered
                    // inside it - which is what makes the refusals above a
                    // boundary rather than a socket family that stopped working.
                    let inner_ok = match socket(AddressFamily::UNIX, SocketType::STREAM, None) {
                        Ok(listener) => {
                            bind(&listener, &abstract_addr(INNER)).is_ok()
                                && listen(&listener, 4).is_ok()
                                && match socket(AddressFamily::UNIX, SocketType::STREAM, None) {
                                    Ok(dialer) => connect(&dialer, &abstract_addr(INNER)).is_ok(),
                                    Err(_) => false,
                                }
                        }
                        Err(_) => false,
                    };
                    let line = format!(
                        "{} {} {} {} {}\n",
                        moved as usize,
                        carried_ok as usize,
                        stranger_refused as usize,
                        rebind_ok as usize,
                        inner_ok as usize
                    );
                    let _ = write(&inner_w, line.as_bytes());
                    drop(inner_w);
                    let mut byte = [0u8; 1];
                    let _ = read(&release_r, &mut byte);
                    raw_exit(0);
                }
                r.check("the task that carries the names was forked", child > 0);
                if child > 0 {
                    let mut buf = [0u8; REPORT_MAX];
                    let n = read_line(&inner_r, &mut buf);
                    let said: Vec<isize> = String::from_utf8_lossy(&buf[..n])
                        .split_whitespace()
                        .filter_map(|f| f.parse().ok())
                        .collect();
                    r.check("the carried child reports five words", said.len() == 5);
                    if said.len() == 5 {
                        r.check("unshare(CLONE_NEWNET) is served there", said[0] == 1);
                        r.check(
                            "a socket made here and carried across still dials this namespace's abstract name",
                            said[1] == 1,
                        );
                        r.check(
                            "while a socket made inside that namespace cannot see the name",
                            said[2] == 1,
                        );
                        r.check(
                            "and the very name is free to take again there: a name is the namespace's",
                            said[3] == 1,
                        );
                        r.check(
                            "the namespace has abstract names of its own, planted and answered inside it",
                            said[4] == 1,
                        );
                    }
                    // The other direction, asked from here while the child holds
                    // its name: the child's name is not this namespace's to find.
                    let stranger = match socket(AddressFamily::UNIX, SocketType::STREAM, None) {
                        Ok(probe) => {
                            connect(&probe, &abstract_addr(INNER)) == Err(Errno::CONNREFUSED)
                        }
                        Err(_) => false,
                    };
                    r.check(
                        "a name bound inside the fresh namespace is unknown in the machine's",
                        stranger,
                    );
                    // The control: this side's own table is untouched, so the
                    // refusal above is the boundary and not a broken registry.
                    let mine = match socket(AddressFamily::UNIX, SocketType::STREAM, None) {
                        Ok(probe) => connect(&probe, &abstract_addr(HOME)).is_ok(),
                        Err(_) => false,
                    };
                    r.check("while the machine's own name still answers here", mine);
                    drop(release_w);
                    let status = rustix::process::waitpid(
                        rustix::process::Pid::from_raw(child as i32),
                        rustix::process::WaitOptions::empty(),
                    );
                    r.check(
                        "and the child that carried the names is reaped",
                        matches!(&status, Ok(Some((_, st))) if st.exited() && st.exit_status() == Some(0)),
                    );
                }
            }
        }
    }

    // ---- The wire can enter a namespace ----
    //
    // Last, and deliberately so: it is the one claim here that takes the
    // machine's only NIC away for a moment, and it puts everything back before
    // it asserts anything.
    wire_moves_into_a_namespace(&mut r);

    // ---- Whose capability a raw socket asks for ----
    //
    // The two raw doors in this file that ask a *capability* - `AF_PACKET`, and
    // `AF_INET`'s `SOCK_RAW` a program reaches for as `ping` and a DHCP client -
    // and the question is not "does the caller hold `CAP_NET_RAW`" but "is the
    // caller privileged over the **network namespace it is standing in**" -
    // Linux's `ns_capable(sock_net(sk)->user_ns, CAP_NET_RAW)` for both.
    // Measured on a current host, one probe per door in the same three
    // positions (`.workbuddy/probe_ns_owner.c`, `.workbuddy/probe_rawip_ns.c`,
    // Linux 7.2.9): a caller inside a bare user namespace - holding the whole
    // set *in that namespace* - is still `EPERM` for both, because the network
    // namespace it is in is the machine's, and served for both the moment it has
    // made a network namespace of its own, because that one's owner is the
    // caller. A child does the whole dance, because the caller of this test
    // cannot move itself into a user namespace and back.
    let packet_go = pipe();
    r.check("the packet pipe is made", packet_go.is_ok());
    if let Ok((from_child, to_child)) = packet_go {
        let child = clone_word(libc::SIGCHLD as usize);
        if child == 0 {
            // The read end is the parent's to read: this side writes and then
            // parks nowhere at all - it exits as soon as the report is out.
            drop(from_child);
            let unshare_word = |word: usize| -> isize {
                let rc = unsafe { libc::syscall(libc::SYS_unshare, word as libc::c_long) };
                if rc < 0 {
                    -(std::io::Error::last_os_error().raw_os_error().unwrap_or(0) as isize)
                } else {
                    rc as isize
                }
            };
            let answer = |rc: i32| -> isize {
                if rc < 0 {
                    -(std::io::Error::last_os_error().raw_os_error().unwrap_or(0) as isize)
                } else {
                    rc as isize
                }
            };
            let packet = |_: usize| -> isize {
                answer(unsafe {
                    libc::socket(
                        libc::AF_PACKET,
                        libc::SOCK_DGRAM,
                        libc::htons(libc::ETH_P_ALL as u16) as i32,
                    )
                })
            };
            let raw_ip = |_: usize| -> isize {
                answer(unsafe { libc::socket(libc::AF_INET, libc::SOCK_RAW, libc::IPPROTO_ICMP) })
            };
            // A user namespace of its own: empty maps, and the whole capability
            // set *in it* - which is the point, because the set is not the
            // question any more.
            let made_user = unshare_word(CLONE_NEWUSER);
            // Still the machine's network namespace, so its owner is the
            // machine's user namespace, and the caller is not privileged over
            // that one from below it.
            let bare = packet(0);
            let bare_raw = raw_ip(0);
            // One of its own, owned by the namespace it stands in.
            let made_net = unshare_word(libc::CLONE_NEWNET as usize);
            let own = packet(0);
            let own_raw = raw_ip(0);
            let line = format!("{made_user}\n{bare}\n{bare_raw}\n{made_net}\n{own}\n{own_raw}\n");
            let _ = write(&to_child, line.as_bytes());
            drop(to_child);
            raw_exit(0);
        }
        r.check("the packet child is forked", child > 0);
        drop(to_child);
        let mut buf = [0u8; 64];
        let n = read(&from_child, &mut buf).unwrap_or(0);
        let fields: Vec<isize> = String::from_utf8_lossy(&buf[..n])
            .lines()
            .filter_map(|l| l.trim().parse().ok())
            .collect();
        let get = |i: usize| fields.get(i).copied();
        r.check(
            "the child made a user namespace of its own",
            get(0) == Some(0),
        );
        r.check(
            "and a packet socket is EPERM while it stands in the machine's network namespace",
            get(1) == Some(-(libc::EPERM as isize)),
        );
        // The same question, the other raw door: a bare user namespace is
        // refused an `AF_INET` raw socket too, which is the claim `AF_PACKET`
        // alone could not make.
        r.check(
            "and so is an AF_INET raw socket, the same question asked of the same owner",
            get(2) == Some(-(libc::EPERM as isize)),
        );
        r.check(
            "the child made a network namespace of its own",
            get(3) == Some(0),
        );
        // Served, with the same set in the same caller: the question was asked
        // of the namespace's owner, and that owner is now the caller's own.
        r.check(
            "and the same caller, the same set, opens a packet socket in it",
            get(4).is_some_and(|fd| fd >= 0),
        );
        r.check("and a raw IP socket too", get(5).is_some_and(|fd| fd >= 0));
        let mut status = 0;
        let reaped = unsafe { libc::waitpid(child as i32, &mut status, 0) };
        r.check("the packet child is reaped", reaped == child as i32);
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
