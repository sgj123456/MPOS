//! `/dev/uinput`: input devices a userspace program makes for itself.
//!
//! One open file description builds one device. `UI_SET_*BIT` declares what it
//! can report, `UI_DEV_SETUP` names it, `UI_DEV_CREATE` makes it appear as
//! `/dev/input/eventN` with the ordinary evdev face every reader already
//! knows, and every whole `struct input_event` written to the descriptor is what
//! that device reports. `UI_DEV_DESTROY` - or closing the descriptor, or the
//! process exiting - takes the device away again, node and all. That is Linux's
//! contract, and the reason the descriptor *is* the device: a `dup` is one
//! device, a second `open` is another.
//!
//! It exists here because the conformance suite has no hands. A rule about
//! *whose* event a record is could not be checked on the machine at all until a
//! test could put one there itself, and this is the interface Linux gives for
//! exactly that. The ABI's arithmetic - which request means which bit, what a
//! write has to be shaped like - is in `crate::uinput`, ungated so a host build
//! can test it; what is here is the device: the node, the effects of the
//! requests, and the devfs side.
//!
//! What is *not* here, recorded rather than left to be discovered: `UI_ABS_SETUP`
//! (so a synthetic device answers `EVIOCGABS` `EINVAL`), force feedback, and the
//! `/sys/class/input/eventN` tree for a device made at runtime - `sysfs` builds
//! its tree once at boot, so a uinput device gets a `/dev/input` node and an
//! `add` uevent but no class directory. `udev`/`libinput` therefore will not
//! classify it; a reader that opens the node directly - which is what the
//! conformance suite is - sees a device like any other.

use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::cell::RefCell;

use critical_section::Mutex;

use crate::drivers::virtio::input::{EvdevCaps, EvdevNode, INPUT_MAJOR, input_minor};
use crate::evdev::RECORD;
use crate::uinput::{self, BitSet, Device};
use crate::vfs::devfs::DevDirNode;
use crate::vfs::{Inode, Metadata, NodeKind, VfsError, VfsResult};

/// The device one open file description owns.
struct Client {
    /// The description, as `File::ofd` names it - the same key a `dup` shares
    /// and a second `open` does not.
    ofd: u64,
    /// The device it is building, or has built.
    device: Device,
    /// The node `UI_DEV_CREATE` made, for as long as it exists.
    node: Option<Arc<EvdevNode>>,
    /// The name in `/dev/input` that node lives under.
    name: Option<String>,
}

/// The state one `/dev/uinput` node holds: every description using it, and the
/// next free place in the input layer.
struct State {
    clients: Vec<Client>,
    /// The index the next created device takes: `event{index}`, minor 64+index.
    /// It starts one past the devices the boot's bus answered with, and never
    /// goes backwards - a destroyed device's number is not handed out again
    /// while a name for it may still be remembered anywhere.
    next_index: usize,
}

impl State {
    /// The device `ofd` owns, made empty if this is its first use.
    fn client(&mut self, ofd: u64) -> &mut Client {
        if let Some(at) = self.clients.iter().position(|c| c.ofd == ofd) {
            return &mut self.clients[at];
        }
        self.clients.push(Client {
            ofd,
            device: Device::default(),
            node: None,
            name: None,
        });
        self.clients.last_mut().expect("just pushed")
    }
}

/// `/dev/uinput`, as an inode.
pub struct Uinput {
    /// Where a created device's node has to appear: the same `/dev/input` the
    /// bus's own devices are in, because that is where an evdev reader looks.
    input_dir: Arc<DevDirNode>,
    state: Mutex<RefCell<State>>,
    /// The node's mode and owner, kept so `chmod`/`chown` stick (see
    /// `crate::vfs::devfs::NodePerms`).
    perms: crate::vfs::devfs::NodePerms,
}

impl Uinput {
    /// Build the device the boot registers as `/dev/uinput`. `first_index` is
    /// where the next input device goes: one past the devices the bus answered
    /// with, so a synthetic device takes the number after the machine's own.
    pub(crate) fn new(input_dir: Arc<DevDirNode>, first_index: usize) -> Arc<Uinput> {
        Arc::new(Uinput {
            input_dir,
            perms: crate::vfs::devfs::NodePerms::new(0o666, 0, 0),
            state: Mutex::new(RefCell::new(State {
                clients: Vec::new(),
                next_index: first_index,
            })),
        })
    }

    /// `UI_SET_*BIT`: declare one bit on the device `ofd` is building. `false`
    /// is the `EINVAL` Linux answers for a bit the request cannot carry.
    pub fn set_bit(&self, ofd: u64, set: BitSet, code: usize) -> bool {
        critical_section::with(|cs| {
            self.state
                .borrow(cs)
                .borrow_mut()
                .client(ofd)
                .device
                .set_bit(set, code)
        })
    }

