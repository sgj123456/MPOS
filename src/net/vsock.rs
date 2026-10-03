//! AF_VSOCK: the socket family over the virtio socket device.
//!
//! A vsock address is a CID and a port - a CID being the number the
//! hypervisor assigned this guest, and `2` being the host. The family
//! exists for exactly one purpose: a program in the guest talking to a
//! program on the host without an IP stack between them, which is why it
//! takes its own module and its own registry rather than riding the
//! smoltcp stack - that stack answers `ENETDOWN` on a machine with no
//! network card, and AF_UNIX took the same detour for the same reason.
//!
//! The shape mirrors [`super::unix`] family for family: a [`VsockSock`]
//! an fd names, a registry keyed by its id, the phases between
//! `socket(2)` and a connected stream, and an accept queue a
//! `ConnectionRequest` feeds. The differences are the protocol's: the
//! connection manager at the device already *answered* every request the
//! host sent to a listening port - there is no message for "pending, ask
//! later" - so `accept` hands out connections the host already believes
//! exist. And the byte channel is not a pipe of this kernel's: `send` and
//! `recv` are protocol packets, credit-checked by the device, so the
//! blocking discipline is the network stack's - *lock, try, unlock, sleep
//! a tick, repeat* - rather than the pipe's waiter lists.
//!
//! Half-closes follow the flags the protocol carries: a `SHUT_WR` sends
//! the shutdown packet (the device reports it in both directions, which is
//! the packet the crate offers), a `SHUT_RD` is local - the buffered bytes
//! stay readable until drained, then end of file.

use alloc::collections::VecDeque;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::cell::RefCell;

use critical_section::Mutex;

use crate::drivers::virtio::vsock::VirtioVsock;
use crate::err;
use crate::vfs::{Inode, Metadata, NodeKind, Ready, VfsError, VfsResult};
use virtio_drivers::device::socket::{VsockAddr, VsockEvent, VsockEventType};

/// The AF_VSOCK family number, as `linux/vm_sockets.h` spells it.
pub const AF_VSOCK: u16 = 40;

/// The size of `struct sockaddr_vm` on x86_64: family, reserved, port,
/// CID, and a zero tail the struct's fixed size demands.
const SOCKADDR_VM_LEN: usize = 16;

/// `VMADDR_CID_ANY`: the wildcard a `bind` spells when it wants no
/// particular CID - and the only CID a guest can bind, since the host
/// assigned this one.
const CID_ANY: u32 = 0xffff_ffff;

/// The CID a vhost host answers on. The one address a guest almost always
/// dials, which is why it is named.
pub const CID_HOST: u32 = 2;

/// Where anonymous local ports start - the range Linux's vsock layer
/// draws from, so a port in it never collides with one a server would
/// deliberately bind.
const EPHEMERAL_MIN: u32 = 1024;
const EPHEMERAL_MAX: u32 = 4096;

/// One packet of a send, sized to move a stream in few round trips while
/// staying under any peer's receive window. The credit check would refuse
/// a larger buffer in one piece anyway.
const SEND_CHUNK: usize = 4096;

/// The whole family's state: the device, and the sockets it serves.
struct VsockStack {
    dev: Arc<VirtioVsock>,
    /// Every open socket, by id - the same registry shape the network
    /// stack and the unix family keep, so `accept` has somewhere to put
    /// the socket it mints and `close` has somewhere to retire one.
    sockets: Vec<Option<Arc<VsockSock>>>,
    next_ephemeral: u32,
}

static STACK: Mutex<RefCell<Option<VsockStack>>> = Mutex::new(RefCell::new(None));

/// The guest's own CID, if the family is up.
pub fn guest_cid() -> Option<u32> {
    critical_section::with(|cs| {
        STACK
            .borrow(cs)
            .borrow()
            .as_ref()
            .map(|st| st.dev.guest_cid() as u32)
    })
}

