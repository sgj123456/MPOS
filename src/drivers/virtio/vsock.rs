//! virtio-vsock, the device half, through the `virtio-drivers` crate.
//!
//! The vsock device is a wire to the host's socket layer: the crate owns
//! the queues, the packet headers, the credit accounting that keeps a
//! sender from drowning a receiver, and - in its connection manager - the
//! table of connections the host believes in. What is left here is which
//! device to open and the lock discipline around it, exactly the split the
//! network card took.
//!
//! The socket *family* - what `socket(AF_VSOCK)` builds, what `listen`
//! and `accept` mean over a connection the host initiated - lives in
//! `src/net/vsock.rs` with the AF_UNIX family it mirrors, for the same
//! reason AF_UNIX lives there and not here: the machine's network card
//! being absent is no reason for the socket family to be, and a registry
//! that answers `ENETDOWN` before its own device was probed is the bug
//! that module's design exists not to have.
//!
//! One deliberate difference from the network card is worth naming: the
//! connection manager *answers* connection requests itself, for every port
//! it was told to listen on. The host learns a connection exists the
//! moment the request arrives, so this kernel's accept queue is a queue of
//! already-made connections - exactly the shape Linux presents, and the
//! only shape the protocol allows: there is no message for "a request is
//! pending, ask me later".

use alloc::sync::Arc;
use core::cell::RefCell;

use critical_section::Mutex;
use virtio_drivers::device::socket::{VsockAddr, VsockConnectionManager, VsockEvent};
use virtio_drivers::transport::pci::PciTransport;

use super::hal::{HalImpl, transport};
use super::{VSOCK_PER_CONNECTION_BUFFER, VsockDevice};

/// The modern device ID; virtio-vsock never had a legacy form. QEMU names
/// the card with a guest CID on its command line, and the device reports
/// that CID here for `getsockname` to answer with.
const DEVICE_VSOCK: &[u16] = &[0x1053];

pub struct VirtioVsock {
    /// The connection manager, behind a lock: every operation it offers
    /// takes `&mut`, and one device serves every socket in the family.
    manager: Mutex<RefCell<VsockConnectionManager<HalImpl, PciTransport>>>,
    guest_cid: u64,
}

impl VirtioVsock {
    /// The CID the host assigned this guest - the address a peer connects
    /// *to*, and the source address everything this guest sends carries.
    pub fn guest_cid(&self) -> u64 {
        self.guest_cid
    }

    /// Start listening on a port: connection requests the host sends to it
    /// are answered by the manager, and the event is handed to the socket
    /// family to queue for `accept`.
    pub fn listen(&self, port: u32) {
        critical_section::with(|cs| {
            self.manager.borrow(cs).borrow_mut().listen(port);
        });
    }

    /// Stop listening: a port nobody is serving has its requests refused
    /// by the manager, which is the refusal the host expects from a
    /// port with no listener behind it.
    pub fn unlisten(&self, port: u32) {
        critical_section::with(|cs| {
            self.manager.borrow(cs).borrow_mut().unlisten(port);
        });
    }

    /// Ask the host for a connection to `peer` from local `port`. The
    /// request is sent and the call returns; the `Connected` event that
    /// finishes the handshake arrives on a later poll.
    pub fn connect(&self, peer: VsockAddr, port: u32) -> bool {
        critical_section::with(|cs| {
            self.manager
                .borrow(cs)
                .borrow_mut()
                .connect(peer, port)
                .is_ok()
        })
    }

    /// Send one packet. `false` when the peer's receive window is full -
    /// the credit the protocol is named for - or the connection is gone;
    /// the caller sleeps and retries, or reports its failure, as the
    /// socket's blocking rules decide.
    pub fn send(&self, peer: VsockAddr, port: u32, data: &[u8]) -> bool {
        critical_section::with(|cs| {
            self.manager
                .borrow(cs)
                .borrow_mut()
                .send(peer, port, data)
                .is_ok()
        })
    }

