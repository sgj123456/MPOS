//! The network stack: smoltcp driven by the virtio-net adapter.
//!
//! The protocol machinery - IP, ICMP, ARP, TCP, UDP, retransmission, window
//! management - is smoltcp's. What this module adds is the two seams around
//! it: a [`Device`] implementation that moves frames between smoltcp and the
//! virtio-net driver, and a socket registry that gives every userspace socket
//! a stable handle the fd table can name.
//!
//! # Single core, one lock, no sleeping under it
//!
//! Everything the stack owns lives in one [`Stack`] behind
//! `critical_section::Mutex` - the same lock discipline as the rest of the
//! kernel. The rule that follows from it: no network operation may sleep
//! while holding the lock. Blocking calls therefore run a loop of
//! *lock, poll, try, unlock, park*, where the park ends on the stack's own
//! wakeup call - every task waiting on a socket registers itself with
//! [`subscribe`], and [`service`] wakes the whole list after every poll, so
//! a blocked reader resumes the instant anything moved rather than on the
//! next clock tick. A background task polls the interface once a tick, which
//! is what keeps a connection progressing while nobody is blocked on it.
//!
//! # Addresses
//!
//! The stack comes up static: 10.0.2.15/24 with gateway 10.0.2.2, the shape
//! QEMU's user networking always hands out. DNS is not served by the kernel -
//! musl resolves in userspace over UDP sockets, which is why the UDP path has
//! to work but no resolver lives here.
//!
//! # Two fabrics, because a route lookup is destination-only
//!
//! The loopback is real here - `127.0.0.1` is an address of an interface whose
//! device hands every transmitted frame back to the receiver - and it is a
//! *second* interface with a *second* socket set, which is the part that is not
//! optional. smoltcp's `Interface::route` answers `Some(dst)` when the
//! interface carries an address in `dst`'s prefix and otherwise hands the
//! question to the route table; and `Interface::poll` walks every socket in the
//! set it is given. One interface with one set therefore cannot serve both
//! wires: a `127.0.0.1` connect would either match eth0's default route and
//! leave by the NIC, or - with `127.0.0.0/8` added to eth0 - route "directly"
//! and still leave by the NIC. So a socket belongs to exactly one of the two,
//! [`Home`] records which, and it is decided by the first call that names an
//! address (`bind`, `connect`, `sendto`), since smoltcp has no operation that
//! moves a socket between sets.
//!
//! A raw socket is the one kind that cannot choose: its destination is a
//! per-call argument and one socket cannot serve two sets, so it stays on eth0
//! - which is why `ping 127.0.0.1` still travels the virtual wire (and still
//! works, through slirp) rather than the loopback.

pub mod packet;
/// The seam's frame counters and buffer pool (§10.4).
///
/// Reached only through `crate::net`, which the host build skips - so its
/// pins live in `t-netstats`, the program that boots, rather than a
/// `cfg(test)` module here that would never run (`gates.rs`'s dead-host-test
/// check enforces exactly that reading, and `net/socket.rs`'s tail note is the
/// precedent).
pub mod pool;
pub mod rtnl;
pub mod socket;
pub mod unix;
pub mod vsock;

use alloc::collections::BTreeMap;
use alloc::collections::VecDeque;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::cell::RefCell;
use core::sync::atomic::{AtomicU64, Ordering};

use critical_section::Mutex;
use smoltcp::iface::{Config, Interface, Route, SocketHandle, SocketSet};
use smoltcp::phy::{Device, DeviceCapabilities, Medium, RxToken, TxToken};
use smoltcp::socket::raw as raw_sock;
use smoltcp::socket::tcp;
use smoltcp::socket::udp;
use smoltcp::time::Instant as SmolInstant;
use smoltcp::wire::{
    EthernetAddress, HardwareAddress, IpAddress, IpCidr, IpEndpoint, IpListenEndpoint, IpProtocol,
    Ipv4Address,
};

use crate::drivers::virtio::NetDevice;

/// The address QEMU's user network hands out on its virtual wire.
pub(crate) const LOCAL_IP: [u8; 4] = [10, 0, 2, 15];
/// QEMU's default gateway on the same virtual wire.
pub(crate) const GATEWAY_IP: [u8; 4] = [10, 0, 2, 2];
const PREFIX_LEN: u8 = 24;
/// The virtual router: everything not on the local wire goes here.
const GATEWAY: [u8; 4] = [10, 0, 2, 2];
const MTU: usize = 1500;

/// Where anonymous local ports start - the range Linux draws from, so a port
/// in it never collides with one a server would deliberately bind.
const EPHEMERAL_MIN: u16 = 32768;
const EPHEMERAL_MAX: u16 = 60999;

/// Per-socket TCP window. A bigger receive window lets a bulk transfer fill
/// the pipe; the transmit side needs less, since slirp drains it promptly.
///
/// The receive side is sized against the device's ring rather than picked
/// freely: the window advertised to a peer is `min(this buffer, the ring's
/// capacity)`, so a buffer much larger than the ring would only promise
/// something the ring cannot hold. Sixty-four kilobytes is under the 128 KiB
/// the sixty-four receive buffers add up to, which is the pair this machine
/// was measured with.
const TCP_RX_BYTES: usize = 64 * 1024;
const TCP_TX_BYTES: usize = 16 * 1024;

/// The wildcard raw door's receive queue (`Channel::Raw`'s field, and the type
/// `socket::echo_to_raw_doors` fills): complete IPv4 packets, oldest first,
/// each at the length it arrived with. Named here because both this module's
/// channel and the socket module's seam and reader reach the same cells.
///
/// `Vec<u8>` rather than a fixed slot because a slot is a length this kernel
/// would be inventing: the reader hands the caller what it pops, so a
/// 512-byte slot made a 300-byte packet read back as 512 and dropped QEMU's
/// 576-byte BOOTP reply whole. The bound is the *count* (`socket::RAW_ECHO_
/// FRAMES`), which is the one that says how much memory a stopped reader can
/// cost the kernel.
pub(crate) type EchoQueue = alloc::sync::Arc<
    critical_section::Mutex<core::cell::RefCell<alloc::collections::VecDeque<alloc::vec::Vec<u8>>>>,
>;

/// Datagrams the receive side can hold at once, and the shared byte pool
/// behind them. A DNS answer fits in under a kilobyte; four slots cover a
/// resolver that fires at two servers at once.
const UDP_PACKET_SLOTS: usize = 4;
const UDP_PAYLOAD_BYTES: usize = 4 * 1024;

/// How one open socket is wired into the stack: which smoltcp socket it owns,
/// and the endpoint state the syscalls layered on top of it maintain.
pub(crate) enum Channel {
    Tcp {
        /// Which interface's socket set the handle belongs to. A TCP socket is
        /// created on eth0 - the common case - and moved to the loopback when
        /// the first address it is given is one.
        home: Home,
        handle: SocketHandle,
        /// What `bind` recorded, if it ran. `connect` takes this as its
        /// source - a stream socket has nothing to bind before it connects,
        /// so the record is kept until then.
        local: Option<IpEndpoint>,
    },
    Udp {
        home: Home,
        handle: SocketHandle,
        /// The endpoint `connect` pinned, if it did. `send` without a target
        /// uses it; `sendto` overrides per call.
        peer: Option<IpEndpoint>,
        /// Whether `bind` (or the automatic bind on first use) has run.
        bound: bool,
        /// What was actually bound - port and all - so `getsockname` answers
        /// from the record rather than from the socket.
        local: Option<IpEndpoint>,
    },
    /// An `AF_INET6` socket with no machinery behind it. The stack speaks
    /// IPv4 only, but Linux always lets `socket(AF_INET6)` open - the failure
    /// shows up at `connect` as ENETUNREACH, which is what lets a caller
    /// walking a getaddrinfo list fall through to the IPv4 answer. Faking
    /// that shape keeps musl's dual-family resolve honest; refusing the
    /// socket outright turns the whole lookup into EAFNOSUPPORT.
    Inet6,
    /// A `listen`ing TCP socket: the passive-open side, which smoltcp has no
    /// socket of its own for.
    ///
    /// smoltcp's `tcp::Socket` is *one* connection - a listening socket that
    /// completes a handshake stops listening - so a listener here is a **pool**
    /// of sockets in `Listen` plus the queue of the ones that completed, kept
    /// in [`Stack::listeners`] under `id`. That is the shape smoltcp's own
    /// `listen` doc points at: re-listening an open socket needs an `abort`
    /// first, "but this means that incoming connections can actually get
    /// aborted between the abort() and the next listen()". The channel names
    /// the pool rather than a socket because there is no one socket to name.
    TcpListen {
        id: usize,
        /// The address the pool listens on, for `getsockname`: the caller's
        /// bound name, or the wildcard Linux's implicit bind gives an unbound
        /// `listen`.
        local: IpEndpoint,
    },
    /// A raw IP socket: sends and receives complete IP packets with a given
    /// protocol number. `ping` uses this with `IPPROTO_ICMP` (1) to send
    /// ICMP echo requests and receive replies.
    ///
    /// Always [`Home::Eth`], and it is the one channel whose home is not a
    /// decision: a raw socket's destination is a per-call argument (`sendto`),
    /// so a socket that could be aimed at the loopback *and* at the wire would
    /// have to be in both sets. Keeping it on the wire is what leaves
    /// `ping 127.0.0.1` behaving exactly as it did - it travels the virtual
    /// wire, and slirp answers it.
    Raw {
        home: Home,
        handle: SocketHandle,
        /// The IP protocol number this socket is bound to (e.g. 1 for ICMP).
        protocol: u8,
        /// The wildcard door (`IPPROTO_RAW`) keeps its own queue of complete
        /// IPv4 packets here, because it cannot keep them in smoltcp: a raw
        /// socket with *no* protocol filter matches every IPv4 packet, and the
        /// interface then reports each one *handled* (`iface/interface/ipv4.rs`:
        /// `process_udp`/`process_tcp` take the flag from `raw_socket_filter`),
        /// stealing the machine's UDP and TCP delivery. A bounded copy at the
        /// seam - Linux's `recvfrom` answer for this socket is "the packet you
        /// sent, if you are root on a box that echoes", which no client asks -
        /// is honest where an unbounded steal is not. Newest kept, oldest lost,
        /// the same shape as a packet socket's ring. An `Arc` so the seam can
        /// reach it by handle without nesting the stack lock inside itself.
        echoed: EchoQueue,
    },
}

/// Which of the stack's two interfaces a socket belongs to.
///
/// Two, not one, because routing here is by destination alone (see the module
/// header): a socket must be polled by the interface that can actually carry
/// its packets, and smoltcp has no way to move one between socket sets. The
/// value is a *fact about the socket* rather than a property of the address,
/// because it is decided once - by the first `bind`/`connect`/`sendto` - and
/// every later lookup resolves the handle through it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Home {
    /// `eth0`: the virtio NIC and QEMU's user network.
    Eth,
    /// `lo`: the loopback device, carrying `127.0.0.0/8`.
    Lo,
}

/// The syscall-visible facts about one socket, indexed by handle - the number
/// a [`socket::SocketInode`] carries and every operation resolves through.
pub(crate) struct Entry {
    /// The address family the socket was created with. Recorded rather than
    /// consulted today - only `AF_INET` gets past `socket()` - but the
    /// registry's job is to keep the facts, not just the ones in use.
    #[allow(dead_code)]
    pub family: u16,
    pub typ: u16,
    pub channel: Channel,
    /// The socket options `setsockopt` stored and `getsockopt` reads back. Kept
    /// per-socket so a set is not visible on its neighbours, and a query answers
    /// what the process asked for rather than a constant.
    pub opts: SockOpts,
    // A wildcard door's parked readers are *not* here. They live on the door
    // record the seam keeps (`socket::RawDoor`), because the seam runs inside
    // `poll_once` and cannot take this registry's lock to find them - a door
    // reached through the stack would re-enter `NET`'s borrow, which panics
    // (§4.5). The queue (`Channel::Raw`'s `echoed`) is the shared cell that
    // lets the two lists stay apart.
}

/// The `setsockopt`/`getsockopt` state a socket carries. The buffer sizes get
/// the stack's defaults so a query is a real number, not zero; the flags record
/// what was asked for.
#[derive(Clone, Copy, Debug)]
pub(crate) struct SockOpts {
    pub reuseaddr: bool,
    pub keepalive: bool,
    pub nodelay: bool,
    /// `IP_RECVTTL`: every received message also carries the hop count it
    /// arrived with, as a control message. `ping` reads its `ttl=` field
    /// from exactly that, and prints a stack word of garbage when the
    /// answer it asked for never comes.
    pub recv_ttl: bool,
    /// `struct linger`'s two fields, kept apart so `getsockopt` can rebuild it.
    pub linger_on: bool,
    pub linger_sec: i32,
    pub rcvbuf: u32,
    pub sndbuf: u32,
}

/// The receive/transmit buffer size a fresh socket reports until it is asked to
/// change - `smoltcp`'s own default window, rounded to a round number.
const SOCK_BUF_DEFAULT: u32 = 64 * 1024;