/// Whether the family is up at all - `socket(AF_VSOCK)` on a machine
/// without the device answers `EAFNOSUPPORT`, the refusal a family the
/// kernel does not speak gives.
pub fn up() -> bool {
    critical_section::with(|cs| STACK.borrow(cs).borrow().is_some())
}

/// Bring the family up over a device the boot probed. Called once; the
/// device either answered the bus or did not.
pub fn init(dev: Arc<VirtioVsock>) {
    critical_section::with(|cs| {
        let mut stack = STACK.borrow(cs).borrow_mut();
        if stack.is_none() {
            *stack = Some(VsockStack {
                dev,
                sockets: Vec::new(),
                next_ephemeral: EPHEMERAL_MIN,
            });
        }
    });
}

/// The task that keeps events flowing: connection requests, handshakes,
/// buffers filling. Without it a `connect` issued by one task would never
/// see the `Connected` event that finishes it, for the same reason the
/// network stack needs its poller - and like that poller, this one is
/// permanent, because a socket parked in `accept` depends on it for as
/// long as the machine runs.
pub fn spawn_service() {
    crate::sched::spawn_permanent(
        || loop {
            service();
            crate::sched::sleep_ticks(1);
        },
        crate::sched::Priority::NORMAL,
    );
}

/// Poll the device once and route what came off it.
pub fn service() {
    critical_section::with(|cs| {
        let mut stack = STACK.borrow(cs).borrow_mut();
        let Some(stack) = stack.as_mut() else { return };
        // The manager's own bookkeeping has run by the time an event
        // surfaces: bytes are buffered, requests on listening ports are
        // answered. What is left is the family's half - which socket the
        // event belongs to, and what it does to it.
        while let Some(event) = stack.dev.poll_event() {
            route(stack, &event);
        }
    });
}

/// Deliver one event to the socket it belongs to.
fn route(stack: &mut VsockStack, event: &VsockEvent) {
    match event.event_type {
        // The host asked to connect to a port. The manager already
        // answered on the listening port's behalf; the family's part is
        // queuing the connection for `accept` to hand out.
        VsockEventType::ConnectionRequest => {
            let port = event.destination.port;
            if let Some(listener) = stack
                .sockets
                .iter()
                .filter_map(|slot| slot.as_ref())
                .find(|sock| matches!(sock.phase(), Phase::Listening) && sock.port() == Some(port))
            {
                listener.enqueue(event.source);
            } else {
                // A listener that closed between the manager's answer and
                // this poll. The connection is refused the way a closed
                // port is: with a reset, not a silence.
                let _ = stack.dev.force_close(event.source, port);
            }
        }
        // The handshake finished. The connecting socket becomes connected,
        // and whatever task parked in `connect` finds it on its next look.
        VsockEventType::Connected => {
            if let Some(sock) = find_connected_to(stack, &event.source, event.destination.port) {
                sock.set_phase(Phase::Connected);
            }
        }
        // The peer is gone. The socket reads as closed from here on; bytes
        // already buffered stay readable until drained, which is what end
        // of file after buffered data means.
        VsockEventType::Disconnected { .. } => {
            if let Some(sock) = find_connected_to(stack, &event.source, event.destination.port) {
                sock.set_phase(Phase::Closed);
            }
        }
        // Data and credit travel in the manager's bookkeeping; a socket
        // reading drains it by (peer, port), so there is nothing to move.
        VsockEventType::Received { .. }
        | VsockEventType::CreditRequest
        | VsockEventType::CreditUpdate => {}
    }
}

/// The socket connected to `peer` from local `port`, if it is in a phase
/// where such news means anything.
fn find_connected_to(stack: &VsockStack, peer: &VsockAddr, port: u32) -> Option<Arc<VsockSock>> {
    stack
        .sockets
        .iter()
        .filter_map(|slot| slot.as_ref())
        .find(|sock| {
            matches!(sock.phase(), Phase::Connecting | Phase::Connected)
                && sock.port() == Some(port)
                && sock
                    .peer()
                    .is_some_and(|p| p.cid == peer.cid && p.port == peer.port)
        })
        .cloned()
}

