//! virtio-input, through the `virtio-drivers` crate.
//!
//! The virtio input device *is* evdev: it reports events in the Linux input
//! layer's own numbering - EV_KEY, EV_ABS, the lot - and the crate hands
//! them over as the three fields `struct virtio_input_event` carries. What
//! is missing is the two things evdev adds around them: a timestamp, and a
//! reader's ABI. Both are added here.
//!
//! The reader's ABI is `/dev/input/event0`, the node an evdev reader opens,
//! and the record it expects is `struct input_event`: a `timeval`, then the
//! event's type, code and value - twenty-four bytes on x86_64, a whole
//! number of which one `read(2)` returns, never a partial record, exactly
//! as Linux answers. The timestamp is the kernel's wall clock at the moment
//! the service task harvested the event, which is the honest instant: the
//! device does not carry one of its own, and the host does not deliver
//! theirs.
//!
//! The keyboard *also* being the machine's console input is what
//! [`service`] does when the framebuffer console is up (not a headless boot):
//! keyboard events are translated to bytes and routed to the active virtual
//! console, exactly as a serial keystroke would be. The `/dev/input` node stays
//! a second, separate consumer - a program that wants raw events opens the node
//! and reads records, the way every evdev client since the beginning does. On a
//! headless boot there is no framebuffer console, so only the nodes move and the
//! serial UART keeps sole ownership of the shell's input.

use alloc::collections::VecDeque;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::cell::RefCell;

use critical_section::Mutex;
use virtio_drivers::device::input::VirtIOInput;
use virtio_drivers::transport::pci::PciTransport;

use crate::vfs::{Inode, Metadata, NodeKind, Ready, VfsError, VfsResult};

use super::hal::{HalImpl, transport_all};

/// The modern device ID; virtio-input never had a legacy form. Every input
/// device - the keyboard, the tablet, the mouse - answers the same one,
/// which is why the driver probes for *all* of them and names a node per
/// device rather than one node for the first it happened to find.
const DEVICE_INPUT: &[u16] = &[0x1052];

/// One `struct input_event` on x86_64: `timeval` (two `long`s), then the
/// type, code and value. Twenty-four bytes, in the little-endian order
/// the ABI spells them.
const RECORD: usize = 24;

/// One virtio input device paired with the node its events queue into.
type Devices = Vec<(Arc<Dev>, Arc<EvdevNode>)>;

/// The devices the boot found, in the order the bus answered them.
static DEVICES: Mutex<RefCell<Devices>> = Mutex::new(RefCell::new(Vec::new()));

/// One virtio input device behind its evdev node.
struct Dev {
    device: Mutex<RefCell<VirtIOInput<HalImpl, PciTransport>>>,
}

/// The node under `/dev/input`: a name, and a queue of encoded records.
pub struct EvdevNode {
    name: String,
    queue: Mutex<RefCell<VecDeque<[u8; RECORD]>>>,
}

impl EvdevNode {
    /// The device's own name for itself, as `EVIOCGNAME` reports it - the
    /// string the host filled the device's config with, which is how an
    /// evdev reader tells a keyboard from a tablet without reading events.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Encode and queue one event, stamped with the kernel's clock.
    fn push(&self, event: &virtio_drivers::device::input::InputEvent) {
        let since_boot = crate::arch::timer::since_boot_nanos();
        let secs = crate::arch::rtc::boot_epoch() + since_boot / 1_000_000_000;
        let usecs = (since_boot % 1_000_000_000) / 1_000;

        let mut record = [0u8; RECORD];
        record[0..8].copy_from_slice(&(secs as i64).to_le_bytes());
        record[8..16].copy_from_slice(&(usecs as i64).to_le_bytes());
        record[16..18].copy_from_slice(&event.event_type.to_le_bytes());
        record[18..20].copy_from_slice(&event.code.to_le_bytes());
        record[20..24].copy_from_slice(&event.value.to_le_bytes());

        critical_section::with(|cs| {
            let mut queue = self.queue.borrow(cs).borrow_mut();
            // The queue holds what the service harvested since the last
            // read; a reader that leaves it unattended loses nothing it was
            // promised, but a bound keeps a device nobody reads from
            // pinning memory forever. The oldest events are the stalest.
            if queue.len() >= 256 {
                queue.pop_front();
            }
            queue.push_back(record);
        });
    }

    /// Whether a read would return a record now.
    fn has_record(&self) -> bool {
        critical_section::with(|cs| !self.queue.borrow(cs).borrow().is_empty())
    }

    /// Take records into `buf`, whole ones only. Returns `None` when the
    /// buffer cannot hold even one - `EINVAL`, the answer Linux gives a
    /// read smaller than one event.
    fn take(&self, buf: &mut [u8]) -> Option<usize> {
        if buf.len() < RECORD {
            return None;
        }
        let capacity = buf.len() / RECORD;
        critical_section::with(|cs| {
            let mut queue = self.queue.borrow(cs).borrow_mut();
            let mut taken = 0;
            while taken < capacity {
                let Some(record) = queue.pop_front() else {
                    break;
                };
                buf[taken * RECORD..(taken + 1) * RECORD].copy_from_slice(&record);
                taken += 1;
            }
            Some(taken * RECORD)
        })
    }
}

