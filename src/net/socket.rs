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

use smoltcp::socket::tcp;
use smoltcp::socket::udp;
use smoltcp::wire::{IpAddress, IpEndpoint, IpListenEndpoint, Ipv4Address};

use super::Channel;
use crate::arch::timer;
use crate::syscall::err;
use crate::vfs::{Inode, Metadata, NodeKind, Ready, VfsError};

// --- the Linux constants the ABI spells by number ---
pub(crate) const AF_INET: u16 = 2;
pub(crate) const SOCK_STREAM: u16 = 1;
pub(crate) const SOCK_DGRAM: u16 = 2;
const SOCK_NONBLOCK: u32 = 0o4000;
const SOCK_CLOEXEC: u32 = 0o2000000;
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
    let _domain = args[0] as u16;
    if _domain != AF_INET {
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
        _ => return err::ESOCKTNOSUPPORT,
    };
    if !protocol_matches {
        return err::EPROTONOSUPPORT;
    }

    let handle = match super::alloc_socket(typ) {
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
    let mut last_state: Option<tcp::State> = None;
    let mut last_print = 0u64;
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
            let state = sock.state();
            if last_state != Some(state) {
                crate::println!("NET connect: state={} t={}", state, timer::ticks());
                last_state = Some(state);
            }
            match state {
                tcp::State::Established | tcp::State::SynReceived => Ok(true),
                tcp::State::Closed => Err(err::ECONNREFUSED),
                _ => Ok(false),
            }
        });
        if timer::ticks() - last_print >= 25 {
            crate::println!("NET loop t={} deadline={}", timer::ticks(), deadline);
            last_print = timer::ticks();
        }
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
    match typ {
        Some(SOCK_STREAM) => tcp_send(handle, data, nonblock),
        Some(SOCK_DGRAM) => udp_send(handle, data, dst),
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
    let handle = match socket_handle_of_fd(fd) {
        Ok(handle) => handle,
        Err(e) => return e,
    };
    let msghdr = args[1];
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
    let received = match typ {
        Some(SOCK_STREAM) => tcp_recv(handle, out, nonblock),
        Some(SOCK_DGRAM) => udp_recv(handle, out, nonblock),
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
// bind, listen, names, options
// ---------------------------------------------------------------------------

/// `bind(fd, addr, addrlen)`.
///
/// For a stream socket this only records the endpoint - there is nothing to
/// bind before a connection exists, and `connect` takes the record as its
/// source. A datagram socket binds for real: the receive queue starts
/// holding whatever arrives for the port.
pub(crate) fn handle_bind(args: [usize; 6], from_user: bool) -> isize {
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
                *local = Some(endpoint);
                Ok(())
            }
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

/// `socketpair(domain, type, protocol, fds)` - a Unix-domain construct, and
/// the Unix domain is not spoken here. Pipes cover the use cases.
pub(crate) fn handle_socketpair(_args: [usize; 6], _from_user: bool) -> isize {
    err::EOPNOTSUPP
}

/// `getsockname(fd, addr, addrlen)`.
pub(crate) fn handle_getsockname(args: [usize; 6], _from_user: bool) -> isize {
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
    // Nothing bound and nothing connected reads back as 0.0.0.0:0, which is
    // what Linux answers for a fresh socket as well.
    let endpoint = super::with(handle, |stack, entry| {
        stack.poll_once();
        match &entry.channel {
            Channel::Tcp {
                handle: sock_handle,
                ..
            } => stack
                .sockets
                .get_mut::<tcp::Socket>(*sock_handle)
                .local_endpoint()
                .unwrap_or(UNSPECIFIED_ENDPOINT),
            Channel::Udp { local, .. } => local.unwrap_or(UNSPECIFIED_ENDPOINT),
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
