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

pub mod rtnl;
pub mod socket;
pub mod unix;
pub mod vsock;

use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::cell::RefCell;

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

use crate::drivers::virtio::net::VirtioNet;

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

/// Everything the network stack owns, in one place behind one lock.
pub(crate) struct Stack {
    /// eth0: the virtio NIC and QEMU's user network.
    eth: Fabric<NicDevice>,
    /// lo: `127.0.0.0/8`, carried by smoltcp's own loopback device.
    lo: Fabric<smoltcp::phy::Loopback>,
    entries: Vec<Option<Entry>>,
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
    teardown: Vec<(Home, SocketHandle)>,
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

    /// Poll one fabric: advance its interface over its device and its own set,
    /// unless `ip link set` has taken it down.
    fn poll_home(&mut self, home: Home, now: SmolInstant) {
        match home {
            Home::Eth => poll_fabric(&mut self.eth, now),
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
    fn poll_once(&mut self) {
        let now = now();
        self.poll_home(Home::Eth, now);
        self.poll_home(Home::Lo, now);
        self.reap_teardown();
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
            Home::Eth => connect_in(&mut self.eth, h, remote, local),
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
            let (home, h) = self.teardown[i];
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
            let (home, h) = self.teardown.remove(i);
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

/// Which interface a destination address belongs to.
///
/// The whole of the routing decision, and deliberately the only rule: smoltcp
/// routes by destination alone, so a socket aimed at `127.0.0.0/8` must be in
/// the loopback's set and everything else in eth0's. The wildcard address
/// (`0.0.0.0`), a port-0 bind and an unbound socket all land on eth0 - which is
/// where Linux's wildcard listener lives too, and the one place this diverges
/// is that a wildcard listener here does not also answer a loopback client
/// (there is no second socket to answer it with; see `PLAN.md` §4.3).
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
    critical_section::with(|cs| {
        let mut net = NET.borrow(cs).borrow_mut();
        let stack = net.as_mut()?;
        let idx = handle.checked_sub(1)?;
        let mut entry = stack.entries.get_mut(idx)?.take()?;
        let result = f(stack, &mut entry);
        stack.entries[idx] = Some(entry);
        Some(result)
    })
}

/// Run `f` with the stack alone - for operations that do not have a socket
/// open yet (`socket()` itself) or span several (`close_sockets_at_exit`).
pub(crate) fn with_stack<R>(f: impl FnOnce(&mut Stack) -> R) -> Option<R> {
    critical_section::with(|cs| {
        let mut net = NET.borrow(cs).borrow_mut();
        net.as_mut().map(f)
    })
}

/// The live socket table for `/proc/net/tcp` (`tcp` set) or `/proc/net/udp`.
/// Walks every registry entry, reads the smoltcp socket behind the ones of the
/// requested kind, and reduces each to a `NetRow` the procfs formatter prints.
/// A stack that never came up yields nothing.
pub(crate) fn collect_net_sockets(tcp: bool) -> Vec<crate::vfs::procfs::NetRow> {
    use smoltcp::wire::{IpAddress, IpEndpoint, Ipv4Address};
    let zero = || IpEndpoint {
        addr: IpAddress::Ipv4(Ipv4Address::new(0, 0, 0, 0)),
        port: 0,
    };
    let row_of = |ep: IpEndpoint| -> ([u8; 4], u16) {
        match ep.addr {
            IpAddress::Ipv4(v4) => (v4.octets(), ep.port),
        }
    };
    with_stack(|st| {
        let mut rows = Vec::new();
        // A handle is resolved *after* the entry borrow ends: both the entry
        // table and the socket sets live inside this one `Stack`, so reading a
        // socket while iterating the entries is two borrows of the same value.
        // Collecting the TCP handles first is the whole of the fix, and UDP
        // needs none of it - its row comes from the recorded endpoints.
        let mut connected: Vec<(u64, Home, SocketHandle, Option<IpEndpoint>)> = Vec::new();
        for (idx, slot) in st.entries.iter().enumerate() {
            let Some(entry) = slot else { continue };
            let inode = (idx + 1) as u64;
            match &entry.channel {
                Channel::Tcp {
                    home,
                    handle,
                    local,
                } if tcp => {
                    connected.push((inode, *home, *handle, *local));
                }
                Channel::Udp { local, peer, .. } if !tcp => {
                    // UDP reports the bound/connected endpoints the entry
                    // records; its datagram "state" is 0, as Linux shows.
                    rows.push(crate::vfs::procfs::NetRow {
                        local: row_of((*local).unwrap_or_else(zero)),
                        remote: row_of((*peer).unwrap_or_else(zero)),
                        state: 0,
                        uid: 0,
                        inode,
                    });
                }
                _ => {}
            }
        }
        for (inode, home, handle, local) in connected {
            let sock = st.sockets_mut(home).get::<tcp::Socket>(handle);
            let local_ep = sock.local_endpoint().or(local).unwrap_or_else(zero);
            let remote_ep = sock.remote_endpoint().unwrap_or_else(zero);
            rows.push(crate::vfs::procfs::NetRow {
                local: row_of(local_ep),
                remote: row_of(remote_ep),
                state: tcp_proc_state(sock),
                uid: 0,
                inode,
            });
        }
        rows
    })
    .unwrap_or_default()
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
    critical_section::with(|cs| {
        let mut net = NET.borrow(cs).borrow_mut();
        let Some(stack) = net.as_mut() else {
            return Err(crate::syscall::err::ENETDOWN);
        };
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
            },
            // The IPv6 stub: an entry with no smoltcp socket behind it.
            (socket::AF_INET6, _) => Channel::Inet6,
            // Two ways to ask for something that is not here, and Linux
            // names them apart: a family this kernel does not speak at all
            // is EAFNOSUPPORT, while a family it speaks but a type that
            // family does not carry - SOCK_SEQPACKET over IPv4 - is
            // EPROTONOSUPPORT. A caller walking a getaddrinfo list uses
            // the first to skip to the next answer.
            (socket::AF_INET, _) => return Err(crate::syscall::err::EPROTONOSUPPORT),
            _ => return Err(crate::syscall::err::EAFNOSUPPORT),
        };
        let entry = Entry {
            family,
            typ,
            channel: owned,
            opts: SockOpts::default(),
        };
        Ok(match stack.entries.iter().position(|slot| slot.is_none()) {
            Some(pos) => {
                stack.entries[pos] = Some(entry);
                pos + 1
            }
            None => {
                stack.entries.push(Some(entry));
                stack.entries.len()
            }
        })
    })
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

    critical_section::with(|cs| {
        let mut net = NET.borrow(cs).borrow_mut();
        let Some(stack) = net.as_mut() else { return };
        let idx = match handle.checked_sub(1) {
            Some(idx) if idx < stack.entries.len() => idx,
            _ => return,
        };
        let Some(entry) = stack.entries[idx].take() else {
            return;
        };
        let owned = match &entry.channel {
            Channel::Tcp { home, handle, .. } => Some((*home, *handle)),
            Channel::Udp { home, handle, .. } => Some((*home, *handle)),
            Channel::Raw { home, handle, .. } => Some((*home, *handle)),
            // The IPv6 stub owns no smoltcp socket; the registry slot is all
            // there is to free.
            Channel::Inet6 => None,
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
                    && let Some((old_home, oldest)) = stack.teardown.first().copied()
                {
                    stack.teardown.remove(0);
                    stack.sockets_mut(old_home).remove(oldest);
                }
                stack.teardown.push((home, h));
            } else {
                stack.sockets_mut(home).remove(h);
            }
        }
    });
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
        stack.eth.device.net.ack_interrupt();
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
            Home::Eth => &st.eth.iface,
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
        let mut out = Vec::new();
        st.eth.iface.routes_mut().update(|storage| {
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
        eth_up: st.eth.up,
        eth_name: st.eth_name.clone(),
        mtu: st.eth.device.mtu as u32,
        mac: st.eth.device.net.mac(),
    })
}

