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
//! *lock, poll, try, unlock, sleep one tick, repeat*, checking for a fatal
//! signal each round so a `SIGKILL` still lands while a task is parked in
//! `recv`. A background task polls the interface once a tick, which is what
//! keeps a connection progressing while nobody is blocked on it.
//!
//! # Addresses
//!
//! The stack comes up static: 10.0.2.15/24 with gateway 10.0.2.2, the shape
//! QEMU's user networking always hands out. DNS is not served by the kernel -
//! musl resolves in userspace over UDP sockets, which is why the UDP path has
//! to work but no resolver lives here.

pub mod rtnl;
pub mod socket;

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::cell::RefCell;

use critical_section::Mutex;
use smoltcp::iface::{Config, Interface, SocketHandle, SocketSet};
use smoltcp::phy::{Device, DeviceCapabilities, Medium, RxToken, TxToken};
use smoltcp::socket::raw as raw_sock;
use smoltcp::socket::tcp;
use smoltcp::socket::udp;
use smoltcp::time::Instant as SmolInstant;
use smoltcp::wire::{
    EthernetAddress, HardwareAddress, IpAddress, IpCidr, IpEndpoint, IpProtocol, Ipv4Address,
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
const TCP_RX_BYTES: usize = 32 * 1024;
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
        handle: SocketHandle,
        /// What `bind` recorded, if it ran. `connect` takes this as its
        /// source - a stream socket has nothing to bind before it connects,
        /// so the record is kept until then.
        local: Option<IpEndpoint>,
    },
    Udp {
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
    Raw {
        handle: SocketHandle,
        /// The IP protocol number this socket is bound to (e.g. 1 for ICMP).
        protocol: u8,
    },
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
}

/// Everything the network stack owns, in one place behind one lock.
pub(crate) struct Stack {
    iface: Interface,
    device: NicDevice,
    sockets: SocketSet<'static>,
    entries: Vec<Option<Entry>>,
    next_ephemeral: u16,
}

impl Stack {
    /// Advance the interface once: drain received frames into smoltcp, hand
    /// it queued segments to transmit, let its timers retransmit and answer.
    fn poll_once(&mut self) {
        let now = now();
        self.iface.poll(now, &mut self.device, &mut self.sockets);
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

    /// Add a freshly built socket to the socket set and hand back its handle.
    pub(crate) fn add_tcp(&mut self) -> SocketHandle {
        let rx = tcp::SocketBuffer::new(alloc::vec![0u8; TCP_RX_BYTES]);
        let tx = tcp::SocketBuffer::new(alloc::vec![0u8; TCP_TX_BYTES]);
        self.sockets.add(tcp::Socket::new(rx, tx))
    }

    /// The same for a datagram socket.
    pub(crate) fn add_udp(&mut self) -> SocketHandle {
        let rx_meta = alloc::vec![udp::PacketMetadata::EMPTY; UDP_PACKET_SLOTS];
        let tx_meta = alloc::vec![udp::PacketMetadata::EMPTY; UDP_PACKET_SLOTS];
        let rx = udp::PacketBuffer::new(rx_meta, alloc::vec![0u8; UDP_PAYLOAD_BYTES]);
        let tx = udp::PacketBuffer::new(tx_meta, alloc::vec![0u8; UDP_PAYLOAD_BYTES]);
        self.sockets.add(udp::Socket::new(rx, tx))
    }

    /// A raw IP socket bound to the given protocol number. The buffers are
    /// generous enough for ICMP echo requests/replies (which are well under
    /// 1 KiB).
    const RAW_RX_BYTES: usize = 4 * 1024;
    const RAW_TX_BYTES: usize = 4 * 1024;

    pub(crate) fn add_raw(&mut self, protocol: u8) -> SocketHandle {
        let rx = raw_sock::PacketBuffer::new(
            alloc::vec![raw_sock::PacketMetadata::EMPTY; 4],
            alloc::vec![0u8; Self::RAW_RX_BYTES],
        );
        let tx = raw_sock::PacketBuffer::new(
            alloc::vec![raw_sock::PacketMetadata::EMPTY; 4],
            alloc::vec![0u8; Self::RAW_TX_BYTES],
        );
        self.sockets.add(raw_sock::Socket::new(
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
                handle: stack.add_tcp(),
                local: None,
            },
            (socket::AF_INET, socket::SOCK_DGRAM) => Channel::Udp {
                handle: stack.add_udp(),
                peer: None,
                bound: false,
                local: None,
            },
            (socket::AF_INET, socket::SOCK_RAW) => Channel::Raw {
                handle: stack.add_raw(protocol as u8),
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
            Channel::Tcp { handle, .. } => Some(*handle),
            Channel::Udp { handle, .. } => Some(*handle),
            Channel::Raw { handle, .. } => Some(*handle),
            // The IPv6 stub owns no smoltcp socket; the registry slot is all
            // there is to free.
            Channel::Inet6 => None,
        };
        if let Some(h) = owned {
            stack.sockets.remove(h);
        }
    });
}

/// Poll the interface once, unconditionally. The background task calls this
/// every tick; socket operations poll inline before they try, so a response
/// that arrived in the last tick is seen without waiting for the task.
pub fn service() {
    with_stack(|stack| stack.poll_once());
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
            Channel::Tcp { handle: h, .. } => {
                let sock = stack.sockets.get_mut::<tcp::Socket>(*h);
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
            Channel::Udp { handle: h, .. } => {
                let sock = stack.sockets.get_mut::<udp::Socket>(*h);
                crate::vfs::Ready {
                    read: sock.can_recv(),
                    write: sock.can_send(),
                }
            }
            Channel::Raw { handle: h, .. } => {
                let sock = stack.sockets.get_mut::<raw_sock::Socket>(*h);
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

    critical_section::with(|cs| {
        *NET.borrow(cs).borrow_mut() = Some(Stack {
            iface,
            device,
            sockets: SocketSet::new(Vec::new()),
            entries: Vec::new(),
            next_ephemeral: EPHEMERAL_MIN,
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

/// smoltcp's clock, from the kernel tick counter. Smoltcp wants milliseconds
/// and the tick is ten of them at 100 Hz.
fn now() -> SmolInstant {
    let ticks = crate::arch::timer::ticks() as i64;
    SmolInstant::from_millis(ticks * 1000 / crate::arch::timer::TICK_HZ as i64)
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
        caps.max_transmission_unit = MTU;
        caps.max_burst_size = Some(1);
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
