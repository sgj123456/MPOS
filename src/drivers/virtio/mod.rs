//! Virtio drivers.
//!
//! Every device the `virtio-drivers` crate can drive for this kernel is
//! driven from here: the block device, the network adapter, and the six
//! that came with them - the entropy source, the console port, the GPU,
//! the input devices, the sound card, the 9p share and the vsock device.
//! The crate implements VirtIO 1.0 itself: the queues, the feature
//! negotiation, the configuration space. What is left to a driver here is
//! which device to open and what the rest of the kernel calls the result.
//!
//! [`hal`] is the one thing the crate cannot provide, and the one thing
//! this module has to: how to allocate memory a device can reach, and how
//! to turn an address the device gives back into a pointer the kernel can
//! use. [`hal::transport`] is the other shared piece - the probe-and-open
//! step every driver starts with, so the only part that differs between
//! them is the ID the device answers.

//! Every driver here except `hal` reaches its device over PCI today, so the
//! PCI-only ones are x86_64's. `hal` is the machine-neutral `Hal` impl the
//! aarch64 MMIO transport builds on (`PLAN.md` §9.1); the aarch64 `rng`/`blk`/
//! `net` over MMIO come as the drivers stage proceeds, so `rng` is gated to
//! x86_64 until its MMIO path exists.

use virtio_drivers::device::socket::{VsockAddr, VsockEvent};

#[cfg(target_arch = "x86_64")]
pub mod blk;
#[cfg(target_arch = "x86_64")]
pub mod console;
#[cfg(target_arch = "x86_64")]
pub mod fs9p;
#[cfg(target_arch = "x86_64")]
pub mod gpu;
#[cfg(target_arch = "x86_64")]
pub mod gpu_ctrl;
pub mod hal;
#[cfg(target_arch = "x86_64")]
pub mod input;
/// The memory-mapped virtio transport, this machine's path to its devices. Only
/// aarch64 has its virtio behind fixed MMIO windows; x86_64 reaches the same
/// devices over a PCI BAR, so this is gated to aarch64 and is where its first
/// driven device (virtio-rng, `PLAN.md` §9.1 stage 7) is opened.
#[cfg(target_arch = "aarch64")]
pub mod mmio;
#[cfg(target_arch = "x86_64")]
pub mod msix;
#[cfg(target_arch = "x86_64")]
pub mod net;
#[cfg(target_arch = "x86_64")]
pub mod rng;
#[cfg(target_arch = "x86_64")]
pub mod sound;
#[cfg(target_arch = "x86_64")]
pub mod vsock;

/// The name a virtio function's driver has in `/sys`, and the one `lspci -k`
/// prints as "Kernel driver in use".
///
/// One name for all of them, because that is what Linux has: `virtio-pci` is
/// the PCI driver, and the block device, the network adapter and the rest are
/// *children* of it. What a device's `driver` link names is the thing that
/// claimed the PCI function, not the thing that claimed the disk.
pub const PCI_DRIVER: &str = "virtio-pci";

/// The network adapter, as the network stack sees it: the machine supplies one
/// and *which transport carries it* is the machine's business.
///
/// The device family is the same on both machines - virtio-net - but the way it
/// is reached is not: x86_64 takes it over a PCI BAR (`drivers::virtio::net`),
/// while aarch64 takes it over a fixed MMIO window. `net` holds this contract and
/// never the transport, so the stack compiles for a machine whose card is MMIO
/// and the data path (`poll`/`transmit`/`ack_interrupt`) is the same code on
/// both. A machine with no card has no value to hand `net::init`, which is the
/// honest `ENETDOWN` rather than a device that pretends to answer.
pub trait NetDevice: Send + Sync {
    /// This adapter's hardware address - the address smoltcp's interface and
    /// any packet socket's frames carry as their source.
    fn mac(&self) -> [u8; 6];

    /// How many frames the receive ring holds at once. The stack sizes its
    /// advertised TCP window from this, because it is the answer to the one
    /// question a window must not overstate: how much can arrive before
    /// something has to be dropped.
    fn rx_frames(&self) -> usize;

    /// Take one received frame, if the device has one - `None` when the ring
    /// is empty, never a claim of a frame that is not there.
    fn poll(&self) -> Option<alloc::vec::Vec<u8>>;

    /// Hand one frame to the device. `false` is a full or absent queue: the
    /// frame is gone, and TCP retransmits or a datagram is lost, as on the wire.
    fn transmit(&self, frame: &[u8]) -> bool;

    /// Acknowledge the device's interrupt, if it raised one. A no-op on a
    /// machine whose device is polled only.
    fn ack_interrupt(&self);
}