/// eth0's preferred source address - the first IPv4 address the interface
/// carries, which is what a route dump reports as `RTA_PREFSRC` and what
/// `fastfetch` reads as the machine's local IP.
pub(crate) fn primary_ipv4() -> Option<[u8; 4]> {
    with_stack(|st| st.eth.iface.ipv4_addr().map(|a| a.octets())).flatten()
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
        let mut rc = Ok(());
        st.eth.iface.update_ip_addrs(|addrs| {
            if addrs.contains(&cidr) {
                rc = Err(crate::syscall::err::EEXIST);
                return;
            }
            if addrs.push(cidr).is_err() {
                rc = Err(crate::syscall::err::ENOBUFS);
            }
        });
        rc
    })
    .unwrap_or(Err(crate::syscall::err::ENODEV))
}

/// `ip addr del`: the local address itself names the entry, whatever prefix
/// the request carries (Linux matches on the address). Nothing to remove is
/// `EADDRNOTAVAIL`.
pub(crate) fn del_ipv4_addr(ip: [u8; 4]) -> Result<(), isize> {
    with_stack(|st| {
        let mut rc = Err(crate::syscall::err::EADDRNOTAVAIL);
        st.eth.iface.update_ip_addrs(|addrs| {
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
    .unwrap_or(Err(crate::syscall::err::ENODEV))
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
        let mut rc = Ok(());
        st.eth.iface.routes_mut().update(|storage| {
            if storage.iter().any(|r| r.cidr == cidr) {
                rc = Err(crate::syscall::err::EEXIST);
                return;
            }
            let route = Route {
                cidr,
                via_router: via,
                preferred_until: None,
                expires_at: None,
            };
            if storage.push(route).is_err() {
                rc = Err(crate::syscall::err::ENOBUFS);
            }
        });
        rc
    })
    .unwrap_or(Err(crate::syscall::err::ENODEV))
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
        let mut rc = Err(crate::syscall::err::ESRCH);
        st.eth.iface.routes_mut().update(|storage| {
            if let Some(pos) = storage.iter().position(|r| r.cidr == want) {
                storage.remove(pos);
                rc = Ok(());
            }
        });
        rc
    })
    .unwrap_or(Err(crate::syscall::err::ENODEV))
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
            st.eth.up = up;
            Ok(())
        }
        _ => Err(crate::syscall::err::ENODEV),
    })
    .unwrap_or(Err(crate::syscall::err::ENODEV))
}