// ---------------------------------------------------------------------------
// One socket, as the fd table sees it
// ---------------------------------------------------------------------------

/// Where a socket is between `socket(2)` and the end of its connection.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    /// Fresh from `socket(2)`, or bound to no port.
    Fresh,
    /// `connect(2)` sent its request; the answer is an event away.
    Connecting,
    /// The stream is up.
    Connected,
    /// `listen(2)` opened the queue.
    Listening,
    /// The peer is gone, or never answered.
    Closed,
}

struct VsockState {
    phase: Phase,
    /// The local port: what `bind` recorded, what an autobind picked, or
    /// what an accepted connection was requested on. `None` until one of
    /// those has happened.
    local_port: Option<u32>,
    peer: Option<VsockAddr>,
    /// The accepted connections: peers the host already believes connected,
    /// waiting for `accept` to mint their descriptors.
    backlog: VecDeque<VsockAddr>,
    /// The two half-close bits, local facts the send and recv paths check.
    shut_read: bool,
    shut_write: bool,
}

pub struct VsockSock {
    id: usize,
    state: Mutex<RefCell<VsockState>>,
}

impl VsockSock {
    fn new(id: usize) -> Arc<Self> {
        Arc::new(VsockSock {
            id,
            state: Mutex::new(RefCell::new(VsockState {
                phase: Phase::Fresh,
                local_port: None,
                peer: None,
                backlog: VecDeque::new(),
                shut_read: false,
                shut_write: false,
            })),
        })
    }

    /// This socket's registry id - how the close paths find it again from
    /// nothing but an inode.
    pub fn id(&self) -> usize {
        self.id
    }

    fn port(&self) -> Option<u32> {
        critical_section::with(|cs| self.state.borrow(cs).borrow().local_port)
    }

    fn peer(&self) -> Option<VsockAddr> {
        critical_section::with(|cs| self.state.borrow(cs).borrow().peer)
    }

    fn phase(&self) -> Phase {
        critical_section::with(|cs| self.state.borrow(cs).borrow().phase)
    }

    fn set_phase(&self, phase: Phase) {
        critical_section::with(|cs| self.state.borrow(cs).borrow_mut().phase = phase);
    }

    /// Queue one accepted connection for `accept`. The backlog's bound is
    /// the same clamp Linux applies: a `listen` backlog is a hint, and the
    /// host has already been told yes, so the queue holds what arrived.
    fn enqueue(&self, peer: VsockAddr) {
        critical_section::with(|cs| {
            let mut state = self.state.borrow(cs).borrow_mut();
            if state.backlog.len() < 4096 {
                state.backlog.push_back(peer);
            }
        });
    }

    fn take_pending(&self) -> Option<VsockAddr> {
        critical_section::with(|cs| self.state.borrow(cs).borrow_mut().backlog.pop_front())
    }

    /// The socket's own readiness, for `poll(2)` and the file doors: a
    /// listener with a queued connection is readable, a connected socket
    /// with buffered bytes is readable, a closed one is readable because
    /// its `read` answers end of file rather than blocking, and a
    /// connected socket that is not shut for writing can take bytes.
    fn readiness(&self) -> Ready {
        let phase = self.phase();
        critical_section::with(|cs| {
            let state = self.state.borrow(cs).borrow();
            let dev = STACK.borrow(cs).borrow().as_ref().map(|st| st.dev.clone());
            let read = match phase {
                Phase::Listening => !state.backlog.is_empty(),
                Phase::Closed => true,
                _ => {
                    !state.shut_read
                        && dev.is_some_and(|dev| {
                            state
                                .peer
                                .zip(state.local_port)
                                .is_some_and(|(peer, port)| dev.recv_available(peer, port) > 0)
                        })
                }
            };
            let write = phase == Phase::Connected && !state.shut_write && state.peer.is_some();
            Ready { read, write }
        })
    }
}