impl Default for SockOpts {
    fn default() -> SockOpts {
        SockOpts {
            reuseaddr: false,
            keepalive: false,
            nodelay: false,
            recv_ttl: false,
            linger_on: false,
            linger_sec: 0,
            rcvbuf: SOCK_BUF_DEFAULT,
            sndbuf: SOCK_BUF_DEFAULT,
        }
    }
}

/// One interface, the device that carries it and the sockets that live on it.
///
/// The three always move together: an interface is polled *over* a device and
/// *with* a socket set, and smoltcp lets a socket move between sets by no
/// operation at all - so the grouping is not tidiness, it is the fact that
/// makes [`Home`] a decision rather than a lookup. The device is a type
/// parameter because the two fabrics have different ones ([`NicDevice`] and
/// smoltcp's own loopback), and only [`Stack::poll_home`] needs them unified.
struct Fabric<D> {
    iface: Interface,
    device: D,
    sockets: SocketSet<'static>,
    /// The `ip link set <dev> up|down` bit. Down, this fabric stops being
    /// polled, which is what makes a downed link read as a downed link: frames
    /// are neither sent nor received - including on the loopback, which is why
    /// the flag is here rather than a courtesy in the link dump.
    up: bool,
}

/// Advance one fabric, if `ip link set` has left it up. A free function rather
/// than a method because the two fabrics are different types: the match that
/// picks one cannot bind them to a single name, but each arm can call this.
fn poll_fabric<D: Device>(fabric: &mut Fabric<D>, now: SmolInstant) {
    if fabric.up {
        fabric
            .iface
            .poll(now, &mut fabric.device, &mut fabric.sockets);
    }
}

/// Begin a TCP handshake on a socket of one fabric. `Interface::context` is a
/// mutable borrow of the interface that must stay live across the socket's own
/// `connect`, so the two field borrows belong in one place - here - rather than
/// at every call site.
fn connect_in<D: Device>(
    fabric: &mut Fabric<D>,
    h: SocketHandle,
    remote: IpEndpoint,
    local: IpListenEndpoint,
) -> Result<(), tcp::ConnectError> {
    let sock = fabric.sockets.get_mut::<tcp::Socket>(h);
    sock.connect(fabric.iface.context(), remote, local)
}

/// One `listen`ing socket's passive-open machinery: the pool of sockets
/// currently in `Listen`, and the queue of connections that completed a
/// handshake and are waiting for an `accept`.
///
/// The pool is the shape smoltcp forces (see [`Channel::TcpListen`]) and it is
/// also where the backlog is honoured. A socket that completes a handshake
/// *leaves* the pool - smoltcp's socket is the connection now - so the pool is
/// refilled after every poll from what the queue leaves room for, and when the
/// queue is full it is not refilled at all: the next `SYN` then finds no
/// listening socket and is dropped, which is what a full backlog does on Linux
/// too, and it costs no `abort` to arrange.
pub(crate) struct Listener {
    /// Which interface's set the pool lives in. A listener is bound to one
    /// address, so it is on one wire, and every connection it hands out
    /// inherits this.
    home: Home,
    /// The address the pool listens on, in the form smoltcp wants it: the
    /// wildcard is `addr: None` here, and `0.0.0.0` in the channel.
    local: IpListenEndpoint,
    /// What the caller asked `listen` for. Linux admits while `queue_len >
    /// backlog`, so this is "one deeper than it names" - the same rule the
    /// unix family was measured into (§4.1).
    backlog: usize,
    /// Sockets in `Listen`. Between one and [`LISTEN_POOL_MAX`].
    listening: Vec<SocketHandle>,
    /// Completed connections, in arrival order. Bounded by `backlog + 1` plus
    /// whatever the pool had in flight when the queue filled.
    accepted: VecDeque<SocketHandle>,
}

/// How many sockets one listener may keep in `Listen` at once.
///
/// Each `tcp::Socket` is 80 KiB of buffers, so this is a memory decision rather
/// than a semantic one - and it is a small number because it bounds *simultaneous
/// handshakes*, not the backlog: a burst deeper than this relies on the peer's
/// own retransmission, exactly as a queue drop would. Four is enough to complete
/// several connections in one poll without leaving 640 KiB of buffers on a
/// socket that will never use them.
const LISTEN_POOL_MAX: usize = 4;

/// The pool depth a backlog justifies: at least one socket has to be listening
/// or nothing can ever connect.
fn pool_depth(backlog: usize) -> usize {
    backlog.clamp(1, LISTEN_POOL_MAX)
}

/// The wire: the NIC and the interface over it - or nothing at all.
///
/// A net namespace owns `lo` and, if it is the machine's own, the one NIC. A
/// namespace made by `clone(CLONE_NEWNET)` has **no wire**: that is Linux's shape
/// too (a fresh namespace holds `lo` alone until something moves a device in
/// with `ip link set eth0 netns N`, which this kernel has no place for), so the
/// device and the interface are optional and every door that needs one answers
/// the errno a machine with no link gives.
///
/// The **socket set is not optional**, and that is the point: a [`Home`] names
/// *which set* a socket lives in, so the set has to exist or the name stops
/// meaning anything. A wire-less namespace's eth set is simply always empty -
/// nothing can be addressed to a link that is not there.
pub(crate) struct Wire {
    device: Option<NicDevice>,
    iface: Option<Interface>,
    sockets: SocketSet<'static>,
    up: bool,
}

impl Wire {
    /// Advance the wire over its device, unless `ip link set eth0 down` has
    /// taken it down. Nothing to advance without a wire, and nothing to advance
    /// *for*: a downed link is not polled, which is what makes `down` read as
    /// down rather than as a link that quietly keeps talking.
    fn poll(&mut self, now: SmolInstant) {
        if !self.up {
            return;
        }
        if let (Some(device), Some(iface)) = (self.device.as_mut(), self.iface.as_mut()) {
            iface.poll(now, device, &mut self.sockets);
        }
    }

    /// Begin a handshake on a socket of this wire. With no wire there is nothing
    /// to address: `Unaddressable` is smoltcp's own word for "this destination
    /// is not reachable from here", which the caller turns into `ENETUNREACH`.
    fn connect(
        &mut self,
        h: SocketHandle,
        remote: IpEndpoint,
        local: IpListenEndpoint,
    ) -> Result<(), tcp::ConnectError> {
        let Some(iface) = self.iface.as_mut() else {
            return Err(tcp::ConnectError::Unaddressable);
        };
        let sock = self.sockets.get_mut::<tcp::Socket>(h);
        sock.connect(iface.context(), remote, local)
    }
}

/// Everything the network stack owns, in one place behind one lock.
pub(crate) struct Stack {
    /// eth0: the virtio NIC and QEMU's user network - absent in a namespace
    /// with no wire.
    eth: Wire,
    /// lo: `127.0.0.0/8`, carried by smoltcp's own loopback device.
    lo: Fabric<smoltcp::phy::Loopback>,
    pub(crate) entries: Vec<Option<Entry>>,
    /// The passive-open state behind every `listen`ing socket, indexed by the
    /// id a [`Channel::TcpListen`] carries. Slots are reused, so a listener
    /// that goes away costs nothing but a `None`.
    listeners: Vec<Option<Listener>>,
    /// Sockets whose last descriptor is gone but whose teardown is still on the
    /// wire.
    ///
    /// `close(2)` has to unbind the fd immediately - the caller's whole point in
    /// closing is that the descriptor is free for reuse - but smoltcp only puts
    /// the `FIN` (or the `RST` an aborting `SO_LINGER` asks for) out from its
    /// poll loop. Removing the socket at that instant is how a close ends up
    /// saying nothing at all, and the peer finding out by writing into a void.
    ///
    /// They are listed here instead of discovered from the socket set because
    /// the registry slot that named them is already gone, and the list is
    /// bounded because a peer that stops answering may cost a slot, but not the
    /// set: past [`TEARDOWN_MAX`] the oldest is torn off without ceremony, which
    /// is exactly the abrupt close this is here to avoid - and the right price
    /// for one, rather than for thousands. The home rides with the handle
    /// because the socket to reap is in one of two sets.
    ///
    /// The third field is whether the socket had `SO_REUSEADDR` set when it was
    /// closed, and it is kept because a name outlives its descriptor: a
    /// `TIME-WAIT` 4-tuple still refuses `bind` for as long as it is here, and
    /// the option that lets a restart take that name back has to be read from
    /// the *holder* as well as from the caller (see
    /// [`Stack::tcp_name_taken`]).
    teardown: Vec<(Home, SocketHandle, bool)>,
    next_ephemeral: u16,
    /// Tasks parked on any socket's readiness, registered through
    /// [`subscribe`] and woken by [`service`] after every poll. One flat
    /// list rather than a waiter set per entry, because a wake is allowed to
    /// be spurious: the woken task re-checks its own condition and parks
    /// again if it is still false, which is the same licence `poll(2)` gives
    /// itself, and one list is less state to keep consistent.
    waiters: Vec<crate::sched::policy::TaskId>,
    /// eth0's name, settable by `ip link set eth0 name X` - what the link
    /// dump reports and what the address labels are derived from.
    eth_name: String,
}

/// The MTU `ip link set` writes, clamped to the floor the interface hardware
/// accepts. (IPv4's minimum, 68, is the floor; anything a setter says above
/// that the frame path will happily carry, since the driver allocates
/// per-frame.)
pub(crate) const MTU_FLOOR: usize = 68;
/// The MTU ceiling: an IPv4 datagram cannot name a length past 65535, and a
/// link that claims more only lies to whoever reads it back.
pub(crate) const MTU_CEIL: usize = 65536;

/// How many closed-but-still-tearing-down sockets the stack carries at once.
const TEARDOWN_MAX: usize = 32;

