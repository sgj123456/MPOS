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

use alloc::string::String;
use alloc::vec::Vec;
use core::cell::RefCell;

use critical_section::Mutex;
use virtio_drivers::device::input::{AbsInfo, InputEvent, VirtIOInput};
use virtio_drivers::device::socket::{VsockAddr, VsockEvent};
use virtio_drivers::transport::Transport;

use self::hal::HalImpl;

// Every transport this module can speak is compiled on every machine, because
// which one a device is behind is a fact about the *machine* and not about the
// build's architecture: the block device and the network adapter are opened over
// a PCI BAR here and over a memory-mapped window there, and the caller that
// probes for them (`probe`) should not have to be two functions to say so. What
// is arch-bound is an optional capability: MSI-X needs an MSI controller
// (`mpos_msi`), so the module that arms it is gated on that word and a machine
// without the controller has no such module rather than a stub.
pub mod blk;
pub mod console;
pub mod fs9p;
pub mod gpu;
pub mod gpu_ctrl;
pub mod hal;
/// The evdev registry, and the input device nodes it publishes. Gated on the
/// *capability* rather than on the architecture, which is the difference between
/// a switch a port can flip and one it has to edit: `mpos_input_registry` says
/// "this machine has a registry of input devices and nodes for them", and both
/// machines' sysfs, exit hooks and boot wiring read it. The module is written
/// over [`InputDevice`] and never over a transport, so a machine reaches the
/// registry with whatever its own bus answered: `input::init` enumerates
/// virtio-PCI, and the MMIO side hands over the window it opened (`NOTES.md`
/// §15.2).
#[cfg(mpos_input_registry)]
pub mod input;
/// The memory-mapped virtio transport: the path to a device whose firmware
/// describes it as a window rather than as a PCI function. Compiled on every
/// machine now - a machine with no such window simply has none to walk
/// (`mmio::windows` reads the record's device tree, which an ACPI machine does
/// not carry), and the probe below asks both transports on both machines
/// rather than the build deciding which one a device "must" be behind.
pub mod mmio;
/// The MSI-X arming for the PCI transports, on the machines that have an MSI
/// controller. A capability, not an architecture: a machine whose devices are
/// wired to a GIC has no message-signalled delivery to arm and no such module.
#[cfg(mpos_msi)]
pub mod msix;
pub mod net;
/// The one pass that finds this machine's devices and registers them, whichever
/// transport carries each: the boot's device step, and the reason it is one step
/// on both machines rather than one per machine.
pub mod probe;
pub mod rng;
pub mod sound;
pub mod vsock;

/// Bytes from this machine's virtio-rng device, or `None` when it has none.
///
/// One global now holds the source whichever transport installed it, so this
/// is a straight read rather than a per-machine fork (`NOTES.md` §15.2).
/// `crate::rand` folds in "the machine's rng device" and does not learn which
/// machine it is on (`NOTES.md` §14.3).
pub fn entropy() -> Option<[u8; 32]> {
    rng::entropy()
}

/// Register a virtio block device as the kernel's boot disk.
///
/// The name, the number and the tree position are the *device's* facts, not the
/// transport's: a virtio-blk disk is `vda` under major 254 hanging in the virtio
/// tree whichever bus carried it, and a program that follows `/sys/class/block/vda`
/// or matches an event's `DEVPATH` has to land in the same place on either
/// machine. Saying it here rather than at each machine's bring-up is what makes
/// that so, and it is a fix rather than a tidy-up: the second machine registered
/// the same disk with `/dev/vda` where the first writes
/// [`crate::uevent::VIRTIO_BLK_DEVPATH`], so its events announced a `DEVPATH`
/// that is not a device path at all (`NOTES.md` §15.10).
///
/// The boot disk, singular: a machine brings up one, and [`crate::blk::add_disk`]
/// refuses a second under the same name rather than letting two devices share it.
pub fn register_block_disk(dev: alloc::sync::Arc<dyn crate::blk::BlockDev>) {
    crate::blk::add_disk(
        "vda",
        crate::uevent::VIRTIO_BLK_MAJOR,
        crate::uevent::VIRTIO_BLK_DEVPATH,
        dev,
    );
}

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

/// The virtio 9p device, as the host share's filesystem sees it.
///
/// The same split as the four above. The family is one - 9P2000 over the crate's
/// request/answer wire - and everything above the transport is one copy already:
/// the fid table, the `Twalk`/`Tread` encoding and the mapping onto the `Inode`
/// contract are `src/vfs/p9.rs`'s whichever bus the device answered on. What a
/// machine supplies is the transport the crate's driver was built over, so the
/// device's type is erased here and the mount table, the boot's own mount and the
/// `mount -t 9p` factory are the same on both machines (`NOTES.md` §13.29).
///
/// Two methods, because they are the two the protocol client and the mount table
/// ask for. A machine with no share has no device to hand `p9::mount`, which
/// answers `ENOENT` through the same path a named-but-absent tag does.
pub trait P9Device: Send + Sync {
    /// The tag the host exported this share under - the name `mount -t 9p` is
    /// pointed at, and the `aname` a client that wants the host to take it
    /// seriously attaches with.
    fn mount_tag(&self) -> &str;

