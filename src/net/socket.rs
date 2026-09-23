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
// The option names `setsockopt`/`getsockopt` exchange, in the x86_64 numbering.
const SO_REUSEADDR: usize = 2;
const SO_SNDBUF: usize = 7;
const SO_RCVBUF: usize = 8;
const SO_KEEPALIVE: usize = 9;
const SO_LINGER: usize = 13;
const SOL_TCP: usize = 6;
const TCP_NODELAY: usize = 1;

/// The keep-alive probe interval `SO_KEEPALIVE` arms smoltcp with when it is
/// turned on. Linux's default `TCP_KEEPINTVL` - the pace the socket holds to
/// once its idle timer has already fired - so a peer that vanishes is noticed
/// within a couple of these intervals rather than never.
const KEEPALIVE_INTERVAL_SECS: u64 = 75;

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
/// `recv(2)`'s "look without taking" flag.
const MSG_PEEK: usize = 0x02;

/// The `AF_UNIX` stream pair's end behind `fd`, if it is one. `socketpair`
/// mints anonymous duplex pipes - a channel, not a protocol - and the message
/// calls on such a descriptor are its read and write wearing socket clothes:
/// no address to name, no control data to carry. The routing matters, because
/// a channel end is not an IP socket: answering every `send` on one with
/// `ENOTSOCK` is what broke sudo's backchannel between parent and monitor
/// process, which is a socketpair and nothing else.
fn channel_of_fd(fd: i32) -> Option<Arc<crate::vfs::File>> {
    let file = crate::sched::current_fds().clone_at(fd).ok()?;
    // A socketpair end *is* the channel; a connected unix socket *has*
    // one, and answers the message calls through it identically. An
    // unconnected unix fd falls through: `socket_handle` is zero for it,
    // so the calls below reach their own ENOTSOCK-style answer - the
    // `read`/`write` doors are where ENOTCONN speaks first.
    if file.inode().as_duplex().is_none() && file.inode().unix_end().is_none() {
        return None;
    }
    Some(file)
}