/// `ip link set eth0 mtu N`. The floor is the smallest an IPv4 datagram can
/// be; above that the frame path carries whatever a setter names. Out of
/// range is `EINVAL`.
pub(crate) fn set_mtu(mtu: u32) -> Result<(), isize> {
    with_stack(|st| {
        if (MTU_FLOOR..=MTU_CEIL).contains(&(mtu as usize)) {
            st.eth.device.mtu = mtu as usize;
            Ok(())
        } else {
            Err(crate::syscall::err::EINVAL)
        }
    })
    .unwrap_or(Err(crate::syscall::err::ENODEV))
}

/// `ip link set eth0 name X`: one to fifteen bytes, and not the name the
/// loopback already answers to. The new name is what the dumps report from
/// then on - the stack itself never looks itself up by name.
pub(crate) fn set_eth_name(name: &str) -> Result<(), isize> {
    with_stack(|st| {
        if name.is_empty() || name.len() > 15 {
            return Err(crate::syscall::err::EINVAL);
        }
        if name == "lo" {
            return Err(crate::syscall::err::EEXIST);
        }
        st.eth_name = String::from(name);
        Ok(())
    })
    .unwrap_or(Err(crate::syscall::err::ENODEV))
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
                home, handle: h, ..
            } => {
                let sock = stack.sockets_mut(*home).get_mut::<raw_sock::Socket>(*h);
                crate::vfs::Ready {
                    read: sock.can_recv(),
                    write: sock.can_send(),
                }
            }
            // The IPv6 stub has no queues to ask and no route anywhere:
            // never ready, which a caller only reaches if it polls without
            // having tried to connect - and connect already said no.
            Channel::Inet6 => crate::vfs::Ready::NONE,
        }
    });
    polled.unwrap_or(crate::vfs::Ready::NONE)
}

/// Bring the stack up: probe the PCI bus for a virtio-net adapter, build the
/// interface around it, and stamp in the static QEMU-user configuration.
/// Returns `false` when no adapter answers, in which case every socket call
/// answers `ENETDOWN` and the rest of the system runs as before.
pub fn init() -> bool {
    let Some(dev) = VirtioNet::init() else {
        return false;
    };
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
            eth: Fabric {
                iface,
                device,
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
            teardown: Vec::new(),
            next_ephemeral: EPHEMERAL_MIN,
            waiters: Vec::new(),
            eth_name: String::from("eth0"),
        });
    });
    true
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
    SmolInstant::from_millis(crate::arch::timer::since_boot_nanos() as i64 / 1_000_000)
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

// ---------------------------------------------------------------------------
// The smoltcp <-> virtio-net seam
// ---------------------------------------------------------------------------

/// The adapter smoltcp sees. It wraps the kernel's virtio-net driver and
/// carries at most one arrived frame between `receive` calls, because the
/// `Device` contract hands frames over one token pair at a time.
struct NicDevice {
    net: Arc<VirtioNet>,
    /// A frame polled but not yet consumed: `receive` must return the *same*
    /// frame on a repeat call, since smoltcp may drop the token pair without
    /// reading it (a run with nothing to send, for instance).
    pending: Option<Vec<u8>>,
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
            self.pending = self.net.poll();
        }
        let frame = self.pending.take()?;
        Some((
            FrameRx { frame },
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
struct FrameRx {
    frame: Vec<u8>,
}

impl RxToken for FrameRx {
    fn consume<R, F: FnOnce(&[u8]) -> R>(self, f: F) -> R {
        f(&self.frame)
    }
}

/// An outgoing frame: smoltcp fills the buffer, the driver sends it.
struct FrameTx {
    net: Arc<VirtioNet>,
}

impl TxToken for FrameTx {
    fn consume<R, F: FnOnce(&mut [u8]) -> R>(self, len: usize, f: F) -> R {
        let mut frame = alloc::vec![0u8; len];
        let result = f(&mut frame);
        // A full hardware queue drops the frame here. TCP retransmits on its
        // own; a dropped datagram is the lossy-network behavior UDP promises.
        let _ = self.net.transmit(&frame);
        result
    }
}