    /// `UI_DEV_SETUP`: the name and identity the device will report.
    pub fn setup(&self, ofd: u64, name: String, id: [u16; 4]) {
        critical_section::with(|cs| {
            let mut state = self.state.borrow(cs).borrow_mut();
            let device = &mut state.client(ofd).device;
            device.name = name;
            device.id = id;
        });
    }

    /// `UI_DEV_CREATE`: the declaration becomes a device, at the next free
    /// `/dev/input/eventN`.
    ///
    /// `false` is a device with no name yet, which is refused for the same
    /// reason Linux refuses it: the name is what the device reports events *as*,
    /// and a nameless one is one no reader can tell from any other. Asking
    /// twice is not an error and not a second device - it is the same claim.
    pub fn create(&self, ofd: u64) -> bool {
        let made = critical_section::with(|cs| {
            let mut state = self.state.borrow(cs).borrow_mut();
            if state
                .clients
                .iter()
                .any(|c| c.ofd == ofd && c.node.is_some())
            {
                return Ok(None);
            }
            if state.client(ofd).device.name.is_empty() {
                return Err(());
            }
            let index = state.next_index;
            state.next_index += 1;
            let client = state.client(ofd);
            let name = alloc::format!("event{index}");
            let caps = EvdevCaps::synthetic(
                client.device.type_bits(),
                core::array::from_fn(|evtype| client.device.code_bits(evtype as u8).to_vec()),
                client.device.prop_bits().to_vec(),
                client.device.id,
            );
            let node = EvdevNode::new(client.device.name.clone(), index, caps);
            client.node = Some(node.clone());
            client.name = Some(name.clone());
            Ok(Some((node, name, index)))
        });
        let Ok(made) = made else {
            return false;
        };
        if let Some((node, name, index)) = made {
            self.input_dir.insert(&name, node);
            // Announced the way the boot's own input devices are, so a listener
            // - `udev`, or `udevadm monitor` while somebody debugges this - is
            // told a device exists rather than having to poll /dev for one.
            crate::uevent::publish(&crate::uevent::Event {
                action: "add",
                devpath: alloc::format!("/devices/virtual/input/input{index}/{name}"),
                subsystem: "input",
                devname: alloc::format!("input/{name}"),
                major: INPUT_MAJOR,
                minor: input_minor(index),
                extra: &[],
            });
        }
        true
    }

    /// `UI_DEV_DESTROY`, and what closing the description does: the device goes
    /// away - out of `/dev/input`, with its readers told that the stream has
    /// ended. The description itself survives, which is Linux's contract: the
    /// descriptor outlives the device it made, and can make another.
    pub fn destroy(&self, ofd: u64) {
        let gone = critical_section::with(|cs| {
            let mut state = self.state.borrow(cs).borrow_mut();
            let client = state.clients.iter_mut().find(|c| c.ofd == ofd)?;
            Some((client.node.take()?, client.name.take()?))
        });
        let Some((node, name)) = gone else {
            return;
        };
        self.input_dir.remove(&name);
        node.close();
    }

    /// The description is gone: its device goes with it, and its place in the
    /// table too.
    ///
    /// Without this a program that builds a device and forgets to destroy it
    /// would leave a `/dev/input/eventN` behind for the life of the machine -
    /// and an even more permanent problem than the name: the entry is keyed by
    /// an address, and the next description to land on that address would
    /// inherit a device it never made.
    pub fn forget(&self, ofd: u64) {
        self.destroy(ofd);
        critical_section::with(|cs| {
            self.state
                .borrow(cs)
                .borrow_mut()
                .clients
                .retain(|c| c.ofd != ofd);
        });
    }

