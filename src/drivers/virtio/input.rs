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

// One `struct input_event`, and the window of them a device keeps, live in
// `crate::evdev` - ungated, so a host build can test the policy over them.
// This module is the device, the ioctl surface, and the encoding.
use crate::evdev::{RECORD, Records};

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

impl EvdevCaps {
    /// The capability picture of a device userland declared through
    /// `/dev/uinput`: the bitmaps it set, and nothing else.
    ///
    /// There are no axes, because `UI_ABS_SETUP` is not implemented - a
    /// synthetic tablet would answer `EVIOCGABS` `EINVAL`, which is the honest
    /// answer for a device whose geometry nobody described.
    pub fn synthetic(
        type_bits: [u8; 4],
        code_bits: [Vec<u8>; 0x20],
        prop_bits: Vec<u8>,
        id: [u16; 4],
    ) -> EvdevCaps {
        EvdevCaps {
            type_bits,
            code_bits,
            abs: Vec::new(),
            prop_bits,
            id,
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

/// The node under `/dev/input`: a name, the records the device has produced,
/// and the readers of them.
pub struct EvdevNode {
    name: String,
    /// The inode number this node reports in `stat`. A distinct one per
    /// device, so `readdir`/`ls -i` never present two events as one file.
    ino: u64,
    /// Which device of the bus this is, which is what names it: `event0`,
    /// `event1`, and the device number `13:64+index` that `stat` and the
    /// udev database key the device by.
    index: usize,
    /// The window of records and the clients reading them - see
    /// [`crate::evdev`], which is where the policy lives and where it is
    /// tested. One lock covers the whole of it, so a read cannot see a window
    /// where one client's position was moved and another's was not.
    records: Mutex<RefCell<Records>>,
    caps: EvdevCaps,
    /// `O_NONBLOCK` on the description that opened this node: a read with no
    /// record waiting answers `EAGAIN` rather than parking. It is not a nicety
    /// for a compositor but the shape of its main loop - `libinput` opens every
    /// evdev node with `O_NONBLOCK` and pumps it from `epoll`, so a read that
    /// parked where Linux answered `EAGAIN` stops the compositor at its first
    /// empty poll: no repaint, no timers, and the next device in the
    /// enumeration never added.
    nonblocking: AtomicBool,
}

impl EvdevNode {
    /// A node for a device - one the boot found on the virtio bus, or one
    /// `/dev/uinput` made. The two differ in where their records come from,
    /// not in what a reader sees, which is why there is one type for both.
    ///
    /// `index` is the device's place in the input layer: the name is
    /// `event{index}` and the minor is [`input_minor`] of it, so a synthetic
    /// device takes the next number after the bus's own.
    pub fn new(name: String, index: usize, caps: EvdevCaps) -> Arc<EvdevNode> {
        Arc::new(EvdevNode {
            name,
            ino: 0x9000 + index as u64,
            index,
            records: Mutex::new(RefCell::new(Records::new())),
            caps,
            nonblocking: AtomicBool::new(false),
        })
    }

    /// Mirror the description's `O_NONBLOCK` into the node, where the blocking
    /// actually happens. Called on open and by `fcntl(F_SETFL)`/`FIONBIO`, the
    /// same two paths every other blocking node is told by.
    pub fn set_nonblocking(&self, on: bool) {
        self.nonblocking.store(on, Ordering::Relaxed);
    }

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

    /// Whether some description currently holds the device.
    pub fn is_grabbed(&self) -> bool {
        self.grab_owner() != 0
    }

    /// The description holding the grab, or 0.
    pub fn grab_owner(&self) -> u64 {
        with_records(&self.records, |records| records.grabbed_by())
    }

    /// Claim (`on`) or drop the device for `ofd`. `false` is the answer to a
    /// claim by someone else while it is held - Linux's `EBUSY`. Dropping is
    /// never refused, and a release by a description that does not hold the
    /// device leaves it exactly as it was, as on Linux.
    ///
    /// The rule the grab is: while some description holds it, the records are
    /// that description's, and every other reader of the node waits rather than
    /// being handed them. Releasing it does not hand over what arrived while it
    /// was held - see [`crate::evdev`] for the accounting.
    pub fn set_grab(&self, on: bool, ofd: u64) -> bool {
        // A release lifts every other client past the records that were the
        // grabber's, so nobody has anything new to look at until a record is
        // pushed - which is also why a release wakes no one.
        with_records(&self.records, |records| records.set_grab(on, ofd))
    }

    /// A description has opened this node: give it a read position now rather
    /// than at its first read, so an event that arrives in between is still its
    /// to read (see `Records::opened`).
    pub fn opened(&self, ofd: u64) {
        with_records(&self.records, |records| records.opened(ofd));
    }

    /// The description is gone; forget where it had read to. An address is
    /// reused, and an inherited position would be a reader that has read what
    /// it never saw.
    pub fn forget(&self, ofd: u64) {
        with_records(&self.records, |records| records.forget(ofd));
    }

    /// Whether a record is waiting for the client `ofd` right now.
    fn readable(&self, ofd: u64) -> bool {
        with_records(&self.records, |records| records.readable(ofd))
    }

    /// Whether the device behind this node has been taken away.
    fn device_closed(&self) -> bool {
        with_records(&self.records, |records| records.closed())
    }

    /// Take records into `buf` for the client `ofd`, whole ones only. `None` is
    /// a buffer that cannot hold one record - `EINVAL`, the answer Linux gives
    /// a read smaller than one event.
    fn take(&self, ofd: u64, buf: &mut [u8]) -> Option<usize> {
        with_records(&self.records, |records| records.take(ofd, buf))
    }

    /// Report one event as this device's own: what the service task does with
    /// what the virtio ring handed it, and what a write to the `/dev/uinput`
    /// description behind a synthetic device means.
    ///
    /// The stamp is the kernel's clock at this moment, which is Linux's rule
    /// too: a uinput caller writes `struct input_event`s whose timestamp the
    /// input core overwrites, because the time an event happened is not a
    /// caller's to declare.
    pub fn inject(&self, event_type: u16, code: u16, value: i32) {
        let since_boot = crate::arch::timer::since_boot_nanos();
        let secs = crate::arch::rtc::boot_epoch() + since_boot / 1_000_000_000;
        let usecs = (since_boot % 1_000_000_000) / 1_000;

        let mut record = [0u8; RECORD];
        record[0..8].copy_from_slice(&(secs as i64).to_le_bytes());
        record[8..16].copy_from_slice(&(usecs as i64).to_le_bytes());
        record[16..18].copy_from_slice(&event_type.to_le_bytes());
        record[18..20].copy_from_slice(&code.to_le_bytes());
        record[20..24].copy_from_slice(&value.to_le_bytes());

        with_records(&self.records, |records| records.push(record));
        // The record is queued and the lock is released before anyone is woken,
        // so a woken reader that re-checks the window immediately sees it.
        self.wake_waiters();
    }

    fn push(&self, event: &virtio_drivers::device::input::InputEvent) {
        self.inject(event.event_type, event.code, event.value as i32);
    }

    /// Take the device away: every reader is woken, and the next look at it is
    /// the end of input rather than an empty device. This is what
    /// `UI_DEV_DESTROY` means to whoever is reading a `/dev/uinput` device, and
    /// what closing the description that made it means.
    pub fn close(&self) {
        let waiting = with_records(&self.records, |records| {
            records.close();
            records.all_waiters()
        });
        for id in waiting {
            crate::sched::wake(id);
        }
    }

    /// Park `id` on this device as the client `ofd`, and again take it off when
    /// the wait ends.
    fn subscribe(&self, ofd: u64, id: crate::sched::policy::TaskId) {
        with_records(&self.records, |records| records.subscribe(ofd, id));
    }

    fn unsubscribe(&self, id: crate::sched::policy::TaskId) {
        with_records(&self.records, |records| records.unsubscribe(id));
    }

    /// Wake the waits a new record is actually for: the readers the grab allows
    /// and nobody else. Waking a non-owner would be a task switch per keystroke
    /// that ends the way it started.
    fn wake_waiters(&self) {
        // The ids leave the critical section as a copy, because `wake` wants the
        // scheduler's own lock and a wake inside a `critical_section` is how one
        // lock comes to depend on another.
        let due = with_records(&self.records, |records| records.due_waiters());
        for id in due {
            crate::sched::wake(id);
        }
    }
}

/// Run one operation against the record state under its lock.
///
/// One lock covers the window, the client positions and the grab together,
/// because a read that saw half of them would be a read of a state that never
/// existed - a position advanced for one client while another's was not.
fn with_records<R>(records: &Mutex<RefCell<Records>>, op: impl FnOnce(&mut Records) -> R) -> R {
    critical_section::with(|cs| op(&mut records.borrow(cs).borrow_mut()))
}

impl Inode for EvdevNode {
    fn metadata(&self) -> Metadata {
        Metadata {
            ino: self.ino,
            kind: NodeKind::CharDev,
            size: 0,
        }
    }

    /// The device number `stat` reports: input's major 13, and the minor
    /// [`input_minor`] gives the device.
    ///
    /// This is not decoration. It is how a device is *identified* - libudev
    /// keys its database on `c13:64`, and libinput finds a device's
    /// properties (and so the device) through that key - so a node that
    /// reports `0,0` is a node no input stack can see, however well its
    /// events read. weston's "no input devices on entering Weston" was this.
    fn rdev(&self) -> Option<(u32, u32)> {
        Some((13, input_minor(self.index)))
    }

    /// A read blocks until a record is queued *for this client*, then returns
    /// as many whole records as fit. A buffer too small for one record is
    /// `EINVAL` rather than a zero-byte answer, which would read as end of
    /// input: an event stream has no end.
    fn read_at_client(&self, _offset: u64, buf: &mut [u8], client: u64) -> VfsResult<usize> {
        loop {
            if with_records(&self.records, |records| records.closed()) {
                // The device was taken away under this reader. An event stream
                // with no device behind it is at its end, which is the
                // zero-byte answer - the same one a hangup gives.
                return Ok(0);
            }
            match self.take(client, buf) {
                Some(0) => {}
                Some(n) => return Ok(n),
                None => return Err(VfsError::Invalid),
            }
            // The description asked not to wait, and there is nothing to hand
            // it: `EAGAIN`, which is how a caller that polls the node learns
            // there is nothing yet without its whole loop stopping here.
            if self.nonblocking.load(Ordering::Relaxed) {
                return Err(VfsError::Again);
            }
            if crate::sched::has_deliverable_signal() {
                return Ok(0);
            }
            // On the record, not on the clock. The service task that harvests
            // the virtio ring wakes this client the moment a record lands, so a
            // keystroke costs the reader its own wake-up rather than the
            // remainder of the tick - and `block_until` re-checks the condition
            // under the scheduler's lock as it parks, so an event that arrives
            // between the failed `take` and the sleep is not lost.
            let me = crate::sched::current();
            self.subscribe(client, me);
            crate::sched::block_until(|| {
                self.device_closed()
                    || self.readable(client)
                    || crate::sched::has_deliverable_signal()
            });
            self.unsubscribe(me);
        }
    }

    /// The clientless read: a kernel path that never named a description - the
    /// VFS's own `read_file`, or a `splice` source. `read`, `readv` and `pread`
    /// all arrive through `File::read` and carry the real description, so this
    /// is only what is left of the read surface. It reads as a client of its
    /// own, keyed 0 - an address no description can have - so it neither steals
    /// from a real reader nor is served while the device is grabbed.
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> VfsResult<usize> {
        self.read_at_client(offset, buf, 0)
    }

    /// An input device is read, not written. `Unsupported` is what a node
    /// whose kind of file has no operation answers.
    fn write_at(&self, _offset: u64, _buf: &[u8]) -> VfsResult<usize> {
        Err(VfsError::Unsupported)
    }

    fn poll_ready_client(&self, client: u64) -> Ready {
        Ready {
            read: self.readable(client),
            write: false,
        }
    }

    fn poll_ready(&self) -> Ready {
        self.poll_ready_client(0)
    }

    fn poll_subscribe_client(&self, client: u64, id: crate::sched::policy::TaskId) {
        self.subscribe(client, id);
    }

    fn poll_subscribe(&self, id: crate::sched::policy::TaskId) {
        self.subscribe(0, id);
    }

    fn poll_unsubscribe(&self, id: crate::sched::policy::TaskId) {
        self.unsubscribe(id);
    }

    /// The evdev node this is - how `EVIOCGNAME` reaches the name the host
    /// gave the device, without the ioctl layer knowing where input nodes
    /// live.
    #[cfg(target_os = "none")]
    fn as_evdev(&self) -> Option<&EvdevNode> {
        Some(self)
    }
}

/// Forget the read positions of the descriptions a process is taking with it.
///
/// The batch counterpart of [`EvdevNode::forget`], for the exit path, where the
/// descriptors are handed over together rather than closed one at a time: a
/// description that several of them name is forgotten once, and only when the
/// batch holds every reference to it - the same accounting `inotify`'s close
/// events do, and for the same reason, since a `dup` is the same reader.
#[cfg(target_os = "none")]
pub fn close_at_exit(files: &[Arc<crate::vfs::file::File>]) {
    crate::vfs::file::for_each_last(files, |file, ofd| {
        if let Some(ev) = file.inode().as_evdev() {
            ev.forget(ofd);
        }
    });
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
        let node = EvdevNode::new(name.clone(), nodes.len(), caps);
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

/// The event types whose code bitmap `/sys` publishes, with the number of
/// bits Linux declares for each. The count is not the device's: it is the
/// *type's* - `EV_KEY` covers `KEY_MAX` whatever a keyboard implements - and
/// it is what decides how many words the attribute is printed in.
///
/// `ev` is the one that matters most: `udev`'s `input_id` builtin finds the
/// input device behind an `eventN` node by walking up to the first parent with
/// a `capabilities/ev` attribute, so a tree without it is a tree in which no
/// device is ever an input device.
pub const CAPABILITY_TYPES: &[(&str, u8, usize)] = &[
    ("ev", 0, 32),                // EV_SYN: the type bitmap itself
    ("key", EV_KEY as u8, 0x300), // KEY_MAX + 1
    ("rel", 2, 16),               // EV_REL: REL_MAX + 1
    ("abs", 3, 64),               // EV_ABS: ABS_MAX + 1
];

/// The most bytes any of those bitmaps can need: `capabilities/key` covers
/// `KEY_MAX` + 1 bits, and a caller reading one needs room for all of them.
pub const CAPABILITY_BYTES: usize = 0x300 / 8;

/// Harvest everything the devices have reported since the last look. The
/// service task calls this once a tick; the events are stamped as they are
/// taken, which is the first moment this kernel knew about them.
///
/// Two consumers, deliberately: every event is encoded onto its device's
/// `/dev/input/eventN` node (an evdev program reads records from there), *and*
/// when a display is up - i.e. not headless - keyboard events are translated
/// to console bytes and fed to the active virtual console through the same
/// funnel a serial keystroke uses. Routing happens *after* the critical
/// section, so the echo the keystroke provokes can take the pty's parking
/// branch rather than growing a queue unbounded under the lock.
pub fn service() {
    let to_console = !crate::console::headless();
    let mut routed = [0u8; 64];
    let mut n = 0usize;
    critical_section::with(|cs| {
        let devices = DEVICES.borrow(cs).borrow();
        let mut keymap = KEYMAP.borrow(cs).borrow_mut();
        for (dev, node) in devices.iter() {
            let mut device = dev.device.borrow(cs).borrow_mut();
            // A grabbed device belongs to whoever grabbed it. libinput takes
            // the grab the moment it opens an evdev node, and this check is
            // what that means here: the keystrokes stop being *also* console
            // input, so a compositor's typing does not land on the shell
            // sitting behind it. The node still gets every event - that is
            // where the compositor reads them from.
            let to_console = to_console && !node.is_grabbed();
            while let Some(event) = device.pop_pending_event() {
                node.push(&event);
                if to_console
                    && event.event_type == EV_KEY
                    && let Some((bytes, cnt)) = keymap.event(event.code, event.value as i32)
                {
                    for &b in &bytes[..cnt] {
                        if n < routed.len() {
                            routed[n] = b;
                            n += 1;
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