impl Inode for VsockSock {
    /// A socket has no position: `lseek(2)` on one is `ESPIPE`.
    fn seekable(&self) -> bool {
        false
    }

    fn metadata(&self) -> Metadata {
        Metadata {
            ino: 0x7000_c000 + self.id as u64,
            kind: NodeKind::CharDev,
            size: 0,
        }
    }

    fn read_at(&self, _offset: u64, buf: &mut [u8]) -> VfsResult<usize> {
        recv_blocking(self, buf, false).map_err(vfs_error_of)
    }

    fn write_at(&self, _offset: u64, buf: &[u8]) -> VfsResult<usize> {
        send_blocking(self, buf, false).map_err(vfs_error_of)
    }

    fn poll_ready(&self) -> Ready {
        self.readiness()
    }

    fn vsock_id(&self) -> usize {
        self.id
    }
}

/// The `VfsError` an operation that failed in errno terms reports, for the
/// read and write doors a plain `read(2)` on a vsock fd takes.
fn vfs_error_of(code: isize) -> VfsError {
    match code {
        err::EAGAIN => VfsError::Again,
        err::EPIPE => VfsError::BrokenPipe,
        err::ECONNREFUSED | err::ENOTCONN => VfsError::NotConnected,
        _ => VfsError::Corrupt,
    }
}

// ---------------------------------------------------------------------------
// Registration and the syscall-facing operations
// ---------------------------------------------------------------------------

/// `socket(AF_VSOCK, SOCK_STREAM, 0)`: mint a socket and register it.
/// `None` is a family with no device behind it - the caller answers
/// `EAFNOSUPPORT`, not `ENODEV`, because the family is what was refused.
pub fn alloc(typ: u16) -> Option<Arc<VsockSock>> {
    if typ != super::socket::SOCK_STREAM {
        return None;
    }
    critical_section::with(|cs| {
        let mut stack = STACK.borrow(cs).borrow_mut();
        let stack = stack.as_mut()?;
        // The id is the slot's index plus one, so a zero never names a real
        // socket - the same numbering the network registry keeps.
        let (sock, pos) = match stack.sockets.iter().position(|slot| slot.is_none()) {
            Some(pos) => {
                let sock = VsockSock::new(pos + 1);
                stack.sockets[pos] = Some(sock.clone());
                (sock, pos)
            }
            None => {
                let sock = VsockSock::new(stack.sockets.len() + 1);
                stack.sockets.push(Some(sock.clone()));
                (sock, stack.sockets.len() - 1)
            }
        };
        debug_assert_eq!(pos + 1, sock.id);
        Some(sock)
    })
}

/// `bind(fd, sockaddr_vm)`: name the local port. The CID is a formality -
/// the guest's own is the only one the device answers to - and a port of
/// zero asks for an ephemeral one, as on Linux.
pub fn bind(sock: &VsockSock, port: u32) -> isize {
    critical_section::with(|cs| {
        let mut stack = STACK.borrow(cs).borrow_mut();
        let Some(stack) = stack.as_mut() else {
            return err::EBADF;
        };
        // The phase and the port are read first and the borrow dropped:
        // the scan below reads every socket's state, this one's included -
        // it is in the table - and a borrow held across that scan would
        // collide with itself.
        let (phase, bound) = {
            let state = sock.state.borrow(cs).borrow();
            (state.phase, state.local_port.is_some())
        };
        match phase {
            Phase::Fresh | Phase::Listening | Phase::Connecting => {}
            _ => return err::EINVAL,
        }
        if bound {
            return err::EINVAL;
        }
        let port = if port == 0 {
            loop {
                let candidate = stack.next_ephemeral;
                stack.next_ephemeral += 1;
                if stack.next_ephemeral > EPHEMERAL_MAX {
                    stack.next_ephemeral = EPHEMERAL_MIN;
                }
                // One name, one socket - the same check the network stack's
                // bind makes, from the one table that sees every socket.
                if !stack.sockets.iter().any(|slot| {
                    slot.as_ref()
                        .is_some_and(|other| other.port() == Some(candidate))
                }) {
                    break candidate;
                }
            }
        } else {
            port
        };
        let taken = stack.sockets.iter().any(|slot| {
            slot.as_ref()
                .is_some_and(|other| other.port() == Some(port))
        });
        if taken {
            return err::EADDRINUSE;
        }
        sock.state.borrow(cs).borrow_mut().local_port = Some(port);
        0
    })
}

