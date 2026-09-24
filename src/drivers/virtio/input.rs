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
use core::sync::atomic::{AtomicBool, Ordering};

use critical_section::Mutex;
use virtio_drivers::device::input::{AbsInfo, VirtIOInput};
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

/// The device kinds the evdev ABI names by number, as far as the capability
/// bitmap is walked. `EV_KEY`..`EV_FF_STATUS` sit in the low byte; 32 slots
/// cover every type a virtio-input device may advertise.
const EV_SYN: u8 = 0x00;
const EV_ABS: u8 = 0x03;
/// The highest event code EVIOCGABS is asked for - the ABS_MT_* range reaches
/// this far on modern multi-touch devices.
const ABS_MAX: u8 = 0x3f;

/// The capability picture of one input device, read from the virtio config
/// once at init and then frozen: which event types the device answers, the
/// bitmap of codes within each type, the geometry of each absolute axis, the
/// device's identity and its property bits. `libinput` reads all of this -
/// mostly through `EVIOCGBIT` - to decide what a device *is* before it ever
/// reads an event, so it must be here without any event having arrived yet.
pub struct EvdevCaps {
    /// `EVIOCGBIT(EV_SYN)`: a bitmap with bit *t* set when event type *t* is
    /// supported. Four bytes covers types 0..=31.
    type_bits: [u8; 4],
    /// `EVIOCGBIT(t)`: the supported-code bitmap for each event type, indexed
    /// by type; empty when the type is absent.
    code_bits: [Vec<u8>; 0x20],
    /// `EVIOCGABS(axis)`: the geometry of each advertised absolute axis.
    abs: Vec<(u8, AbsInfo)>,
    /// `EVIOCGPROP`: the `INPUT_PROP_*` bitmap.
    prop_bits: Vec<u8>,
    /// `EVIOCGID`: bus / vendor / product / version.
    id: [u16; 4],
}

impl Default for EvdevCaps {
    fn default() -> Self {
        EvdevCaps {
            type_bits: [0; 4],
            code_bits: core::array::from_fn(|_| Vec::new()),
            abs: Vec::new(),
            prop_bits: Vec::new(),
            id: [0; 4],
        }
    }
}

/// Query the device's whole capability picture. Every `ev_bits`/`abs_info`
/// call walks the virtio config space; a device that does not answer one is
/// simply missing that entry, which is the honest, conservative picture.
fn read_caps(device: &mut VirtIOInput<HalImpl, PciTransport>) -> EvdevCaps {
    let mut caps = EvdevCaps::default();
    for t in 0..0x20u8 {
        if let Ok(bits) = device.ev_bits(t)
            && !bits.is_empty()
        {
            caps.code_bits[t as usize] = bits.into_vec();
            caps.type_bits[(t / 8) as usize] |= 1 << (t % 8);
        }
    }
    // Every real input device frames its events with EV_SYN, whether or not
    // the config space lists it explicitly.
    caps.type_bits[0] |= 1 << EV_SYN;
    // Walk the EV_ABS code bitmap and ask for the geometry of each axis.
    let abs_map = caps.code_bits[EV_ABS as usize].clone();
    for code in 0..abs_map.len() * 8 {
        if code > ABS_MAX as usize {
            break;
        }
        if abs_map[code / 8] & (1u8 << (code % 8)) == 0 {
            continue;
        }
        if let Ok(info) = device.abs_info(code as u8) {
            caps.abs.push((code as u8, info));
        }
    }
    if let Ok(prop) = device.prop_bits() {
        caps.prop_bits = prop.into_vec();
    }
    if let Ok(ids) = device.ids() {
        caps.id = [ids.bustype, ids.vendor, ids.product, ids.version];
    }
    caps
}

/// The evdev major number - `INPUT_MAJOR` in the kernel's own headers. Every
/// `/dev/input/eventN` node carries it, and `libinput`/udev match on it.
pub const INPUT_MAJOR: u32 = 13;
/// The minor a node at bus index `i` reports: evdev numbers its `eventN`
/// from 64, one per device, in the order the bus answered them.
pub fn input_minor(index: usize) -> u32 {
    64 + index as u32
}

/// The node under `/dev/input`: a name, and a queue of encoded records.
pub struct EvdevNode {
    name: String,
    /// The inode number this node reports in `stat`. A distinct one per
    /// device, so `readdir`/`ls -i` never present two events as one file.
    ino: u64,
    queue: Mutex<RefCell<VecDeque<[u8; RECORD]>>>,
    caps: EvdevCaps,
    /// Whether some process currently holds `EVIOCGRAB` on this device. This
    /// kernel has a single seat, so a grab is remembered but never refused;
    /// Linux's contention `EBUSY` needs per-owner arbitration this lacks.
    grab: AtomicBool,
}

impl EvdevNode {
    /// The device's own name for itself, as `EVIOCGNAME` reports it - the
    /// string the host filled the device's config with, which is how an
    /// evdev reader tells a keyboard from a tablet without reading events.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The evdev ABI version, always `1`.
    pub fn version(&self) -> i32 {
        1
    }

    /// `EVIOCGID`: bus / vendor / product / version, in the struct's order.
    pub fn id(&self) -> [u16; 4] {
        self.caps.id
    }

    /// `EVIOCGBIT(evtype, len)`: the supported-event-type bitmap when
    /// `evtype` is `EV_SYN`, otherwise the supported-code bitmap for that
    /// type. Copies up to `out.len()` bytes and reports how many it wrote.
    pub fn eviocgbit(&self, evtype: u8, out: &mut [u8]) -> usize {
        let src: &[u8] = if evtype == EV_SYN {
            &self.caps.type_bits
        } else {
            self.caps
                .code_bits
                .get(evtype as usize)
                .map(|v| v.as_slice())
                .unwrap_or(&[])
        };
        let n = src.len().min(out.len());
        out[..n].copy_from_slice(&src[..n]);
        n
    }

    /// `EVIOCGABS(axis)`: the axis geometry as the six `int`s of
    /// `struct input_absinfo` - `value, min, max, fuzz, flat, resolution`
    /// (`value` is the last-seen position, which this kernel does not track).
    pub fn eviocgabs(&self, axis: u8) -> Option<[i32; 6]> {
        self.caps
            .abs
            .iter()
            .find(|(code, _)| *code == axis)
            .map(|(_, info)| {
                [
                    0i32,
                    info.min as i32,
                    info.max as i32,
                    info.fuzz as i32,
                    info.flat as i32,
                    info.res as i32,
                ]
            })
    }

    /// `EVIOCGPROP`: the `INPUT_PROP_*` bitmap.
    pub fn prop_bits(&self) -> &[u8] {
        &self.caps.prop_bits
    }

    /// `EVIOCGRAB`: whether the device is grabbed, and the act of grabbing or
    /// releasing it.
    pub fn is_grabbed(&self) -> bool {
        self.grab.load(Ordering::Relaxed)
    }

    pub fn set_grab(&self, on: bool) {
        self.grab.store(on, Ordering::Relaxed);
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
            ino: self.ino,
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
        // Read the whole capability picture before the device is parked
        // behind the service table - libinput asks for it at open time.
        let caps = read_caps(&mut device);
        let ino = 0x9000 + nodes.len() as u64;
        let node = Arc::new(EvdevNode {
            name: name.clone(),
            ino,
            queue: Mutex::new(RefCell::new(VecDeque::new())),
            caps,
            grab: AtomicBool::new(false),
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