impl Stack {
    /// The socket set a socket with this home lives in.
    fn sockets_mut(&mut self, home: Home) -> &mut SocketSet<'static> {
        match home {
            Home::Eth => &mut self.eth.sockets,
            Home::Lo => &mut self.lo.sockets,
        }
    }

    /// The same set, read-only - what a question about a socket's *state*
    /// needs, and all `tcp_name_taken` is allowed, since it runs while the
    /// registry entry being bound is borrowed away from the stack.
    fn sockets(&self, home: Home) -> &SocketSet<'static> {
        match home {
            Home::Eth => &self.eth.sockets,
            Home::Lo => &self.lo.sockets,
        }
    }

    /// Poll one fabric: advance its interface over its device and its own set,
    /// unless `ip link set` has taken it down.
    fn poll_home(&mut self, home: Home, now: SmolInstant) {
        match home {
            Home::Eth => self.eth.poll(now),
            Home::Lo => poll_fabric(&mut self.lo, now),
        }
    }

    /// Advance every interface that is up once: drain received frames into
    /// smoltcp, hand it queued segments to transmit, let its timers retransmit
    /// and answer. A link administratively down does not advance - the whole
    /// point of `ip link set eth0 down` is that the machine stops talking.
    ///
    /// The loopback is polled second and unconditionally on its own flag: a
    /// socket it carries must progress even when the NIC is down, which is the
    /// one thing the single-interface version could not express.
    pub(crate) fn poll_once(&mut self) {
        let now = now();
        self.poll_home(Home::Eth, now);
        self.poll_home(Home::Lo, now);
        // After the poll, because a handshake completes *by* it: the socket
        // that moved from `Listen` to established is the one this has to move
        // from the pool to the queue, and the pool it refills is what lets the
        // next connection in.
        self.service_listeners();
        self.reap_teardown();
    }

    /// Move every completed handshake out of a listener's pool and into its
    /// accept queue, then refill the pool from what the queue leaves room for.
    ///
    /// This is what a listener *is* here, and it has to run inside the poll
    /// rather than at `accept`: the handshake is smoltcp's, it advances only
    /// when the interface is polled, and a socket that stopped listening is a
    /// socket no further connection can arrive on until this refills.
    fn service_listeners(&mut self) {
        for id in 0..self.listeners.len() {
            let Some((home, local, backlog, depth)) = self.listeners[id]
                .as_ref()
                .map(|l| (l.home, l.local, l.backlog, pool_depth(l.backlog)))
            else {
                continue;
            };

            // 1. Sort the pool: a socket that left `Listen` is a connection.
            // Taken out whole so the borrow of `listeners` ends before the
            // socket set is touched at all.
            let pool = self.listeners[id]
                .as_mut()
                .map(|l| core::mem::take(&mut l.listening))
                .unwrap_or_default();
            let mut idle: Vec<SocketHandle> = Vec::with_capacity(pool.len());
            let mut done: Vec<SocketHandle> = Vec::new();
            let mut dead: Vec<SocketHandle> = Vec::new();
            for h in pool {
                match self.tcp_socket(home, h).state() {
                    // Waiting for the next connection: the socket *is* the
                    // listener it was listening as.
                    tcp::State::Listen => idle.push(h),
                    // A handshake under way. It stays in the pool (there is
                    // nothing to accept yet) and leaves it on the poll that
                    // completes it - which is why more than one is kept.
                    tcp::State::SynReceived => idle.push(h),
                    // Complete: this socket is a connection now, and the
                    // queue is where `accept` looks.
                    tcp::State::Established | tcp::State::CloseWait => done.push(h),
                    // A handshake that died - smoltcp aborts a SYN-RECEIVED
                    // socket to `Closed` once its retransmissions run out - or
                    // a connection that ended before anybody accepted it. It
                    // is not listening any more, so it goes and the refill
                    // below replaces it: keeping it would shrink the pool one
                    // dead handshake at a time until nothing could connect.
                    _ => dead.push(h),
                }
            }
            for h in dead {
                self.sockets_mut(home).remove(h);
            }

            // 2. Refill, bounded by what the queue can still take. A full
            // queue leaves the pool empty on purpose: with nothing in
            // `Listen`, the next `SYN` is dropped by smoltcp rather than
            // answered and immediately refused, which is how a full backlog
            // behaves on Linux and needs no `abort` (which this stack cannot
            // turn into a reset anyway - smoltcp emits resets in reply to an
            // arriving packet, not as an egress of their own).
            let queued = self.listeners[id]
                .as_ref()
                .map(|l| l.accepted.len())
                .unwrap_or(0);
            let room = (backlog + 1).saturating_sub(queued + done.len());
            let want = room.min(depth);
            let mut listening = idle;
            while listening.len() < want {
                let h = self.add_tcp(home);
                if self.tcp_socket(home, h).listen(local).is_err() {
                    // Only an open socket refuses, and a socket this fresh is
                    // not one; a refusal here would leave a stray, so drop it
                    // rather than grow the pool with something unlisted.
                    self.sockets_mut(home).remove(h);
                    break;
                }
                listening.push(h);
            }

            if let Some(l) = self.listeners[id].as_mut() {
                l.listening = listening;
                l.accepted.extend(done);
            }
        }
    }

    /// One of this stack's TCP sockets, in whichever set its home names.
    fn tcp_socket(&mut self, home: Home, h: SocketHandle) -> &mut tcp::Socket<'static> {
        self.sockets_mut(home).get_mut::<tcp::Socket>(h)
    }

    /// Turn a bound, still-pristine TCP socket into a listener: build the
    /// pool, register it, and hand back the id the channel carries.
    ///
    /// `first` is the socket the caller already owns - reusing it saves an
    /// 80 KiB allocation and is safe because the caller has established it is
    /// pristine (see [`Stack::settle_tcp`]).
    pub(crate) fn open_listener(
        &mut self,
        home: Home,
        local: IpListenEndpoint,
        backlog: usize,
        first: SocketHandle,
    ) -> usize {
        let depth = pool_depth(backlog);
        let mut listening: Vec<SocketHandle> = Vec::with_capacity(depth);
        while listening.len() + 1 < depth {
            listening.push(self.add_tcp(home));
        }
        listening.push(first);
        // A socket that refuses to listen is dropped rather than carried: the
        // only refusal this can meet is an open socket, which the caller has
        // already excluded, so this is the belt to that braces.
        listening.retain(|h| self.tcp_socket(home, *h).listen(local).is_ok());
        let listener = Listener {
            home,
            local,
            backlog,
            listening,
            accepted: VecDeque::new(),
        };
        match self.listeners.iter().position(|slot| slot.is_none()) {
            Some(i) => {
                self.listeners[i] = Some(listener);
                i
            }
            None => {
                self.listeners.push(Some(listener));
                self.listeners.len() - 1
            }
        }
    }

    /// Tear a listener down: every socket it holds goes, and the slot is
    /// freed.
    ///
    /// The connections still in the queue are closed rather than abandoned,
    /// which gets their `FIN` out through the ordinary teardown discipline - a
    /// definite end for a client that connected, where Linux answers `RST`.
    /// That divergence is the documented one (§4.3): smoltcp's resets are
    /// emitted in reply to an arriving packet, so an unsolicited reset is not
    /// something this stack can send at all, and a `FIN` is the honest
    /// substitute.
    fn close_listener(&mut self, id: usize, reuse: bool) {
        let Some(listener) = self.listeners.get_mut(id).and_then(|slot| slot.take()) else {
            return;
        };
        let home = listener.home;
        // A socket still in `Listen` has nothing outstanding and no state
        // anybody is waiting on: it is removed, not closed.
        for h in listener.listening {
            self.sockets_mut(home).remove(h);
        }
        // `reuse` is the listener's own `SO_REUSEADDR`, which is also what an
        // accepted connection carries (Linux copies it across at accept, and
        // `install_accepted` does the same) - so it is the flag each of these
        // still-undelivered connections holds its name with.
        for h in listener.accepted {
            self.tcp_socket(home, h).close();
            if self.teardown.len() >= TEARDOWN_MAX
                && let Some((old_home, oldest, _)) = self.teardown.first().copied()
            {
                self.teardown.remove(0);
                self.sockets_mut(old_home).remove(oldest);
            }
            self.teardown.push((home, h, reuse));
        }
    }

    /// Hand one completed connection to `accept`: the id and the socket, or
    /// `None` when the queue is empty.
    pub(crate) fn listener_take(&mut self, id: usize) -> Option<(Home, SocketHandle)> {
        let listener = self.listeners.get_mut(id)?.as_mut()?;
        let h = listener.accepted.pop_front()?;
        Some((listener.home, h))
    }

    /// Whether this listener has a connection waiting - the question `accept`
    /// parks on, and the same one `poll(2)` reports it readable for.
    pub(crate) fn listener_has_pending(&self, id: usize) -> bool {
        self.listeners
            .get(id)
            .and_then(|slot| slot.as_ref())
            .is_some_and(|l| !l.accepted.is_empty())
    }

    /// Re-set a listener's backlog. Linux accepts a second `listen` and lets it
    /// change the queue length, rather than refusing the call, so this is what
    /// the second one does; the pool follows on the next poll.
    fn set_backlog(&mut self, id: usize, backlog: usize) {
        if let Some(l) = self.listeners.get_mut(id).and_then(|slot| slot.as_mut()) {
            l.backlog = backlog;
        }
    }

    /// Whether a TCP socket is still in `Closed` - the precondition for every
    /// act that needs the socket to carry no state, which is `listen` and the
    /// move between fabrics. A socket that is open has started something.
    fn tcp_is_closed(&mut self, home: Home, h: SocketHandle) -> bool {
        matches!(self.tcp_socket(home, h).state(), tcp::State::Closed)
    }

    /// Start a TCP handshake on `h`, which lives in `home`'s set: the socket's
    /// own `connect` needs the interface's routing context *beside* the socket,
    /// and those are two fields of one `Fabric`.
    pub(crate) fn tcp_connect(
        &mut self,
        home: Home,
        h: SocketHandle,
        remote: IpEndpoint,
        local: IpListenEndpoint,
    ) -> Result<(), tcp::ConnectError> {
        match home {
            Home::Eth => self.eth.connect(h, remote, local),
            Home::Lo => connect_in(&mut self.lo, h, remote, local),
        }
    }

    /// Free the sockets whose teardown finished after their last descriptor
    /// went - see [`Stack::teardown`]. Checked after the poll rather than
    /// inside it, because a socket reaches `Closed` by the very processing this
    /// poll performs.
    fn reap_teardown(&mut self) {
        if self.teardown.is_empty() {
            return;
        }
        let mut finished: Vec<usize> = Vec::new();
        for i in 0..self.teardown.len() {
            let (home, h, _) = self.teardown[i];
            // `Closed` is the end of the graceful machine *and* the state an
            // aborted socket is dropped into - and smoltcp emits the reset from
            // there, during the poll this runs after. So one condition covers
            // both teardowns, and reaping after the poll is what lets the reset
            // actually leave before the socket disappears.
            if matches!(
                self.sockets_mut(home).get_mut::<tcp::Socket>(h).state(),
                tcp::State::Closed
            ) {
                finished.push(i);
            }
        }
        // Backwards, so each index still names the entry it was recorded for.
        for i in finished.into_iter().rev() {
            let (home, h, _) = self.teardown.remove(i);
            self.sockets_mut(home).remove(h);
        }
    }

    /// Draw the next anonymous local port.
    fn alloc_ephemeral(&mut self) -> u16 {
        let port = self.next_ephemeral;
        self.next_ephemeral += 1;
        if self.next_ephemeral > EPHEMERAL_MAX {
            self.next_ephemeral = EPHEMERAL_MIN;
        }
        port
    }

    /// Draw an ephemeral port that is *free* for `addr`, which is what a bind
    /// that named port 0 gets on Linux: the kernel chooses, and it never
    /// chooses a name it would then refuse.
    ///
    /// The draw has to check at all only because of `TIME-WAIT`: an implicit
    /// bind that landed on a closed connection's port would be `EADDRINUSE`
    /// for a port the caller never named, while Linux would simply have picked
    /// the next one. `None` is the whole range spoken for - `EADDRINUSE`, and
    /// it takes a machine with 28 000 sockets on one address to reach.
    pub(crate) fn bind_ephemeral(&mut self, addr: IpAddress, reuse: bool) -> Option<u16> {
        // One turn of the range and no more: the counter returns to where it
        // started, and past that there is nothing left that has not been tried.
        let span = EPHEMERAL_MAX - EPHEMERAL_MIN + 1;
        for _ in 0..span {
            let port = self.alloc_ephemeral();
            if !self.tcp_name_taken(IpEndpoint { addr, port }, reuse) {
                return Some(port);
            }
        }
        None
    }

    /// Whether `local` is a name this stack has already given to somebody else,
    /// for a `bind` (or the implicit bind behind `listen`) that asked for
    /// `SO_REUSEADDR` or did not.
    ///
    /// Linux's rule, measured on this host's kernel before it was written
    /// (`NOTES.md` §4.3.3 carries the table). One clause decides every row:
    ///
    /// > a holder that is **listening** always keeps its name; any other
    /// > holder gives it up to a caller that set `SO_REUSEADDR` *and* whose
    /// > holder had set it too.
    ///
    /// The measurements that produced it, all on `127.0.0.1` with the
    /// holder's own `SO_REUSEADDR` and the new socket's varied independently:
    ///
    /// | the name is held by | holder's | new socket's | bind |
    /// |---|---|---|---|
    /// | a listening socket | either | either | `EADDRINUSE` |
    /// | a `TIME-WAIT` 4-tuple | no | yes | `EADDRINUSE` |
    /// | a `TIME-WAIT` 4-tuple | yes | no | `EADDRINUSE` |
    /// | a `TIME-WAIT` 4-tuple | yes | yes | **succeeds** |
    /// | an established connection | either | not both | `EADDRINUSE` |
    /// | a bound, never-connected socket | yes | yes | **succeeds** |
    ///
    /// So "the new socket asked for it" is *not* enough, which is the shape one
    /// writes from the man page and the reason this was measured: a restart
    /// comes back onto its own port because the listener set the option and the
    /// connections it accepted *inherited* it (also measured, and why
    /// `install_accepted` hands it on), not because the new socket asked
    /// loudly. And the exemption is not about `TIME-WAIT` at all - a bound or
    /// established socket grants it too, which is what lets a port be rebound
    /// while a connection is still open on it.
    ///
    /// Two names on one port collide when either is the wildcard or when they
    /// are the same address (see [`names_overlap`]).
    ///
    /// The scan is over the *registry* - plus the teardown list, which is where
    /// a closed connection lives until it is reaped - rather than over one
    /// socket set, and that is not tidiness: the two fabrics are one address
    /// space to the caller, and which set this stack happens to keep a socket
    /// in is not a fact a bind may depend on.
    pub(crate) fn tcp_name_taken(&self, local: IpEndpoint, reuse: bool) -> bool {
        let held = |names: IpEndpoint, listening: bool, holder_reuse: bool| {
            names_overlap(names, local) && (listening || !(reuse && holder_reuse))
        };
        self.entries.iter().flatten().any(|e| match &e.channel {
            // A connection, or a bound socket that has not connected: its name
            // is the one `bind` recorded, and failing that the one the socket
            // itself carries - an accepted connection records nothing (`local`
            // is `None`) and answers `getsockname` from its own endpoint, so
            // that is the name to hold it to here as well.
            Channel::Tcp {
                home,
                handle,
                local,
            } => {
                let Some(names) = (*local).or_else(|| self.tcp_name_at(*home, *handle)) else {
                    return false;
                };
                held(names, false, e.opts.reuseaddr)
            }
            Channel::TcpListen { local: names, .. } => held(*names, true, e.opts.reuseaddr),
            _ => false,
        }) || self.teardown.iter().any(|(home, h, holder_reuse)| {
            self.tcp_name_at(*home, *h)
                .is_some_and(|names| held(names, false, *holder_reuse))
        })
    }

    /// The name the TCP socket at `home`/`h` still holds, if it holds one.
    ///
    /// A socket that is listening answers from its listen endpoint rather than
    /// from a 4-tuple; those reach here through the registry (`TcpListen`), so
    /// this is the connected and closing half. The 4-tuple survives every state
    /// down to `Closed`, which is the state `reap_teardown` removes - and a
    /// socket past its last packet holds nothing, which is why `Closed` is not
    /// a name.
    ///
    /// One divergence rides here with the rule: smoltcp's wait is ten seconds
    /// where Linux's is sixty (`TCP_TIMEWAIT_LEN`), so a name comes back
    /// sooner on this machine. The rule about *who* may take it is the same.
    fn tcp_name_at(&self, home: Home, h: SocketHandle) -> Option<IpEndpoint> {
        let sock = self.sockets(home).get::<tcp::Socket>(h);
        if matches!(sock.state(), tcp::State::Closed) {
            return None;
        }
        sock.local_endpoint()
    }

    /// Add a freshly built TCP socket to `home`'s set and hand back its handle.
    pub(crate) fn add_tcp(&mut self, home: Home) -> SocketHandle {
        let rx = tcp::SocketBuffer::new(alloc::vec![0u8; TCP_RX_BYTES]);
        let tx = tcp::SocketBuffer::new(alloc::vec![0u8; TCP_TX_BYTES]);
        self.sockets_mut(home).add(tcp::Socket::new(rx, tx))
    }

    /// The same for a datagram socket.
    pub(crate) fn add_udp(&mut self, home: Home) -> SocketHandle {
        let rx_meta = alloc::vec![udp::PacketMetadata::EMPTY; UDP_PACKET_SLOTS];
        let tx_meta = alloc::vec![udp::PacketMetadata::EMPTY; UDP_PACKET_SLOTS];
        let rx = udp::PacketBuffer::new(rx_meta, alloc::vec![0u8; UDP_PAYLOAD_BYTES]);
        let tx = udp::PacketBuffer::new(tx_meta, alloc::vec![0u8; UDP_PAYLOAD_BYTES]);
        self.sockets_mut(home).add(udp::Socket::new(rx, tx))
    }

    /// A raw IP socket bound to the given protocol number. The buffers are
    /// generous enough for ICMP echo requests/replies (which are well under
    /// 1 KiB).
    const RAW_RX_BYTES: usize = 4 * 1024;
    const RAW_TX_BYTES: usize = 4 * 1024;

    pub(crate) fn add_raw(&mut self, home: Home, protocol: u8) -> SocketHandle {
        let rx = raw_sock::PacketBuffer::new(
            alloc::vec![raw_sock::PacketMetadata::EMPTY; 4],
            alloc::vec![0u8; Self::RAW_RX_BYTES],
        );
        let tx = raw_sock::PacketBuffer::new(
            alloc::vec![raw_sock::PacketMetadata::EMPTY; 4],
            alloc::vec![0u8; Self::RAW_TX_BYTES],
        );
        self.sockets_mut(home).add(raw_sock::Socket::new(
            None,
            Some(ip_to_smoltcp_protocol(protocol)),
            rx,
            tx,
        ))
    }

    /// The next anonymous local port for `stack` callers inside a `with`.
    pub(crate) fn ephemeral(&mut self) -> u16 {
        self.alloc_ephemeral()
    }

    /// Move a TCP socket to `want`'s set, if it is not already there and has
    /// not been used yet, and report the home it lives in from now on.
    ///
    /// The move has to happen *before* the call that names the address - a
    /// `bind` that only records the endpoint, a `connect` about to start - and
    /// it is only safe then: a socket that has begun a handshake carries state
    /// smoltcp has no way to hand to another set, so this removes and adds a
    /// *fresh* socket and every byte the old one held is deliberately nothing.
    /// `Closed` is exactly "nothing has happened yet" for a TCP socket: our own
    /// `bind` does not touch it (it records the endpoint in the registry and
    /// defers to `connect`), and the states past it are the handshake's.
    pub(crate) fn settle_tcp(&mut self, home: &mut Home, h: &mut SocketHandle, want: Home) {
        if *home == want {
            return;
        }
        debug_assert!(
            matches!(
                self.sockets_mut(*home).get_mut::<tcp::Socket>(*h).state(),
                tcp::State::Closed
            ),
            "a TCP socket that has left Closed cannot change interface"
        );
        self.sockets_mut(*home).remove(*h);
        *h = self.add_tcp(want);
        *home = want;
    }

    /// The same for a datagram socket. The caller's precondition is the same
    /// one, spelled in the registry rather than the socket: `!bound &&
    /// peer.is_none()`, checked *before* `sock.bind` runs - smoltcp keeps a
    /// bound UDP socket's name, and a new socket would lose it silently.
    pub(crate) fn settle_udp(&mut self, home: &mut Home, h: &mut SocketHandle, want: Home) {
        if *home == want {
            return;
        }
        self.sockets_mut(*home).remove(*h);
        *h = self.add_udp(want);
        *home = want;
    }
}