/// `listen(fd, backlog)`: open the accept queue. Unbound sockets autobind
/// first, which is what Linux does and what a caller that skips `bind`
/// expects to work.
pub fn listen(sock: &VsockSock, _backlog: i32) -> isize {
    let Some(dev) =
        critical_section::with(|cs| STACK.borrow(cs).borrow().as_ref().map(|st| st.dev.clone()))
    else {
        return err::EBADF;
    };
    if sock.phase() != Phase::Fresh {
        return err::EINVAL;
    }
    if sock.port().is_none() {
        let rc = bind(sock, 0);
        if rc != 0 {
            return rc;
        }
    }
    let Some(port) = sock.port() else {
        return err::EINVAL;
    };
    dev.listen(port);
    sock.set_phase(Phase::Listening);
    0
}

/// `connect(fd, sockaddr_vm)`: ask the host for a stream, and unless the
/// caller asked not to wait, drive the handshake to an answer - the same
/// poll loop the TCP connect runs, deadline included.
pub fn connect(sock: &VsockSock, cid: u32, port: u32, nonblock: bool) -> isize {
    let Some(dev) =
        critical_section::with(|cs| STACK.borrow(cs).borrow().as_ref().map(|st| st.dev.clone()))
    else {
        return err::EBADF;
    };
    let guest_cid = dev.guest_cid() as u32;

    // The socket's port, peer and phase all move under the stack lock,
    // where the ephemeral allocation lives - and the one pass that sets
    // them is also the one that rules out the states a connect cannot
    // start from.
    let local_port = critical_section::with(|cs| {
        let mut stack = STACK.borrow(cs).borrow_mut();
        let Some(stack) = stack.as_mut() else {
            return Err(err::EBADF);
        };
        // The state is read first and each borrow dropped: the port scan
        // below reads every socket's state - this one's included, it is in
        // the table - and a borrow held across that scan would collide
        // with itself.
        let phase = {
            let state = sock.state.borrow(cs).borrow();
            state.phase
        };
        match phase {
            Phase::Connected | Phase::Connecting => return Err(err::EISCONN),
            // A listener has a role already; connected-to is not it.
            Phase::Listening => return Err(err::EINVAL),
            Phase::Fresh | Phase::Closed => {}
        }
        // The autobind a connect on a fresh socket performs, so
        // `getsockname` can answer before the peer does.
        let bound = {
            let state = sock.state.borrow(cs).borrow();
            state.local_port
        };
        let port = match bound {
            Some(port) => port,
            None => loop {
                let candidate = stack.next_ephemeral;
                stack.next_ephemeral += 1;
                if stack.next_ephemeral > EPHEMERAL_MAX {
                    stack.next_ephemeral = EPHEMERAL_MIN;
                }
                if !stack.sockets.iter().any(|slot| {
                    slot.as_ref()
                        .is_some_and(|other| other.port() == Some(candidate))
                }) {
                    break candidate;
                }
            },
        };
        let mut state = sock.state.borrow(cs).borrow_mut();
        state.local_port = Some(port);
        // The device's VsockAddr carries the CID as the protocol spells it -
        // 64 bits - while the sockaddr this kernel's callers parse is the
        // 32-bit `sockaddr_vm`. The cast is the ABI's own narrowing: a CID
        // that did not fit a u32 is not one this guest can be told about.
        state.peer = Some(VsockAddr {
            cid: cid as u64,
            port,
        });
        state.phase = Phase::Connecting;
        state.shut_read = false;
        state.shut_write = false;
        Ok(port)
    });
    let local_port = match local_port {
        Ok(port) => port,
        Err(e) => return e,
    };

    if cid == CID_ANY || cid == guest_cid {
        // Nowhere for the request to go: the wildcard is not an address,
        // and dialing the guest's own CID has no loopback the device
        // serves. EADDRNOTAVAIL is what a Linux vsock connect answers for
        // an address it cannot reach.
        sock.set_phase(Phase::Closed);
        return err::EADDRNOTAVAIL;
    }

    if !dev.connect(
        VsockAddr {
            cid: cid as u64,
            port,
        },
        local_port,
    ) {
        sock.set_phase(Phase::Closed);
        return err::ECONNREFUSED;
    }

    if nonblock {
        // Started, not finished - the shape a non-blocking connect promises.
        return err::EINPROGRESS;
    }

    // The handshake loop: the service task routes the `Connected` event to
    // this socket, so the wait is a look at the phase per tick, with a
    // fatal signal and a deadline checked on every round - thirty seconds,
    // the same silence a TCP connect answers ETIMEDOUT for.
    let deadline = crate::hal::ticks() + 30 * crate::hal::TICK_HZ as u64;
    loop {
        match sock.phase() {
            Phase::Connected => return 0,
            Phase::Closed => return err::ECONNREFUSED,
            _ => {}
        }
        service();
        if crate::sched::has_deliverable_signal() {
            return err::EINTR;
        }
        if crate::hal::ticks() >= deadline {
            return err::ETIMEDOUT;
        }
        crate::sched::sleep_ticks(1);
    }
}

