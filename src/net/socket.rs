//! Socket syscalls and the inode that carries a socket through the fd table.
//!
//! A socket is an anonymous channel, not a file: nothing on any filesystem
//! names it, and the descriptor table is the only thing that does. It still
//! has to ride the same `Arc<dyn Inode>` every descriptor names, which is
//! what [`SocketInode`] is for - a handle wrapped in the [`Inode`] interface,
//! answering metadata and readiness and nothing else. Every real operation
//! comes in through the syscalls below, which resolve the fd back to the
//! handle and call into the stack in `super`.
//!
//! The calling convention is Linux's, argument shape for argument shape:
//! `sockaddr_in` in host order for the family and network order for the port
//! and address, `socklen_t` in-out for anything that writes one back. The
//! family is `AF_INET` or nothing - musl's resolver works over UDP sockets,
//! apk talks plain HTTP over TCP, and every other family answers
//! `EAFNOSUPPORT`, which is exactly the answer musl's syslog treats as
//! "fall back to the console".
//!
//! Blocking follows the stack's one rule: no sleeping with the lock held. A
//! blocking `recv` or `connect` is a loop of *lock, poll, try, unlock, sleep
//! one tick*, with a fatal-signal check each round so a `SIGKILL` still ends
//! a task parked on a quiet socket.

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::slice;

use smoltcp::socket::raw as raw_sock;
use smoltcp::socket::tcp;
use smoltcp::socket::udp;
use smoltcp::wire::{IpAddress, IpEndpoint, IpListenEndpoint, Ipv4Address};

use super::Channel;
use crate::syscall::err;
use crate::vfs::{Inode, Metadata, NodeKind, Ready, VfsError};

// --- the Linux constants the ABI spells by number ---
pub(crate) const AF_UNIX: u16 = 1;
pub(crate) const AF_INET: u16 = 2;
/// The IPv6 family number. Accepted at `socket()` so address-list walkers
/// behave as they do on Linux, with every operation answering ENOTUNREACH
/// further down.
pub(crate) const AF_INET6: u16 = 10;
pub(crate) const SOCK_STREAM: u16 = 1;
pub(crate) const SOCK_DGRAM: u16 = 2;
pub(crate) const SOCK_RAW: u16 = 3;
/// `SOCK_SEQPACKET`: message boundaries over a stream. Nothing here carries
/// them, but the number is part of the ABI and is named to be answered, not
/// to be mistaken for something else.
const SOCK_SEQPACKET: u16 = 5;
const SOCK_NONBLOCK: u32 = 0o4000;
const SOCK_CLOEXEC: u32 = 0o2000000;
const IPPROTO_ICMP: u32 = 1;
const IPPROTO_TCP: u32 = 6;
const IPPROTO_UDP: u32 = 17;

const SOL_SOCKET: usize = 1;
const SO_TYPE: usize = 3;
const SO_ERROR: usize = 4;

/// The length of a `sockaddr_in` on x86_64: family, port, address, padding.
const SOCKADDR_IN_LEN: usize = 16;

/// What an unnamed socket reports as its name: all zeros.
const UNSPECIFIED_ENDPOINT: IpEndpoint = IpEndpoint {
    addr: IpAddress::Ipv4(Ipv4Address::new(0, 0, 0, 0)),
    port: 0,
};

/// Where `msg_flags` sits in a 56-byte `struct msghdr` on x86_64.
const MSGHDR_SIZE: usize = 56;
const MSGHDR_FLAGS: usize = 48;

// ---------------------------------------------------------------------------
// The inode
// ---------------------------------------------------------------------------

/// A socket as the fd table sees it: a handle and a stub inode around it.
///
/// The handle is everything - reads, writes, options and teardown all go
/// through the syscalls here, and the registry in `super` resolves it to the
/// socket's buffers and state. The inode itself refuses I/O: a `read(2)` on
/// a socket is answered by the syscall layer before any inode method runs,
/// and anything that gets here directly finds `Unsupported`.
pub struct SocketInode {
    pub handle: usize,
}

/// The route-netlink session a descriptor names, if it names one - how the
/// send/receive paths route this family's traffic away from the IP stack.
fn rtnl_handle_of_fd(fd: i32) -> Option<usize> {
    let file = crate::sched::current_fds().clone_at(fd).ok()?;
    let handle = file.inode().socket_handle();
    crate::net::rtnl::exists(handle).then_some(handle)
}

/// `recv(2)`'s "do not sleep" flag, as `linux/socket.h` spells it.
const MSG_DONTWAIT: usize = 0x40;

/// The next dump answer for a route-netlink session, honoring the caller's
/// wish not to wait: `MSG_DONTWAIT` on the call or `O_NONBLOCK` on the
/// description turns an empty queue into `EAGAIN` rather than a park.
///
/// The empty-queue case matters in its own right. musl's `getifaddrs` reads
/// with `MSG_DONTWAIT` and a queue that is never empty, so it never noticed
/// this kernel used to block; fastfetch's default-route probe filtered every
/// queued answer out on the `nlmsg_pid` field, reached the empty queue - and
/// waited forever on an answer that had already come and gone. Linux answers
/// that loop with `EAGAIN` and an error return; so does this now.
fn rtnl_answer(handle: usize, flags: usize, fd: i32) -> Option<Vec<u8>> {
    let dontwait =
        flags & MSG_DONTWAIT != 0 || fd_status(fd) & crate::vfs::file::O_NONBLOCK != 0;
    if dontwait && !crate::net::rtnl::ready(handle) {
        return None;
    }
    crate::net::rtnl::recv_blocking(handle)
}

/// A route-netlink socket: a queue of answers to the dump requests its
/// caller has sent. The answers are built in [`crate::net::rtnl`] when the
/// request arrives; reading one pops it.
pub struct NlRouteInode {
    pub handle: usize,
}

impl Inode for NlRouteInode {
    fn metadata(&self) -> Metadata {
        Metadata {
            ino: 0x7000_8000 + self.handle as u64,
            kind: NodeKind::CharDev,
            size: 0,
        }
    }

    fn socket_handle(&self) -> usize {
        self.handle
    }

    fn read_at(&self, _offset: u64, _buf: &mut [u8]) -> Result<usize, VfsError> {
        Err(VfsError::Unsupported)
    }

    fn write_at(&self, _offset: u64, _buf: &[u8]) -> Result<usize, VfsError> {
        Err(VfsError::Unsupported)
    }

    fn poll_ready(&self) -> Ready {
        Ready {
            read: crate::net::rtnl::ready(self.handle),
            write: true,
        }
    }
}

impl Inode for SocketInode {
    fn metadata(&self) -> Metadata {
        Metadata {
            // A made-up inode number in a range nothing else claims: sockets
            // are anonymous, but `fstat` has to say something.
            ino: 0x7000_0000 + self.handle as u64,
            kind: NodeKind::CharDev,
            size: 0,
        }
    }

    fn socket_handle(&self) -> usize {
        self.handle
    }

    fn read_at(&self, _offset: u64, _buf: &mut [u8]) -> Result<usize, VfsError> {
        Err(VfsError::Unsupported)
    }

    fn write_at(&self, _offset: u64, _buf: &[u8]) -> Result<usize, VfsError> {
        Err(VfsError::Unsupported)
    }

    fn poll_ready(&self) -> Ready {
        super::socket_ready(self.handle)
    }
}

