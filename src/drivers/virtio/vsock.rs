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
//!
//! What the boot opened is a [`VsockDevice`], not a transport: the crate's
//! driver is built over `PciTransport` on the first machine ([`init_global`])
//! and over the MMIO transport on the second (`super::mmio::vsock_device`), and
//! both wrap it in the same [`VirtioVsock`] - whose ten methods are one
//! implementation over the one crate driver. So everything above the transport -
//! the family's phases, ports and accept queue in `src/net/vsock.rs`, and the
//! pump that keeps its events moving - is one copy on both machines, and the only
//! lines that name a bus are the two probes that open it (`PLAN.md` §13.28).

#[cfg(target_arch = "x86_64")]
use alloc::sync::Arc;
use core::cell::RefCell;

use critical_section::Mutex;
use virtio_drivers::device::socket::{VirtIOSocket, VsockAddr, VsockConnectionManager, VsockEvent};
use virtio_drivers::transport::Transport;

use super::hal::HalImpl;
#[cfg(target_arch = "x86_64")]
use super::hal::transport;
use super::{VSOCK_PER_CONNECTION_BUFFER, VsockDevice};

/// The modern device ID; virtio-vsock never had a legacy form. QEMU names
/// the card with a guest CID on its command line, and the device reports
/// that CID here for `getsockname` to answer with. The MMIO side finds its
/// window by this device's own `DeviceID` (`19`) instead, which is the machine
/// layer's read, not this module's.
#[cfg(target_arch = "x86_64")]
const DEVICE_VSOCK: &[u16] = &[0x1053];

/// A virtio-vsock device over the transport it arrived on - `PciTransport` on the
/// first machine, the MMIO transport on the second.
///
/// The crate's every operation needs `&mut self` and the family holds its device
/// by `Arc`, so the lock is what turns the two into the `&self` methods
/// [`VsockDevice`] promises. The ten of them are one implementation over the one
/// crate driver, which is the point: a second transport does not get a second copy
/// of them, it gets its own `Transport` and nothing else.
pub struct VirtioVsock<T: Transport> {
    /// The connection manager, behind a lock: every operation it offers
    /// takes `&mut`, and one device serves every socket in the family.
    manager: Mutex<RefCell<VsockConnectionManager<HalImpl, T>>>,
    guest_cid: u64,
}

impl<T: Transport> VirtioVsock<T> {
    /// Wrap the crate's driver as the family's device.
    ///
    /// The CID the host assigned this guest is read once, here, because the
    /// manager owns the driver from this point on (`PLAN.md` §13.28). The
    /// connection manager is built with this kernel's receive window, which is
    /// the only tuning the protocol leaves to the guest.
    pub fn new(driver: VirtIOSocket<HalImpl, T>) -> Self {
        let guest_cid = driver.guest_cid();
        let manager =
            VsockConnectionManager::new_with_capacity(driver, VSOCK_PER_CONNECTION_BUFFER);
        VirtioVsock {
            manager: Mutex::new(RefCell::new(manager)),
            guest_cid,
        }
    }
}

/// The device half of the seam `net/vsock.rs` holds. The family names this
/// contract and never `virtio-pci`, so a machine reaching the same device over
/// MMIO is a second transport for these ten methods rather than a second family.
impl<T: Transport + Send> VsockDevice for VirtioVsock<T> {
    /// The CID the host assigned this guest - the address a peer connects
    /// *to*, and the source address everything this guest sends carries.
    fn guest_cid(&self) -> u64 {
        self.guest_cid
    }

    /// Start listening on a port: connection requests the host sends to it
    /// are answered by the manager, and the event is handed to the socket
    /// family to queue for `accept`.
    fn listen(&self, port: u32) {
        critical_section::with(|cs| {
            self.manager.borrow(cs).borrow_mut().listen(port);
        });
    }

    /// Stop listening: a port nobody is serving has its requests refused
    /// by the manager, which is the refusal the host expects from a
    /// port with no listener behind it.
    fn unlisten(&self, port: u32) {
        critical_section::with(|cs| {
            self.manager.borrow(cs).borrow_mut().unlisten(port);
        });
    }

    /// Ask the host for a connection to `peer` from local `port`. The
    /// request is sent and the call returns; the `Connected` event that
    /// finishes the handshake arrives on a later poll.
    fn connect(&self, peer: VsockAddr, port: u32) -> bool {
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
    fn send(&self, peer: VsockAddr, port: u32, data: &[u8]) -> bool {
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
    fn recv(&self, peer: VsockAddr, port: u32, out: &mut [u8]) -> usize {
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
    fn recv_available(&self, peer: VsockAddr, port: u32) -> usize {
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
    fn poll_event(&self) -> Option<VsockEvent> {
        critical_section::with(|cs| self.manager.borrow(cs).borrow_mut().poll().ok().flatten())
    }

    /// Ask the peer to stop sending and us to stop receiving - the
    /// protocol's clean shutdown, which the peer acknowledges with its own
    /// event and the manager finishes by closing.
    fn shutdown(&self, peer: VsockAddr, port: u32) -> bool {
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
    fn force_close(&self, peer: VsockAddr, port: u32) -> bool {
        critical_section::with(|cs| {
            self.manager
                .borrow(cs)
                .borrow_mut()
                .force_close(peer, port)
                .is_ok()
        })
    }
}

/// Probe the PCI bus and bring up the first virtio-vsock device.
///
/// The first machine's door into the family; a machine that reaches the same
/// device over an MMIO window opens it there (`super::mmio::vsock_device`) and
/// hands it to `net::vsock::init` the same way. The handle is the contract
/// rather than the concrete device, so the caller never learns which bus
/// answered.
#[cfg(target_arch = "x86_64")]
pub fn init_global() -> Option<Arc<dyn VsockDevice>> {
    let driver = VirtIOSocket::<HalImpl, _>::new(transport(DEVICE_VSOCK)?).ok()?;
    Some(Arc::new(VirtioVsock::new(driver)))
}