/// `accept(fd, addr, addrlen)`: hand out a connection the host made. The
/// new socket arrives as a registered, connected socket; the caller opens
/// the descriptor and writes the peer's name, which is why the peer comes
/// back with the socket.
pub fn accept(listener: &VsockSock, nonblock: bool) -> Result<(Arc<VsockSock>, VsockAddr), isize> {
    if listener.phase() != Phase::Listening {
        return Err(err::EINVAL);
    }
    loop {
        if let Some(peer) = listener.take_pending() {
            let Some(sock) = alloc(super::socket::SOCK_STREAM) else {
                return Err(err::EBADF);
            };
            critical_section::with(|cs| {
                let mut state = sock.state.borrow(cs).borrow_mut();
                state.phase = Phase::Connected;
                state.peer = Some(peer);
                state.local_port = Some(listener.port().unwrap_or(0));
            });
            return Ok((sock, peer));
        }
        if nonblock {
            return Err(err::EAGAIN);
        }
        if crate::sched::has_deliverable_signal() {
            return Err(err::EINTR);
        }
        crate::sched::sleep_ticks(1);
    }
}

/// Push bytes into a connected stream, chunked to what a peer's credit
/// window can take in one packet. A partial send is a completed answer,
/// as on every stream socket; the blocking loop covers the case where the
/// peer's window is closed for now.
pub(crate) fn send_blocking(sock: &VsockSock, data: &[u8], nonblock: bool) -> Result<usize, isize> {
    let Some(peer) = sock.peer() else {
        return Err(err::ENOTCONN);
    };
    let Some(port) = sock.port() else {
        return Err(err::ENOTCONN);
    };
    if sock.phase() != Phase::Connected {
        // A peer that reset mid-stream is a broken pipe, and bytes written
        // into a closed connection report exactly that.
        if sock.phase() == Phase::Closed {
            return Err(err::EPIPE);
        }
        return Err(err::ENOTCONN);
    }
    if critical_section::with(|cs| sock.state.borrow(cs).borrow().shut_write) {
        return Err(err::EPIPE);
    }

    let Some(dev) =
        critical_section::with(|cs| STACK.borrow(cs).borrow().as_ref().map(|st| st.dev.clone()))
    else {
        return Err(err::EBADF);
    };

    let mut sent = 0usize;
    while sent < data.len() {
        let piece = &data[sent..data.len().min(sent + SEND_CHUNK)];
        if dev.send(peer, port, piece) {
            sent += piece.len();
            continue;
        }
        // The window is closed or the peer is gone. A tick later the
        // credit update may have arrived; a closed connection is learned
        // from the events the service routes.
        if nonblock {
            return if sent > 0 { Ok(sent) } else { Err(err::EAGAIN) };
        }
        if crate::sched::has_deliverable_signal() {
            return if sent > 0 { Ok(sent) } else { Err(err::EINTR) };
        }
        if sock.phase() != Phase::Connected {
            return if sent > 0 { Ok(sent) } else { Err(err::EPIPE) };
        }
        crate::sched::sleep_ticks(1);
    }
    Ok(sent)
}