/// Whether two bound names are the same name, in the sense a second `bind`
/// cares about: the same port, and either the same address or the wildcard on
/// one side.
///
/// The wildcard is the half that is easy to leave out and wrong to: `0.0.0.0`
/// promises every address this machine has, so it overlaps `127.0.0.1:80` as
/// much as it overlaps itself (measured - the specific bind beside a wildcard
/// one is `EADDRINUSE`), while `127.0.0.1:80` and `127.0.0.2:80` are two names
/// and both stand.
fn names_overlap(a: IpEndpoint, b: IpEndpoint) -> bool {
    let wild = |addr: IpAddress| matches!(addr, IpAddress::Ipv4(v4) if v4.is_unspecified());
    a.port == b.port && (a.addr == b.addr || wild(a.addr) || wild(b.addr))
}

/// Which interface a destination address belongs to.
///
/// The whole of the routing decision, and deliberately the only rule: smoltcp
/// routes by destination alone, so a socket aimed at `127.0.0.0/8` must be in
/// the loopback's set and everything else in eth0's. The wildcard address
/// (`0.0.0.0`), a port-0 bind and an unbound socket all land on eth0 - which is
/// where Linux's wildcard listener lives too, and the one place this diverges
/// is that a wildcard listener here does not also answer a loopback client
/// (there is no second socket to answer it with; see `NOTES.md` §4.3).
pub(crate) fn home_of(addr: IpAddress) -> Home {
    match addr {
        IpAddress::Ipv4(v4) => match v4.octets() {
            [127, ..] => Home::Lo,
            _ => Home::Eth,
        },
    }
}

/// Run `f` with the stack and the entry `handle` names. `None` means the
/// stack is down or the handle is stale - the caller maps that to `EBADF` or
/// `ENETDOWN` as it prefers. `f` runs with interrupts off: it must not sleep.
///
/// The entry is lifted out of its slot for the duration. Handing `f` both
/// the stack and a reference borrowed *from* the stack would be two mutable
/// borrows of one value; taking the entry out and putting it back gives `f`
/// genuinely disjoint access to both. A panicking `f` loses the entry - and
/// a panic ends the machine, so there is nothing left to lose it from.
pub(crate) fn with<R>(handle: usize, f: impl FnOnce(&mut Stack, &mut Entry) -> R) -> Option<R> {
    // The stack the **socket** was made in, not the caller's: a descriptor can
    // outlive the namespace it was made in (a task that `unshare`s keeps its
    // sockets, and a descriptor can be handed to a task that never was in that
    // namespace), and Linux resolves it against the socket's own `sk_net`. That
    // is what `HOMES` records, and it is the difference between a socket that
    // works across a namespace change and one that resolves in the wrong stack.
    let (ns, idx) = home_of_handle(handle)?;
    with_namespace(ns, |stack| {
        // The entry is lifted out of its slot for the duration: handing `f` a
        // reference borrowed *from* the stack would be two mutable borrows of one
        // value. A panicking `f` loses the entry - and a panic ends the machine,
        // so there is nothing left to lose it from.
        let mut entry = stack.entries.get_mut(idx)?.take()?;
        let result = f(stack, &mut entry);
        stack.entries[idx] = Some(entry);
        Some(result)
    })
    .flatten()
}

/// Run `f` with the stack alone - for operations that do not have a socket
/// open yet (`socket()` itself) or span several (`close_sockets_at_exit`).
/// eth0's hardware address - the one number a packet socket's frames must
/// carry as their source, read from the live device rather than restated.
pub(crate) fn packet_mac() -> Option<[u8; 6]> {
    with_stack(|st| st.eth.device.as_ref().map(|d| d.net.mac())).flatten()
}

/// Hand one complete Ethernet frame to the device, outside the stack lock
/// (the Arc outlives the borrow, and the transmit is the driver's own).
/// `ENETDOWN` with no card; a refused queue is `EIO`, not a lost silence.
pub(crate) fn packet_transmit(frame: &[u8]) -> Result<usize, isize> {
    let device: Option<Arc<dyn NetDevice>> =
        with_stack(|st| st.eth.device.as_ref().map(|d| d.net.clone())).flatten();
    match device {
        Some(device) => {
            if device.transmit(frame) {
                Ok(frame.len())
            } else {
                Err(crate::err::EIO)
            }
        }
        None => Err(crate::err::ENETDOWN),
    }
}

/// Run `f` with the stack of the namespace **the caller is in**.
///
/// This is the whole of what a network namespace changes: the address and route
/// tables, the port space, the listeners and the socket sets a task reaches are
/// its own namespace's. A task that never asked for one holds no namespace
/// number ([`crate::sched::current_net_ns`] answers `None`) and resolves to the
/// machine's stack - so for every path that existed before namespaces this is
/// still that one stack, unchanged.
pub(crate) fn with_stack<R>(f: impl FnOnce(&mut Stack) -> R) -> Option<R> {
    with_namespace(crate::sched::current_net_ns(), f)
}

/// Run `f` with the stack of a **named** namespace - `None` for the machine's
/// own. The one resolver every other door ends at: a socket's operations go
/// through it with the namespace the *socket* was made in (`HOMES`), and the
/// creation paths with the namespace the *caller* is in
/// ([`crate::sched::current_net_ns`]).
pub(crate) fn with_namespace<R>(ns: Option<u64>, f: impl FnOnce(&mut Stack) -> R) -> Option<R> {
    match ns {
        None => with_machine_stack(f),
        // The machine's own number names the machine's own stack: a `setns` back
        // to it is a way *back*, not a lookup that cannot succeed.
        Some(id) if id == crate::vfs::ns::machine_net_id() => with_machine_stack(f),
        Some(id) => critical_section::with(|cs| {
            let mut stacks = NAMESPACES.borrow(cs).borrow_mut();
            stacks
                .iter_mut()
                .find(|(number, _)| *number == id)
                .map(|(_, stack)| f(stack))
        }),
    }
}

/// The live socket table for `/proc/net/tcp` (`tcp` set) or `/proc/net/udp`.
/// Walks every registry entry, reads the smoltcp socket behind the ones of the
/// requested kind, and reduces each to a `NetRow` the procfs formatter prints.
/// A stack that never came up yields nothing.
///
/// Gated with `procfs`'s own condition rather than a new `mpos_*` capability:
/// the only caller is `/proc/net`, so this collector and the `NetRow` type it
/// builds exist exactly where procfs does. That condition used to be "x86_64",
/// because procfs was the first machine's alone; procfs is built on both
/// machines now (`/proc` is what a real init - Alpine's openrc - reads, and the
/// socket table it carries is the same stack on both), so the condition is
/// "bare metal" and the collector comes with it instead of inventing a
/// capability no other caller would declare.
#[cfg(target_os = "none")]
pub(crate) fn collect_net_sockets(tcp: bool) -> Vec<crate::vfs::procfs::NetRow> {
    use smoltcp::wire::{IpAddress, IpEndpoint, Ipv4Address};
    let zero = || IpEndpoint {
        addr: IpAddress::Ipv4(Ipv4Address::new(0, 0, 0, 0)),
        port: 0,
    };
    /// What one registry entry contributes to a row.
    ///
    /// The entry itself cannot be carried out of the stack it lives in - reading
    /// the socket behind it borrows that same stack - so the connection facts
    /// are reduced to this while the lock is held.
    enum Shape {
        Tcp(Home, SocketHandle, Option<IpEndpoint>),
        Listen(IpEndpoint),
        Udp(Option<IpEndpoint>, Option<IpEndpoint>),
        Other,
    }
    let row_of = |ep: IpEndpoint| -> ([u8; 4], u16) {
        match ep.addr {
            IpAddress::Ipv4(v4) => (v4.octets(), ep.port),
        }
    };
    // The reader's namespace's sockets, each under the handle its descriptor
    // carries - which is the number `readlink /proc/<pid>/fd/N` prints, so the
    // table and the descriptor agree. A socket made in *another* namespace is
    // not this reader's to see, and one whose entry is gone by the time the row
    // is built is simply skipped: the table is a snapshot of live state.
    let mine = normalize(crate::sched::current_net_ns());
    let mut rows = Vec::new();
    let mut connected: Vec<(u64, Option<u64>, Home, SocketHandle, Option<IpEndpoint>)> = Vec::new();
    let mine = normalize(mine);
    for (handle, ns, idx) in all_homes() {
        if ns != mine {
            continue;
        }
        let inode = handle;
        // What a row needs from the entry, lifted out while its namespace's
        // stack is locked: the entry itself cannot be held across the socket
        // read below, because the socket lives in that same stack.
        let shape = with_namespace(ns, |st| {
            st.entries
                .get(idx)
                .and_then(|slot| slot.as_ref())
                .map(|entry| match &entry.channel {
                    Channel::Tcp {
                        home,
                        handle,
                        local,
                    } => Shape::Tcp(*home, *handle, *local),
                    Channel::TcpListen { local, .. } => Shape::Listen(*local),
                    Channel::Udp { local, peer, .. } => Shape::Udp(*local, *peer),
                    _ => Shape::Other,
                })
        })
        .flatten();
        match shape {
            Some(Shape::Tcp(home, sock, local)) if tcp => {
                connected.push((inode, ns, home, sock, local));
            }
            // A listener is one row, in `LISTEN`, on the address it listens
            // on - which is where `ss`/`netstat` find a server. The pool
            // behind it is not a connection and has no row.
            Some(Shape::Listen(local)) if tcp => {
                rows.push(crate::vfs::procfs::NetRow {
                    local: row_of(local),
                    remote: row_of(zero()),
                    state: 10,
                    uid: 0,
                    inode,
                });
            }
            // UDP reports the bound/connected endpoints the entry records;
            // its datagram "state" is 0, as Linux shows.
            Some(Shape::Udp(local, peer)) if !tcp => {
                rows.push(crate::vfs::procfs::NetRow {
                    local: row_of(local.unwrap_or_else(zero)),
                    remote: row_of(peer.unwrap_or_else(zero)),
                    state: 0,
                    uid: 0,
                    inode,
                });
            }
            _ => {}
        }
    }
    for (inode, ns, home, handle, local) in connected {
        let info = with_namespace(ns, |st| {
            let sock = st.sockets_mut(home).get::<tcp::Socket>(handle);
            let local_ep = sock.local_endpoint().or(local).unwrap_or_else(zero);
            let remote_ep = sock.remote_endpoint().unwrap_or_else(zero);
            (local_ep, remote_ep, tcp_proc_state(sock))
        });
        let (local_ep, remote_ep, state) = match info {
            Some(three) => three,
            None => continue,
        };
        rows.push(crate::vfs::procfs::NetRow {
            local: row_of(local_ep),
            remote: row_of(remote_ep),
            state,
            uid: 0,
            inode,
        });
    }
    rows
}

