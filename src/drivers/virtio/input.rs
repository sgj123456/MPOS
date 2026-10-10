//! virtio-input, through the `virtio-drivers` crate.
//!
//! The virtio input device *is* evdev: it reports events in the Linux input
//! layer's own numbering - EV_KEY, EV_ABS, the lot - and the crate hands
//! them over as the three fields `struct virtio_input_event` carries. What
//! is missing is the two things evdev adds around them: a timestamp, and a
//! reader's ABI. Both are added here.
//!
//! What the registry holds is [`InputDevice`], not a transport. The crate's
//! driver is opened over `PciTransport` on the first machine ([`init`]) and over
//! the MMIO transport on the second (`super::mmio`), and both hand the registry
//! the same contract - so the nodes, the ioctl vocabulary, the record window and
//! the service task below are one copy on both machines (`NOTES.md` §15.2).
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
//! [`service`] does when a text console is live (`crate::console::vt_live`):
//! keyboard events are translated to bytes and routed to the active virtual
//! console, exactly as a serial keystroke would be. The `/dev/input` node stays
//! a second, separate consumer - a program that wants raw events opens the node
//! and reads records, the way every evdev client since the beginning does. On a
//! machine that has no framebuffer console at all - a boot whose console list
//! names the wire alone and no session has claimed the card - only the nodes
//! move, and the serial UART keeps sole ownership of the shell's input.

use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::cell::RefCell;
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use critical_section::Mutex;
use virtio_drivers::device::input::AbsInfo;
use virtio_drivers::device::input::VirtIOInput;

use crate::vfs::{Inode, Metadata, NodeKind, Ready, VfsError, VfsResult};

use super::InputDevice;
use super::VirtioInputDev;
use super::hal::{HalImpl, transport_all};

/// The modern device ID; virtio-input never had a legacy form. Every input
/// device - the keyboard, the tablet, the mouse - answers the same one,
/// which is why the driver probes for *all* of them and names a node per
/// device rather than one node for the first it happened to find.
///
/// It names the PCI vendor's ID range, so it is the first machine's: aarch64
/// finds the same device by the `virtio,mmio` window its device tree names
/// (`super::mmio`), and both arrive at [`register`] holding the same
/// [`InputDevice`].
const DEVICE_INPUT: &[u16] = &[0x1052];

// One `struct input_event`, and the window of them a device keeps, live in
// `crate::evdev` - ungated, so a host build can test the policy over them.
// This module is the device, the ioctl surface, and the encoding.
use crate::evdev::{RECORD, Records};

/// One input device paired with the node its events queue into.
///
/// The device is the *contract*, not a transport: which bus answered it is the
/// machine's fact and the registry never asks, so `EvdevNode`, the record
/// window behind it and the service task below are one copy on both machines
/// (`NOTES.md` §15.2).
type Devices = Vec<(Arc<dyn InputDevice>, Arc<EvdevNode>)>;