/// Take bytes out of a connected stream's buffer. A drained connection
/// whose peer has gone answers end of file, which is the fact a reader
/// waiting for the last of a shutdown needs to see.
pub(crate) fn recv_blocking(
    sock: &VsockSock,
    out: &mut [u8],
    nonblock: bool,
) -> Result<usize, isize> {
    let Some(peer) = sock.peer() else {
        return Err(err::ENOTCONN);
    };
    let Some(port) = sock.port() else {
        return Err(err::ENOTCONN);
    };
    if critical_section::with(|cs| sock.state.borrow(cs).borrow().shut_read) {
        return Ok(0);
    }

    let Some(dev) =
        critical_section::with(|cs| STACK.borrow(cs).borrow().as_ref().map(|st| st.dev.clone()))
    else {
        return Err(err::EBADF);
    };

    loop {
        let got = dev.recv(peer, port, out);
        if got > 0 {
            return Ok(got);
        }
        if sock.phase() == Phase::Closed {
            // Everything buffered was drained; the peer is gone. End of
            // file, the answer `read(2)` keeps for a finished stream.
            return Ok(0);
        }
        if nonblock {
            return Err(err::EAGAIN);
        }
        if crate::sched::has_deliverable_signal() {
            return Err(err::EINTR);
        }
        crate::sched::sleep_ticks(1);
    }
}

/// `shutdown(fd, how)`: the half-close the protocol carries. A write
/// shutdown sends the packet; a read shutdown is a local fact, and the
/// buffered bytes keep arriving until the socket says otherwise.
pub fn shutdown(sock: &VsockSock, how: usize) -> isize {
    const SHUT_RD: usize = 0;
    const SHUT_WR: usize = 1;
    const SHUT_RDWR: usize = 2;

    let Some(dev) =
        critical_section::with(|cs| STACK.borrow(cs).borrow().as_ref().map(|st| st.dev.clone()))
    else {
        return err::EBADF;
    };
    let (peer, port) = match sock.peer().zip(sock.port()) {
        Some(pair) if sock.phase() == Phase::Connected => pair,
        _ => return err::ENOTCONN,
    };
    if how == SHUT_RD || how == SHUT_RDWR {
        critical_section::with(|cs| sock.state.borrow(cs).borrow_mut().shut_read = true);
    }
    if how == SHUT_WR || how == SHUT_RDWR {
        critical_section::with(|cs| sock.state.borrow(cs).borrow_mut().shut_write = true);
        // The packet the protocol offers says both directions; the local
        // bits are the half the caller actually asked for.
        if !dev.shutdown(peer, port) {
            return err::ENOTCONN;
        }
    }
    0
}