/// The virtio socket device, as the `AF_VSOCK` family sees it.
///
/// The same split as [`NetDevice`] and for the same reason: the family in
/// `src/net/vsock.rs` is protocol logic - phases, ports, the accept queue of
/// connections the host already believes in - while the transport that carries
/// its packets (PCI on x86_64, MMIO on aarch64) is the machine's. A machine
/// without the device has no value to hand `net::vsock::init`, and the family
/// answers `EAFNOSUPPORT` exactly as it does when no device probed.
pub trait VsockDevice: Send + Sync {
    /// The CID the hypervisor assigned this guest - the address a peer
    /// connects *to*, and the source address everything this guest sends
    /// carries.
    fn guest_cid(&self) -> u64;

    /// Start listening on `port`: connection requests the host sends to it are
    /// answered by the device, and the event is handed to the family for
    /// `accept`.
    fn listen(&self, port: u32);

    /// Stop listening: a port nobody serves has its requests refused the way a
    /// port with no listener is refused.
    fn unlisten(&self, port: u32);

    /// Ask the host for a connection to `peer` from local `port`. The request
    /// is sent and the call returns; the event that finishes the handshake
    /// arrives on a later [`poll_event`](Self::poll_event).
    fn connect(&self, peer: VsockAddr, port: u32) -> bool;

    /// Send one packet. `false` when the peer's receive window is full - the
    /// credit the protocol is named for - or the connection is gone.
    fn send(&self, peer: VsockAddr, port: u32, data: &[u8]) -> bool;

    /// Drain buffered bytes into `out`, answering the count. Zero is "nothing
    /// yet", not end of file; the end is a disconnect event.
    fn recv(&self, peer: VsockAddr, port: u32, out: &mut [u8]) -> usize;

    /// How many bytes are buffered on the connection - what `poll(2)` reads and
    /// a non-blocking `recv` sizes itself by.
    fn recv_available(&self, peer: VsockAddr, port: u32) -> usize;

    /// Take one event off the wire, if the host has one. The device's own
    /// bookkeeping has already run for it, so what comes back is the family's
    /// half of the news.
    fn poll_event(&self) -> Option<VsockEvent>;

    /// Ask the peer to stop sending and us to stop receiving - the protocol's
    /// clean shutdown.
    fn shutdown(&self, peer: VsockAddr, port: u32) -> bool;

    /// Close the connection now, without waiting for the peer: the reset a
    /// closing descriptor sends and a refused connection is answered with.
    fn force_close(&self, peer: VsockAddr, port: u32) -> bool;
}

/// How much receive room this kernel advertises per vsock connection. The peer
/// may not send more than this before waiting for a credit update, so the number
/// is a window - and the device holds a buffer of the same size per connection,
/// so the window is one the kernel can actually honour.
///
/// It lives here rather than in the PCI driver because it is the *family's* one
/// real buffer number, not the transport's: `getsockopt(SO_SNDBUF/SO_RCVBUF)`
/// reports it, and that answer must not depend on which machine opened the
/// device.
pub const VSOCK_PER_CONNECTION_BUFFER: u32 = 8 * 1024;

/// The task that keeps the polled devices moving between userland calls.
///
/// Two families need it: the input devices, whose events arrive whether
/// or not anyone is reading, and the vsock family, whose handshakes and
/// buffers advance while nobody is parked on them. One task polls both
/// once a tick - the same arrangement the network stack's service task
/// keeps with its interface - because a reader blocked on either is
/// waiting on a device nobody else is going to ask.
#[cfg(target_arch = "x86_64")]
pub fn spawn_service() {
    // Permanent: a listener parked in `accept` and a reader waiting on an
    // input event both depend on this task for as long as the machine runs.
    // The boot code's `drain` waits for every task a check started to be
    // reaped; this one never will be, which is what `spawn_permanent` is
    // for.
    crate::sched::spawn_permanent(
        || loop {
            input::service();
            crate::net::vsock::service();
            // The host's own events for the display. A window resize or a
            // fullscreen toggle arrives as `VIRTIO_GPU_EVENT_DISPLAY` in the
            // GPU's device configuration, and one word read a tick is what
            // notices it - the same cheap poll as the two above, and the
            // reason a display resize needs no interrupt. The *size* is not
            // read here and the screen is not rebuilt here: the render task
            // owns the device, re-reads the size and advertises it as a hotplug
            // uevent, and the scanout only follows a compositor's commit.
            if crate::drivers::video::render::running() && gpu::take_display_event() {
                crate::drivers::video::render::request_display_probe();
            }
            crate::sched::sleep_ticks(1);
        },
        crate::sched::Priority::NORMAL,
    );
}