impl Inode for EvdevNode {
    fn metadata(&self) -> Metadata {
        Metadata {
            ino: 0x9000,
            kind: NodeKind::CharDev,
            size: 0,
        }
    }

    /// A read blocks until at least one record is queued, then returns as
    /// many whole records as fit. A buffer too small for one record is
    /// `EINVAL` rather than a zero-byte answer, which would read as end of
    /// input: an event stream has no end.
    fn read_at(&self, _offset: u64, buf: &mut [u8]) -> VfsResult<usize> {
        loop {
            match self.take(buf) {
                Some(0) => {}
                Some(n) => return Ok(n),
                None => return Err(VfsError::Invalid),
            }
            if crate::sched::has_deliverable_signal() {
                return Ok(0);
            }
            crate::sched::sleep_ticks(1);
        }
    }

    /// An input device is read, not written. `Unsupported` is what a node
    /// whose kind of file has no operation answers.
    fn write_at(&self, _offset: u64, _buf: &[u8]) -> VfsResult<usize> {
        Err(VfsError::Unsupported)
    }

    fn poll_ready(&self) -> Ready {
        Ready {
            read: self.has_record(),
            write: false,
        }
    }

    /// The evdev node this is - how `EVIOCGNAME` reaches the name the host
    /// gave the device, without the ioctl layer knowing where input nodes
    /// live.
    #[cfg(target_os = "none")]
    fn as_evdev(&self) -> Option<&EvdevNode> {
        Some(self)
    }
}

/// Probe the PCI bus for every virtio input device and build a node for
/// each. The pairs are what the devfs builder registers as
/// `/dev/input/event0`, `event1`, ... and what the service task polls.
pub fn init() -> Vec<(Arc<EvdevNode>, String)> {
    let mut nodes = Vec::new();
    let mut devices = Vec::new();
    for transport in transport_all(DEVICE_INPUT) {
        let Ok(mut device) = VirtIOInput::<HalImpl, _>::new(transport) else {
            continue;
        };
        let name = device
            .name()
            .unwrap_or_else(|_| String::from("virtio_input"));
        let node = Arc::new(EvdevNode {
            name: name.clone(),
            queue: Mutex::new(RefCell::new(VecDeque::new())),
        });
        devices.push((
            Arc::new(Dev {
                device: Mutex::new(RefCell::new(device)),
            }),
            node.clone(),
        ));
        nodes.push((node, name));
    }
    critical_section::with(|cs| {
        let mut table = DEVICES.borrow(cs).borrow_mut();
        table.extend(devices);
    });
    nodes
}

/// Every evdev node the boot built, in bus order - what the devfs builder
/// registers as `/dev/input/event0`, `event1`, .... The devices themselves
/// are already in the service table; this is the door the device names
/// come out of.
pub fn nodes() -> Vec<Arc<EvdevNode>> {
    critical_section::with(|cs| {
        DEVICES
            .borrow(cs)
            .borrow()
            .iter()
            .map(|(_, node)| node.clone())
            .collect()
    })
}

/// The keyboard's console translation state - the latched shift/ctrl/caps - kept
/// across ticks so a `Shift` press on one event colours the letter that follows.
static KEYMAP: Mutex<RefCell<crate::vfs::term::keymap::Keymap>> =
    Mutex::new(RefCell::new(crate::vfs::term::keymap::Keymap::new()));

/// `EV_KEY`, the event type a keyboard press is.
const EV_KEY: u16 = 1;

/// Harvest everything the devices have reported since the last look. The
/// service task calls this once a tick; the events are stamped as they are
/// taken, which is the first moment this kernel knew about them.
///
/// Two consumers, deliberately: every event is encoded onto its device's
/// `/dev/input/eventN` node (an evdev program reads records from there), *and*
/// - when a display is up, i.e. not headless - keyboard events are translated to
/// console bytes and fed to the active virtual console through the same funnel
/// a serial keystroke uses. Routing happens *after* the critical section, so the
/// echo the keystroke provokes can take the pty's parking branch rather than
/// growing a queue unbounded under the lock.
pub fn service() {
    let to_console = !crate::console::headless();
    let mut routed = [0u8; 64];
    let mut n = 0usize;
    critical_section::with(|cs| {
        let devices = DEVICES.borrow(cs).borrow();
        let mut keymap = KEYMAP.borrow(cs).borrow_mut();
        for (dev, node) in devices.iter() {
            let mut device = dev.device.borrow(cs).borrow_mut();
            while let Some(event) = device.pop_pending_event() {
                node.push(&event);
                if to_console && event.event_type == EV_KEY {
                    if let Some((bytes, cnt)) = keymap.event(event.code, event.value as i32) {
                        for &b in &bytes[..cnt] {
                            if n < routed.len() {
                                routed[n] = b;
                                n += 1;
                            }
                        }
                    }
                }
            }
        }
    });
    for &b in &routed[..n] {
        crate::vfs::vcmux::route_input(b);
    }
}