/// The devices the boot found, in the order the bus answered them.
static DEVICES: Mutex<RefCell<Devices>> = Mutex::new(RefCell::new(Vec::new()));

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
fn read_caps(device: &dyn InputDevice) -> EvdevCaps {
    let mut caps = EvdevCaps::default();
    for t in 0..0x20u8 {
        let bits = device.ev_bits(t);
        if !bits.is_empty() {
            caps.code_bits[t as usize] = bits;
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
        if let Some(info) = device.abs_info(code as u8) {
            caps.abs.push((code as u8, info));
        }
    }
    // A device that answers no property bitmap leaves the default empty one,
    // which is the same picture the ABI reports for it.
    caps.prop_bits = device.prop_bits();
    if let Some(id) = device.ids() {
        caps.id = id;
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
    /// The key repeat the device reports through `EVIOCGREP` and remembers from
    /// `EVIOCSREP`, in milliseconds - delay then period. Linux's input core
    /// defaults these to `250` and `33` (`REP_DELAY`/`REP_PERIOD`), measured
    /// here on a real keyboard's `EVIOCGREP`; a device that advertises `EV_REP`
    /// answers with them and stores what a caller sets, rather than the
    /// zeroed-buffer false-success this node used to hand back.
    rep_delay: AtomicU32,
    rep_period: AtomicU32,
    /// The node's mode and owner, kept so `chmod`/`chown` - udev's
    /// `MODE=0660 GROUP=input` among them - stick and are what `stat` and the
    /// permission checks read (see `crate::vfs::devfs::NodePerms`).
    perms: crate::vfs::devfs::NodePerms,
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
            rep_delay: AtomicU32::new(250),
            rep_period: AtomicU32::new(33),
            perms: crate::vfs::devfs::NodePerms::new(0o666, 0, 0),
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

    /// Whether the device advertises event type `evtype` in its own type
    /// picture - the same bit `EVIOCGBIT(EV_SYN)` exposes. Linux keys several
    /// ioctls' support on exactly this (`test_bit(EV_FF, ...)` for force
    /// feedback, `EV_REP` for the repeat rate), so the answer belongs here, on
    /// the device, not to a guess in the syscall layer.
    pub fn supports_type(&self, evtype: u8) -> bool {
        self.caps
            .type_bits
            .get(evtype as usize / 8)
            .is_some_and(|byte| byte & (1u8 << (evtype % 8)) != 0)
    }

    /// The repeat `EVIOCGREP` reports: `(delay, period)` in milliseconds.
    pub fn repeat(&self) -> (u32, u32) {
        (
            self.rep_delay.load(Ordering::Relaxed),
            self.rep_period.load(Ordering::Relaxed),
        )
    }

    /// `EVIOCSREP`: store the repeat a caller set, so the next `EVIOCGREP`
    /// reads it back - the round-trip Linux's input core provides.
    pub fn set_repeat(&self, delay: u32, period: u32) {
        self.rep_delay.store(delay, Ordering::Relaxed);
        self.rep_period.store(period, Ordering::Relaxed);
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

    /// Revoke (`on`) or restore this description's access to the device -
    /// `EVIOCREVOKE`. A revoke must also break a reader parked in `read`, so the
    /// waits belonging to `ofd` are woken outside the lock, exactly as
    /// [`EvdevNode::close`] wakes them when the device goes away.
    pub fn eviorevoke(&self, ofd: u64, on: bool) {
        let waiting = with_records(&self.records, |records| {
            records.set_revoked(ofd, on);
            records.waiters_of(ofd)
        });
        for id in waiting {
            // The record window hands back a number, not a task: naming one is
            // the driver's business, since the crate cannot (`evdev::WaiterId`).
            crate::sched::wake(crate::sched::TaskId(id));
        }
    }

    /// Whether this description's access has been revoked: the read and ioctl
    /// paths turn the fact into `ENODEV`.
    pub fn is_revoked(&self, ofd: u64) -> bool {
        with_records(&self.records, |records| records.is_revoked(ofd))
    }

    /// Serve one of this evdev node's `'E'`-typed ioctls. The ABI layer has
    /// already resolved the descriptor to this node and checked the request's
    /// magic byte, so `request & 0xff` is the command number and the `_IOC`
    /// word's size/direction fields drive the buffer copies. This is Linux's
    /// `evdev_unlocked_ioctl`: the device - not the syscall switchboard - owns
    /// its request vocabulary and answers every read from its own state. It
    /// reaches a caller's memory only through `crate::uaccess`, the same
    /// leaf a driver validates a user pointer with (V-9, closed by B3).
    pub fn ioctl(&self, ofd: u64, request: u32, buf: usize, from_user: bool) -> isize {
        use crate::err;
        use crate::uaccess::buffer_ok;
        use core::mem::size_of;
        let size = ((request >> 16) & 0x3fff) as usize;
        let dir = (request >> 30) & 0x3;
        let nr = request & 0xff;
        // A revoked description answers `ENODEV` to every request it makes of
        // the device from here on - Linux's `evdev` gates each op on
        // `client->revoked` - with the single exception of `EVIOCREVOKE`
        // itself, which stays serviceable (a revoked client calling it again is
        // the idempotent revoke Linux allows; `EVIOCREVOKE` is one-way and
        // there is no ioctl to reverse it).
        if (dir, nr) != (1, 0x91) && self.is_revoked(ofd) {
            return err::ENODEV;
        }
        // Copy `src` into the caller's read buffer, no more than it asked for.
        let give = |src: &[u8]| -> isize {
            let n = src.len().min(size);
            if n == 0 {
                return 0;
            }
            if !buffer_ok(buf, n, true, from_user) {
                return err::EFAULT;
            }
            unsafe {
                core::ptr::copy_nonoverlapping(src.as_ptr(), buf as *mut u8, n);
            }
            0
        };
        // A pressed/switch-state getter (`EVIOCGKEY`/`LED`/`SND`/`SW`): Linux
        // copies `min(len, bitmap_bytes)` bytes of the current state - and
        // returns *that count*, not `0` (measured: KEY=96, LED/SND/SW=8 at rest,
        // on both a mouse and a keyboard, so the size is the global `<TYPE>_MAX`
        // bitmap `BITS_TO_LONGS(MAX+1)*8`, not the device's used codes). This
        // kernel keeps no pressed state, so the picture is that many zero bytes;
        // the point of the count is that a caller learns the real bitmap size
        // and a buffer shorter than it is filled short, exactly as on Linux -
        // never the old `give(&[])` that answered success having written nothing.
        let give_state = |bitmap_bytes: usize| -> isize {
            let n = size.min(bitmap_bytes);
            if n == 0 {
                return 0;
            }
            if !buffer_ok(buf, n, true, from_user) {
                return err::EFAULT;
            }
            unsafe {
                core::ptr::write_bytes(buf as *mut u8, 0, n);
            }
            n as isize
        };
        match (dir, nr) {
            // Reads.
            (2, 0x01) => give(&1i32.to_ne_bytes()), // EVIOCGVERSION
            (2, 0x02) => {
                // EVIOCGID: struct input_id {u16 bustype,vendor,product,version}.
                let id = self.id();
                let mut b = [0u8; 8];
                for (i, w) in id.iter().enumerate() {
                    b[i * 2..i * 2 + 2].copy_from_slice(&w.to_ne_bytes());
                }
                give(&b)
            }
            (2, 0x06) => {
                // EVIOCGNAME: the device's name for itself - the string the host
                // filled the virtio device's config with, which is how an evdev
                // reader tells a keyboard from a tablet without reading events.
                // The request word carries the caller's buffer size in the `_IOC`
                // size field, so the limit comes out of the request rather than
                // being a separate argument.
                //
                // Three details are `strlcpy`'s, and they are what a caller has
                // always been promised. The name is what stands before its *first*
                // NUL: a device reports a C string, and the terminator the virtio
                // config carries is not part of it. A size of zero is the length
                // query, answered with the length and no copy. And the answer is
                // that length *excluding* the terminating NUL even when the copy
                // had to be cut short - which is how a caller with a buffer too
                // small learns that the name did not fit.
                let name = self.name().as_bytes();
                let text = name.split(|&byte| byte == 0).next().unwrap_or(&[]);
                if size == 0 {
                    return text.len() as isize;
                }
                if !buffer_ok(buf, size, true, from_user) {
                    return err::EFAULT;
                }
                let n = text.len().min(size - 1);
                unsafe {
                    core::ptr::copy_nonoverlapping(text.as_ptr(), buf as *mut u8, n);
                    (buf as *mut u8).add(n).write_volatile(0);
                }
                text.len() as isize
            }
            (2, 0x07) | (2, 0x08) => {
                // EVIOCGPHYS/EVIOCGUNIQ: the device's physical path and its
                // unique id. A virtio input device has neither - they are
                // strings the kernel's input core fills from the driver, and
                // none of them puts one here.
                //
                // The answer is `ENOENT`, not `ENOTTY`, and it has to be:
                // libevdev's `libevdev_set_fd` tolerates a failure here only
                // when errno is `ENOENT` ("uinput has no phys"), the way Linux's
                // own `str_to_user` answers for a NULL attribute. An `ENOTTY`
                // makes the whole read fatal, and the failure surfaces far from
                // here: libinput gives up with "failed to create input device"
                // for every node on the machine, and weston comes up with a
                // screen and no keyboard.
                err::ENOENT
            }
            (2, 0x09) => give(self.prop_bits()), // EVIOCGPROP
            (2, 0x20..=0x3f) => {
                // EVIOCGBIT(evtype): the supported-type or -code bitmap.
                let mut tmp = [0u8; 64];
                let n = self.eviocgbit((nr - 0x20) as u8, &mut tmp);
                give(&tmp[..n])
            }
            (2, 0x40..=0x7f) => {
                // EVIOCGABS(axis): struct input_absinfo, six ints.
                match self.eviocgabs((nr - 0x40) as u8) {
                    Some(a) => {
                        let mut b = [0u8; 6 * size_of::<i32>()];
                        for (i, v) in a.iter().enumerate() {
                            b[i * 4..i * 4 + 4].copy_from_slice(&v.to_ne_bytes());
                        }
                        give(&b)
                    }
                    // An axis the device does not have is EINVAL, as Linux's
                    // input_get_absinfo answers.
                    None => err::EINVAL,
                }
            }
            (2, 0x84) => give(&0i32.to_ne_bytes()), // EVIOCGEFFECTS
            (2, 0x18) => give_state(96),            // EVIOCGKEY: BITS_TO_LONGS(KEY_MAX+1)*8
            (2, 0x19) => give_state(8),             // EVIOCGLED
            (2, 0x1a) => give_state(8),             // EVIOCGSND
            (2, 0x1b) => give_state(8),             // EVIOCGSW
            (2, 0x0a) => {
                // EVIOCGMTSLOTS: the multi-touch slot values. Measured on a node
                // without multi-touch this is `EINVAL`, and no evdev node here is
                // a multitouch device (virtio-input's tablets are single-pointer,
                // no `ABS_MT_SLOT`), so `EINVAL` is Linux's answer for these
                // shapes. A real multi-touch node would return the per-slot value
                // arrays - a device that does not exist here, so the request is
                // refused, not faked with the old `give(&[])` success.
                err::EINVAL
            }
            (2, 0x03) => {
                // EVIOCGREP: the key repeat. Measured on real Linux: a device that
                // advertises `EV_REP` returns its `(delay, period)` in ms - the
                // input core's `250`/`33` by default - and one that does not
                // answers `ENOSYS`. The node now stores the pair (defaulted to
                // Linux's own values), so the get reports the device's real
                // picture instead of the old `give(&[])`, which answered success
                // while leaving the caller's two ints whatever memory held. The
                // device's `EV_REP` bit is what the refusal rests on, exactly as
                // `evdev` keys it.
                if !self.supports_type(0x14 /* EV_REP */) {
                    return err::ENOSYS;
                }
                let (delay, period) = self.repeat();
                let mut b = [0u8; 8];
                b[0..4].copy_from_slice(&delay.to_ne_bytes());
                b[4..8].copy_from_slice(&period.to_ne_bytes());
                give(&b)
            }
            (2, 0x04) => {
                // EVIOCGKEYCODE / EVIOCGKEYCODE_V2: the current keymap entry.
                // Measured on real Linux: a device with no input keycodes array
                // answers `EINVAL`, a keyboard that has one returns the mapping.
                // An evdev node here models no per-device keycodes array (the
                // console keymap is compiled in, not reachable through the input
                // device), which is the keymapless shape Linux refuses with
                // `EINVAL`. The arm used to answer `give(&[])` - success while
                // leaving the caller's struct untouched, the §4.2 false-success
                // this removes.
                err::EINVAL
            }
            // Writes.
            (1, 0x90) => {
                // EVIOCGRAB: claim the device for *this* open file description,
                // and refuse a claim on a device somebody else holds with the
                // `EBUSY` the man page spells. Keyed on the description because
                // that is what makes the claim die with `close(2)` below rather
                // than outliving it in whatever process happens to have the
                // device open twice.
                if size_of::<i32>() > size || !buffer_ok(buf, size_of::<i32>(), false, from_user) {
                    return err::EINVAL;
                }
                let on = unsafe { (buf as *const i32).read_unaligned() } != 0;
                if self.set_grab(on, ofd) {
                    0
                } else {
                    err::EBUSY
                }
            }
            (1, 0x91) => {
                // EVIOCREVOKE. Per Linux's `evdev.c` - `case EVIOCREVOKE: if (p)
                // return -EINVAL; else return evdev_revoke(...)` - the argument
                // must be NULL: any non-NULL pointer is refused with `EINVAL`,
                // and the request revokes this description. It is *one-way*:
                // there is no ioctl to give the descriptor back (the caller
                // closes and reopens), so a revoked description calling this
                // again is just the idempotent revoke (`0`) - the entry check
                // above keeps `EVIOCREVOKE` itself serviceable on a revoked fd
                // for exactly that. Revoking is what makes the entry check bite:
                // every later read and ioctl on this descriptor answers `ENODEV`,
                // and a parked reader is woken - the seat mechanism that takes a
                // device back from a reader that no longer owns it. The old arm
                // read `arg` as a *value* (`1` revoke, `0` restore, `>1`
                // `EINVAL`) - the wrong shape: a correct `ioctl(fd, EVIOCREVOKE,
                // NULL)` (arg `0`) landed in the restore branch and did the
                // opposite of what the caller asked, and there was a reversible
                // state Linux never had.
                if buf != 0 {
                    return err::EINVAL;
                }
                self.eviorevoke(ofd, true);
                0
            }
            (1, 0x80) | (1, 0x81) => {
                // EVIOCSFF / EVIOCRMFF: queue or erase a force-feedback effect.
                // Measured on real Linux (every device here): a node without
                // `EV_FF` answers `ENOSYS` to both - `evdev` keys them on
                // `test_bit(EV_FF)`. No evdev node on this machine advertises
                // `EV_FF` (virtio-input and uinput carry no effect types here)
                // and the kernel implements no effect store, so `ENOSYS` is
                // Linux's answer for exactly this device shape. The old `0` told
                // a caller an effect was queued when nothing was - success for a
                // thing not done. The request is device-driven in the sense
                // Linux means: the device's own `EV_FF` picture is what the
                // refusal rests on.
                err::ENOSYS
            }
            (1, 0x04) => {
                // EVIOCSKEYCODE / EVIOCSKEYCODE_V2: remap one key. Measured: a
                // device with no keycodes array answers `EINVAL`, a keyboard
                // stores the mapping and returns `0`. An evdev node here has no
                // per-device keycodes array (see the get above), so `EINVAL` is
                // Linux's answer for the device's real shape; the old `0` claimed
                // a remap no reader would ever observe. `EVIOCREVOKE` is no
                // longer in this group - it is implemented above, because its
                // revoke has one unambiguous shape.
                err::EINVAL
            }
            (1, 0x03) => {
                // EVIOCSREP: set the key repeat. Guarded on the device's `EV_REP`
                // bit like the get (measured: `ENOSYS` where the device does not
                // carry repeat). Linux reads the two ints out of the caller's
                // buffer with `get_user`, so a bad pointer is `EFAULT` and a
                // valid one is stored and answered `0` - the round-trip
                // `EVIOCGREP` above then reports.
                if !self.supports_type(0x14 /* EV_REP */) {
                    return err::ENOSYS;
                }
                if !buffer_ok(buf, size_of::<u32>() * 2, false, from_user) {
                    return err::EFAULT;
                }
                let delay = unsafe { (buf as *const u32).read_unaligned() };
                let period = unsafe { (buf as *const u32).add(1).read_unaligned() };
                self.set_repeat(delay, period);
                0
            }
            _ => err::ENOTTY,
        }
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
        let since_boot = crate::hal::since_boot_nanos();
        let secs = crate::hal::boot_epoch() + since_boot / 1_000_000_000;
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
        let value = self.clamp_abs(event.event_type, event.code, event.value as i32);
        self.inject(event.event_type, event.code, value);
    }

    /// Bring an `EV_ABS` report inside the `[min, max]` this node advertises for
    /// that axis through `EVIOCGABS`.
    ///
    /// The virtio-tablet's absolute value is produced by the host against the
    /// *host window* geometry, which during a resize is not the size the guest
    /// scanout is driven at; QEMU relays it without clamping, so a raw value can
    /// exceed the `0x7FFF` the device config declares. libinput maps a position
    /// as `(value - min) / (max - min)`, and a value past `max` puts the pointer
    /// off the screen - and warns, every such event, that the axis is outside its
    /// own range. Clamping to the advertised range keeps the pointer on the
    /// screen and makes the two - what the device says and what it sends - agree.
    /// Only the position axes are touched; every other event passes through.
    fn clamp_abs(&self, event_type: u16, code: u16, value: i32) -> i32 {
        if event_type != EV_ABS as u16 {
            return value;
        }
        // `min`/`max` are the axis' own geometry as `eviocgabs` reads it back; a
        // code with no advertised range (uinput, or an axis not in the table) is
        // left exactly as it arrived rather than clamped to a guess.
        let abs = self.caps.abs.iter().find(|(c, _)| *c as u16 == code);
        match abs {
            Some((_, info)) => {
                let min = info.min as i32;
                let max = info.max as i32;
                value.clamp(min, max)
            }
            None => value,
        }
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
            // The record window hands back a number, not a task: naming one is
            // the driver's business, since the crate cannot (`evdev::WaiterId`).
            crate::sched::wake(crate::sched::TaskId(id));
        }
    }

    /// Park `id` on this device as the client `ofd`, and again take it off when
    /// the wait ends.
    fn subscribe(&self, ofd: u64, id: crate::sched::policy::TaskId) {
        with_records(&self.records, |records| records.subscribe(ofd, id.0));
    }

    fn unsubscribe(&self, id: crate::sched::policy::TaskId) {
        with_records(&self.records, |records| records.unsubscribe(id.0));
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
            crate::sched::wake(crate::sched::TaskId(id));
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

    fn keeps_modes(&self) -> bool {
        self.perms.keeps()
    }

    fn mode(&self) -> u32 {
        self.perms.mode()
    }

    fn set_mode(&self, mode: u32) {
        self.perms.set_mode(mode);
    }

    fn owner(&self) -> (u32, u32) {
        self.perms.owner()
    }

    fn set_owner(&self, uid: u32, gid: u32) {
        self.perms.set_owner(uid, gid);
    }

    fn atime(&self) -> u64 {
        self.perms.atime()
    }

    fn mtime(&self) -> u64 {
        self.perms.mtime()
    }

    fn set_times(&self, atime: u64, mtime: u64) -> VfsResult<()> {
        self.perms.set_times(atime, mtime);
        Ok(())
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
            // A revoked description is not at the end of input (the `closed()`
            // branch below, which answers `0`); it has lost access outright, and
            // every read answers `ENODEV` - Linux's `evdev_read` once the client
            // carries the revoke bit. Checked first, because a revoked reader
            // must not block waiting for records it can never be given.
            if self.is_revoked(client) {
                return Err(VfsError::Revoked);
            }
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
            // A catchable signal ends the wait with `EINTR`, not with 0. The
            // two answers mean opposite things on this door and only one of
            // them is retryable: 0 is what a device taken away answers (the
            // branch above), so a reader told its stream had ended stops
            // reading for good - which is what libinput does with it.
            if crate::sched::has_deliverable_signal() {
                return Err(VfsError::Interrupted);
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
                    || self.is_revoked(client)
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
}

/// Forget the read positions of the descriptions a process is taking with it.
///
/// The batch counterpart of [`EvdevNode::forget`], for the exit path, where the
/// descriptors are handed over together rather than closed one at a time: a
/// description that several of them name is forgotten once, and only when the
/// batch holds every reference to it - the same accounting `inotify`'s close
/// events do, and for the same reason, since a `dup` is the same reader.
#[cfg(target_os = "none")]
pub fn close_at_exit(files: &[Arc<crate::vfs::file::File>], _owner: u32) {
    crate::vfs::file::for_each_last(files, |file, ofd| {
        if let Some(ev) = file.inode().as_any().downcast_ref::<EvdevNode>() {
            ev.forget(ofd);
        }
    });
}

/// Build the evdev registry from a machine's input devices, one node per device
/// in the order given, and hand back the node names for the boot to print.
///
/// This is the one door into the registry, and it takes the *contract* rather
/// than a bus. Each machine opens its own transport - [`init`] below enumerates
/// virtio-PCI, `super::mmio` opens the window its device tree named - and every
/// line here is the same on both: the capability picture read once at init, the
/// `event0`, `event1`, ... numbering, the pairs the devfs builder publishes as
/// `/dev/input/eventN` and the service task drains.
pub fn register(devices: Vec<Arc<dyn InputDevice>>) -> Vec<(Arc<EvdevNode>, String)> {
    let mut nodes = Vec::new();
    let mut table = Vec::new();
    for device in devices {
        // The name and the whole capability picture are read here, before the
        // device is parked behind the service table - libinput asks for both at
        // open time, with no event having arrived yet.
        let name = device
            .name()
            .unwrap_or_else(|| String::from("virtio_input"));
        let caps = read_caps(&*device);
        let node = EvdevNode::new(name.clone(), nodes.len(), caps);
        table.push((device, node.clone()));
        nodes.push((node, name));
    }
    critical_section::with(|cs| {
        let mut devices = DEVICES.borrow(cs).borrow_mut();
        devices.extend(table);
    });
    // The nodes are this driver's to publish: the registry is up, so the device
    // set userland is handed should carry them. `devices::publish` states the
    // arrangement; this is the input half of it.
    crate::drivers::devices::publish("virtio-input", publish_nodes);
    nodes
}

/// Publish `/dev/input/eventN` and the `/dev/uinput` door for a device set
/// built after the registry came up.
///
/// `/dev/input` exists whether or not the bus answered with a device: it is
/// where `uinput` puts the devices userland makes, and a directory that only
/// appears when the machine happens to have a keyboard is a directory a program
/// cannot depend on. Each node is also announced with `SUBSYSTEM=input` and
/// DEVNAME pointing at the `/dev/input` path it actually lives under, because
/// udev and libinput find a device by the pair of its sysfs `dev` number and
/// this node.
fn publish_nodes(dev: &crate::vfs::devfs::DevFs) {
    let input_dir = dev.register_dir("input");
    let evdevs = nodes();
    for (index, node) in evdevs.iter().enumerate() {
        let name = alloc::format!("event{index}");
        let major = INPUT_MAJOR;
        let minor = input_minor(index);
        input_dir.insert(&name, node.clone());
        // The event node's own path, not the input device it reports through.
        // Linux's uevent for `/dev/input/event0` is
        // `DEVPATH=.../input0/event0` beside `DEVNAME=input/event0`, and udev
        // keys its database entry by the syspath the event names - so
        // announcing the parent with the child's devnum stores an entry that
        // neither `udevadm info` on the event node nor libinput can find. The
        // device is then never tagged `ID_INPUT`, libinput's seat enumeration
        // passes every one of them by, and weston comes up with a screen and
        // "no input devices found".
        crate::uevent::publish(&crate::uevent::Event {
            action: "add",
            devpath: alloc::format!("/devices/virtual/input/input{index}/{name}"),
            subsystem: "input",
            devname: alloc::format!("input/{name}"),
            major,
            minor,
            extra: &[],
        });
    }

    // `/dev/uinput`: the interface a program makes an input device of its own
    // through. It hands the created devices' nodes into the same `/dev/input`,
    // and takes the next number after the ones the bus answered with, so a
    // synthetic device is `event2` on a machine with a keyboard and a tablet.
    dev.register_node(
        "uinput",
        crate::drivers::uinput::Uinput::new(input_dir, evdevs.len()),
    );
    crate::uevent::publish(&crate::uevent::Event {
        action: "add",
        devpath: alloc::string::String::from("/devices/virtual/misc/uinput"),
        subsystem: "misc",
        devname: alloc::string::String::from("uinput"),
        major: crate::uinput::MAJOR,
        minor: crate::uinput::MINOR,
        extra: &[],
    });
}

/// Probe the PCI bus for every virtio input device and build a node for each.
/// The pairs are what the devfs builder registers as `/dev/input/event0`,
/// `event1`, ... and what the service task polls.
///
/// The first machine's door into [`register`]. The devices it walks for are the
/// virtio-PCI functions answering [`DEVICE_INPUT`], each of which becomes a
/// [`VirtioInputDev`] over its own `PciTransport` - so the transport stops here
/// and everything above is the contract. A function that does not open as an
/// input device is skipped rather than guessed at, which is why a machine whose
/// bus answered with none gets an empty list and an honestly empty `/dev/input`.
pub fn init() -> Vec<(Arc<EvdevNode>, String)> {
    let devices: Vec<Arc<dyn InputDevice>> = transport_all(DEVICE_INPUT)
        .into_iter()
        .filter_map(|transport| {
            let device = VirtIOInput::<HalImpl, _>::new(transport).ok()?;
            Some(Arc::new(VirtioInputDev::new(device)) as Arc<dyn InputDevice>)
        })
        .collect();
    register(devices)
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
static KEYMAP: Mutex<RefCell<crate::term::keymap::Keymap>> =
    Mutex::new(RefCell::new(crate::term::keymap::Keymap::new()));

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
/// when a text console is live - see [`crate::console::vt_live`] - keyboard
/// events are translated to console bytes and fed to the active virtual console
/// through the same funnel a serial keystroke uses. Routing happens *after* the
/// critical section, so the echo the keystroke provokes can take the pty's
/// parking branch rather than growing a queue unbounded under the lock.
pub fn service() {
    // Whether a keystroke can reach a screen at all: a boot whose console list
    // names the wire alone keeps the console there, and this path is the
    // framebuffer console's keyboard, so there is nothing here to feed - until
    // somebody claims the card, at which point a text console can come back to
    // the screen and `grids_live` makes a text console live again.
    let screen_live = crate::console::vt_live();
    // Whether the session that owns the card still owns the screen. Read once
    // per tick and outside the critical section below, since it asks the
    // console mux, which keeps its own state.
    let session_active = crate::vfs::vcmux::session_active();
    let mut routed = [0u8; 64];
    let mut n = 0usize;
    critical_section::with(|cs| {
        let devices = DEVICES.borrow(cs).borrow();
        let mut keymap = KEYMAP.borrow(cs).borrow_mut();
        for (dev, node) in devices.iter() {
            // A grabbed device belongs to whoever grabbed it. libinput takes
            // the grab the moment it opens an evdev node, and this check is
            // what that means here: the keystrokes stop being *also* console
            // input, so a compositor's typing does not land on the shell
            // sitting behind it. The node still gets every event - that is
            // where the compositor reads them from - *while its session holds
            // the screen*. After a switch the claim is all that is left, and a
            // claim outlived its screen must not swallow the machine's input,
            // so the events go to the console instead; they are dropped for
            // the grabbed node rather than queued, which is what a compositor
            // that comes back to its seat expects anyway - a stream of frames
            // from a session nobody was looking at is not a resume point, it
            // is a backlog of stale motion.
            let grabbed = node.is_grabbed();
            // A claim on the device keeps the keyboard off the console for
            // exactly as long as the claimer's session is on the screen. That
            // conjunction - not the grab alone - is the hand-over: a test or a
            // console program that grabs a device, and a compositor that has
            // released its screen, must not swallow the machine's input.
            let compositor_holds = grabbed && session_active;
            // The node keeps the event unless the claim is stale: while the
            // compositor is on the screen (so it reads it), and when nobody has
            // grabbed at all (so a future evdev reader finds it). A grab
            // outlived by its screen drops the event rather than queueing it,
            // because a backlog of frames from a session nobody was watching is
            // stale motion, not a resume point.
            let delivers = !grabbed || session_active;
            // The console's turn is *not* the complement of the node's: with no
            // grab on the device both happen, which is what a plain GUI console
            // needs - the keystroke reaches the shell *and* sits on the node for
            // anyone reading evdev. It is the complement of the compositor's
            // hold, and only that, that takes the keyboard from the shell.
            //
            // `KDSKBMODE(K_OFF)` then takes the keys away from *this* console's
            // session as well, which is what it is for on Linux - the mode
            // belongs to the virtual console's keyboard, not to the machine's.
            // The serial line is the machine's, and it is unaffected: a
            // compositor must be able to silence the console it is drawing on
            // without silencing the only wire a person can still type into.
            let keymap_on =
                crate::vfs::vcmux::kb_mode(crate::term::vc::active()) != crate::term::vc::K_OFF;
            // Whether the console on the screen has been put in graphics mode by
            // the session driving it - which is the compositor's own word, via
            // `KDSETMODE`, that the screen is its to draw and the text shell
            // underneath is not to be typed at. This - not the grab, and not the
            // master/console binding, which `delivers` above still uses to decide
            // whether a grabbed node is fed or dropped - is what takes the
            // keyboard off the shell: while the active console is graphics, a
            // keystroke belongs to the compositor and only to it, whether or not
            // libinput has taken `EVIOCGRAB` on the node yet. A `--mode mirror`
            // text console is `KD_TEXT`, so this is false there and the shell keeps
            // its keyboard exactly as the plain-console case requires.
            let graphics = crate::vfs::vcmux::kd_mode(crate::term::vc::active())
                == crate::term::vc::KD_GRAPHICS;
            let console_wants = screen_live && !compositor_holds && !graphics && keymap_on;
            // Whether the mux just saw a `^A` and is waiting for the byte that
            // completes it. Read once a tick, outside the per-event loop, since
            // the mux consumes the latch asynchronously and the whole escape
            // sequence normally arrives within this one batch.
            let mux_wants = crate::vfs::vcmux::mux_wants();
            while let Some(event) = dev.pop_pending_event() {
                // The node is fed first and independently of the console, so a
                // mouse (`EV_REL`) event is never lost to the console-routing
                // check below - only keyboard events are ever routed to a shell.
                if delivers {
                    node.push(&event);
                }
                if event.event_type != EV_KEY {
                    continue;
                }
                let Some((bytes, cnt)) = keymap.event(event.code, event.value as i32) else {
                    continue;
                };
                // What the keyboard owes the console mux. `console_wants` is the
                // shell's ordinary turn - the console not held by a compositor,
                // in text mode, keyboard not off. On top of it the operator's
                // escape must always reach the mux on a screen boot, even while a
                // compositor owns the keyboard: `^A` itself, and the byte right
                // after it once the mux has latched the `^A` (`mux_wants`) - which
                // is how `^A <digit>` completes. An escape hatch that only works
                // when the compositor agrees is no hatch, and a machine wedged on
                // the compositor has no compositor left to let you out; the serial
                // line already reaches the mux unconditionally, and this is the
                // keyboard's half of that door. Every other byte honours
                // `console_wants`, so a compositor's typing stays off the shell.
                if !console_wants && !screen_live {
                    continue;
                }
                let escape = screen_live && (mux_wants || bytes[..cnt].contains(&0x01));
                if !(console_wants || escape) {
                    continue;
                }
                for &b in &bytes[..cnt] {
                    if !console_wants && b != 0x01 && !mux_wants {
                        continue;
                    }
                    if n < routed.len() {
                        routed[n] = b;
                        n += 1;
                    }
                }
            }
        }
    });
    for &b in &routed[..n] {
        crate::vfs::vcmux::route_input(b);
    }
}