/// Whether a call on `fd` must answer without parking: `MSG_DONTWAIT` on the
/// call, or `O_NONBLOCK` on the descriptor.
fn dontwait(fd: i32, flags: usize) -> bool {
    flags & MSG_DONTWAIT != 0 || fd_status(fd) & crate::vfs::file::O_NONBLOCK != 0
}

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
    let dontwait = flags & MSG_DONTWAIT != 0 || fd_status(fd) & crate::vfs::file::O_NONBLOCK != 0;
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
    /// A socket has no position: `lseek(2)` on one is `ESPIPE`.
    fn seekable(&self) -> bool {
        false
    }

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
    /// A socket has no position: `lseek(2)` on one is `ESPIPE`.
    fn seekable(&self) -> bool {
        false
    }

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
                let fd = match crate::vfs::open_inode(Arc::new(NlRouteInode { handle })) {
                    Ok(fd) => fd,
                    Err(e) => return crate::syscall::err_of(e),
                };
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
        let fd = match crate::vfs::open_inode(Arc::new(SocketInode { handle })) {
            Ok(fd) => fd,
            Err(e) => return crate::syscall::err_of(e),
        };
        if (args[1] as u32) & SOCK_NONBLOCK != 0
            && let Ok(file) = crate::sched::current_fds().clone_at(fd)
        {
            file.set_status(crate::vfs::file::O_NONBLOCK);
        }
        return fd as isize;
    }

    // AF_UNIX: the path family, answered by `src/net/unix.rs`. The fd is a
    // `UnixSock` inode - not a network registry entry - because unix sockets
    // must work on a machine with no network card at all, the same reason
    // netlink took its own path through this function.
    if domain == AF_UNIX {
        let raw_type = args[1] as u32;
        let cloexec = raw_type & SOCK_CLOEXEC != 0;
        let nonblock = raw_type & SOCK_NONBLOCK != 0;
        let typ = raw_type & !(SOCK_NONBLOCK | SOCK_CLOEXEC);
        if args[2] != 0 {
            return err::EPROTONOSUPPORT;
        }
        if typ != SOCK_STREAM as u32 {
            // DGRAM and SEQPACKET over unix answer the refusal socketpair
            // already makes for them; STREAM serves every target daemon.
            return err::EOPNOTSUPP;
        }
        let sock = crate::net::unix::alloc(typ as u16);
        let fd = match crate::vfs::open_inode(sock) {
            Ok(fd) => fd,
            Err(e) => return crate::syscall::err_of(e),
        };
        if nonblock && let Ok(file) = crate::sched::current_fds().clone_at(fd) {
            file.set_status(crate::vfs::file::O_NONBLOCK);
        }
        if cloexec {
            let _ = crate::sched::current_fds().fd_flags_set(fd, crate::vfs::file::FD_CLOEXEC);
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
    let fd = match crate::vfs::open_inode(Arc::new(SocketInode { handle })) {
        Ok(fd) => fd,
        Err(e) => return crate::syscall::err_of(e),
    };
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
    // The last reference to a unix socket retires its slot (and its name's
    // table key) the same way a network socket's frees its registry entry.
    let unix_id = file.inode().unix_id();
    if unix_id != 0 {
        if Arc::strong_count(file) == 1 {
            super::unix::release(unix_id);
        }
        return;
    }
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
        // Unix sockets retire by their own slot number, on the same
        // last-reference rule: the batch holds all the references when it
        // closes. The id space is disjoint from the network registry's -
        // unix ids carry their own sequence - so the dedup list below is
        // kept honest by tagging each closed id by kind.
        let unix_id = file.inode().unix_id();
        if unix_id != 0 {
            // The two id spaces share this dedup list, so unix ids carry
            // a tag bit the network handles cannot reach (they are small
            // slot indexes into the stack's Vec).
            const UNIX_TAG: usize = 1 << 40;
            if closed.contains(&(unix_id | UNIX_TAG)) {
                continue;
            }
            let ptr = Arc::as_ptr(file) as *const ();
            let total = Arc::strong_count(file);
            let in_batch = files
                .iter()
                .filter(|other| Arc::as_ptr(other) as *const () == ptr)
                .count();
            if total == in_batch {
                closed.push(unix_id | UNIX_TAG);
                super::unix::release(unix_id);
            }
            continue;
        }
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
    // A unix socket's connect is a path: resolve, hand the listener's
    // queue the handshake. The whole act is `unix::connect_path`.
    if let Ok(file) = crate::sched::current_fds().clone_at(fd) {
        if let Some(sock) = file.inode().as_unix() {
            let raw = match read_sockaddr_un(args[1], args[2], from_user) {
                Ok(raw) => raw,
                Err(e) => return e,
            };
            let path = match crate::net::unix::sockaddr_un_path(&raw) {
                Ok(path) => path,
                Err(e) => return e,
            };
            let nonblock = fd_status(fd) & crate::vfs::file::O_NONBLOCK != 0;
            return crate::net::unix::connect_path(sock, &path, nonblock);
        }
    }
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
        if crate::sched::has_deliverable_signal() {
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
    // A connected pair's end: the send *is* a write. There is no destination
    // to parse and no control data this kernel carries, so the whole of the
    // call is the bytes; a caller that names a destination on a connected
    // socket is told so, as Linux does.
    if let Some(file) = channel_of_fd(fd) {
        if !crate::syscall::buffer_ok(buf, len, false, from_user) {
            return err::EFAULT;
        }
        if args[4] != 0 {
            return err::EISCONN;
        }
        if dontwait(fd, args[3]) && !file.inode().poll_ready().write {
            return err::EAGAIN;
        }
        let data = unsafe { slice::from_raw_parts(buf as *const u8, len) };
        return file
            .write(data)
            .map(|n| n as isize)
            .unwrap_or_else(crate::syscall::err_of);
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
    // A connected pair's end, in scatter-gather form: gather, then write.
    // No name is legal to send to and no control data is carried, so a
    // caller that set either is told so.
    if let Some(file) = channel_of_fd(fd) {
        let (name, _namelen, iov, iovlen) = match read_msghdr(args[1], from_user) {
            Ok(fields) => fields,
            Err(e) => return e,
        };
        if name != 0 {
            return err::EISCONN;
        }
        let mut data: Vec<u8> = Vec::new();
        if let Err(e) = gather_iovecs(iov, iovlen, from_user, &mut data) {
            return e;
        }
        if dontwait(fd, args[2]) && !file.inode().poll_ready().write {
            return err::EAGAIN;
        }
        return file
            .write(&data)
            .map(|n| n as isize)
            .unwrap_or_else(crate::syscall::err_of);
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
        if crate::sched::has_deliverable_signal() {
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
    // The read side of a connected pair's end. A peek would need to look at
    // the front of the pipe's queue without taking it, which a pipe cannot
    // do - refusing beats consuming bytes the caller meant to keep.
    if let Some(file) = channel_of_fd(fd) {
        if !crate::syscall::buffer_ok(buf, len, true, from_user) {
            return err::EFAULT;
        }
        if args[3] & MSG_PEEK != 0 {
            return err::EOPNOTSUPP;
        }
        if dontwait(fd, args[3]) && !file.inode().poll_ready().read {
            return err::EAGAIN;
        }
        let out = unsafe { slice::from_raw_parts_mut(buf as *mut u8, len) };
        return file
            .read(out)
            .map(|n| n as isize)
            .unwrap_or_else(crate::syscall::err_of);
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
    // The read side of a connected pair's end, in scatter-gather form: one
    // contiguous read into the first iovec, no name and no control data to
    // report - the fields go back zeroed, which is how Linux says "none".
    if let Some(file) = channel_of_fd(fd) {
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
        if args[2] & MSG_PEEK != 0 {
            // A peek needs the front of the queue without the take, and a
            // pipe has no way to look. Refusing beats consuming bytes the
            // caller meant to keep.
            return err::EOPNOTSUPP;
        }
        if dontwait(fd, args[2]) && !file.inode().poll_ready().read {
            return err::EAGAIN;
        }
        let out = unsafe { slice::from_raw_parts_mut(buf as *mut u8, len) };
        let n = match file.read(out) {
            Ok(n) => n as isize,
            Err(e) => return crate::syscall::err_of(e),
        };
        if !crate::syscall::buffer_ok(msghdr, MSGHDR_SIZE, true, from_user) {
            return err::EFAULT;
        }
        unsafe {
            ((msghdr + MSGHDR_FLAGS) as *mut i32).write_volatile(0);
            if name != 0 && namelen != 0 {
                ((msghdr + 8) as *mut u32).write_unaligned(0);
            }
            ((msghdr + 40) as *mut u64).write_unaligned(0); // msg_controllen
        }
        return n;
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
        if crate::sched::has_deliverable_signal() {
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
        if crate::sched::has_deliverable_signal() {
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
        // Any signal with a handler ends the wait, not just a fatal one, and
        // the signal is left pending for the gate to deliver on the way out.
        // `ping` is the shape this is for: it arms `ITIMER_REAL` with a
        // SIGALRM handler and expects the blocking read to come back `EINTR`
        // once a second so that it can send the next request - a wait that
        // only noticed fatal signals never gave it the chance.
        if crate::sched::has_deliverable_signal() {
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
    // AF_UNIX: the path bind. The address is a `sockaddr_un`; the socket's
    // whole namespace story - node, table, news - is `unix::bind_path`.
    if let Ok(file) = crate::sched::current_fds().clone_at(args[0] as i32) {
        if let Some(sock) = file.inode().as_unix() {
            let raw = match read_sockaddr_un(args[1], args[2], from_user) {
                Ok(raw) => raw,
                Err(e) => return e,
            };
            let path = match crate::net::unix::sockaddr_un_path(&raw) {
                Ok(path) => path,
                Err(e) => return e,
            };
            return crate::net::unix::bind_path(sock, &path);
        }
    }
    // The netlink probes look up the *network registry's* handle - a unix
    // socket has none, so it can never be mistaken for one, even though
    // `sockaddr_un` and `sockaddr_nl` share the family's first bytes and a
    // unix bind carries enough bytes to satisfy either reader.
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

/// `listen(fd, backlog)` - a unix listener's queue opens here; the network
/// sockets keep the straight refusal (their accept machinery, with the
/// handshake and the half-close states it implies, is not built).
pub(crate) fn handle_listen(args: [usize; 6], _from_user: bool) -> isize {
    if let Ok(file) = crate::sched::current_fds().clone_at(args[0] as i32) {
        if let Some(sock) = file.inode().as_unix() {
            return match crate::net::unix::listen(sock, args[1] as i32) {
                Ok(()) => 0,
                Err(e) => e,
            };
        }
    }
    err::EOPNOTSUPP
}

/// `accept(fd, addr, addrlen)` - a unix listener hands out the connected
/// socket a `connect` queued; see [`handle_listen`] for the network
/// refusal. The flags of the `accept4` spelling ride through both
/// parameters the modern call separates.
pub(crate) fn handle_accept(args: [usize; 6], _from_user: bool) -> isize {
    accept_common(args[0] as i32, args[1], args[2], 0)
}

/// The accept body for one set of descriptor flags (`SOCK_CLOEXEC |
/// SOCK_NONBLOCK` as `accept4` spells them).
fn accept_common(fd: i32, addr_ptr: usize, len_ptr: usize, flags: usize) -> isize {
    const SOCK_CLOEXEC_F: usize = 0o200_0000;
    const SOCK_NONBLOCK_F: usize = 0x800;
    let listener_arc = match crate::sched::current_fds().clone_at(fd) {
        Ok(f) => f,
        Err(e) => return crate::syscall::err_of(e),
    };
    // The borrow that asks "is this a unix socket, and which id" cannot
    // outlive the statement; the id, and the Arc the registry hands back
    // for it, can.
    let Some(listener_id) = listener_arc.inode().as_unix().map(|sock| sock.id()) else {
        return err::EOPNOTSUPP;
    };
    drop(listener_arc);
    let listener = match crate::net::unix::by_id(listener_id) {
        Some(sock) => sock,
        None => return err::EBADF,
    };
    let nonblock = flags & SOCK_NONBLOCK_F != 0
        || fd_status(fd) & crate::vfs::file::O_NONBLOCK != 0;
    let id = match crate::net::unix::accept_face(&listener, nonblock) {
        Ok(id) => id,
        Err(e) => return e,
    };
    // The accepted socket arrives as a fresh descriptor on the same
    // connected channel; `getsockname` on it answers the listener's name
    // once the caller asks (the write-back below), and with an absent
    // address it is simply a new fd.
    let sock = match crate::net::unix::by_id(id) {
        Some(sock) => sock,
        None => return err::EBADF,
    };
    let new_fd = match crate::vfs::open_inode(sock) {
        Ok(fd) => fd,
        Err(e) => return crate::syscall::err_of(e),
    };
    if flags & SOCK_NONBLOCK_F != 0 && let Ok(file) = crate::sched::current_fds().clone_at(new_fd) {
        file.set_status(crate::vfs::file::O_NONBLOCK);
    }
    if flags & SOCK_CLOEXEC_F != 0 {
        let _ = crate::sched::current_fds().fd_flags_set(new_fd, crate::vfs::file::FD_CLOEXEC);
    }
    // The caller's `addr`/`addrlen`, when present: the peer's name is the
    // listener's bound path, which is what Linux writes back.
    if addr_ptr != 0 && len_ptr != 0 {
        crate::net::unix::write_name_by_id(id, addr_ptr, len_ptr);
    }
    new_fd as isize
}

/// `accept4(fd, addr, addrlen, flags)` - `accept` with the two flags the
/// modern spelling takes.
///
/// The flags are validated first, before the socket is looked at - the
/// man page's own order, and the one that matters to a caller probing for
/// the feature: an unknown bit is `EINVAL` whatever the socket is doing.
/// The two known bits belong to the descriptor an accepted connection
/// would arrive on, and this kernel's accept is the honest refusal the
/// flag-less form gives, so they are accepted and unused - the same
/// treatment `pipe2` gives a flag it cannot yet honour.
pub(crate) fn handle_accept4(args: [usize; 6], from_user: bool) -> isize {
    const SOCK_CLOEXEC: usize = 0o200_0000;
    const SOCK_NONBLOCK: usize = 0x800;
    if args[3] & !(SOCK_CLOEXEC | SOCK_NONBLOCK) != 0 {
        return err::EINVAL;
    }
    // The flags belong to the descriptor an accepted socket arrives on -
    // a unix listener honours them; the network refusal stays.
    if let Ok(file) = crate::sched::current_fds().clone_at(args[0] as i32) {
        if file.inode().as_unix().is_some() {
            return accept_common(args[0] as i32, args[1], args[2], args[3]);
        }
    }
    handle_accept(args, from_user)
}

/// One `struct mmsghdr`: a full `struct msghdr` (56 bytes) followed by the
/// `unsigned int msg_len` the multiple-message calls write their per-entry
/// byte counts into, padded to the 8-byte alignment the next record needs.
const MMSGHDR_SIZE: usize = 64;
/// Where `msg_len` sits inside a `struct mmsghdr`.
const MMSGHDR_LEN: usize = 56;
/// The ABI's ceiling on the record count of a multiple-message call - the
/// same `UIO_MAXIOV` the vectored I/O calls are bounded by.
const UIO_MAXIOV: usize = 1024;

/// `sendmmsg(fd, msgvec, vlen, flags)` - `sendmsg` over an array of
/// messages, one syscall for the round trip.
///
/// Each entry is sent as its own `sendmsg`; the byte count lands in the
/// entry's `msg_len` field, and the answer is how many entries went out. An
/// entry that fails ends the call: the count of what already left is the
/// answer - the man page's own "succeeds, and returns the number of messages
/// sent" for the partial case - and nothing is the answer only when nothing
/// left, in which case the failure itself is what the caller hears.
pub(crate) fn handle_sendmmsg(args: [usize; 6], from_user: bool) -> isize {
    let (fd, msgvec, vlen, flags) = (args[0] as i32, args[1], args[2], args[3]);
    if vlen == 0 {
        return 0;
    }
    if vlen > UIO_MAXIOV {
        return err::EINVAL;
    }
    let len = match vlen.checked_mul(MMSGHDR_SIZE) {
        Some(len) => len,
        None => return err::EINVAL,
    };
    // The headers are read and the lengths written back, so both
    // directions of the array must be reachable.
    if !crate::syscall::buffer_ok(msgvec, len, true, from_user) {
        return err::EFAULT;
    }

    let mut sent = 0usize;
    for index in 0..vlen {
        let at = msgvec + index * MMSGHDR_SIZE;
        let n = handle_sendmsg([fd as usize, at, flags, 0, 0, 0], from_user);
        if n < 0 {
            if sent > 0 {
                return sent as isize;
            }
            return n;
        }
        unsafe { ((at + MMSGHDR_LEN) as *mut u32).write_volatile(n as u32) };
        sent += 1;
    }
    sent as isize
}

/// `recvmmsg(fd, msgvec, vlen, flags, timeout)` - `recvmsg` over an array
/// of messages, with a clock on the whole call.
///
/// A blocking call with no timeout waits the way `recvmsg` itself waits,
/// message by message. A timeout paces the whole call instead: every
/// attempt is a nonblocking one, the deadline is checked between them, and
/// when it expires the answer is how many arrived - zero included, which
/// is the man page's own answer for "the clock ran out before anything
/// came". `MSG_WAITFORONE` is honoured as its name says: after the first
/// message, nothing blocks - whatever is queued arrives and the call
/// answers.
pub(crate) fn handle_recvmmsg(args: [usize; 6], from_user: bool) -> isize {
    const MSG_WAITFORONE: usize = 0x10000;
    let (fd, msgvec, vlen, flags, timeout) = (args[0] as i32, args[1], args[2], args[3], args[4]);
    if vlen == 0 {
        return 0;
    }
    if vlen > UIO_MAXIOV {
        return err::EINVAL;
    }
    let len = match vlen.checked_mul(MMSGHDR_SIZE) {
        Some(len) => len,
        None => return err::EINVAL,
    };
    if !crate::syscall::buffer_ok(msgvec, len, true, from_user) {
        return err::EFAULT;
    }
    let deadline = if timeout != 0 {
        if !crate::syscall::buffer_ok(timeout, 16, false, from_user) {
            return err::EFAULT;
        }
        let (sec, nsec) = unsafe {
            (
                (timeout as *const i64).read_volatile(),
                ((timeout + 8) as *const i64).read_volatile(),
            )
        };
        if sec < 0 || !(0..1_000_000_000).contains(&nsec) {
            return err::EINVAL;
        }
        // A timeout is a lower bound, so the sub-second part rounds up -
        // the same fold `ppoll` makes. Both fields were validated above,
        // so both are non-negative and the u64 arithmetic below is the
        // whole story.
        let hz = crate::arch::timer::TICK_HZ as u64;
        let ms = (sec as u64)
            .saturating_mul(1_000)
            .saturating_add((nsec as u64).div_ceil(1_000_000));
        Some(crate::arch::timer::ticks().saturating_add(ms.saturating_mul(hz) / 1000))
    } else {
        None
    };
    let wait_for_one = flags & MSG_WAITFORONE != 0;

    let mut received = 0usize;
    for index in 0..vlen {
        let at = msgvec + index * MMSGHDR_SIZE;
        let n = if index == 0 && deadline.is_none() {
            // The first message of an unclocked call is the ordinary
            // blocking receive - `recvmsg`'s own wait.
            handle_recvmsg([fd as usize, at, flags, 0, 0, 0], from_user)
        } else {
            match recv_one_bounded(fd, at, flags, from_user, deadline) {
                Ok(n) => n as isize,
                Err(e) => {
                    if e == err::EWOULDBLOCK && (deadline.is_some() || wait_for_one) {
                        // The clock ran out, or `MSG_WAITFORONE` said one
                        // was enough: the count is the answer, timeout and
                        // all, and a caller with none asks for `EAGAIN`.
                        if received == 0 && wait_for_one {
                            return err::EWOULDBLOCK;
                        }
                        return received as isize;
                    }
                    if e == err::EWOULDBLOCK && received == 0 {
                        return err::EWOULDBLOCK;
                    }
                    if received > 0 {
                        return received as isize;
                    }
                    return e;
                }
            }
        };
        if n < 0 {
            if received > 0 {
                return received as isize;
            }
            return n;
        }
        unsafe { ((at + MMSGHDR_LEN) as *mut u32).write_volatile(n as u32) };
        received += 1;
    }
    received as isize
}

/// One nonblocking `recvmsg` attempt, paced by a deadline: the descriptor
/// is made nonblocking for the duration, and `EWOULDBLOCK` is retried a
/// tick at a time until either data arrives or the clock runs out. The
/// `Err(EWOULDBLOCK)` the clock produces is indistinguishable from the
/// socket's own - deliberately, because to the caller both mean "nothing
/// else is coming now".
fn recv_one_bounded(
    fd: i32,
    at: usize,
    flags: usize,
    from_user: bool,
    deadline: Option<u64>,
) -> Result<usize, isize> {
    let file = crate::sched::current_fds()
        .clone_at(fd)
        .map_err(|_| err::EBADF)?;
    let saved = file.status();
    file.set_status(saved | crate::vfs::file::O_NONBLOCK);
    let answer = loop {
        let n = handle_recvmsg([fd as usize, at, flags, 0, 0, 0], from_user);
        if n != err::EWOULDBLOCK {
            break Ok(n as usize);
        }
        match deadline {
            None => break Err(err::EWOULDBLOCK),
            Some(end) if crate::arch::timer::ticks() >= end => break Err(err::EWOULDBLOCK),
            _ => crate::sched::sleep_ticks(1),
        }
    };
    file.set_status(saved);
    answer
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
    let fd0 = match crate::vfs::file::open_inode(first) {
        Ok(fd) => fd,
        Err(e) => return crate::syscall::err_of(e),
    };
    let fd1 = match crate::vfs::file::open_inode(second) {
        Ok(fd) => fd,
        Err(e) => {
            // The first landed but the allowance ran out on the second: give
            // the first back rather than leak a descriptor the caller never
            // learns the number of.
            let _ = crate::vfs::close(fd0);
            return crate::syscall::err_of(e);
        }
    };
    let fds = [fd0, fd1];
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
/// Read a `sockaddr_un` of at most the header's whole size, as far as the
/// caller's `addrlen` reaches. The length bounds are the parser's question
/// (it owns the family and path rules); this reads the bytes and lets it
/// answer them.
fn read_sockaddr_un(ptr: usize, len: usize, from_user: bool) -> Result<alloc::vec::Vec<u8>, isize> {
    if ptr == 0 || len < 3 || len > 2 + crate::net::unix::SUN_PATH_MAX {
        return Err(err::EINVAL);
    }
    if !crate::syscall::buffer_ok(ptr, len, false, from_user) {
        return Err(err::EFAULT);
    }
    let mut raw = alloc::vec![0u8; len];
    unsafe {
        core::ptr::copy_nonoverlapping(ptr as *const u8, raw.as_mut_ptr(), len);
    }
    Ok(raw)
}

pub(crate) fn handle_getsockname(args: [usize; 6], _from_user: bool) -> isize {
    let fd = args[0] as i32;
    let (addr_ptr, len_ptr) = (args[1], args[2]);
    // A unix socket's name is its bind path (or the empty name before it),
    // written back in `sockaddr_un` form with the in-out length the
    // man page spells.
    if let Ok(file) = crate::sched::current_fds().clone_at(fd) {
        if let Some(sock) = file.inode().as_unix() {
            return crate::net::unix::write_name(sock, addr_ptr, len_ptr);
        }
    }
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
    // A unix stream's peer carries no name of its own in this kernel: the
    // client never binds, and the accepted half is a fresh anonymous end -
    // so the answer is the empty `AF_UNIX` sockaddr Linux gives for an
    // unnamed socket. An unconnected one has no peer to name at all,
    // which is `ENOTCONN` and not an empty answer: "nobody is there" is
    // a different fact from "everybody is anonymous".
    if let Ok(file) = crate::sched::current_fds().clone_at(fd) {
        if let Some(sock) = file.inode().as_unix() {
            if sock.unix_end().is_none() {
                return err::ENOTCONN;
            }
            return crate::net::unix::write_name(sock, addr_ptr, len_ptr);
        }
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
    // A connected unix socket half-closes through the same channel a
    // socketpair end does - the `unix_end` the file doors already route by.
    if let Ok(file) = crate::sched::current_fds().clone_at(args[0] as i32) {
        if file.inode().unix_id() != 0 {
            let Some(end) = file.inode().unix_end() else {
                // ENOTCONN is Linux's answer to shutdown on a socket that
                // never connected; an unconnected unix fd is exactly that.
                return err::ENOTCONN;
            };
            if how == SHUT_RD || how == SHUT_RDWR {
                end.shut_read();
            }
            if how == SHUT_WR || how == SHUT_RDWR {
                end.shut_write();
            }
            return 0;
        }
    }
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
/// Every option is remembered so `getsockopt` reads back what the caller set.
/// Two of them also reach the stack: `TCP_NODELAY` switches Nagle off on the
/// live socket and `SO_KEEPALIVE` arms its probe timer. The rest - buffer
/// sizes, linger, reuse - are recorded only: timeouts live in user code
/// (`poll`) and `SO_REUSEADDR` is moot while one process binds at a time.
pub(crate) fn handle_setsockopt(args: [usize; 6], from_user: bool) -> isize {
    let (fd, level, optname, optval, optlen) =
        (args[0] as i32, args[1], args[2], args[3], args[4]);
    let handle = match socket_handle_of_fd(fd) {
        Ok(handle) => handle,
        Err(e) => return e,
    };
    // An int-valued option is 4 bytes; `struct linger` is two ints.
    let need = if (level, optname) == (SOL_SOCKET, SO_LINGER) {
        8
    } else {
        4
    };
    if optval == 0 || optlen < need || !crate::syscall::buffer_ok(optval, need, false, from_user) {
        return err::EINVAL;
    }
    let val = unsafe { (optval as *const i32).read_unaligned() };
    super::with(handle, |stack, entry| {
        // The smoltcp socket behind this channel, when it is a TCP one. Two of
        // the options below actually steer the stack and so are applied here,
        // not merely remembered; `get_mut` downcasts and panics on a mismatch,
        // so it is only ever called with a handle this `Entry` owns as TCP.
        let tcp_sock = match entry.channel {
            Channel::Tcp { handle: h, .. } => Some(h),
            _ => None,
        };
        match (level, optname) {
            (SOL_SOCKET, SO_REUSEADDR) => entry.opts.reuseaddr = val != 0,
            (SOL_SOCKET, SO_KEEPALIVE) => {
                entry.opts.keepalive = val != 0;
                // Turning keep-alive on arms smoltcp's probe timer; the interval
                // is Linux's default `TCP_KEEPINTVL`, which is the pace it then
                // holds the socket to.
                if let Some(h) = tcp_sock {
                    let interval = (val != 0)
                        .then(|| smoltcp::time::Duration::from_secs(KEEPALIVE_INTERVAL_SECS));
                    stack.sockets.get_mut::<tcp::Socket>(h).set_keep_alive(interval);
                }
            }
            (SOL_SOCKET, SO_SNDBUF) => {
                if val > 0 {
                    entry.opts.sndbuf = val as u32;
                }
            }
            (SOL_SOCKET, SO_RCVBUF) => {
                if val > 0 {
                    entry.opts.rcvbuf = val as u32;
                }
            }
            (SOL_SOCKET, SO_LINGER) => {
                let t = unsafe { ((optval + 4) as *const i32).read_unaligned() };
                entry.opts.linger_on = val != 0;
                entry.opts.linger_sec = t;
            }
            (SOL_TCP, TCP_NODELAY) => {
                entry.opts.nodelay = val != 0;
                // `TCP_NODELAY` is Nagle switched off, exactly the inverse of
                // the flag smoltcp names.
                if let Some(h) = tcp_sock {
                    stack.sockets.get_mut::<tcp::Socket>(h).set_nagle_enabled(val == 0);
                }
            }
            // An option this stack does not model is accepted and forgotten, the
            // way a lenient kernel lets a caller set an exotic one and ignore it.
            _ => {}
        }
    });
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
    if !crate::syscall::buffer_ok(optlen_ptr, 4, true, from_user) {
        return err::EFAULT;
    }
    let cap = unsafe { (optlen_ptr as *const u32).read_unaligned() };
    // `SO_LINGER` answers a `struct linger { int l_onoff; int l_linger; }`, so
    // it is the one option eight bytes wide.
    if (level, optname) == (SOL_SOCKET, SO_LINGER) {
        if cap < 8 || !crate::syscall::buffer_ok(optval, 8, true, from_user) {
            return err::EINVAL;
        }
        let (on, t) = super::with(handle, |_, entry| {
            (entry.opts.linger_on as i32, entry.opts.linger_sec)
        })
        .unwrap_or((0, 0));
        unsafe {
            (optval as *mut i32).write_unaligned(on);
            ((optval + 4) as *mut i32).write_unaligned(t);
            (optlen_ptr as *mut u32).write_unaligned(8);
        }
        return 0;
    }
    if cap < 4 || !crate::syscall::buffer_ok(optval, 4, true, from_user) {
        return err::EINVAL;
    }
    let value = match (level, optname) {
        (SOL_SOCKET, SO_TYPE) => super::with(handle, |_, entry| entry.typ as i32).unwrap_or(0),
        // Errors are reported by the calls that hit them, so a query reads
        // back "no error pending" - which for this stack is always true.
        (SOL_SOCKET, SO_ERROR) => 0,
        (SOL_SOCKET, SO_REUSEADDR) => {
            super::with(handle, |_, entry| entry.opts.reuseaddr as i32).unwrap_or(0)
        }
        (SOL_SOCKET, SO_KEEPALIVE) => {
            super::with(handle, |_, entry| entry.opts.keepalive as i32).unwrap_or(0)
        }
        (SOL_SOCKET, SO_SNDBUF) => {
            super::with(handle, |_, entry| entry.opts.sndbuf as i32).unwrap_or(0)
        }
        (SOL_SOCKET, SO_RCVBUF) => {
            super::with(handle, |_, entry| entry.opts.rcvbuf as i32).unwrap_or(0)
        }
        (SOL_TCP, TCP_NODELAY) => {
            super::with(handle, |_, entry| entry.opts.nodelay as i32).unwrap_or(0)
        }
        // An unmodelled option reads as unset (zero), matching the permissive
        // `setsockopt` above rather than answering ENOPROTOOPT.
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

#[cfg(all(test, not(target_os = "none")))]
mod tests {
    use super::*;

    /// A pair as `socketpair` mints it, both fds closed when the test ends.
    /// The handlers run with `from_user = false`, so the buffers they touch
    /// are ordinary kernel-side locals - the host test harness's whole trick.
    fn a_pair() -> (i32, i32) {
        let mut out = [0i32; 2];
        let args = [
            AF_UNIX as usize,
            SOCK_STREAM as usize,
            0,
            out.as_mut_ptr() as usize,
            0,
            0,
        ];
        assert_eq!(handle_socketpair(args, false), 0);
        (out[0], out[1])
    }

    #[test]
    fn a_pair_takes_send_and_answers_recv() {
        let (a, b) = a_pair();

        // The parent's status message, sent the way sudo does: `send`, which
        // is `sendto` with no destination.
        let args = [a as usize, b"status: 0".as_ptr() as usize, 9, 0, 0, 0];
        assert_eq!(handle_sendto(args, false), 9);

        let mut buf = [0u8; 64];
        let args = [b as usize, buf.as_mut_ptr() as usize, buf.len(), 0, 0, 0];
        assert_eq!(handle_recvfrom(args, false), 9);
        assert_eq!(&buf[..9], b"status: 0");

        // And the other direction, because a pair is two-way.
        let args = [b as usize, b"ack".as_ptr() as usize, 3, 0, 0, 0];
        assert_eq!(handle_sendto(args, false), 3);
        let args = [a as usize, buf.as_mut_ptr() as usize, buf.len(), 0, 0, 0];
        assert_eq!(handle_recvfrom(args, false), 3);
        assert_eq!(&buf[..3], b"ack");
    }

    #[test]
    fn msg_forms_carry_the_bytes_and_report_nothing_else() {
        let (a, b) = a_pair();

        // msghdr for a kernel-space gather send: no name, two iovecs.
        let iovecs: [usize; 4] = [b"one ".as_ptr() as usize, 4, b"two ".as_ptr() as usize, 4];
        let mut msghdr = [0u8; MSGHDR_SIZE];
        let (iov_ptr,) = (iovecs.as_ptr() as usize,);
        set64(&mut msghdr, 16, iov_ptr); // msg_iov
        set64(&mut msghdr, 24, 2); // msg_iovlen
        let args = [a as usize, msghdr.as_ptr() as usize, 0, 0, 0, 0];
        assert_eq!(handle_sendmsg(args, false), 8);

        // The receive side: one iovec, and the answer says "no address, no
        // control data, no flags" - the message shape of "nothing to say".
        let mut rbuf = [0u8; 64];
        let mut riov: [usize; 2] = [rbuf.as_mut_ptr() as usize, rbuf.len()];
        let mut rmsghdr = [0u8; MSGHDR_SIZE];
        set64(&mut rmsghdr, 16, riov.as_mut_ptr() as usize);
        set64(&mut rmsghdr, 24, 1);
        set64(&mut rmsghdr, 8, 8); // msg_namelen nonzero: the caller asked
        let args = [b as usize, rmsghdr.as_ptr() as usize, 0, 0, 0, 0];
        assert_eq!(handle_recvmsg(args, false), 8);
        assert_eq!(&rbuf[..8], b"one two ");
        // The caller's name request comes back answered with "none".
        assert_eq!(le64(&rmsghdr, 8), 0);
        assert_eq!(le64(&rmsghdr, 40), 0); // msg_controllen
        assert_eq!(le32(&rmsghdr, 48), 0); // msg_flags
        let _ = &mut riov;
    }

    #[test]
    fn refusals_match_a_connected_channel() {
        let (a, b) = a_pair();
        let _ = &b;

        // A destination on a connected pair: EISCONN, as Linux answers.
        let bogus_sockaddr = [0u8; 16];
        let args = [
            a as usize,
            b"to whom?".as_ptr() as usize,
            8,
            0,
            bogus_sockaddr.as_ptr() as usize,
            16,
        ];
        assert_eq!(handle_sendto(args, false), err::EISCONN);

        // DONTWAIT on an empty channel: EAGAIN, not a park.
        let mut buf = [0u8; 8];
        let args = [
            a as usize,
            buf.as_mut_ptr() as usize,
            buf.len(),
            MSG_DONTWAIT,
            0,
            0,
        ];
        assert_eq!(handle_recvfrom(args, false), err::EAGAIN);
    }

    fn set64(bytes: &mut [u8], at: usize, value: usize) {
        bytes[at..at + 8].copy_from_slice(&(value as u64).to_le_bytes());
    }

    fn le64(bytes: &[u8], at: usize) -> usize {
        u64::from_le_bytes(bytes[at..at + 8].try_into().expect("in-bounds")) as usize
    }

    fn le32(bytes: &[u8], at: usize) -> u32 {
        u32::from_le_bytes(bytes[at..at + 4].try_into().expect("in-bounds"))
    }
}