/// The per-interface counters for `/proc/net/dev`: the seam's totals beside the
/// link's live MTU, in `pool::NetDevStats` rows.
///
/// Same gating and same shape as [`collect_net_sockets`] - the only caller is
/// `/proc/net`, so it exists exactly where procfs does - with one addition the
/// socket table never needed: the numbers *must* answer even when the stack
/// never came up. A machine with no adapter has a stack that was never built
/// (`init` is not called) but still has an eth0 to name in the file, whose
/// counters are the zeros the statics hold; reading the MTU through the stack
/// therefore defaults to the boot value rather than dropping the row. The
/// counters themselves live in `pool`'s statics precisely so this door never
/// depends on the stack existing.
#[cfg(target_os = "none")]
pub(crate) fn net_dev_rows() -> Vec<pool::NetDevStats> {
    let mtu = with_stack(|st| st.eth.device.as_ref().map(|d| d.mtu))
        .flatten()
        .unwrap_or(MTU);
    pool::net_dev_stats(mtu)
}

/// A smoltcp TCP socket's state as the numeric `/proc/net/tcp` `st` byte
/// (LISTEN=10, ESTABLISHED=1, TIME_WAIT=6, ...).
fn tcp_proc_state(sock: &tcp::Socket) -> u8 {
    if sock.is_listening() {
        return 10;
    }
    match sock.state() {
        tcp::State::Established => 1,
        tcp::State::SynSent => 2,
        tcp::State::SynReceived => 3,
        tcp::State::FinWait1 => 4,
        tcp::State::FinWait2 => 5,
        tcp::State::TimeWait => 6,
        tcp::State::CloseWait => 8,
        tcp::State::LastAck => 9,
        tcp::State::Closing => 11,
        _ => 7, // Closed and any unrecognised state read as CLOSE.
    }
}

/// Register a new socket of the given family and type, and hand back its
/// handle. The smoltcp socket - with its buffers - is built inside the lock,
/// since the socket set is part of the stack. `ENETDOWN` when the stack never
/// came up: a kernel without a network card has no sockets to give.
///
/// `protocol` is the IP protocol number for raw sockets (e.g. 1 for ICMP);
/// ignored for TCP and UDP.
pub(crate) fn alloc_socket(family: u16, typ: u16, protocol: u32) -> Result<usize, isize> {
    // The **caller's** namespace's stack: a socket is made in the stack the
    // caller resolves through, and the handle it is given records that fact
    // (`attach`), so every later operation finds it wherever it lives - even
    // after the caller (or a task it handed the descriptor to) changes
    // namespace.
    let ns = crate::sched::current_net_ns();
    with_namespace(ns, |stack| {
        let owned = match (family, typ) {
            (socket::AF_INET, socket::SOCK_STREAM) => Channel::Tcp {
                // Created on eth0 because that is what most sockets are for;
                // the first address the socket is given moves it to the
                // loopback if that is where it belongs. See `Home`.
                home: Home::Eth,
                handle: stack.add_tcp(Home::Eth),
                local: None,
            },
            (socket::AF_INET, socket::SOCK_DGRAM) => Channel::Udp {
                home: Home::Eth,
                handle: stack.add_udp(Home::Eth),
                peer: None,
                bound: false,
                local: None,
            },
            (socket::AF_INET, socket::SOCK_RAW) => Channel::Raw {
                // The one channel that never moves: a raw socket's destination
                // is per call, so it cannot be tied to one interface's set.
                home: Home::Eth,
                handle: stack.add_raw(Home::Eth, protocol as u8),
                protocol: protocol as u8,
                echoed: alloc::sync::Arc::new(critical_section::Mutex::new(
                    core::cell::RefCell::new(alloc::collections::VecDeque::new()),
                )),
            },
            // The IPv6 stub: an entry with no smoltcp socket behind it.
            (socket::AF_INET6, _) => Channel::Inet6,
            // Two ways to ask for something that is not here, and Linux
            // names them apart: a family this kernel does not speak at all
            // is EAFNOSUPPORT, while a family it speaks but a type that
            // family does not carry - SOCK_SEQPACKET over IPv4 - is
            // EPROTONOSUPPORT. A caller walking a getaddrinfo list uses
            // the first to skip to the next answer.
            (socket::AF_INET, _) => return Err(crate::err::EPROTONOSUPPORT),
            _ => return Err(crate::err::EAFNOSUPPORT),
        };
        let entry = Entry {
            family,
            typ,
            channel: owned,
            opts: SockOpts::default(),
        };
        // The slot, then the handle that names it: the handle is global (see
        // `attach`) so a descriptor's number means one socket on the whole
        // machine, and the mapping is what says which stack that socket is in.
        let local = match stack.entries.iter().position(|slot| slot.is_none()) {
            Some(pos) => {
                stack.entries[pos] = Some(entry);
                pos
            }
            None => {
                stack.entries.push(Some(entry));
                stack.entries.len() - 1
            }
        };
        Ok(attach(ns, local))
    })
    .unwrap_or(Err(crate::err::ENETDOWN))
}

/// Register an `Entry` for a socket that already exists and hand back its
/// handle - what `accept` needs, where the smoltcp socket is one the listener's
/// pool completed rather than one this call builds.
///
/// `EBADF` with no stack behind it, matching the register path's `ENETDOWN`
/// being reserved for "there is no network card": a descriptor cannot be
/// waiting to be accepted on a stack that was never up.
///
/// `opts` is the option set the new socket starts with rather than
/// [`SockOpts::default`] for one caller: an accepted connection inherits the
/// listener's `SO_REUSEADDR`, which is Linux's behaviour (`inet_csk_clone_lock`
/// copies it) and the fact that makes a restart work at all - the connections
/// the listener accepted are what sits in `TIME-WAIT` holding its port, and the
/// option that lets the next bind take that port back has to be read from
/// *them* as well as from the caller (`Stack::tcp_name_taken`).
pub(crate) fn adopt_socket(channel: Channel, typ: u16, opts: SockOpts) -> Result<usize, isize> {
    // The caller's namespace's stack, for `alloc_socket`'s reason.
    let ns = crate::sched::current_net_ns();
    with_namespace(ns, |stack| {
        let entry = Entry {
            family: socket::AF_INET,
            typ,
            channel,
            opts,
        };
        // The slot, then the handle that names it: the handle is global (see
        // `attach`) so a descriptor's number means one socket on the whole
        // machine, and the mapping is what says which stack that socket is in.
        let local = match stack.entries.iter().position(|slot| slot.is_none()) {
            Some(pos) => {
                stack.entries[pos] = Some(entry);
                pos
            }
            None => {
                stack.entries.push(Some(entry));
                stack.entries.len() - 1
            }
        };
        Ok(attach(ns, local))
    })
    .unwrap_or(Err(crate::err::ENETDOWN))
}

/// `listen(2)` on an IPv4 socket: open the passive-open side, or re-set the
/// backlog of one already open.
///
/// `None` when the handle is not this registry's - a netlink or rtnetlink
/// socket, which the caller answers `EOPNOTSUPP` for as it always did.
///
/// The rules are Linux's, each measured on this host's kernel before being
/// written:
///
///   * `listen` on an **unbound** socket succeeds: it implicitly binds the
///     wildcard address and an ephemeral port, and `getsockname` then reports
///     `0.0.0.0:port`.
///   * `listen` on an **already listening** socket succeeds and re-sets the
///     backlog.
///   * `listen` on a **connected** socket is `EINVAL`.
///   * `listen` on a **datagram or raw** socket is `EOPNOTSUPP` - which is
///     what `t-udp` already pins.
pub(crate) fn tcp_listen(handle: usize, backlog: i32) -> Option<isize> {
    with(handle, |stack, entry| {
        // The backlog is clamped before either family sees the call, and a
        // negative one is `SOMAXCONN` rather than zero - the rule, and the
        // `-1` Rust's std sends, belong to `net::unix::clamp_backlog`. The
        // ceiling above that is `LISTEN_POOL_MAX` in practice because that is
        // all the pool grows to, not because the queue is short: `accepted`
        // holds `backlog + 1` completed connections.
        let backlog = crate::net::unix::clamp_backlog(backlog);
        let (mut home, mut sock_handle, local) = match &entry.channel {
            Channel::TcpListen { id, .. } => {
                stack.set_backlog(*id, backlog);
                return 0;
            }
            Channel::Tcp {
                home,
                handle,
                local,
            } => (*home, *handle, *local),
            _ => return crate::err::EOPNOTSUPP,
        };
        // A socket that has already connected - or been shut down - is not
        // something `listen` can turn around; Linux answers `EINVAL`, and the
        // pool would have to be built on a socket carrying another
        // connection's state, which smoltcp cannot hand on.
        if !stack.tcp_is_closed(home, sock_handle) {
            return crate::err::EINVAL;
        }
        let endpoint = match local {
            Some(l) => l,
            None => {
                // The implicit bind: the socket is named before it can listen,
                // because a listener with no port is not a listener. The
                // wildcard is the address Linux gives it, and the port is one
                // nothing else holds - the same rule `bind` applies, because it
                // is the same act (see [`Stack::tcp_name_taken`]). A listener
                // that had to be refused here would be one this stack cannot
                // name at all, which is `EADDRINUSE` and not a silent overlap.
                let addr = IpAddress::v4(0, 0, 0, 0);
                let Some(port) = stack.bind_ephemeral(addr, entry.opts.reuseaddr) else {
                    return crate::err::EADDRINUSE;
                };
                IpEndpoint { addr, port }
            }
        };
        // Which wire: the bound address decides, exactly as it does for
        // `bind` and `connect`, and this is the last moment the socket can
        // move - `listen` is what gives it state.
        let want = home_of(endpoint.addr);
        stack.settle_tcp(&mut home, &mut sock_handle, want);
        // smoltcp wants the wildcard as an absent address rather than as
        // `0.0.0.0`, and the two spellings have to agree with what
        // `getsockname` will report.
        let listen_at = match endpoint.addr {
            IpAddress::Ipv4(v4) if v4.is_unspecified() => IpListenEndpoint {
                addr: None,
                port: endpoint.port,
            },
            other => IpListenEndpoint {
                addr: Some(other),
                port: endpoint.port,
            },
        };
        let id = stack.open_listener(home, listen_at, backlog, sock_handle);
        entry.channel = Channel::TcpListen {
            id,
            local: endpoint,
        };
        0
    })
}

/// The peer a completed connection was accepted from, for the address `accept`
/// writes back.
pub(crate) fn tcp_peer(home: Home, h: SocketHandle) -> Option<IpEndpoint> {
    with_stack(|st| st.tcp_socket(home, h).remote_endpoint()).flatten()
}