    /// One round trip: send `req`, wait, and answer how many bytes of `resp` the
    /// answer filled. The caller sizes `resp` for the largest answer it can
    /// accept, which is the whole of the flow control this transport has.
    fn request(&self, req: &[u8], resp: &mut [u8]) -> bool;
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

/// The host-driven console port, as the kernel's `/dev/hvc0` sees it.
///
/// The same split as the three above. The family is one - a virtio console
/// port, a second serial line whose far end is whatever the host wired it to -
/// and everything above the transport is one copy already: the blocking read
/// that polls until bytes arrive, the write-through, and the device node the
/// devfs builder publishes are the same code whichever bus the port answered
/// on. The crate's own driver is the same one too; what a machine supplies is
/// its `Transport`.
///
/// A port whose far end has no reader is still a port: the host's socket
/// accepts bytes whether or not anyone is connected, and answers nothing
/// until somebody writes. That is the honest `send`, not a failure.
pub trait ConsolePort: Send + Sync {
    /// The port's size in characters - columns and rows - when the host's far
    /// end reports a shape. `None` is a port the host did not describe.
    fn size(&self) -> Option<(u16, u16)>;

    /// Drain whatever the host has written into the port, up to `buf`'s length,
    /// answering how many bytes landed. Zero is "nothing yet", not end of file:
    /// a console port has no end while the host holds it open.
    fn drain(&self, buf: &mut [u8]) -> usize;

    /// Hand `buf` to the host, answering whether the device took it - the
    /// queue's answer, not a reader's.
    fn send(&self, buf: &[u8]) -> bool;
}

/// The entropy pipe, as `/dev/hwrng` and the kernel seed see it: a request
/// for bytes and the device's answer of how many landed.
///
/// The same split as the console port and the sound card, and for the same
/// reason. The family is one - virtio-rng - and everything above the
/// transport was written twice: two wrapper types, two globals, two `fill`
/// bodies, two `entropy()` reads and two devfs node definitions that differed
/// only in which handle they closed over. What a machine supplies is the
/// transport the crate's driver is built over, so the device's type is erased
/// here and one copy of each of those answers serves both buses
/// (`NOTES.md` §15.2).
pub trait EntropySource: Send + Sync {
    /// Fill `out` with bytes the host generated, answering how many landed -
    /// the whole buffer for a healthy device, fewer or none for a quiet one.
    fn fill(&self, out: &mut [u8]) -> usize;
}

/// The sound card, as `/dev/dsp` sees it: the stream userland writes PCM frames
/// into, and the parameters that shape them.
///
/// The same split as the console port and for the same reason. The family is one
/// - virtio-sound - and everything above the transport is one copy already: the
/// OSS parameter ioctls, the period-sized staging buffer, the blocking hand-off
/// to the device. What a machine supplies is the transport the crate's driver is
/// built over, so the card's own type is erased here and the devfs node, the
/// ioctl vocabulary and the boot's probe line are the same on both
/// (`NOTES.md` §13.27).
///
/// Three methods, because they are the three the device face and the boot
/// actually ask for. The rest of the card - which rates it lists, which formats,
/// its channel range - is its own bookkeeping and stays behind them.
pub trait SoundCard: Send + Sync {
    /// What the stream is running at right now: `(rate Hz, channels)`. The
    /// sample format is always little-endian S16 - the one the driver requires
    /// at probe - so it needs no half here.
    fn format(&self) -> (u32, u8);

    /// Rebuild the stream at `rate` Hz and `channels`, rounding the rate onto
    /// the device's own list, and answer with what it actually settled on - the
    /// OSS contract, which reports rather than promises. `Err` is an errno.
    fn configure(&self, rate: u32, channels: u8) -> Result<(u32, u8), isize>;

    /// Hand `buf` to the output stream in period-sized pieces, answering how
    /// many bytes the device took. A short answer is a full ring, not a
    /// failure: the caller tries again for the rest.
    fn play(&self, buf: &[u8]) -> usize;
}

/// The virtio input device, as the evdev registry sees it: the capability
/// picture a reader asks for before any event has arrived, and the events
/// themselves.
///
/// The same split as [`NetDevice`] and [`VsockDevice`], and for the same
/// reason. The family is one - virtio-input, whose events are already Linux's
/// own input numbering - and everything above the transport is one copy
/// already: the `EvdevNode`s, the `/dev/input/eventN` numbering, the ioctl
/// vocabulary and the service task that harvests the rings are the same code
/// on a machine whose keyboard arrives over an MMIO window as on one whose
/// keyboard arrives over a BAR. What differs is only which bus answered, and a
/// driver says so by implementing this over its own transport rather than by
/// the registry naming one.
///
/// A question the device cannot answer - it reports no name, no property bits,
/// an axis it does not have - answers `None` or empty rather than inventing
/// one, which is the conservative picture the evdev ABI is written against.
pub trait InputDevice: Send + Sync {
    /// The device's own name, as `EVIOCGNAME` reports it.
    fn name(&self) -> Option<String>;