    /// The `'U'`-typed requests: the whole of `/dev/uinput`'s contract.
    ///
    /// The *vocabulary* - the request numbers, which bit-setter names which
    /// type, `struct uinput_setup`'s layout and its parser - is the `mpos-uinput`
    /// crate's and stays there, host-tested. What lives here is the dispatch
    /// over this device's own state and the errno each request owes, which is
    /// the same split the `'E'` family took into `EvdevNode::ioctl` and the
    /// `'K'` family into `Tty::kd_ioctl` (`NOTES.md` §11.2 Step B2). The syscall
    /// layer still resolves the descriptor (`uinput_ioctl` in
    /// `syscall/linux/fs.rs`): the bad-fd `EBADF` and the not-a-uinput-node
    /// `ENOTTY` are the user boundary's to give.
    pub fn ioctl(&self, ofd: u64, request: u32, buf: usize, from_user: bool) -> isize {
        use core::mem::size_of;
        let size = ((request >> 16) & 0x3fff) as usize;
        let dir = (request >> 30) & 0x3;
        let Some(nr) = crate::uinput::nr_of(request) else {
            return crate::err::ENOTTY;
        };

        // Every bit-setter carries one `int`: the bit's number.
        if let Some(set) = crate::uinput::bit_request(nr) {
            if size < size_of::<i32>()
                || !crate::uaccess::buffer_ok(buf, size_of::<i32>(), false, from_user)
            {
                return crate::err::EINVAL;
            }
            let code = unsafe { (buf as *const i32).read_unaligned() } as usize;
            return if self.set_bit(ofd, set, code) {
                0
            } else {
                crate::err::EINVAL
            };
        }
        match (dir, nr) {
            (0, crate::uinput::DEV_CREATE) => {
                if self.create(ofd) {
                    0
                } else {
                    crate::err::EINVAL
                }
            }
            (0, crate::uinput::DEV_DESTROY) => {
                self.destroy(ofd);
                0
            }
            (1, crate::uinput::DEV_SETUP) => {
                if size < crate::uinput::SETUP_SIZE
                    || !crate::uaccess::buffer_ok(buf, crate::uinput::SETUP_SIZE, false, from_user)
                {
                    return crate::err::EINVAL;
                }
                // Read the caller's structure in one go: it is fixed-size and
                // plain, and this is the only place its layout is depended on.
                let bytes = unsafe {
                    core::slice::from_raw_parts(buf as *const u8, crate::uinput::SETUP_SIZE)
                };
                match crate::uinput::parse_setup(bytes) {
                    Some((name, id, _effects)) => {
                        self.setup(ofd, name, id);
                        0
                    }
                    None => crate::err::EINVAL,
                }
            }
            (2, crate::uinput::GET_VERSION) => {
                let version = crate::uinput::VERSION;
                if size < size_of::<u32>()
                    || !crate::uaccess::buffer_ok(buf, size_of::<u32>(), true, from_user)
                {
                    return crate::err::EINVAL;
                }
                unsafe { (buf as *mut u32).write_unaligned(version) };
                0
            }
            _ => crate::err::ENOTTY,
        }
    }

    /// Report the records a write carried, answering how many bytes landed.
    /// `None` is a description with no device behind it yet - a write before
    /// `UI_DEV_CREATE`, which Linux also refuses.
    fn inject(&self, ofd: u64, bytes: &[u8]) -> Option<usize> {
        let node = critical_section::with(|cs| {
            self.state
                .borrow(cs)
                .borrow()
                .clients
                .iter()
                .find(|c| c.ofd == ofd)
                .and_then(|c| c.node.clone())
        })?;
        for record in bytes.chunks(RECORD) {
            // Only the three fields the device reports are read. The timestamp
            // the caller wrote is not theirs to declare, which is Linux's rule
            // as well: the time an event happened is the kernel's to know.
            let event_type = u16::from_ne_bytes([record[16], record[17]]);
            let code = u16::from_ne_bytes([record[18], record[19]]);
            let value = i32::from_ne_bytes([record[20], record[21], record[22], record[23]]);
            node.inject(event_type, code, value);
        }
        Some(bytes.len())
    }
}

/// Forget the devices the descriptions a process is taking with it had made.
///
/// The batch counterpart of [`Uinput::forget`], for the exit path, where the
/// descriptors are handed over together rather than closed one at a time - see
/// [`crate::vfs::file::for_each_last`], which is the rule for which of them is
/// really gone.
#[cfg(target_os = "none")]
pub fn close_at_exit(files: &[Arc<crate::vfs::file::File>], _owner: u32) {
    crate::vfs::file::for_each_last(files, |file, ofd| {
        if let Some(dev) = file.inode().as_any().downcast_ref::<Uinput>() {
            dev.forget(ofd);
        }
    });
}

impl Inode for Uinput {
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

    fn metadata(&self) -> Metadata {
        Metadata {
            ino: 0x9f00,
            kind: NodeKind::CharDev,
            size: 0,
        }
    }

    /// The misc device number the node reports - `10:223`, which is the pairing
    /// udev's rule for naming `/dev/uinput` is written against.
    fn rdev(&self) -> Option<(u32, u32)> {
        Some((uinput::MAJOR, uinput::MINOR))
    }

    /// Reading the descriptor itself is not a thing: the events belong to the
    /// device it made, and Linux refuses this with `EINVAL` too.
    fn read_at(&self, _offset: u64, _buf: &mut [u8]) -> VfsResult<usize> {
        Err(VfsError::Invalid)
    }

    /// A write of whole `struct input_event` records, which the device behind
    /// this description then reports. A length that is not a whole number of
    /// them is `EINVAL`: a partial record is the caller's own buffering bug, and
    /// a byte count that does not describe what was reported is a way to never
    /// find out.
    fn write_at_client(&self, _offset: u64, buf: &[u8], client: u64) -> VfsResult<usize> {
        if uinput::record_count(buf.len()).is_none() {
            return Err(VfsError::Invalid);
        }
        self.inject(client, buf).ok_or(VfsError::Invalid)
    }

    /// There is no position in a device's event stream to seek to.
    fn seekable(&self) -> bool {
        false
    }
}