/// Tear down socket `handle`: its registry slot goes empty and its smoltcp
/// socket is removed from the set, which frees its buffers.
pub(crate) fn free_entry(handle: usize) {
    // A netlink socket is not in this registry - it has its own table - so
    // its teardown happens here, where every socket's last close arrives.
    if crate::uevent::exists(handle) {
        crate::uevent::close(handle);
        return;
    }

    // The *socket's* namespace, not the caller's: the last `close` of a
    // descriptor may arrive from a task that changed namespace since, or never
    // was in it, and the entry to free is the one this handle was minted for.
    let Some((ns, idx)) = home_of_handle(handle) else {
        return;
    };
    with_namespace(ns, |stack| {
        let Some(entry) = stack.entries.get_mut(idx).and_then(|slot| slot.take()) else {
            return;
        };
        // A listener owns a *pool* of sockets and a queue of connections, not
        // one socket, so its teardown is its own act: the queued connections
        // are closed (see `Stack::close_listener`), the ordinary teardown path
        // is for one socket, and neither the pool nor the channel has one.
        if let Channel::TcpListen { id, .. } = &entry.channel {
            stack.close_listener(*id, entry.opts.reuseaddr);
            return;
        }
        let owned = match &entry.channel {
            Channel::Tcp { home, handle, .. } => Some((*home, *handle)),
            Channel::Udp { home, handle, .. } => Some((*home, *handle)),
            Channel::Raw { home, handle, .. } => Some((*home, *handle)),
            // The IPv6 stub owns no smoltcp socket; the registry slot is all
            // there is to free. The listener returned above.
            Channel::Inet6 | Channel::TcpListen { .. } => None,
        };
        if let Some((home, h)) = owned {
            // A TCP socket's last close is the one that has something left to
            // say. `close()` on smoltcp does not transmit - it marks the socket
            // closing and waits for the poll loop to put the `FIN` out - so
            // removing it here would delete the packet that has not been sent
            // yet, and the peer would learn about the close by writing into a
            // closed port. The other kinds are connectionless: nothing is
            // outstanding, and their buffers come back now.
            if matches!(&entry.channel, Channel::Tcp { .. }) {
                stack.sockets_mut(home).get_mut::<tcp::Socket>(h).close();
                // Bounded, and the oldest goes first: 32 unanswered teardowns is
                // already more than any real peer owes a machine that closed on
                // it, and the alternative is a set that fills up and refuses new
                // sockets forever.
                if stack.teardown.len() >= TEARDOWN_MAX
                    && let Some((old_home, oldest, _)) = stack.teardown.first().copied()
                {
                    stack.teardown.remove(0);
                    stack.sockets_mut(old_home).remove(oldest);
                }
                stack.teardown.push((home, h, entry.opts.reuseaddr));
            } else {
                stack.sockets_mut(home).remove(h);
            }
        }
        // The wildcard door's queue is the one piece of a raw channel that is
        // not smoltcp's, so its retirement is this table's act: drop the Arc,
        // and the seam stops copying frames into it from here on. A stillborn
        // entry (a socket whose last close lands while another task sits in
        // `recvfrom`) wakes nothing - that reader's own next step reads the
        // slot gone and answers `EBADF`, which is every other teardown's race
        // answer here too. `entry` is dropped with the closure, and the queue
        // goes with it - the seam holds no reference of its own.
    });
    // The handle is spent whatever the closure did with the entry: a number
    // that outlived its socket would name a slot the next `socket()` may take,
    // which is the aliasing this whole mapping exists to prevent.
    detach(handle);
}

/// Poll the interface once, unconditionally, then wake every task registered
/// as waiting on socket readiness. The background task calls this every tick;
/// socket operations poll inline before they try, so a response that arrived
/// in the last tick is seen without waiting for the task.
///
/// The wake is deliberately unselective - every waiter, every poll. The
/// alternative, tracking which socket changed and waking only its waiters,
/// is state the stack would have to keep true through smoltcp's dispatch,
/// while a spurious wake costs one re-check of a condition that is already
/// in memory. The waiters are woken *after* the poll, outside the stack's
/// lock, so a woken task never runs under one.
pub fn service() {
    let waiters = with_stack(|stack| {
        stack.poll_once();
        stack.waiters.clone()
    })
    .unwrap_or_default();
    for id in waiters {
        crate::sched::wake(id);
    }
}

/// Register `id` to be woken after every stack poll, until the matching
/// [`unsubscribe`]. The `poll_subscribe` on [`socket::SocketInode`] is what
/// routes a `poll(2)`/`epoll` wait through here; the blocked socket calls
/// in `socket.rs` use it directly. A task already registered is not added
/// twice.
pub(crate) fn subscribe(id: crate::sched::policy::TaskId) {
    with_stack(|stack| {
        if !stack.waiters.contains(&id) {
            stack.waiters.push(id);
        }
    });
}

/// Take `id` back out of the wake list - the wait it was registered for has
/// ended, however it ended. A task that is not registered is a no-op, which
/// is what makes an interrupted wait's cleanup unconditional.
pub(crate) fn unsubscribe(id: crate::sched::policy::TaskId) {
    with_stack(|stack| stack.waiters.retain(|w| *w != id));
}

/// Wake everything parked on a socket, without touching the network.
///
/// A wake only means "look again": each waiter re-reads its own condition and
/// sleeps on if nothing has changed, so waking the whole list is cheap and
/// never wrong. This is the entry a *device* event needs. A uevent becomes
/// ready on a netlink socket long before the next frame arrives, and on a
/// machine with nothing to receive - which a cold-plug is exactly - a listener
/// that is only ever woken by network traffic never wakes at all. That is how
/// `udevd` ended up with an empty database here: the events were queued, the
/// daemon was asleep, and `udevadm trigger` returned having changed nothing.
pub(crate) fn wake_waiters() {
    let waiters = with_stack(|stack| stack.waiters.clone()).unwrap_or_default();
    for id in waiters {
        crate::sched::wake(id);
    }
}

/// The interrupt-shaped entry into the stack: poll, wake the registered
/// waiters, then acknowledge the device.
///
/// The acknowledgement is the one step [`service`] cannot take - a poll from
/// a syscall or the background task has no interrupt to acknowledge - and it
/// is what lets the next arrival raise the vector again. The local APIC's
/// EOI, which ends the delivery itself, belongs to the caller's handler, not
/// to the stack: it is machine bookkeeping, not network state.
pub fn irq_service() {
    let waiters = with_stack(|stack| {
        stack.poll_once();
        // A namespace with no wire has no interrupt to acknowledge; the
        // service task still walks its other interfaces.
        if let Some(device) = stack.eth.device.as_ref() {
            device.net.ack_interrupt();
        }
        stack.waiters.clone()
    })
    .unwrap_or_default();
    for id in waiters {
        crate::sched::wake(id);
    }
}

// ---------------------------------------------------------------------------
// The rtnetlink write side: what `ip addr add` and friends actually change
// ---------------------------------------------------------------------------

/// One IPv4 address the address dump reports: the address and its prefix
/// length.
pub(crate) struct AddrFacts {
    pub ip: [u8; 4],
    pub prefix: u8,
}

/// One route the route dump reports: the destination network, its prefix
/// length, and the router the prefix leaves by.
pub(crate) struct RouteFacts {
    pub dst: [u8; 4],
    pub dst_len: u8,
    pub gateway: [u8; 4],
}

/// Everything a link dump reports about the two interfaces: the loopback's
/// and eth0's administrative flags, eth0's name, MTU and MAC. `None` means
/// the stack never came up - no NIC, no links worth the name.
pub(crate) struct LinkFacts {
    pub lo_up: bool,
    /// Whether there is a wire at all - the difference between `eth0 down` and
    /// no `eth0`, which the link dump has to keep.
    pub eth_present: bool,
    pub eth_up: bool,
    pub eth_name: String,
    pub mtu: u32,
    pub mac: [u8; 6],
}

/// eth0's IPv4 addresses, in the order the interface holds them.
///
/// eth0's and not "the addresses this machine has": the loopback's are
/// [`lo_addrs`], because a route dump has to say *which* interface an address
/// belongs to and the two are separate interfaces here.
pub(crate) fn ipv4_addrs() -> Vec<AddrFacts> {
    addrs_of(Home::Eth)
}

/// The loopback's IPv4 addresses. One, as the machine is configured - but read
/// from the live interface rather than from a constant, so §4.4's "the dumps
/// read the live tables" stays true for the interface that is actually up.
pub(crate) fn lo_addrs() -> Vec<AddrFacts> {
    addrs_of(Home::Lo)
}

fn addrs_of(home: Home) -> Vec<AddrFacts> {
    with_stack(|st| {
        let iface = match home {
            Home::Eth => match st.eth.iface.as_ref() {
                Some(iface) => iface,
                // An interface that is not there has no addresses to list, and
                // an empty list is what a `getifaddrs` walk in a wire-less
                // namespace must see.
                None => return Vec::new(),
            },
            Home::Lo => &st.lo.iface,
        };
        iface
            .ip_addrs()
            .iter()
            .filter_map(|cidr| match cidr {
                IpCidr::Ipv4(v4) => Some(AddrFacts {
                    ip: v4.address().octets(),
                    prefix: v4.prefix_len(),
                }),
                // The stack builds without IPv6, so the enum's other arm
                // does not exist here; the wildcard would be unreachable.
                #[allow(unreachable_patterns)]
                _ => None,
            })
            .collect()
    })
    .unwrap_or_default()
}

/// The routing table as it stands, one entry per prefix. eth0's table: the
/// loopback has none, which is the fact that keeps a lo-set socket aimed off-box
/// silent rather than leaky.
pub(crate) fn routes() -> Vec<RouteFacts> {
    with_stack(|st| {
        // A namespace with no wire has no routing table: every entry it could
        // hold is a way out of the machine, and there is nowhere to go.
        let Some(wire) = st.eth.iface.as_mut() else {
            return Vec::new();
        };
        let mut out = Vec::new();
        wire.routes_mut().update(|storage| {
            for route in storage.iter() {
                let (dst, dst_len) = match route.cidr {
                    IpCidr::Ipv4(v4) => (v4.address().octets(), v4.prefix_len()),
                    // See `ipv4_addrs` for why the wildcard is gated.
                    #[allow(unreachable_patterns)]
                    _ => continue,
                };
                let gateway = match route.via_router {
                    IpAddress::Ipv4(v4) => v4.octets(),
                    #[allow(unreachable_patterns)]
                    _ => continue,
                };
                out.push(RouteFacts {
                    dst,
                    dst_len,
                    gateway,
                });
            }
        });
        out
    })
    .unwrap_or_default()
}

/// The whole of what a link dump reports, or `None` with no stack behind it.
pub(crate) fn link_facts() -> Option<LinkFacts> {
    with_stack(|st| LinkFacts {
        lo_up: st.lo.up,
        // A wire-less namespace has no eth0 *at all*, which is not the same
        // report as an eth0 that is down: `eth_present` is what the link dump
        // reads before it writes the entry, so a namespace with no wire lists
        // `lo` and nothing else rather than a phantom NIC with no carrier.
        eth_present: st.eth.device.is_some(),
        eth_up: st.eth.up && st.eth.device.is_some(),
        eth_name: st.eth_name.clone(),
        mtu: st.eth.device.as_ref().map_or(MTU as u32, |d| d.mtu as u32),
        mac: st.eth.device.as_ref().map_or([0; 6], |d| d.net.mac()),
    })
}

/// eth0's preferred source address - the first IPv4 address the interface
/// carries, which is what a route dump reports as `RTA_PREFSRC` and what
/// `fastfetch` reads as the machine's local IP.
pub(crate) fn primary_ipv4() -> Option<[u8; 4]> {
    with_stack(|st| {
        st.eth
            .iface
            .as_ref()
            .and_then(|i| i.ipv4_addr().map(|a| a.octets()))
    })
    .flatten()
}

/// `ip addr add` on eth0: the address the interface does not yet carry. An
/// exact duplicate is Linux's `EEXIST`; a table with no room left (eight is
/// the interface's bound) is `ENOBUFS`. No stack at all is `ENODEV` - there
/// is no device to configure.
pub(crate) fn add_ipv4_addr(ip: [u8; 4], prefix: u8) -> Result<(), isize> {
    with_stack(|st| {
        let cidr = IpCidr::new(
            IpAddress::Ipv4(Ipv4Address::new(ip[0], ip[1], ip[2], ip[3])),
            prefix,
        );
        let Some(iface) = st.eth.iface.as_mut() else {
            // No wire, no device to configure: the same `ENODEV` the absent
            // stack gives at the end of this function.
            return Err(crate::err::ENODEV);
        };
        let mut rc = Ok(());
        iface.update_ip_addrs(|addrs| {
            if addrs.contains(&cidr) {
                rc = Err(crate::err::EEXIST);
                return;
            }
            if addrs.push(cidr).is_err() {
                rc = Err(crate::err::ENOBUFS);
            }
        });
        rc
    })
    .unwrap_or(Err(crate::err::ENODEV))
}

/// `ip addr del`: the local address itself names the entry, whatever prefix
/// the request carries (Linux matches on the address). Nothing to remove is
/// `EADDRNOTAVAIL`.
pub(crate) fn del_ipv4_addr(ip: [u8; 4]) -> Result<(), isize> {
    with_stack(|st| {
        let Some(iface) = st.eth.iface.as_mut() else {
            return Err(crate::err::ENODEV);
        };
        let mut rc = Err(crate::err::EADDRNOTAVAIL);
        iface.update_ip_addrs(|addrs| {
            let hit = addrs.iter().position(|a| match a {
                IpCidr::Ipv4(v4) => v4.address().octets() == ip,
                // See `ipv4_addrs` for why the wildcard is gated.
                #[allow(unreachable_patterns)]
                _ => false,
            });
            if let Some(pos) = hit {
                addrs.remove(pos);
                rc = Ok(());
            }
        });
        rc
    })
    .unwrap_or(Err(crate::err::ENODEV))
}