/// Receive one device event.
///
/// Three things go back beyond the payload, and a real listener needs all
/// three to accept the message at all:
///
///   * `msg_name` - a `sockaddr_nl` with `nl_pid = 0`, which is how a netlink
///     peer is told "this came from the kernel" rather than from another
///     process.
///   * `SCM_CREDENTIALS` - `uid 0, gid 0, pid 0`. `nlplug-findfs` drops every
///     message that arrives without credentials, which makes this the
///     difference between an initramfs that finds its root and one that waits
///     forever.
///   * `msg_flags`, cleared: nothing here is truncated or exceptional.
fn recvmsg_netlink(
    msghdr: usize,
    buf: usize,
    len: usize,
    name: usize,
    namelen: u32,
    handle: usize,
) -> isize {
    // A datagram is delivered whole or not at all; a caller with a shorter
    // buffer gets what fits, as Linux's datagram sockets do.
    let Some(event) = crate::uevent::recv(handle) else {
        return err::EAGAIN;
    };
    let n = event.len().min(len);
    unsafe {
        core::ptr::copy_nonoverlapping(event.as_ptr(), buf as *mut u8, n);
    }

    if name != 0 && namelen != 0 {
        let addr = name as *mut u8;
        unsafe {
            core::ptr::write_bytes(addr, 0, 12);
            (addr as *mut u16).write_unaligned(crate::uevent::AF_NETLINK);
            // nl_pid 0 is the kernel; nl_groups 1 is the group this arrived on.
            (addr.add(8) as *mut u32).write_unaligned(1);
            // msg_namelen is the socklen_t in-out at msghdr + 8.
            ((msghdr + 8) as *mut u32).write_unaligned(12);
        }
    }

    // Control data, if the caller left room for it. The values are fixed
    // because the kernel is always the sender here.
    const SOL_SOCKET: i32 = 1;
    const SCM_CREDENTIALS: i32 = 2;
    const CMSG_LEN: usize = 16 + 12; // header + struct ucred
    let control = unsafe { ((msghdr + 32) as *const usize).read_unaligned() };
    let control_len = unsafe { ((msghdr + 40) as *const usize).read_unaligned() };
    if control != 0 && control_len >= CMSG_LEN {
        unsafe {
            let at = control as *mut u8;
            core::ptr::write_bytes(at, 0, CMSG_LEN);
            (at as *mut u64).write_unaligned(CMSG_LEN as u64); // cmsg_len
            (at.add(8) as *mut i32).write_unaligned(SOL_SOCKET); // cmsg_level
            (at.add(12) as *mut i32).write_unaligned(SCM_CREDENTIALS); // cmsg_type
            // struct ucred { pid, uid, gid } - the kernel's own.
            (at.add(16) as *mut i32).write_unaligned(0);
            (at.add(20) as *mut u32).write_unaligned(0);
            (at.add(24) as *mut u32).write_unaligned(0);
            ((msghdr + 40) as *mut usize).write_unaligned(CMSG_LEN);
        }
    } else if control_len != 0 {
        // No room for credentials. Linux answers a listener whose control
        // buffer is too small with EMSGSIZE rather than sending it a message
        // it must discard.
        return err::EMSGSIZE;
    }

    unsafe {
        ((msghdr + MSGHDR_FLAGS) as *mut i32).write_volatile(0);
    }
    n as isize
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

/// The socket handle `fd` names, or the errno for why it does not: `EBADF`
/// for no descriptor, `ENOTSOCK` for one that names a file, pipe or tty.
fn socket_handle_of_fd(fd: i32) -> Result<usize, isize> {
    let file = crate::sched::current_fds()
        .clone_at(fd)
        .map_err(|_| err::EBADF)?;
    let handle = file.inode().socket_handle();
    if handle == 0 {
        return Err(err::ENOTSOCK);
    }
    Ok(handle)
}

/// The open flags of `fd`'s description - where `O_NONBLOCK` lives.
fn fd_status(fd: i32) -> u32 {
    crate::sched::current_fds()
        .clone_at(fd)
        .map(|file| file.status())
        .unwrap_or(0)
}

/// Parse a user `sockaddr_in`. Only `AF_INET` passes; everything else is
/// `EAFNOSUPPORT`, before any byte is trusted.
fn parse_sockaddr_in(ptr: usize, len: usize, from_user: bool) -> Result<IpEndpoint, isize> {
    if ptr == 0 || len < SOCKADDR_IN_LEN {
        return Err(err::EINVAL);
    }
    if !crate::syscall::buffer_ok(ptr, SOCKADDR_IN_LEN, false, from_user) {
        return Err(err::EFAULT);
    }
    let raw = unsafe { slice::from_raw_parts(ptr as *const u8, SOCKADDR_IN_LEN) };
    let family = u16::from_ne_bytes([raw[0], raw[1]]);
    if family != AF_INET {
        return Err(err::EAFNOSUPPORT);
    }
    let port = u16::from_be_bytes([raw[2], raw[3]]);
    let addr = Ipv4Address::new(raw[4], raw[5], raw[6], raw[7]);
    Ok(IpEndpoint {
        addr: IpAddress::Ipv4(addr),
        port,
    })
}

/// Write a `sockaddr_in` and the length that goes with it: the shape
/// `recvfrom`, `getsockname` and `getpeername` owe their callers.
fn write_sockaddr_in(ptr: usize, len_ptr: usize, endpoint: IpEndpoint) -> Result<(), isize> {
    if !crate::syscall::buffer_ok(ptr, SOCKADDR_IN_LEN, true, true)
        || !crate::syscall::buffer_ok(len_ptr, 4, true, true)
    {
        return Err(err::EFAULT);
    }
    let mut bytes = [0u8; SOCKADDR_IN_LEN];
    bytes[0..2].copy_from_slice(&AF_INET.to_ne_bytes());
    bytes[2..4].copy_from_slice(&endpoint.port.to_be_bytes());
    // The stack holds only IPv4 addresses today, so the pattern is total;
    // the fallback arm is there for the day smoltcp grows a v6 variant.
    #[allow(unreachable_patterns)]
    let octets = match endpoint.addr {
        IpAddress::Ipv4(v4) => v4.octets(),
        _ => [0, 0, 0, 0],
    };
    bytes[4..8].copy_from_slice(&octets);
    unsafe {
        core::ptr::copy_nonoverlapping(bytes.as_ptr(), ptr as *mut u8, SOCKADDR_IN_LEN);
        (len_ptr as *mut u32).write_volatile(SOCKADDR_IN_LEN as u32);
    }
    Ok(())
}

/// Validate the output slots a `recvfrom`-shaped call will fill, before any
/// packet is consumed: consuming a datagram and then failing to report where
/// it came from would lose information the caller cannot get back.
fn validate_source_slot(src: Option<(usize, usize)>) -> Result<(), isize> {
    let Some((addr_ptr, len_ptr)) = src else {
        return Ok(());
    };
    if addr_ptr == 0 || len_ptr == 0 {
        return Err(err::EFAULT);
    }
    if !crate::syscall::buffer_ok(addr_ptr, SOCKADDR_IN_LEN, true, true)
        || !crate::syscall::buffer_ok(len_ptr, 4, true, true)
    {
        return Err(err::EFAULT);
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// socket(), close-time teardown
// ---------------------------------------------------------------------------

/// `socket(domain, type, protocol)`.
///
/// The type word carries the flag bits Linux lets callers fold in: a DNS
/// resolver asks for `SOCK_NONBLOCK` right in the call, and the flag has to
/// land on the open description as `O_NONBLOCK` or it is lost.
pub(crate) fn handle_socket(args: [usize; 6], _from_user: bool) -> isize {
    let domain = args[0] as u16;

    // Netlink first: it is not a network socket, and a machine with no
    // network card still needs its device events.
    if domain == crate::uevent::AF_NETLINK {
        let typ = (args[1] as u32) & !(SOCK_NONBLOCK | SOCK_CLOEXEC);
        // SOCK_DGRAM is what the uevent listener opens; SOCK_RAW is what
        // busybox `ip` opens for its route dumps.
        if typ != SOCK_DGRAM as u32 && typ != SOCK_RAW as u32 {
            return err::ESOCKTNOSUPPORT;
        }
        // Two protocols exist here: the uevent channel the initramfs's device
        // discovery speaks, and the route half `ip` reads its interfaces
        // through. A caller asking for anything else gets the same answer
        // Linux gives for a protocol a family does not carry.
        match args[2] as u32 {
            crate::uevent::NETLINK_KOBJECT_UEVENT => {}
            crate::net::rtnl::NETLINK_ROUTE => {
                let handle = crate::net::rtnl::open();
                let fd = crate::vfs::open_inode(Arc::new(NlRouteInode { handle }));
                if (args[1] as u32) & SOCK_NONBLOCK != 0
                    && let Ok(file) = crate::sched::current_fds().clone_at(fd)
                {
                    file.set_status(crate::vfs::file::O_NONBLOCK);
                }
                return fd as isize;
            }
            _ => return err::EPROTONOSUPPORT,
        }
        let handle = crate::uevent::open();
        let fd = crate::vfs::open_inode(Arc::new(SocketInode { handle }));
        if (args[1] as u32) & SOCK_NONBLOCK != 0
            && let Ok(file) = crate::sched::current_fds().clone_at(fd)
        {
            file.set_status(crate::vfs::file::O_NONBLOCK);
        }
        return fd as isize;
    }

    if domain != AF_INET && domain != AF_INET6 {
        return err::EAFNOSUPPORT;
    }
    let raw_type = args[1] as u32;
    let nonblock = raw_type & SOCK_NONBLOCK != 0;
    let _ = raw_type & SOCK_CLOEXEC != 0;
    let typ = (raw_type & !(SOCK_NONBLOCK | SOCK_CLOEXEC)) as u16;
    let protocol = args[2] as u32;

    let protocol_matches = match typ {
        SOCK_STREAM => protocol == 0 || protocol == IPPROTO_TCP,
        SOCK_DGRAM => protocol == 0 || protocol == IPPROTO_UDP,
        // Raw sockets: IPPROTO_ICMP (1) is what `ping` needs. Protocol 0
        // is allowed too - Linux defaults to the "raw" protocol for the
        // family, which for AF_INET is ICMP.
        SOCK_RAW => protocol == 0 || protocol == IPPROTO_ICMP,
        _ => return err::ESOCKTNOSUPPORT,
    };
    if !protocol_matches {
        return err::EPROTONOSUPPORT;
    }

    let handle = match super::alloc_socket(domain, typ, protocol) {
        Ok(handle) => handle,
        Err(e) => return e,
    };
    let fd = crate::vfs::open_inode(Arc::new(SocketInode { handle }));
    if nonblock && let Ok(file) = crate::sched::current_fds().clone_at(fd) {
        file.set_status(crate::vfs::file::O_NONBLOCK);
    }
    fd as isize
}

/// The last close of a socket: tear the channel down.
///
/// Called with a description whose registry slot is being emptied. "Last" is
/// a strong-count question - a `dup`'d fd or a fork's copy holds another
/// reference to the same description, and the socket lives until the last
/// one of those is gone. This runs after the table entry is removed, so a
/// count of one means the reference in hand is the only one left.
pub(crate) fn close_last_socket(file: &Arc<crate::vfs::File>) {
    let handle = file.inode().socket_handle();
    if handle == 0 || Arc::strong_count(file) != 1 {
        return;
    }
    super::free_entry(handle);
}

/// Batch form of [`close_last_socket`] for process exit: every description
/// of the dying process, possibly naming the same socket through several
/// fds. A socket goes only when the batch holds *all* of its references.
pub(crate) fn close_sockets_at_exit(files: &[Arc<crate::vfs::File>]) {
    let mut closed: Vec<usize> = Vec::new();
    for file in files {
        let handle = file.inode().socket_handle();
        if handle == 0 || closed.contains(&handle) {
            continue;
        }
        let ptr = Arc::as_ptr(file) as *const ();
        let total = Arc::strong_count(file);
        let in_batch = files
            .iter()
            .filter(|other| Arc::as_ptr(other) as *const () == ptr)
            .count();
        if total == in_batch {
            super::free_entry(handle);
            closed.push(handle);
        }
    }
}

// ---------------------------------------------------------------------------
// connect
// ---------------------------------------------------------------------------

/// `connect(fd, addr, addrlen)`.
///
/// A stream socket starts its handshake here and, unless the caller asked
/// for non-blocking, finishes it in a poll loop. A datagram socket just
/// pins its peer: every later `send` is aimed at it without further ado,
/// which is the whole of what UDP `connect` means.
pub(crate) fn handle_connect(args: [usize; 6], from_user: bool) -> isize {
    let fd = args[0] as i32;
    let handle = match socket_handle_of_fd(fd) {
        Ok(handle) => handle,
        Err(e) => return e,
    };
    let endpoint = match parse_sockaddr_in(args[1], args[2], from_user) {
        Ok(endpoint) => endpoint,
        Err(e) => return e,
    };
    let nonblock = fd_status(fd) & crate::vfs::file::O_NONBLOCK != 0;

    // The IPv6 stub has no route to anywhere: connect fails exactly the way
    // it does on a Linux box with no IPv6 route, and the caller moves on to
    // the next address in its list.
    let v6 = super::with(handle, |_, entry| matches!(entry.channel, Channel::Inet6));
    if v6.unwrap_or(false) {
        return err::ENETUNREACH;
    }

    let typ = super::with(handle, |_, entry| entry.typ);
    match typ {
        Some(SOCK_STREAM) => connect_tcp(handle, endpoint, nonblock),
        Some(SOCK_DGRAM) => {
            let pinned = super::with(handle, |_, entry| {
                if let Channel::Udp { peer, .. } = &mut entry.channel {
                    *peer = Some(endpoint);
                }
            });
            match pinned {
                Some(()) => 0,
                None => err::EBADF,
            }
        }
        Some(_) => err::EINVAL,
        None => err::EBADF,
    }
}

/// Drive a TCP handshake to completion.
fn connect_tcp(handle: usize, endpoint: IpEndpoint, nonblock: bool) -> isize {
    // First step: with the lock held, kick off the handshake. Everything
    // after that needs the network to answer, and the network answers on
    // poll ticks - so the lock is dropped for every one of those.
    let started = super::with(handle, |stack, entry| {
        stack.poll_once();
        let (sock_handle, local) = match &entry.channel {
            Channel::Tcp { handle, local } => (*handle, *local),
            _ => return Err(err::ENOTSOCK),
        };
        // The source port is settled before the socket is borrowed: drawing
        // an ephemeral port needs the whole stack, and the borrow of the
        // socket would be in the way.
        let port = match local {
            Some(local) if local.port != 0 => local.port,
            _ => stack.ephemeral(),
        };
        let sock = stack.sockets.get_mut::<tcp::Socket>(sock_handle);
        match sock.state() {
            tcp::State::Established | tcp::State::SynReceived => return Err(err::EISCONN),
            tcp::State::Closed => {}
            _ => return Err(err::EALREADY),
        }
        // The smoltcp connect refuses port 0, so there is no "let it pick" -
        // the ephemeral allocation is ours to do, as it is on Linux.
        let local = IpListenEndpoint { addr: None, port };
        let cx = stack.iface.context();
        match sock.connect(cx, endpoint, local) {
            Ok(()) => Ok(()),
            Err(tcp::ConnectError::InvalidState) => Err(err::EALREADY),
            Err(tcp::ConnectError::Unaddressable) => Err(err::EADDRNOTAVAIL),
        }
    });
    match started {
        Some(Ok(())) => {}
        Some(Err(e)) => return e,
        None => return err::EBADF,
    }
    if nonblock {
        // Started, not finished - the shape a non-blocking connect promises.
        return err::EINPROGRESS;
    }

    // The handshake loop. A reset closes the socket (state back to Closed);
    // thirty seconds of silence is what Linux answers ETIMEDOUT for.
    let deadline = crate::arch::timer::ticks() + 30 * crate::arch::timer::TICK_HZ as u64;
    loop {
        let step = super::with(handle, |stack, entry| {
            stack.poll_once();
            let Channel::Tcp {
                handle: sock_handle,
                ..
            } = &entry.channel
            else {
                return Err(err::ENOTSOCK);
            };
            let sock = stack.sockets.get_mut::<tcp::Socket>(*sock_handle);
            match sock.state() {
                tcp::State::Established | tcp::State::SynReceived => Ok(true),
                tcp::State::Closed => Err(err::ECONNREFUSED),
                _ => Ok(false),
            }
        });
        match step {
            Some(Ok(true)) => return 0,
            Some(Ok(false)) => {}
            Some(Err(e)) => return e,
            None => return err::EBADF,
        }
        if crate::sched::take_fatal_signal().is_some() {
            return err::EINTR;
        }
        if crate::arch::timer::ticks() >= deadline {
            return err::ETIMEDOUT;
        }
        crate::sched::sleep_ticks(1);
    }
}

// ---------------------------------------------------------------------------
// send family
// ---------------------------------------------------------------------------

/// `sendto(fd, buf, len, flags, addr, addrlen)` - and `send` with a null
/// address, which is the same call with no per-call destination.
pub(crate) fn handle_sendto(args: [usize; 6], from_user: bool) -> isize {
    let fd = args[0] as i32;
    let (buf, len) = (args[1], args[2]);
    // A route-netlink socket takes the payload whole: the netlink message
    // protocol lives in the data, and the destination sockaddr is the
    // kernel's own netlink address, not one to parse as an IP endpoint.
    if let Some(handle) = rtnl_handle_of_fd(fd) {
        if !crate::syscall::buffer_ok(buf, len, false, from_user) {
            return err::EFAULT;
        }
        let data = unsafe { slice::from_raw_parts(buf as *const u8, len) };
        return crate::net::rtnl::send(handle, data);
    }
    let handle = match socket_handle_of_fd(fd) {
        Ok(handle) => handle,
        Err(e) => return e,
    };
    if !crate::syscall::buffer_ok(buf, len, false, from_user) {
        return err::EFAULT;
    }
    let data = unsafe { slice::from_raw_parts(buf as *const u8, len) };
    let dst = if args[4] != 0 {
        match parse_sockaddr_in(args[4], args[5], from_user) {
            Ok(endpoint) => Some(endpoint),
            Err(e) => return e,
        }
    } else {
        None
    };
    let nonblock = fd_status(fd) & crate::vfs::file::O_NONBLOCK != 0;
    send_common(handle, data, dst, nonblock)
}

/// `sendmsg(fd, msghdr, flags)` - scatter-gather send. The iovecs are
/// collected into one buffer (the stack wants contiguous bytes anyway) and
/// the destination comes from `msg_name`, if there is one.
pub(crate) fn handle_sendmsg(args: [usize; 6], from_user: bool) -> isize {
    let fd = args[0] as i32;
    if let Some(handle) = rtnl_handle_of_fd(fd) {
        let (_name, _namelen, iov, iovlen) = match read_msghdr(args[1], from_user) {
            Ok(fields) => fields,
            Err(e) => return e,
        };
        let mut data: Vec<u8> = Vec::new();
        if let Err(e) = gather_iovecs(iov, iovlen, from_user, &mut data) {
            return e;
        }
        return crate::net::rtnl::send(handle, &data);
    }
    let handle = match socket_handle_of_fd(fd) {
        Ok(handle) => handle,
        Err(e) => return e,
    };
    let (name, namelen, iov, iovlen) = match read_msghdr(args[1], from_user) {
        Ok(fields) => fields,
        Err(e) => return e,
    };
    let mut data: Vec<u8> = Vec::new();
    if let Err(e) = gather_iovecs(iov, iovlen, from_user, &mut data) {
        return e;
    }
    let dst = if name != 0 && namelen != 0 {
        match parse_sockaddr_in(name, namelen as usize, from_user) {
            Ok(endpoint) => Some(endpoint),
            Err(e) => return e,
        }
    } else {
        None
    };
    let nonblock = fd_status(fd) & crate::vfs::file::O_NONBLOCK != 0;
    send_common(handle, &data, dst, nonblock)
}

enum SendStep {
    Sent(usize),
    Wait,
}

fn send_common(handle: usize, data: &[u8], dst: Option<IpEndpoint>, nonblock: bool) -> isize {
    let typ = super::with(handle, |_, entry| entry.typ);
    if typ.is_some() {
        let v6 = super::with(handle, |_, entry| matches!(entry.channel, Channel::Inet6));
        if v6.unwrap_or(false) {
            return err::ENETUNREACH;
        }
    }
    match typ {
        Some(SOCK_STREAM) => tcp_send(handle, data, nonblock),
        Some(SOCK_DGRAM) => udp_send(handle, data, dst),
        Some(SOCK_RAW) => raw_send(handle, data, dst),
        Some(_) => err::EINVAL,
        None => err::EBADF,
    }
}

/// Push bytes into a connected stream socket, blocking while the buffer is
/// full. An already-full buffer after a partial push is a completed partial
/// send: the caller asked for `len` and got fewer bytes back, which is a
/// legal answer for `send`.
fn tcp_send(handle: usize, data: &[u8], nonblock: bool) -> isize {
    let mut sent = 0usize;
    loop {
        let step = super::with(handle, |stack, entry| {
            stack.poll_once();
            let Channel::Tcp {
                handle: sock_handle,
                ..
            } = &entry.channel
            else {
                return Err(err::ENOTSOCK);
            };
            let sock = stack.sockets.get_mut::<tcp::Socket>(*sock_handle);
            if !sock.may_send() {
                // The connection is over - reset, timed out or shut down.
                // Writing into it reports the break, as EPIPE.
                return Err(err::EPIPE);
            }
            match sock.send_slice(&data[sent..]) {
                Ok(0) => Ok(SendStep::Wait),
                Ok(n) => Ok(SendStep::Sent(n)),
                Err(_) => Err(err::EPIPE),
            }
        });
        match step {
            Some(Ok(SendStep::Sent(n))) => {
                sent += n;
                if sent == data.len() {
                    return sent as isize;
                }
            }
            Some(Ok(SendStep::Wait)) => {
                if nonblock {
                    return if sent > 0 {
                        sent as isize
                    } else {
                        err::EWOULDBLOCK
                    };
                }
            }
            Some(Err(e)) => return e,
            None => return err::EBADF,
        }
        if crate::sched::take_fatal_signal().is_some() {
            return if sent > 0 { sent as isize } else { err::EINTR };
        }
        crate::sched::sleep_ticks(1);
    }
}

/// Send one datagram. A first send on an unbound socket binds an anonymous
/// port first, which is what Linux's automatic bind on send does.
fn udp_send(handle: usize, data: &[u8], dst: Option<IpEndpoint>) -> isize {
    let sent = super::with(handle, |stack, entry| {
        stack.poll_once();
        let Channel::Udp {
            handle: sock_handle,
            peer,
            bound,
            ..
        } = &mut entry.channel
        else {
            return Err(err::ENOTSOCK);
        };
        let Some(target) = dst.or(*peer) else {
            return Err(err::EDESTADDRREQ);
        };
        if !*bound {
            let port = stack.ephemeral();
            let sock = stack.sockets.get_mut::<udp::Socket>(*sock_handle);
            if sock.bind(IpListenEndpoint { addr: None, port }).is_err() {
                return Err(err::EADDRINUSE);
            }
            *bound = true;
        }
        let sock = stack.sockets.get_mut::<udp::Socket>(*sock_handle);
        match sock.send_slice(data, target) {
            Ok(()) => Ok(data.len() as isize),
            // A datagram bigger than the send pool is never going to fit.
            Err(udp::SendError::BufferFull) => Err(err::EMSGSIZE),
            Err(udp::SendError::Unaddressable) => Err(err::EINVAL),
        }
    });
    match sent {
        Some(Ok(n)) => n,
        Some(Err(e)) => e,
        None => err::EBADF,
    }
}

// ---------------------------------------------------------------------------
// recv family
// ---------------------------------------------------------------------------

/// `recvfrom(fd, buf, len, flags, src_addr, src_addrlen)` - and `recv`, with
/// no source slots.
pub(crate) fn handle_recvfrom(args: [usize; 6], from_user: bool) -> isize {
    let fd = args[0] as i32;
    let (buf, len) = (args[1], args[2]);
    if let Some(handle) = rtnl_handle_of_fd(fd) {
        if !crate::syscall::buffer_ok(buf, len, true, from_user) {
            return err::EFAULT;
        }
        // args[3] is the flags word: `MSG_DONTWAIT` here means the caller
        // wants an answer only if one is queued.
        let Some(msg) = rtnl_answer(handle, args[3], fd) else {
            return err::EAGAIN;
        };
        let n = msg.len().min(len);
        unsafe {
            core::ptr::copy_nonoverlapping(msg.as_ptr(), buf as *mut u8, n);
            if args[4] != 0 {
                // sockaddr_nl: the answer came from the kernel, pid 0.
                let at = |offset: usize| (args[4] + offset) as *mut u8;
                (at(0) as *mut u16).write_unaligned(crate::uevent::AF_NETLINK);
                (at(4) as *mut u32).write_unaligned(0); // nl_pid
                (at(8) as *mut u32).write_unaligned(0); // nl_groups
                if args[5] != 0 {
                    (args[5] as *mut u32).write_unaligned(12);
                }
            }
        }
        return n as isize;
    }
    let handle = match socket_handle_of_fd(fd) {
        Ok(handle) => handle,
        Err(e) => return e,
    };
    if !crate::syscall::buffer_ok(buf, len, true, from_user) {
        return err::EFAULT;
    }
    let out = unsafe { slice::from_raw_parts_mut(buf as *mut u8, len) };
    let src = if args[4] != 0 {
        Some((args[4], args[5]))
    } else {
        None
    };
    if let Err(e) = validate_source_slot(src) {
        return e;
    }
    let nonblock = fd_status(fd) & crate::vfs::file::O_NONBLOCK != 0;
    recv_common(handle, out, src, nonblock)
}

/// `recvmsg(fd, msghdr, flags)` - scatter-gather receive. The first non-empty
/// iovec takes the bytes (one contiguous read; the stack has the rest only
/// if a later call asks), the source goes into `msg_name`, and `msg_flags`
/// is cleared.
pub(crate) fn handle_recvmsg(args: [usize; 6], from_user: bool) -> isize {
    let fd = args[0] as i32;
    let msghdr = args[1];
    if let Some(handle) = rtnl_handle_of_fd(fd) {
        let (name, namelen, iov, iovlen) = match read_msghdr(msghdr, from_user) {
            Ok(fields) => fields,
            Err(e) => return e,
        };
        let Some((buf, len)) = first_iovec(iov, iovlen, from_user) else {
            return err::EINVAL;
        };
        if len == 0 {
            return err::EINVAL;
        }
        // args[2] is the flags word: `MSG_DONTWAIT` here means the caller
        // wants an answer only if one is queued.
        let Some(msg) = rtnl_answer(handle, args[2], fd) else {
            return err::EAGAIN;
        };
        let n = msg.len().min(len);
        unsafe {
            core::ptr::copy_nonoverlapping(msg.as_ptr(), buf as *mut u8, n);
            if name != 0 && namelen != 0 {
                let at = |offset: usize| (name + offset) as *mut u8;
                (at(0) as *mut u16).write_unaligned(crate::uevent::AF_NETLINK);
                (at(4) as *mut u32).write_unaligned(0); // nl_pid
                (at(8) as *mut u32).write_unaligned(0); // nl_groups
                ((msghdr + 8) as *mut u32).write_unaligned(12);
            }
        }
        return n as isize;
    }
    let handle = match socket_handle_of_fd(fd) {
        Ok(handle) => handle,
        Err(e) => return e,
    };
    let (name, namelen, iov, iovlen) = match read_msghdr(msghdr, from_user) {
        Ok(fields) => fields,
        Err(e) => return e,
    };
    let Some((buf, len)) = first_iovec(iov, iovlen, from_user) else {
        return err::EINVAL;
    };
    if len == 0 {
        return err::EINVAL;
    }
    // The source goes back into `msg_name`, its length into `msg_namelen`
    // at offset 8 - updated in place, as the socklen_t in-out it is.
    let src = if name != 0 && namelen != 0 {
        Some((name, msghdr + 8))
    } else {
        None
    };
    if let Err(e) = validate_source_slot(src) {
        return e;
    }
    if crate::uevent::exists(handle) {
        return recvmsg_netlink(msghdr, buf, len, name, namelen, handle);
    }
    let out = unsafe { slice::from_raw_parts_mut(buf as *mut u8, len) };
    let nonblock = fd_status(fd) & crate::vfs::file::O_NONBLOCK != 0;
    let n = match recv_common(handle, out, src, nonblock) {
        n if n >= 0 => n as usize,
        e => return e,
    };
    // A received message carries no control data or exceptional flags here.
    if !crate::syscall::buffer_ok(msghdr, MSGHDR_SIZE, true, from_user) {
        return err::EFAULT;
    }
    unsafe {
        ((msghdr + MSGHDR_FLAGS) as *mut i32).write_volatile(0);
    }
    n as isize
}

enum RecvStep {
    Got(usize, Option<IpEndpoint>),
    Wait,
    Err(isize),
}

fn recv_common(
    handle: usize,
    out: &mut [u8],
    src: Option<(usize, usize)>,
    nonblock: bool,
) -> isize {
    let typ = super::with(handle, |_, entry| entry.typ);
    if typ.is_some() {
        let v6 = super::with(handle, |_, entry| matches!(entry.channel, Channel::Inet6));
        if v6.unwrap_or(false) {
            return err::ENETUNREACH;
        }
    }
    let received = match typ {
        Some(SOCK_STREAM) => tcp_recv(handle, out, nonblock),
        Some(SOCK_DGRAM) => udp_recv(handle, out, nonblock),
        Some(SOCK_RAW) => raw_recv(handle, out, nonblock),
        Some(_) => return err::EINVAL,
        None => return err::EBADF,
    };
    let (n, endpoint) = match received {
        Ok(answer) => answer,
        Err(e) => return e,
    };
    if let (Some((addr_ptr, len_ptr)), Some(endpoint)) = (src, endpoint)
        && write_sockaddr_in(addr_ptr, len_ptr, endpoint).is_err()
    {
        return err::EFAULT;
    }
    n as isize
}

/// Pull bytes from a stream socket. End of stream is `Ok(0)` - a FIN read to
/// the end, or a socket that was never really up - and "nothing yet" is the
/// wait-and-retry case, exactly as a pipe read behaves. smoltcp spells the
/// two zero cases differently: an empty buffer is `Ok(0)`, a drained FIN is
/// `Err(Finished)`.
fn tcp_recv(
    handle: usize,
    out: &mut [u8],
    nonblock: bool,
) -> Result<(usize, Option<IpEndpoint>), isize> {
    loop {
        let step = super::with(handle, |stack, entry| {
            stack.poll_once();
            let Channel::Tcp {
                handle: sock_handle,
                ..
            } = &entry.channel
            else {
                return RecvStep::Err(err::ENOTSOCK);
            };
            let sock = stack.sockets.get_mut::<tcp::Socket>(*sock_handle);
            match sock.recv_slice(out) {
                Ok(0) => RecvStep::Wait,
                Ok(n) => RecvStep::Got(n, None),
                Err(tcp::RecvError::Finished) => RecvStep::Got(0, None),
                // Not connected, or reset: reads report the break.
                Err(tcp::RecvError::InvalidState) => RecvStep::Err(err::ENOTCONN),
            }
        });
        match step {
            Some(RecvStep::Got(n, ep)) => return Ok((n, ep)),
            Some(RecvStep::Err(e)) => return Err(e),
            Some(RecvStep::Wait) | None => {}
        }
        if nonblock {
            return Err(err::EWOULDBLOCK);
        }
        if crate::sched::take_fatal_signal().is_some() {
            return Err(err::EINTR);
        }
        crate::sched::sleep_ticks(1);
    }
}

/// Take one datagram. A first receive on an unbound socket binds an anonymous
/// port, so a reply can find its way back.
fn udp_recv(
    handle: usize,
    out: &mut [u8],
    nonblock: bool,
) -> Result<(usize, Option<IpEndpoint>), isize> {
    loop {
        let step = super::with(handle, |stack, entry| {
            stack.poll_once();
            let Channel::Udp {
                handle: sock_handle,
                bound,
                ..
            } = &mut entry.channel
            else {
                return RecvStep::Err(err::ENOTSOCK);
            };
            if !*bound {
                let port = stack.ephemeral();
                let sock = stack.sockets.get_mut::<udp::Socket>(*sock_handle);
                if sock.bind(IpListenEndpoint { addr: None, port }).is_err() {
                    return RecvStep::Err(err::EADDRINUSE);
                }
                *bound = true;
            }
            let sock = stack.sockets.get_mut::<udp::Socket>(*sock_handle);
            match sock.recv_slice(out) {
                Ok((n, meta)) => RecvStep::Got(n, Some(meta.endpoint)),
                Err(udp::RecvError::Exhausted) => RecvStep::Wait,
                // The datagram did not fit and smoltcp dropped it: there is
                // no partial answer to give, so the caller hears that the
                // packet was too big rather than reading nothing forever.
                Err(udp::RecvError::Truncated) => RecvStep::Err(err::EMSGSIZE),
            }
        });
        match step {
            Some(RecvStep::Got(n, ep)) => return Ok((n, ep)),
            Some(RecvStep::Err(e)) => return Err(e),
            Some(RecvStep::Wait) | None => {}
        }
        if nonblock {
            return Err(err::EWOULDBLOCK);
        }
        if crate::sched::take_fatal_signal().is_some() {
            return Err(err::EINTR);
        }
        crate::sched::sleep_ticks(1);
    }
}

// ---------------------------------------------------------------------------
// raw IP sockets
// ---------------------------------------------------------------------------

/// Send a raw IP packet. The caller provides the ICMP payload; we prepend
/// the 20-byte IPv4 header and hand the complete packet to smoltcp's raw
/// socket. A destination address is mandatory for raw sockets (there is no
/// `connect` to pin one).
fn raw_send(handle: usize, data: &[u8], dst: Option<IpEndpoint>) -> isize {
    let target = match dst {
        Some(ep) => ep,
        None => return err::EDESTADDRREQ,
    };
    let sent = super::with(handle, |stack, entry| {
        stack.poll_once();
        let Channel::Raw {
            handle: sock_handle,
            protocol,
        } = &entry.channel
        else {
            return Err(err::ENOTSOCK);
        };
        let sock = stack.sockets.get_mut::<raw_sock::Socket>(*sock_handle);
        // Build a complete IPv4 packet: 20-byte header + ICMP payload.
        let total_len = 20 + data.len();
        if total_len > 65535 {
            return Err(err::EMSGSIZE);
        }
        let mut pkt = alloc::vec![0u8; total_len];
        // IPv4 header (no options, IHL=5).
        pkt[0] = 0x45; // version=4, IHL=5
        pkt[1] = 0; // DSCP/ECN
        let len = (total_len as u16).to_be_bytes();
        pkt[2] = len[0];
        pkt[3] = len[1];
        pkt[4] = 0;
        pkt[5] = 0; // identification
        pkt[6] = 0x40; // flags: Don't Fragment
        pkt[7] = 0; // fragment offset
        pkt[8] = 64; // TTL
        pkt[9] = *protocol; // protocol
        // checksum = 0 (smoltcp fills it in)
        pkt[10] = 0;
        pkt[11] = 0;
        // source: local IP
        pkt[12] = super::LOCAL_IP[0];
        pkt[13] = super::LOCAL_IP[1];
        pkt[14] = super::LOCAL_IP[2];
        pkt[15] = super::LOCAL_IP[3];
        // destination
        let octets = match target.addr {
            IpAddress::Ipv4(addr) => addr.octets(),
        };
        pkt[16] = octets[0];
        pkt[17] = octets[1];
        pkt[18] = octets[2];
        pkt[19] = octets[3];
        // payload
        pkt[20..].copy_from_slice(data);
        match sock.send_slice(&pkt) {
            Ok(()) => Ok(data.len() as isize),
            Err(raw_sock::SendError::BufferFull) => Err(err::ENOMEM),
        }
    });
    match sent {
        Some(Ok(n)) => n,
        Some(Err(e)) => e,
        None => err::EBADF,
    }
}

/// Receive a raw IP packet. The smoltcp raw socket delivers complete IP
/// packets; we strip the 20-byte header and return only the ICMP payload
/// to the caller - matching what Linux does for `SOCK_RAW, IPPROTO_ICMP`.
fn raw_recv(
    handle: usize,
    out: &mut [u8],
    nonblock: bool,
) -> Result<(usize, Option<IpEndpoint>), isize> {
    loop {
        // Receive into a temporary buffer inside the `with` closure, then
        // copy the ICMP payload to the caller's buffer.
        let result: Option<Result<(usize, Option<IpEndpoint>), isize>> =
            super::with(handle, |stack, entry| {
                stack.poll_once();
                let Channel::Raw {
                    handle: sock_handle,
                    ..
                } = &entry.channel
                else {
                    return Err(err::ENOTSOCK);
                };
                let sock = stack.sockets.get_mut::<raw_sock::Socket>(*sock_handle);
                let mut tmp = alloc::vec![0u8; 1500];
                let n = match sock.recv_slice(&mut tmp) {
                    Ok(n) => n,
                    Err(raw_sock::RecvError::Exhausted) => return Err(err::EWOULDBLOCK),
                    Err(raw_sock::RecvError::Truncated) => return Err(err::EMSGSIZE),
                };
                tmp.truncate(n);
                // Strip the IPv4 header: the minimum IHL is 5 (20 bytes).
                if n < 20 {
                    return Err(err::EINVAL);
                }
                let ihl = ((tmp[0] & 0x0F) as usize) * 4;
                if ihl > n {
                    return Err(err::EINVAL);
                }
                let payload = &tmp[ihl..];
                if payload.len() > out.len() {
                    return Err(err::EMSGSIZE);
                }
                // Build the source address from the IP header.
                let src = IpEndpoint::new(IpAddress::v4(tmp[12], tmp[13], tmp[14], tmp[15]), 0);
                // Copy payload to caller's buffer.
                out[..payload.len()].copy_from_slice(payload);
                Ok((payload.len(), Some(src)))
            });
        match result {
            Some(Ok((n, ep))) => return Ok((n, ep)),
            Some(Err(e)) if e == err::EWOULDBLOCK && !nonblock => {}
            Some(Err(e)) => return Err(e),
            None => return Err(err::EBADF),
        }
        if nonblock {
            return Err(err::EWOULDBLOCK);
        }
        if crate::sched::take_fatal_signal().is_some() {
            return Err(err::EINTR);
        }
        crate::sched::sleep_ticks(1);
    }
}

// ---------------------------------------------------------------------------
// bind, listen, names, options
// ---------------------------------------------------------------------------

/// `bind(fd, addr, addrlen)`.
///
/// For a stream socket this only records the endpoint - there is nothing to
/// bind before a connection exists, and `connect` takes the record as its
/// source. A datagram socket binds for real: the receive queue starts
/// holding whatever arrives for the port.
pub(crate) fn handle_bind(args: [usize; 6], from_user: bool) -> isize {
    if let Ok(handle) = socket_handle_of_fd(args[0] as i32)
        && crate::uevent::exists(handle)
    {
        // `struct sockaddr_nl`: family, pad, pid, groups. Only the group mask
        // matters - `nlplug-findfs` binds with all groups set, which is how a
        // listener says "tell me about every device event".
        const SOCKADDR_NL_SIZE: usize = 12;
        let addr = args[1];
        if !crate::syscall::buffer_ok(addr, SOCKADDR_NL_SIZE, false, from_user) {
            return err::EFAULT;
        }
        let family = unsafe { (addr as *const u16).read_unaligned() };
        if family != crate::uevent::AF_NETLINK {
            return err::EINVAL;
        }
        let groups = unsafe { ((addr + 8) as *const u32).read_unaligned() };
        crate::uevent::bind(handle, groups);
        return 0;
    }
    if let Ok(handle) = socket_handle_of_fd(args[0] as i32)
        && crate::net::rtnl::exists(handle)
    {
        // A route session binds to name its port and join groups. The port id
        // matters beyond ceremony: every dump answer this session reads back
        // carries it in `nlmsg_pid`, and callers that filter on that field -
        // fastfetch's default-route probe - drop everything else as foreign
        // mail. An explicit nonzero pid is honored, zero asks for one.
        const SOCKADDR_NL_SIZE: usize = 12;
        let addr = args[1];
        if args[2] < SOCKADDR_NL_SIZE
            || !crate::syscall::buffer_ok(addr, SOCKADDR_NL_SIZE, false, from_user)
        {
            return err::EINVAL;
        }
        let family = unsafe { (addr as *const u16).read_unaligned() };
        if family != crate::uevent::AF_NETLINK {
            return err::EINVAL;
        }
        let pid = unsafe { ((addr + 4) as *const u32).read_unaligned() };
        return crate::net::rtnl::bind(handle, pid);
    }

    let fd = args[0] as i32;
    let handle = match socket_handle_of_fd(fd) {
        Ok(handle) => handle,
        Err(e) => return e,
    };
    let endpoint = match parse_sockaddr_in(args[1], args[2], from_user) {
        Ok(endpoint) => endpoint,
        Err(e) => return e,
    };
    let bound = super::with(handle, |stack, entry| {
        match &mut entry.channel {
            Channel::Tcp { local, .. } => {
                if local.is_some() {
                    return Err(err::EINVAL);
                }
                // A port of 0 means "pick one", and it is picked here rather
                // than at connect: `getsockname` has to be able to answer
                // before the caller connects to anything, which is the whole
                // point of binding first. The stack is told at `connect`,
                // which is where smoltcp wants the local endpoint - smoltcp's
                // own `connect` refuses port 0 and takes the source port as an
                // argument.
                let port = if endpoint.port == 0 {
                    stack.ephemeral()
                } else {
                    endpoint.port
                };
                // One name, one socket. Nothing underneath tracks the
                // ownership - smoltcp binds each socket on its own and the
                // interface only picks between them when it polls - so the
                // table that sees every socket is where the check has to live.
                // This entry is lifted out of its slot while the closure runs,
                // so it cannot match itself.
                let taken = stack.entries.iter().any(|slot| {
                    matches!(slot, Some(super::Entry {
                        channel: Channel::Tcp { local: Some(l), .. },
                        ..
                    }) if l.port == port)
                });
                if taken {
                    return Err(err::EADDRINUSE);
                }
                *local = Some(IpEndpoint {
                    addr: endpoint.addr,
                    port,
                });
                Ok(())
            }
            // The stub takes no addresses: its family has none to name.
            Channel::Inet6 => Err(err::EAFNOSUPPORT),
            // Raw sockets do not bind: `ping` uses `sendto` with a
            // per-call destination.
            Channel::Raw { .. } => Ok(()),
            Channel::Udp {
                handle: sock_handle,
                bound,
                local,
                ..
            } => {
                if *bound {
                    return Err(err::EINVAL);
                }
                let addr = match endpoint.addr {
                    IpAddress::Ipv4(v4) if v4.is_unspecified() => None,
                    other => Some(other),
                };
                let port = if endpoint.port == 0 {
                    stack.ephemeral()
                } else {
                    endpoint.port
                };
                let sock = stack.sockets.get_mut::<udp::Socket>(*sock_handle);
                match sock.bind(IpListenEndpoint { addr, port }) {
                    Ok(()) => {
                        *bound = true;
                        // Remember what was actually bound, port and all, so
                        // `getsockname` can answer without asking the socket.
                        *local = Some(IpEndpoint {
                            addr: endpoint.addr,
                            port,
                        });
                        Ok(())
                    }
                    Err(_) => Err(err::EADDRINUSE),
                }
            }
        }
    });
    match bound {
        Some(Ok(())) => 0,
        Some(Err(e)) => e,
        None => err::EBADF,
    }
}

/// `listen(fd, backlog)` - accepting connections is not on the menu yet, and
/// a server this kernel cannot run gets a straight answer rather than a
/// silent failure.
pub(crate) fn handle_listen(_args: [usize; 6], _from_user: bool) -> isize {
    err::EOPNOTSUPP
}

/// `accept(fd, addr, addrlen)` - see [`handle_listen`].
pub(crate) fn handle_accept(_args: [usize; 6], _from_user: bool) -> isize {
    err::EOPNOTSUPP
}

/// `socketpair(domain, type, protocol, fds)`.
///
/// One shape is real: `AF_UNIX`/`SOCK_STREAM`, a connected, bidirectional
/// byte stream handed back as two descriptors. It is how a program gives a
/// helper process a private channel - busybox's `wget` builds one to talk to
/// its `ssl_client` child, which is the whole of its HTTPS support - so a
/// kernel that answers `EOPNOTSUPP` here cannot do HTTPS at all, however
/// well the TLS library on top of it works.
///
/// There is no Unix-domain *address* here, and no filesystem node: the two
/// ends are anonymous and exist only as descriptors, which is the only kind
/// `socketpair(2)` ever makes.
pub(crate) fn handle_socketpair(args: [usize; 6], from_user: bool) -> isize {
    let (domain, raw_type, protocol, out) = (args[0] as u16, args[1] as u32, args[2], args[3]);
    match domain {
        AF_UNIX => {}
        // A family this kernel speaks, but which has no pair operation: Linux
        // creates the socket, finds no `->socketpair`, and answers
        // `EOPNOTSUPP`. `EAFNOSUPPORT` would say the family is unknown, and a
        // caller that distinguishes the two would draw the wrong conclusion.
        AF_INET | AF_INET6 => return err::EOPNOTSUPP,
        _ => return err::EAFNOSUPPORT,
    }
    let type_bits = raw_type & !(SOCK_NONBLOCK | SOCK_CLOEXEC);
    match type_bits {
        t if t == SOCK_STREAM as u32 => {}
        // Pairs that carry message boundaries rather than a byte stream: real
        // shapes on Linux, and shapes this kernel does not have. Answering
        // plainly beats answering wrongly - a caller that gets a pair here
        // would go on to rely on boundaries that are not there.
        t if t == SOCK_DGRAM as u32 || t == SOCK_SEQPACKET as u32 => {
            return err::EOPNOTSUPP;
        }
        _ => return err::ESOCKTNOSUPPORT,
    }
    if protocol != 0 {
        return err::EPROTONOSUPPORT;
    }
    if out == 0 || !crate::syscall::buffer_ok(out, 2 * size_of::<i32>(), true, from_user) {
        return err::EFAULT;
    }

    let (first, second) = crate::vfs::pipe::DuplexEnd::pair();
    let fds = [
        crate::vfs::file::open_inode(first),
        crate::vfs::file::open_inode(second),
    ];
    let table = crate::sched::current_fds();
    for fd in fds {
        if raw_type & SOCK_CLOEXEC != 0 {
            let _ = table.fd_flags_set(fd, crate::vfs::file::FD_CLOEXEC);
        }
        if raw_type & SOCK_NONBLOCK != 0
            && let Ok(file) = table.clone_at(fd)
        {
            file.set_status(crate::vfs::file::O_NONBLOCK);
        }
    }
    unsafe {
        let slot = out as *mut i32;
        slot.write(fds[0]);
        slot.add(1).write(fds[1]);
    }
    0
}

/// `getsockname(fd, addr, addrlen)`.
pub(crate) fn handle_getsockname(args: [usize; 6], _from_user: bool) -> isize {
    let fd = args[0] as i32;
    let (addr_ptr, len_ptr) = (args[1], args[2]);
    // A route-netlink session names itself with a `sockaddr_nl`, not a
    // `sockaddr_in`. The port id `bind` assigned is the one thing a caller
    // needs from here: fastfetch's default-route probe reads it back and
    // then filters every dump answer by `nlmsg_pid == portid`, so answering
    // this wrong (or failing it - the fallback callers adopt is their own
    // process id) strands the exchange.
    if let Some(handle) = rtnl_handle_of_fd(fd) {
        const SOCKADDR_NL_SIZE: usize = 12;
        if addr_ptr == 0
            || len_ptr == 0
            || !crate::syscall::buffer_ok(addr_ptr, SOCKADDR_NL_SIZE, true, true)
            || !crate::syscall::buffer_ok(len_ptr, 4, true, true)
        {
            return err::EFAULT;
        }
        let pid = crate::net::rtnl::portid(handle).unwrap_or(0);
        unsafe {
            let at = |offset: usize| (addr_ptr + offset) as *mut u8;
            (at(0) as *mut u16).write_unaligned(crate::uevent::AF_NETLINK);
            (at(2) as *mut u16).write_unaligned(0); // nl_pad
            (at(4) as *mut u32).write_unaligned(pid); // nl_pid
            (at(8) as *mut u32).write_unaligned(0); // nl_groups
            (len_ptr as *mut u32).write_unaligned(SOCKADDR_NL_SIZE as u32);
        }
        return 0;
    }
    let handle = match socket_handle_of_fd(fd) {
        Ok(handle) => handle,
        Err(e) => return e,
    };
    if addr_ptr == 0 || len_ptr == 0 {
        return err::EFAULT;
    }
    if !crate::syscall::buffer_ok(addr_ptr, SOCKADDR_IN_LEN, true, true)
        || !crate::syscall::buffer_ok(len_ptr, 4, true, true)
    {
        return err::EFAULT;
    }
    // Nothing bound and nothing connected reads back as 0.0.0.0:0, which is
    // what Linux answers for a fresh socket as well.
    let endpoint = super::with(handle, |stack, entry| {
        stack.poll_once();
        match &entry.channel {
            Channel::Tcp {
                handle: sock_handle,
                local,
            } => match local {
                // The name `bind` settled - the one the caller is asking
                // about, and the one `connect` will send from.
                Some(local) => *local,
                // Not bound, but possibly connected: `connect` picks the
                // source port and hands it to the stack, which knows it even
                // though nothing here recorded it.
                None => stack
                    .sockets
                    .get_mut::<tcp::Socket>(*sock_handle)
                    .local_endpoint()
                    .unwrap_or(UNSPECIFIED_ENDPOINT),
            },
            Channel::Udp { local, .. } => local.unwrap_or(UNSPECIFIED_ENDPOINT),
            // A raw socket has no bound address; reads back as unspecified.
            Channel::Raw { .. } => UNSPECIFIED_ENDPOINT,
            // A stub that cannot connect reads back as unspecified, the same
            // as a fresh IPv4 socket does.
            Channel::Inet6 => UNSPECIFIED_ENDPOINT,
        }
    });
    match endpoint {
        Some(endpoint) => match write_sockaddr_in(addr_ptr, len_ptr, endpoint) {
            Ok(()) => 0,
            Err(e) => e,
        },
        None => err::EBADF,
    }
}

/// `getpeername(fd, addr, addrlen)`.
pub(crate) fn handle_getpeername(args: [usize; 6], _from_user: bool) -> isize {
    let fd = args[0] as i32;
    let (addr_ptr, len_ptr) = (args[1], args[2]);
    let handle = match socket_handle_of_fd(fd) {
        Ok(handle) => handle,
        Err(e) => return e,
    };
    if addr_ptr == 0 || len_ptr == 0 {
        return err::EFAULT;
    }
    if !crate::syscall::buffer_ok(addr_ptr, SOCKADDR_IN_LEN, true, true)
        || !crate::syscall::buffer_ok(len_ptr, 4, true, true)
    {
        return err::EFAULT;
    }
    let endpoint = super::with(handle, |stack, entry| {
        stack.poll_once();
        match &entry.channel {
            Channel::Tcp {
                handle: sock_handle,
                ..
            } => {
                let sock = stack.sockets.get_mut::<tcp::Socket>(*sock_handle);
                if !sock.is_active() {
                    return Err(err::ENOTCONN);
                }
                match sock.remote_endpoint() {
                    Some(endpoint) => Ok(endpoint),
                    None => Err(err::ENOTCONN),
                }
            }
            Channel::Udp { peer, .. } => match peer {
                Some(endpoint) => Ok(*endpoint),
                None => Err(err::ENOTCONN),
            },
            // Nowhere to be connected to, so no peer to name.
            Channel::Inet6 => Err(err::ENOTCONN),
            // Raw sockets are connectionless; no peer.
            Channel::Raw { .. } => Err(err::ENOTCONN),
        }
    });
    match endpoint {
        Some(Ok(endpoint)) => match write_sockaddr_in(addr_ptr, len_ptr, endpoint) {
            Ok(()) => 0,
            Err(e) => e,
        },
        Some(Err(e)) => e,
        None => err::EBADF,
    }
}

/// `shutdown(fd, how)` - a half-close for streams, a no-op for datagrams.
pub(crate) fn handle_shutdown(args: [usize; 6], _from_user: bool) -> isize {
    const SHUT_RD: usize = 0;
    const SHUT_WR: usize = 1;
    const SHUT_RDWR: usize = 2;

    let how = args[1];
    if how > SHUT_RDWR {
        return err::EINVAL;
    }
    // A `socketpair` end is not in the network stack - it is a pair of buffers
    // with no address and no connection - so its half-close is done on the
    // node itself. Everything else is a network socket, or is not one at all.
    if let Ok(file) = crate::sched::current_fds().clone_at(args[0] as i32)
        && let Some(end) = file.inode().as_duplex()
    {
        if how == SHUT_RD || how == SHUT_RDWR {
            end.shut_read();
        }
        if how == SHUT_WR || how == SHUT_RDWR {
            end.shut_write();
        }
        return 0;
    }

    let handle = match socket_handle_of_fd(args[0] as i32) {
        Ok(handle) => handle,
        Err(e) => return e,
    };
    let done = super::with(handle, |stack, entry| match &entry.channel {
        Channel::Tcp {
            handle: sock_handle,
            ..
        } => {
            stack.sockets.get_mut::<tcp::Socket>(*sock_handle).close();
        }
        Channel::Udp { .. } => {}
        // Raw sockets are connectionless; nothing to tear down.
        Channel::Raw { .. } => {}
        // Nothing is connected, so there is nothing to tear down.
        Channel::Inet6 => {}
    });
    match done {
        Some(()) => 0,
        None => err::EBADF,
    }
}

/// `setsockopt(fd, level, optname, optval, optlen)`.
///
/// Options are accepted and forgotten. Nothing musl or apk sets on these
/// sockets changes what the stack does with them - timeouts live in user
/// code (`poll`), and `SO_REUSEADDR` is moot with one process binding.
pub(crate) fn handle_setsockopt(_args: [usize; 6], _from_user: bool) -> isize {
    0
}

/// `getsockopt(fd, level, optname, optval, optlen)`.
///
/// `SO_TYPE` answers what `socket()` was told; `SO_ERROR` answers zero,
/// because every error this stack has is reported by the call that hit it.
/// Anything else reads back zero - a probe that wanted a real answer gets
/// the neutral one, and a caller checking whether an option *can* be set is
/// not something musl does on these paths.
pub(crate) fn handle_getsockopt(args: [usize; 6], from_user: bool) -> isize {
    let (fd, level, optname, optval, optlen_ptr) =
        (args[0] as i32, args[1], args[2], args[3], args[4]);
    let handle = match socket_handle_of_fd(fd) {
        Ok(handle) => handle,
        Err(e) => return e,
    };
    if optval == 0 || optlen_ptr == 0 {
        return err::EFAULT;
    }
    if !crate::syscall::buffer_ok(optval, 4, true, from_user)
        || !crate::syscall::buffer_ok(optlen_ptr, 4, true, from_user)
    {
        return err::EFAULT;
    }
    let value = match (level, optname) {
        (SOL_SOCKET, SO_TYPE) => super::with(handle, |_, entry| entry.typ as i32).unwrap_or(0),
        // Errors are reported by the calls that hit them, so a query reads
        // back "no error pending" - which for this stack is always true.
        (SOL_SOCKET, SO_ERROR) => 0,
        _ => 0,
    };
    unsafe {
        (optval as *mut i32).write_volatile(value);
        (optlen_ptr as *mut u32).write_volatile(4);
    }
    0
}

// ---------------------------------------------------------------------------
// msghdr plumbing
// ---------------------------------------------------------------------------

/// Read the four fields of a `struct msghdr` that matter here: destination
/// name and length, and the iovec array and its length.
fn read_msghdr(ptr: usize, from_user: bool) -> Result<(usize, u32, usize, usize), isize> {
    if ptr == 0 {
        return Err(err::EFAULT);
    }
    if !crate::syscall::buffer_ok(ptr, MSGHDR_SIZE, false, from_user) {
        return Err(err::EFAULT);
    }
    unsafe {
        let name = (ptr as *const usize).read_volatile();
        let namelen = ((ptr + 8) as *const u32).read_volatile();
        let iov = ((ptr + 16) as *const usize).read_volatile();
        let iovlen = ((ptr + 24) as *const usize).read_volatile();
        Ok((name, namelen, iov, iovlen))
    }
}

/// Collect `iovlen` iovecs into `data`, validating each against the user
/// address space as it goes. The count is capped where Linux caps it.
fn gather_iovecs(
    iov: usize,
    iovlen: usize,
    from_user: bool,
    data: &mut Vec<u8>,
) -> Result<(), isize> {
    if iovlen > 1024 {
        return Err(err::EINVAL);
    }
    if iovlen == 0 {
        return Ok(());
    }
    if iov == 0 || !crate::syscall::buffer_ok(iov, iovlen * 16, false, from_user) {
        return Err(err::EFAULT);
    }
    for i in 0..iovlen {
        let base = iov + i * 16;
        let (ptr, len) = unsafe {
            (
                (base as *const usize).read_volatile(),
                ((base + 8) as *const usize).read_volatile(),
            )
        };
        if len == 0 {
            continue;
        }
        if data.len() + len > crate::syscall::MAX_IO {
            return Err(err::EINVAL);
        }
        if !crate::syscall::buffer_ok(ptr, len, false, from_user) {
            return Err(err::EFAULT);
        }
        data.extend_from_slice(unsafe { slice::from_raw_parts(ptr as *const u8, len) });
    }
    Ok(())
}

/// The first non-empty iovec, as a raw (pointer, length) pair.
fn first_iovec(iov: usize, iovlen: usize, from_user: bool) -> Option<(usize, usize)> {
    if iovlen == 0 || iov == 0 || !crate::syscall::buffer_ok(iov, iovlen * 16, false, from_user) {
        return None;
    }
    for i in 0..iovlen {
        let base = iov + i * 16;
        let (ptr, len) = unsafe {
            (
                (base as *const usize).read_volatile(),
                ((base + 8) as *const usize).read_volatile(),
            )
        };
        if len > 0 {
            return Some((ptr, len));
        }
    }
    None
}