    /// `EVIOCGBIT(t)`: the supported-code bitmap for event type `t`, empty when
    /// the device does not answer for that type.
    fn ev_bits(&self, event_type: u8) -> Vec<u8>;

    /// `EVIOCGABS(axis)`: the geometry of one absolute axis, when the device has
    /// it.
    fn abs_info(&self, axis: u8) -> Option<AbsInfo>;

    /// `EVIOCGPROP`: the `INPUT_PROP_*` bitmap.
    fn prop_bits(&self) -> Vec<u8>;

    /// `EVIOCGID`: bus / vendor / product / version, in the struct's order.
    fn ids(&self) -> Option<[u16; 4]>;

    /// Take one event the device has reported, if it has one - `None` when its
    /// ring is empty, never a claim of an event that is not there.
    fn pop_pending_event(&self) -> Option<InputEvent>;
}

/// A `virtio-input` device behind the kernel's lock, generic over the transport
/// the crate's driver was built on - `PciTransport` on x86_64, the MMIO
/// transport on aarch64.
///
/// The crate's every method needs `&mut self` and the registry holds its devices
/// by `Arc`, so the lock is what turns the two into the `&self` methods
/// [`InputDevice`] promises. The six of them are one implementation over the one
/// crate driver, which is the point: a second transport does not get a second
/// copy of them, it gets its own `Transport` and nothing else. A machine's
/// driver module opens its transport, builds one of these, and the registry
/// above never learns which one it was.
///
/// `T: Send` is the whole of what the two `unsafe impl`s in the crate require
/// for the device to be shareable (`VirtQueue` is `Send + Sync` whatever the
/// `Hal`), which is what lets `VirtioInputDev<T>` satisfy the contract's own
/// `Send + Sync`.
pub struct VirtioInputDev<T: Transport> {
    device: Mutex<RefCell<VirtIOInput<HalImpl, T>>>,
}

impl<T: Transport> VirtioInputDev<T> {
    /// The device, parked behind the lock the contract's methods take.
    pub fn new(device: VirtIOInput<HalImpl, T>) -> Self {
        VirtioInputDev {
            device: Mutex::new(RefCell::new(device)),
        }
    }
}

impl<T: Transport + Send> InputDevice for VirtioInputDev<T> {
    fn name(&self) -> Option<String> {
        critical_section::with(|cs| self.device.borrow(cs).borrow_mut().name().ok())
    }

    fn ev_bits(&self, event_type: u8) -> Vec<u8> {
        critical_section::with(|cs| {
            self.device
                .borrow(cs)
                .borrow_mut()
                .ev_bits(event_type)
                .map(|bits| bits.into_vec())
                .unwrap_or_default()
        })
    }

    fn abs_info(&self, axis: u8) -> Option<AbsInfo> {
        critical_section::with(|cs| self.device.borrow(cs).borrow_mut().abs_info(axis).ok())
    }

    fn prop_bits(&self) -> Vec<u8> {
        critical_section::with(|cs| {
            self.device
                .borrow(cs)
                .borrow_mut()
                .prop_bits()
                .map(|bits| bits.into_vec())
                .unwrap_or_default()
        })
    }

    fn ids(&self) -> Option<[u16; 4]> {
        critical_section::with(|cs| {
            self.device
                .borrow(cs)
                .borrow_mut()
                .ids()
                .ok()
                .map(|ids| [ids.bustype, ids.vendor, ids.product, ids.version])
        })
    }

    fn pop_pending_event(&self) -> Option<InputEvent> {
        critical_section::with(|cs| self.device.borrow(cs).borrow_mut().pop_pending_event())
    }
}

/// The task that keeps the polled devices moving between userland calls.
///
/// The input devices need it - their events arrive whether or not anyone is
/// reading - and so do the vsock family, whose handshakes and buffers advance
/// while nobody is parked on them, and the display's own events. One task polls
/// them once a tick - the same arrangement the network stack's service task keeps
/// with its interface - because a reader blocked on any of them is waiting on a
/// device nobody else is going to ask.
///
/// All three halves are the *contract's*, not a machine's: a machine that declares
/// `mpos_input_registry` has input devices and needs this pump, and both machines
/// have had a vsock device and a GPU card since §13.28/§13.26 - whichever bus they
/// arrived on. So this function is gated on the capability rather than on an
/// architecture, and each half is honest when its device is absent: `input::service`
/// walks an empty registry, `net::vsock::service` finds no family and returns, and
/// `gpu::take_display_event` finds no configuration window (`CONFIG` is 0) and
/// answers `false`. A machine with none of the three pays one predicate per tick
/// and nothing else.
#[cfg(mpos_input_registry)]
pub fn spawn_service() {
    // Permanent: a listener parked in `accept` and a reader waiting on an
    // input event both depend on this task for as long as the machine runs.
    // The boot code's `drain` waits for every task a check started to be
    // reaped; this one never will be, which is what `spawn_permanent` is
    // for.
    crate::sched::spawn_permanent(
        || loop {
            input::service();
            // The vsock family's half. A machine whose vsock device never probed
            // has no family to poll and this returns without touching a device -
            // the same honesty the input half keeps on an empty registry.
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