/// `ip route add`: a destination prefix and the router it leaves by. The
/// prefix already routed is `EEXIST` (a `replace` settles its own conflict by
/// deleting first); a table with no room left (four is its bound) is
/// `ENOBUFS`.
pub(crate) fn add_route(dst: [u8; 4], dst_len: u8, gateway: [u8; 4]) -> Result<(), isize> {
    with_stack(|st| {
        let cidr = IpCidr::new(
            IpAddress::Ipv4(Ipv4Address::new(dst[0], dst[1], dst[2], dst[3])),
            dst_len,
        );
        let via = IpAddress::Ipv4(Ipv4Address::new(
            gateway[0], gateway[1], gateway[2], gateway[3],
        ));
        let Some(iface) = st.eth.iface.as_mut() else {
            return Err(crate::err::ENODEV);
        };
        let mut rc = Ok(());
        iface.routes_mut().update(|storage| {
            if storage.iter().any(|r| r.cidr == cidr) {
                rc = Err(crate::err::EEXIST);
                return;
            }
            let route = Route {
                cidr,
                via_router: via,
                preferred_until: None,
                expires_at: None,
            };
            if storage.push(route).is_err() {
                rc = Err(crate::err::ENOBUFS);
            }
        });
        rc
    })
    .unwrap_or(Err(crate::err::ENODEV))
}

/// `ip route del`: the destination prefix names the route. No such route is
/// `ESRCH` - "no such process", Linux's own answer for a route that is not
/// there to delete.
pub(crate) fn del_route(dst: [u8; 4], dst_len: u8) -> Result<(), isize> {
    with_stack(|st| {
        let want = IpCidr::new(
            IpAddress::Ipv4(Ipv4Address::new(dst[0], dst[1], dst[2], dst[3])),
            dst_len,
        );
        let Some(iface) = st.eth.iface.as_mut() else {
            return Err(crate::err::ENODEV);
        };
        let mut rc = Err(crate::err::ESRCH);
        iface.routes_mut().update(|storage| {
            if let Some(pos) = storage.iter().position(|r| r.cidr == want) {
                storage.remove(pos);
                rc = Ok(());
            }
        });
        rc
    })
    .unwrap_or(Err(crate::err::ENODEV))
}

/// `ip link set <dev> up|down`. Index 1 is the loopback, index 2 is eth0; each
/// flag gates its own interface's polling, so a downed link really stops
/// carrying frames - including the loopback, which is no longer a device that
/// cannot be turned off. Anything else is `ENODEV`.
pub(crate) fn set_link_up(index: u32, up: bool) -> Result<(), isize> {
    with_stack(|st| match index {
        1 => {
            st.lo.up = up;
            Ok(())
        }
        2 => {
            if st.eth.device.is_none() {
                // `ip link set eth0 up` in a namespace that has no wire: there
                // is no device to bring up, which is `ENODEV` and not a success
                // that would leave the caller believing the link is running.
                return Err(crate::err::ENODEV);
            }
            st.eth.up = up;
            Ok(())
        }
        _ => Err(crate::err::ENODEV),
    })
    .unwrap_or(Err(crate::err::ENODEV))
}

/// `ip link set eth0 mtu N`. The floor is the smallest an IPv4 datagram can
/// be; above that the frame path carries whatever a setter names. Out of
/// range is `EINVAL`.
pub(crate) fn set_mtu(mtu: u32) -> Result<(), isize> {
    with_stack(|st| {
        let Some(device) = st.eth.device.as_mut() else {
            return Err(crate::err::ENODEV);
        };
        if (MTU_FLOOR..=MTU_CEIL).contains(&(mtu as usize)) {
            device.mtu = mtu as usize;
            Ok(())
        } else {
            Err(crate::err::EINVAL)
        }
    })
    .unwrap_or(Err(crate::err::ENODEV))
}

/// `ip link set eth0 name X`: one to fifteen bytes, and not the name the
/// loopback already answers to. The new name is what the dumps report from
/// then on - the stack itself never looks itself up by name.
pub(crate) fn set_eth_name(name: &str) -> Result<(), isize> {
    with_stack(|st| {
        if name.is_empty() || name.len() > 15 {
            return Err(crate::err::EINVAL);
        }
        if name == "lo" {
            return Err(crate::err::EEXIST);
        }
        st.eth_name = String::from(name);
        Ok(())
    })
    .unwrap_or(Err(crate::err::ENODEV))
}

/// What a descriptor on this socket could do right now, for `poll(2)`.
///
/// The interface is polled first so the answer reflects frames that arrived
/// since the last tick. A TCP socket in a closing phase reports readable even
/// with an empty buffer - a read then returns end-of-file rather than
/// blocking, which is the fact a caller waiting for a connection to finish
/// needs to see.
pub(crate) fn socket_ready(handle: usize) -> crate::vfs::Ready {
    if crate::uevent::exists(handle) {
        // A device event is either waiting or not; nothing is ever blocked
        // behind a send, so the write side is always ready.
        return crate::vfs::Ready {
            read: crate::uevent::ready(handle),
            write: true,
        };
    }
    let polled = with(handle, |stack, entry| {
        stack.poll_once();
        match &entry.channel {
            Channel::Tcp {
                home, handle: h, ..
            } => {
                let sock = stack.sockets_mut(*home).get_mut::<tcp::Socket>(*h);
                let closing = matches!(
                    sock.state(),
                    tcp::State::CloseWait
                        | tcp::State::LastAck
                        | tcp::State::Closing
                        | tcp::State::TimeWait
                        | tcp::State::Closed
                );
                crate::vfs::Ready {
                    read: sock.can_recv() || closing || !sock.may_recv(),
                    write: sock.can_send(),
                }
            }
            Channel::Udp {
                home, handle: h, ..
            } => {
                let sock = stack.sockets_mut(*home).get_mut::<udp::Socket>(*h);
                crate::vfs::Ready {
                    read: sock.can_recv(),
                    write: sock.can_send(),
                }
            }
            Channel::Raw {
                home,
                handle: h,
                echoed,
                ..
            } => {
                let sock = stack.sockets_mut(*home).get_mut::<raw_sock::Socket>(*h);
                // The wildcard door's own queue counts as readable - it is the
                // only one that ever fills (the seam copies a delivered packet
                // in), and its readiness is what ends a blocking `recvfrom`.
                let echoed_ready =
                    critical_section::with(|cs| !echoed.borrow(cs).borrow().is_empty());
                crate::vfs::Ready {
                    read: sock.can_recv() || echoed_ready,
                    write: sock.can_send(),
                }
            }
            // A listener is readable when a connection is waiting to be
            // accepted and never writable - Linux's answer, and what makes a
            // `poll`/`select`/`epoll` accept loop work without blocking.
            Channel::TcpListen { id, .. } => crate::vfs::Ready {
                read: stack
                    .listeners
                    .get(*id)
                    .and_then(|slot| slot.as_ref())
                    .is_some_and(|l| !l.accepted.is_empty()),
                write: false,
            },
            // The IPv6 stub has no queues to ask and no route anywhere:
            // never ready, which a caller only reaches if it polls without
            // having tried to connect - and connect already said no.
            Channel::Inet6 => crate::vfs::Ready::NONE,
        }
    });
    polled.unwrap_or(crate::vfs::Ready::NONE)
}

/// Bring the stack up over `dev`, the machine's network adapter: build the
/// interface around it and stamp in the static QEMU-user configuration.
///
/// Finding the adapter is the *machine's* step, not the stack's - a PCI probe
/// on x86_64, an MMIO window on aarch64 - which is why it arrives as an
/// argument through the [`NetDevice`] seam. A machine with no adapter never
/// calls this and leaves every socket call answering `ENETDOWN`, which is where
/// the stack already was before it came up.
pub fn init(dev: Arc<dyn NetDevice>) {
    // Fill the seam's frame pool before the first frame can arrive (§10.4).
    // Here rather than at the first take because a steady-path allocation is
    // exactly what the pool exists to remove, and `init` runs once with the
    // allocator up and no lock held against a frame.
    pool::init();
    let mac = dev.mac();

    let mut device = NicDevice {
        net: dev,
        pending: None,
        mtu: MTU,
    };
    let mut iface = Interface::new(
        Config::new(HardwareAddress::Ethernet(EthernetAddress(mac))),
        &mut device,
        now(),
    );
    iface.update_ip_addrs(|addrs| {
        let _ = addrs.push(IpCidr::new(
            IpAddress::v4(LOCAL_IP[0], LOCAL_IP[1], LOCAL_IP[2], LOCAL_IP[3]),
            PREFIX_LEN,
        ));
    });
    let _ = iface.routes_mut().add_default_ipv4_route(Ipv4Address::new(
        GATEWAY[0], GATEWAY[1], GATEWAY[2], GATEWAY[3],
    ));

    // The loopback: smoltcp's own device, which hands every transmitted frame
    // back to the next receive. It carries `127.0.0.0/8` - the prefix Linux
    // gives `lo`, so the whole range routes here rather than only the one
    // address - and it has no routes at all, which is deliberate: a socket in
    // its set aimed at anything off-box finds no route and emits nothing,
    // rather than leaking onto the wire it was kept away from.
    //
    // It borrows eth0's MAC because Linux's `lo` has no hardware address and
    // smoltcp's Ethernet medium insists on one, and because a self-addressed
    // first packet resolves by ARP against ourselves - the answer has to be the
    // address the frame was sent to.
    let mut lo_device = smoltcp::phy::Loopback::new(Medium::Ethernet);
    let mut lo_iface = Interface::new(
        Config::new(HardwareAddress::Ethernet(EthernetAddress(mac))),
        &mut lo_device,
        now(),
    );
    lo_iface.update_ip_addrs(|addrs| {
        let _ = addrs.push(IpCidr::new(IpAddress::v4(127, 0, 0, 1), 8));
    });

    critical_section::with(|cs| {
        *NET.borrow(cs).borrow_mut() = Some(Stack {
            eth: Wire {
                iface: Some(iface),
                device: Some(device),
                sockets: SocketSet::new(Vec::new()),
                up: true,
            },
            lo: Fabric {
                iface: lo_iface,
                device: lo_device,
                sockets: SocketSet::new(Vec::new()),
                up: true,
            },
            entries: Vec::new(),
            listeners: Vec::new(),
            teardown: Vec::new(),
            next_ephemeral: EPHEMERAL_MIN,
            waiters: Vec::new(),
            eth_name: String::from("eth0"),
        });
    });
}

impl Stack {
    /// A namespace's own stack: `lo`, and no wire.
    ///
    /// The loopback is built exactly the way the machine's is - smoltcp's own
    /// loopback device over an Ethernet medium - and it comes **up, carrying
    /// `127.0.0.1/8`**. That is a deliberate divergence from Linux, where a fresh
    /// namespace's `lo` is down and unaddressed until an init script runs
    /// `ip link set lo up` and `ip addr add`, and the reason is this kernel's own
    /// refusal: `RTM_NEWADDR` on `lo` answers `EOPNOTSUPP` (its one address is the
    /// kernel's, not a caller's to change - `rtnl.rs`'s measurement), so a
    /// namespace whose loopback started empty could never be given one, and its
    /// private network would be unusable rather than private. The pin holds both
    /// halves: the child's own loopback answers, and asking to add the address is
    /// refused.
    ///
    /// eth0 is **absent** rather than down, because a namespace has no device to
    /// bring up: moving one in is `ip link set ... netns`, which this kernel has no
    /// place for yet.
    pub(crate) fn deviceless() -> Stack {
        let mut lo_device = smoltcp::phy::Loopback::new(Medium::Ethernet);
        let mut lo_iface = Interface::new(
            Config::new(HardwareAddress::Ethernet(EthernetAddress([
                0x02, 0, 0, 0, 0, 0,
            ]))),
            &mut lo_device,
            now(),
        );
        lo_iface.update_ip_addrs(|addrs| {
            let _ = addrs.push(IpCidr::new(IpAddress::v4(127, 0, 0, 1), 8));
        });
        Stack {
            eth: Wire {
                device: None,
                iface: None,
                sockets: SocketSet::new(Vec::new()),
                up: false,
            },
            lo: Fabric {
                iface: lo_iface,
                device: lo_device,
                sockets: SocketSet::new(Vec::new()),
                up: true,
            },
            entries: Vec::new(),
            listeners: Vec::new(),
            teardown: Vec::new(),
            next_ephemeral: EPHEMERAL_MIN,
            waiters: Vec::new(),
            eth_name: String::from("eth0"),
        }
    }
}

/// Spawn the task that keeps the interface moving between userland polls.
///
/// Without it a connection only progresses while someone is blocked on a
/// socket call - a `connect` from one process would not see its SYN-ACK
/// unless it polled. One tick of sleep per round is the cost of progress:
/// at 100 ticks a second the stack checks for arrived frames 10 times more
/// often than slirp needs to answer.
pub fn spawn_service() {
    // Permanent: a connection has to keep making progress while the task that
    // opened it sits blocked in the kernel, and this is the task that does it -
    // so it runs for the machine's whole life. Started as permanent, not as an
    // ordinary task, because the boot code's `drain` waits for every task a
    // check started to be reaped and this one never will be.
    crate::sched::spawn_permanent(
        || loop {
            service();
            crate::sched::sleep_ticks(1);
        },
        crate::sched::Priority::NORMAL,
    );
}