    /// Drain received bytes into `out`. The count is how many the
    /// connection had buffered; zero is "nothing yet", not end of file -
    /// the end is a `Disconnected` event, which the socket layer sees.
    pub fn recv(&self, peer: VsockAddr, port: u32, out: &mut [u8]) -> usize {
        critical_section::with(|cs| {
            self.manager
                .borrow(cs)
                .borrow_mut()
                .recv(peer, port, out)
                .unwrap_or(0)
        })
    }

    /// How many bytes are buffered on the connection - the answer `poll(2)`
    /// reads and a non-blocking `recv` sizes itself by.
    pub fn recv_available(&self, peer: VsockAddr, port: u32) -> usize {
        critical_section::with(|cs| {
            self.manager
                .borrow(cs)
                .borrow_mut()
                .recv_buffer_available_bytes(peer, port)
                .unwrap_or(0)
        })
    }

    /// Take one event off the wire, if the host has one. The manager has
    /// already done its own bookkeeping for it - the bytes a `Received`
    /// event names are already in its buffer, a request on a listening
    /// port is already answered - so what comes back here is the *socket
    /// family's* half of the news.
    pub fn poll_event(&self) -> Option<VsockEvent> {
        critical_section::with(|cs| self.manager.borrow(cs).borrow_mut().poll().ok().flatten())
    }

    /// Ask the peer to stop sending and us to stop receiving - the
    /// protocol's clean shutdown, which the peer acknowledges with its own
    /// event and the manager finishes by closing.
    pub fn shutdown(&self, peer: VsockAddr, port: u32) -> bool {
        critical_section::with(|cs| {
            self.manager
                .borrow(cs)
                .borrow_mut()
                .shutdown(peer, port)
                .is_ok()
        })
    }

    /// Close the connection now, without waiting for the peer: the reset
    /// the protocol spells `RST`, which is what a closing descriptor sends
    /// and what a refused connection is answered with.
    pub fn force_close(&self, peer: VsockAddr, port: u32) -> bool {
        critical_section::with(|cs| {
            self.manager
                .borrow(cs)
                .borrow_mut()
                .force_close(peer, port)
                .is_ok()
        })
    }

    /// Probe the PCI bus and bring up the first virtio-vsock device. The
    /// connection manager inside is built with this kernel's receive
    /// window, which is the only tuning the protocol leaves to the guest.
    pub fn init() -> Option<Arc<Self>> {
        let driver = virtio_drivers::device::socket::VirtIOSocket::<HalImpl, _>::new(transport(
            DEVICE_VSOCK,
        )?)
        .ok()?;
        let guest_cid = driver.guest_cid();
        let manager =
            VsockConnectionManager::new_with_capacity(driver, VSOCK_PER_CONNECTION_BUFFER);
        Some(Arc::new(VirtioVsock {
            manager: Mutex::new(RefCell::new(manager)),
            guest_cid,
        }))
    }
}

/// The PCI transport's implementation of the seam `net/vsock.rs` holds. The
/// family names this contract and never `virtio-pci`, so a machine reaching the
/// same device over MMIO is a second impl of this block, not a second family.
impl VsockDevice for VirtioVsock {
    fn guest_cid(&self) -> u64 {
        self.guest_cid()
    }

    fn listen(&self, port: u32) {
        self.listen(port)
    }

    fn unlisten(&self, port: u32) {
        self.unlisten(port)
    }

    fn connect(&self, peer: VsockAddr, port: u32) -> bool {
        self.connect(peer, port)
    }

    fn send(&self, peer: VsockAddr, port: u32, data: &[u8]) -> bool {
        self.send(peer, port, data)
    }

    fn recv(&self, peer: VsockAddr, port: u32, out: &mut [u8]) -> usize {
        self.recv(peer, port, out)
    }

    fn recv_available(&self, peer: VsockAddr, port: u32) -> usize {
        self.recv_available(peer, port)
    }

    fn poll_event(&self) -> Option<VsockEvent> {
        self.poll_event()
    }

    fn shutdown(&self, peer: VsockAddr, port: u32) -> bool {
        self.shutdown(peer, port)
    }

    fn force_close(&self, peer: VsockAddr, port: u32) -> bool {
        self.force_close(peer, port)
    }
}