/// The last close of a socket: retire the slot, tell the device. The
/// connection - if one was ever made - is reset rather than shut, because
/// nobody is left to read the peer's farewell.
pub fn release(id: usize) {
    critical_section::with(|cs| {
        let mut stack = STACK.borrow(cs).borrow_mut();
        let Some(stack) = stack.as_mut() else { return };
        let Some(sock) = stack.sockets.get(id.wrapping_sub(1)).cloned().flatten() else {
            return;
        };
        stack.sockets[id - 1] = None;
        let state = sock.state.borrow(cs).borrow();
        match state.phase {
            Phase::Listening => {
                if let Some(port) = state.local_port {
                    stack.dev.unlisten(port);
                }
            }
            Phase::Connected | Phase::Connecting => {
                if let (Some(peer), Some(port)) = (state.peer, state.local_port) {
                    let _ = stack.dev.force_close(peer, port);
                }
            }
            _ => {}
        }
    });
}

/// `getsockname`'s facts: the guest CID and the local port, zero when
/// nothing is bound yet - the all-zero answer a fresh socket gives.
pub fn sockname(sock: &VsockSock) -> (u32, u32) {
    let cid = guest_cid().unwrap_or(0);
    (cid, sock.port().unwrap_or(0))
}

/// `getpeername`'s facts, or `None` for a socket with no peer.
pub fn peername(sock: &VsockSock) -> Option<(u32, u32)> {
    if sock.phase() != Phase::Connected && sock.phase() != Phase::Closed {
        return None;
    }
    // The CID comes back as the 32 bits `sockaddr_vm` carries, narrowed
    // from the device's 64-bit spelling - the same cast the connect above
    // widened in the other direction.
    sock.peer().map(|peer| (peer.cid as u32, peer.port))
}

/// Write a `sockaddr_vm` and its length - the shape `accept`,
/// `getsockname` and `getpeername` owe their callers.
pub fn write_sockaddr_vm(ptr: usize, len_ptr: usize, cid: u32, port: u32) -> isize {
    if ptr == 0
        || len_ptr == 0
        || !crate::uaccess::buffer_ok(ptr, SOCKADDR_VM_LEN, true, true)
        || !crate::uaccess::buffer_ok(len_ptr, 4, true, true)
    {
        return err::EFAULT;
    }
    unsafe {
        let at = |offset: usize| (ptr + offset) as *mut u8;
        (at(0) as *mut u16).write_unaligned(AF_VSOCK);
        (at(2) as *mut u16).write_unaligned(0); // svm_reserved1
        (at(4) as *mut u32).write_unaligned(port);
        (at(8) as *mut u32).write_unaligned(cid);
        (len_ptr as *mut u32).write_unaligned(SOCKADDR_VM_LEN as u32);
    }
    0
}

/// Parse a user `sockaddr_vm` into `(cid, port)`. Only the family's own
/// shape passes, and only with the reserved word zero - the check the
/// kernel makes before trusting either field.
pub fn parse_sockaddr_vm(ptr: usize, len: usize, from_user: bool) -> Result<(u32, u32), isize> {
    if ptr == 0 || len < SOCKADDR_VM_LEN {
        return Err(err::EINVAL);
    }
    if !crate::uaccess::buffer_ok(ptr, SOCKADDR_VM_LEN, false, from_user) {
        return Err(err::EFAULT);
    }
    let raw = unsafe { core::slice::from_raw_parts(ptr as *const u8, SOCKADDR_VM_LEN) };
    let family = u16::from_ne_bytes([raw[0], raw[1]]);
    if family != AF_VSOCK {
        return Err(err::EAFNOSUPPORT);
    }
    let reserved = u16::from_ne_bytes([raw[2], raw[3]]);
    if reserved != 0 {
        return Err(err::EINVAL);
    }
    let port = u32::from_le_bytes([raw[4], raw[5], raw[6], raw[7]]);
    let cid = u32::from_le_bytes([raw[8], raw[9], raw[10], raw[11]]);
    Ok((cid, port))
}