/// smoltcp's clock. Smoltcp wants milliseconds; the source is the TSC's
/// sub-tick nanoseconds rather than the tick counter, so a retransmission
/// timer or an RTT sample is measured to the resolution the hardware reads
/// at instead of snapping to whole ticks - the tick still drives *when* the
/// stack is polled, but no longer rounds what it measures.
fn now() -> SmolInstant {
    SmolInstant::from_millis(crate::hal::since_boot_nanos() as i64 / 1_000_000)
}

/// Convert a raw IP protocol number to smoltcp's `IpProtocol` enum.
fn ip_to_smoltcp_protocol(num: u8) -> IpProtocol {
    match num {
        1 => IpProtocol::Icmp,
        6 => IpProtocol::Tcp,
        17 => IpProtocol::Udp,
        _ => IpProtocol::Unknown(num),
    }
}

/// The whole stack, behind the usual kernel lock. `None` until [`init`].
static NET: Mutex<RefCell<Option<Stack>>> = Mutex::new(RefCell::new(None));

/// Run `f` with the machine's own stack - the one every boot path builds, and the
/// init namespace's.
pub(crate) fn with_machine_stack<R>(f: impl FnOnce(&mut Stack) -> R) -> Option<R> {
    critical_section::with(|cs| {
        let mut net = NET.borrow(cs).borrow_mut();
        net.as_mut().map(f)
    })
}

/// Every network namespace but the machine's own, keyed by the number the
/// identity layer minted for it.
///
/// A `Vec` scanned linearly rather than a map: a machine has a handful of these
/// at most, and each one is a whole stack with its own socket sets. Nothing frees
/// one - a **named gap**, because the identity carries no reference count: a
/// namespace whose last task left keeps its stack until the machine stops, where
/// Linux frees it with the last `struct net`.
static NAMESPACES: Mutex<RefCell<Vec<(u64, Stack)>>> = Mutex::new(RefCell::new(Vec::new()));

/// Which namespace each socket descriptor's entry lives in, and where in that
/// namespace's registry it sits.
///
/// The one fact a handle cannot carry: a handle is an index into *some* stack's
/// entry table, and which stack that is is not derivable from the number. Linux
/// keeps the same fact inside the socket itself - a `struct sock` holds its
/// `sk_net` - and that is what lets a descriptor outlive a namespace change: a
/// socket made before `unshare(CLONE_NEWNET)` goes on working against the stack
/// it was made in, and one handed to a task in another namespace goes on working
/// *there*, against that same stack. The entries live in per-namespace tables
/// here, so the address of the entry is a side table keyed by the handle, and
/// the handles come from **one** counter so two namespaces can never mint the
/// same number.
static HOMES: Mutex<RefCell<BTreeMap<u64, (Option<u64>, usize)>>> =
    Mutex::new(RefCell::new(BTreeMap::new()));

/// The next registry handle: one for the machine, not one per stack.
static NEXT_HANDLE: AtomicU64 = AtomicU64::new(1);

/// The machine's own namespace is spelled `None` everywhere, so a task that
/// `setns`es back into it is the same key as a task that never left - which is
/// what makes the tables comparable without asking the identity layer at every
/// comparison.
///
/// Shared with `unix.rs`, which keeps the family's names per namespace: the two
/// registries have to agree on the spelling, or a unix socket made after a
/// `setns` back would be in a namespace of its own that nothing else can name.
pub(crate) fn normalize(ns: Option<u64>) -> Option<u64> {
    if ns == Some(crate::vfs::ns::machine_net_id()) {
        None
    } else {
        ns
    }
}

/// Record where a socket's entry lives, answering the handle that names it.
fn attach(ns: Option<u64>, local: usize) -> usize {
    let ns = normalize(ns);
    let handle = NEXT_HANDLE.fetch_add(1, Ordering::Relaxed);
    critical_section::with(|cs| {
        HOMES.borrow(cs).borrow_mut().insert(handle, (ns, local));
    });
    handle as usize
}

/// Where a handle's entry lives, or `None` for a handle nobody minted.
fn home_of_handle(handle: usize) -> Option<(Option<u64>, usize)> {
    critical_section::with(|cs| HOMES.borrow(cs).borrow().get(&(handle as u64)).copied())
}

/// Forget a handle, at the end of its socket's life.
fn detach(handle: usize) {
    critical_section::with(|cs| {
        HOMES.borrow(cs).borrow_mut().remove(&(handle as u64));
    });
}

/// Every handle and where it lives - what `/proc/net` walks, one row per socket
/// of the reader's namespace.
fn all_homes() -> Vec<(u64, Option<u64>, usize)> {
    critical_section::with(|cs| {
        HOMES
            .borrow(cs)
            .borrow()
            .iter()
            .map(|(handle, (ns, idx))| (*handle, *ns, *idx))
            .collect()
    })
}

/// Make a network namespace: a number from the one pool, whose stack - `lo`, and
/// no wire - is in the registry before the call returns.
///
/// Both halves in one call on purpose: a caller that holds the number must be
/// able to resolve the stack, which is what a `clone` child's first `socket`
/// depends on. Minting the number anywhere else would leave a window where a
/// namespace exists on paper and has nothing behind it.
pub fn mint_namespace() -> u64 {
    let id = crate::vfs::ns::mint();
    critical_section::with(|cs| {
        NAMESPACES
            .borrow(cs)
            .borrow_mut()
            .push((id, Stack::deviceless()));
    });
    id
}

// ---------------------------------------------------------------------------
// The smoltcp <-> virtio-net seam
// ---------------------------------------------------------------------------

/// The adapter smoltcp sees. It wraps the kernel's virtio-net driver and
/// carries at most one arrived frame between `receive` calls, because the
/// `Device` contract hands frames over one token pair at a time.
struct NicDevice {
    net: Arc<dyn NetDevice>,
    /// The frame polled from the driver and not yet handed to smoltcp, with its
    /// pool guard.
    ///
    /// The handover **clears** this slot: from `receive`'s return the token owns
    /// the frame, so the next call polls the driver again. That is not a detail.
    /// A `receive` that kept handing out a token pair after the first frame gave
    /// the stack an *empty* frame for ever, and smoltcp's ingress loop reads an
    /// empty frame as "a packet was processed, try again"
    /// (`iface/interface/mod.rs`, `socket_ingress`), so `poll` never returned:
    /// measured on the first gate run after §10.4's pool landed, as a machine
    /// that stopped at `t-epoll-socket` and never printed again — that loop runs
    /// inside the stack's critical section, so the timer interrupt was off too.
    ///
    /// A [`pool::PendingFrame`] rather than a bare `Vec` because the guard is
    /// what returns the pool slot when the frame's life ends: the token carries
    /// the same guard, so both endings (the stack read the frame; smoltcp took
    /// the pair and dropped it unread) hand the slot back exactly once. §10.4's
    /// one Drop guard, by design — every other recycle point is an explicit put
    /// at the site of last use.
    pending: Option<pool::PendingFrame>,
    /// The MTU the link reports. Settable through `RTM_SETLINK`; defaults to
    /// [MTU] at boot. smoltcp reads it via `capabilities()` on every poll, so
    /// a change applies to the next frame without any state to flush.
    mtu: usize,
}

impl Device for NicDevice {
    type RxToken<'a>
        = FrameRx
    where
        Self: 'a;
    type TxToken<'a>
        = FrameTx
    where
        Self: 'a;

    fn receive(
        &mut self,
        _timestamp: SmolInstant,
    ) -> Option<(Self::RxToken<'_>, Self::TxToken<'_>)> {
        if self.pending.is_none() {
            // The seam's own copy-out of the driver's frame (§10.4's RX half):
            // `NetDevice::poll` hands back bytes it borrowed from the ring, and
            // this is where they become an owned buffer - so this is where the
            // pool takes its slot. A pooled take keeps the ring's bounce from
            // being a fresh heap allocation on every arrival.
            let frame = self.net.poll()?;
            // An empty frame is "nothing arrived", not a frame. Answering it
            // with a token would be the same hang the handover below records
            // from the other side: smoltcp reads an empty frame as *processed*
            // and asks again, so its ingress loop would never end - inside the
            // stack's critical section, with the clock off. `?`-style `None` is
            // also the truthful answer: a zero-length frame is a driver that
            // read nothing.
            if frame.is_empty() {
                return None;
            }
            let len = frame.len();
            let mut buf = pool::take_rx(len);
            buf.copy_from_slice(&frame);
            self.pending = Some(pool::PendingFrame::new(buf));
        }
        // The handover — and it *clears* the slot: from here the token owns the
        // frame, guard included, so the next `receive` polls the driver for the
        // next arrival. Leaving the frame in place is what made the stack see
        // one frame and then an endless run of empty ones, which is the hang
        // this field's own note records.
        let pending = self.pending.take()?;
        // One count per frame crossing the seam, taken here rather than at
        // `packet::deliver` below: a frame polled is a frame received whether
        // or not smoltcp's token pair is later read, and this is the one line
        // every arrival passes - the sockets' reader and the raw family both
        // consume the same token. (§10.4's instrument; see `pool.rs`.)
        pool::record_rx(pending.as_slice().len());
        // The packet sockets see the frame before the stack decides whether
        // it is any of the IP layer's business - which is the whole point of
        // the family (see `packet`).
        packet::deliver(pending.as_slice());
        // The wildcard raw door (`AF_INET`+`SOCK_RAW`+`IPPROTO_RAW`) cannot
        // hold a filterless smoltcp socket - it would swallow the machine's
        // UDP and TCP delivery - so its receive queue is filled by a bounded
        // copy taken at this seam, before the stack decides whose business
        // the frame is. `None` costs nothing: no such socket is open.
        socket::echo_to_raw_doors(pending.as_slice());
        Some((
            FrameRx { frame: pending },
            FrameTx {
                net: self.net.clone(),
            },
        ))
    }

    fn transmit(&mut self, _timestamp: SmolInstant) -> Option<Self::TxToken<'_>> {
        Some(FrameTx {
            net: self.net.clone(),
        })
    }

    fn capabilities(&self) -> DeviceCapabilities {
        let mut caps = DeviceCapabilities::default();
        caps.medium = Medium::Ethernet;
        caps.max_transmission_unit = self.mtu;
        // Not a description of the hardware - smoltcp multiplies this by the
        // segment size and clamps every advertised receive window to the
        // result (`iface/packet.rs`, and the comment there calls it "a
        // terrible hack"). It is the one place the library lets a driver say
        // how many frames can pile up before the next one has to be dropped,
        // which for this device is exactly the receive ring.
        //
        // `Some(1)` - the value in smoltcp's own example, and what this was -
        // advertised 1460 bytes: one segment in flight, so the peer had to
        // wait a round trip for every segment. Measured on this machine that
        // is 72 KB/s of download, and it looks exactly like slirp being slow.
        // The window is what it was spending.
        caps.max_burst_size = Some(self.net.rx_frames());
        caps
    }
}

/// One received frame, handed to smoltcp as raw bytes.
///
/// The frame carries its pool guard, and that is the whole recycle story for
/// this direction: `consume` reads the bytes, and the guard returns the slot
/// when the token drops — whether that happens here (the stack read the frame)
/// or on the path where smoltcp takes the token pair and drops it unread. One
/// owner, both endings, and no `put_rx` call at either site.
struct FrameRx {
    frame: pool::PendingFrame,
}

impl RxToken for FrameRx {
    fn consume<R, F: FnOnce(&[u8]) -> R>(self, f: F) -> R {
        // The guard is dropped at the end of this call, after `f` has read the
        // bytes: §10.4's site of last use is the token's own end, and the two
        // endings share it because they share the owner.
        let Self { frame } = self;
        f(frame.as_slice())
    }
}

/// An outgoing frame: smoltcp fills the buffer, the driver sends it.
struct FrameTx {
    net: Arc<dyn NetDevice>,
}

impl TxToken for FrameTx {
    fn consume<R, F: FnOnce(&mut [u8]) -> R>(self, len: usize, f: F) -> R {
        // §10.4's TX half: the buffer smoltcp fills comes from the pool when a
        // slot can serve it and from a fresh allocation otherwise - the
        // fallback is counted as a miss rather than hidden.
        let mut frame = pool::take_tx(len);
        let result = f(&mut frame);
        // One count per outgoing frame built at the seam, taken *before* the
        // transmit's own success or failure: a full hardware queue drops the
        // frame here (the comment below), and Linux's `tx_packets` counts the
        // frames the stack handed the driver, which is what this column can
        // honestly mean. (§10.4's instrument; see `pool.rs`.)
        pool::record_tx(len);
        // A full hardware queue drops the frame here. TCP retransmits on its
        // own; a dropped datagram is the lossy-network behavior UDP promises.
        let _ = self.net.transmit(&frame);
        // The seam's TX recycle point: `transmit` copies into the driver's own
        // buffer, so this Vec's last use ended with that call and the slot
        // comes straight back.
        pool::put_tx(frame);
        result
    }
}
