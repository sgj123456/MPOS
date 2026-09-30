//! `/sys`: the sysfs tree.
//!
//! On Linux this is where drivers publish themselves, and it is a large and
//! growing surface. What this kernel must publish is smaller, and it is not
//! optional: userland that has to *find* a device reads `/sys` to learn which
//! devices exist, and - for the ones that appeared before it started looking -
//! writes to a device's `uevent` attribute to have the kernel announce it
//! again. mkinitfs's `nlplug-findfs` is exactly that program, and an initramfs
//! with no `/sys` to walk cannot find its root.
//!
//! So the tree is the block devices, spelled the way Linux spells them:
//!
//!   /class/block/vda3/dev      "254:3" - the numbers `/dev` nodes are made of
//!   /class/block/vda3/uevent   the keys a device event carries, and writable:
//!                              writing `add` re-announces the device
//!   /kernel/ostype             what this kernel is
//!
//! The numbering comes from [`crate::uevent::Numbering`], the same function
//! the events and the `/dev` nodes use, so the three cannot disagree about
//! what `vda3` is.

use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::cell::RefCell;

use critical_section::Mutex;

use super::{FileSystem, Ino, Inode, Metadata, NodeKind, VfsError, VfsResult};
use crate::pci::Device;
use crate::uevent::{Event, Numbering};

/// A sysfs directory. Its entries are fixed at mount time: what the kernel
/// has is what the kernel has, and a directory that grew one on demand would
/// be a second device registry.
struct SysDir {
    ino: Ino,
    entries: Mutex<RefCell<BTreeMap<String, Arc<dyn Inode>>>>,
}

impl SysDir {
    fn new(ino: Ino) -> Arc<SysDir> {
        Arc::new(SysDir {
            ino,
            entries: Mutex::new(RefCell::new(BTreeMap::new())),
        })
    }

    fn add(&self, name: &str, node: Arc<dyn Inode>) -> Arc<dyn Inode> {
        critical_section::with(|cs| {
            let mut entries = self.entries.borrow(cs).borrow_mut();
            // A name is taken or it is free. Inserting over one would drop
            // whatever was there - silently, and only for the reader who went
            // looking for it - so the second add of a name is a bug to fix,
            // not a directory to replace.
            assert!(
                !entries.contains_key(name),
                "sysfs: {name} is already in this directory"
            );
            entries.insert(String::from(name), node.clone());
        });
        node
    }

    fn add_dir(&self, name: &str, ino: Ino) -> Arc<SysDir> {
        let dir = SysDir::new(ino);
        self.add(name, dir.clone());
        dir
    }
}

impl Inode for SysDir {
    fn metadata(&self) -> Metadata {
        Metadata {
            ino: self.ino,
            kind: NodeKind::Dir,
            size: 0,
        }
    }

    fn lookup(&self, name: &str) -> VfsResult<Arc<dyn Inode>> {
        critical_section::with(|cs| {
            self.entries
                .borrow(cs)
                .borrow()
                .get(name)
                .cloned()
                .ok_or(VfsError::NoEntry)
        })
    }

    fn read_dir(&self) -> VfsResult<Vec<(String, NodeKind)>> {
        critical_section::with(|cs| {
            let entries = self.entries.borrow(cs).borrow();
            Ok(entries
                .iter()
                .map(|(name, node)| (name.clone(), node.metadata().kind))
                .collect())
        })
    }
}

/// A read-only attribute with fixed contents.
struct SysFile {
    ino: Ino,
    content: &'static str,
}

impl Inode for SysFile {
    fn metadata(&self) -> Metadata {
        Metadata {
            ino: self.ino,
            kind: NodeKind::File,
            size: self.content.len() as u64,
        }
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> VfsResult<usize> {
        let bytes = self.content.as_bytes();
        let start = offset.min(bytes.len() as u64) as usize;
        let end = (start + buf.len()).min(bytes.len());
        buf[..end - start].copy_from_slice(&bytes[start..end]);
        Ok(end - start)
    }
}

/// A symbolic link in the device tree.
///
/// `/sys/bus/pci/devices/<bdf>` is not a directory: it is a link to the
/// device's real home under `/sys/devices/...`. The indirection is what
/// `lspci -t` reads the topology out of - it resolves `<bdf>/..`, parses the
/// name it lands on as a device address, and draws an edge when it parses -
/// so a tree of plain directories has no edges in it to draw.
struct SysSymlink {
    ino: Ino,
    target: String,
}

impl Inode for SysSymlink {
    fn metadata(&self) -> Metadata {
        Metadata {
            ino: self.ino,
            kind: NodeKind::Symlink,
            size: self.target.len() as u64,
        }
    }

    fn readlink(&self) -> VfsResult<String> {
        Ok(self.target.clone())
    }
}

/// A device's `config` attribute: the configuration space itself.
///
/// Not a generated string but a *window*. Every read and write goes back to
/// the device through the same mechanism the kernel found it with, so a
/// program sees the live registers rather than a snapshot taken at boot -
/// which is the difference between `lspci -vv` describing the machine as it
/// is and describing it as it was.
///
/// It is 256 bytes because that is `cfg_size` here: the address register of
/// mechanism 1 has eight bits of offset, and the 4096-byte extended space
/// PCIe added is not reachable through it at all.
struct SysConfig {
    ino: Ino,
    bus: u8,
    dev: u8,
    func: u8,
}

impl Inode for SysConfig {
    fn metadata(&self) -> Metadata {
        Metadata {
            ino: self.ino,
            kind: NodeKind::File,
            size: crate::pci::CFG_SIZE,
        }
    }

    #[cfg(target_os = "none")]
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> VfsResult<usize> {
        Ok(crate::drivers::pci::config_read(
            self.bus, self.dev, self.func, offset, buf,
        ))
    }

    #[cfg(target_os = "none")]
    fn write_at(&self, offset: u64, buf: &[u8]) -> VfsResult<usize> {
        Ok(crate::drivers::pci::config_write(
            self.bus, self.dev, self.func, offset, buf,
        ))
    }
}

/// A PCI device's `uevent` attribute: Linux's `pci_uevent` keys, and
/// writable.
///
/// The keys are what a device's identity is read out of without opening its
/// configuration space, and for one reader they are the *only* source:
/// busybox's `lspci` parses this file and nothing else, so a tree without it
/// lists as empty.
///
/// Writing an action re-announces the device, the same way the block devices'
/// attribute does - a listener that started after the device appeared has no
/// other way to hear about it. The event carries no device node, because a
/// PCI function has none; Linux's own event for one is the same, plus the
/// PCI_* keys this kernel's event shape has no room for.
struct SysPciEvent {
    ino: Ino,
    keys: String,
    event: Event,
}

impl Inode for SysPciEvent {
    fn metadata(&self) -> Metadata {
        Metadata {
            ino: self.ino,
            kind: NodeKind::File,
            size: self.keys.len() as u64,
        }
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> VfsResult<usize> {
        let bytes = self.keys.as_bytes();
        let start = offset.min(bytes.len() as u64) as usize;
        let end = (start + buf.len()).min(bytes.len());
        buf[..end - start].copy_from_slice(&bytes[start..end]);
        Ok(end - start)
    }

    fn write_at(&self, _offset: u64, buf: &[u8]) -> VfsResult<usize> {
        // The action, NUL or newline terminated: one of the three the kernel
        // speaks. Anything else is refused, as Linux refuses an action no
        // device has.
        let text: Vec<u8> = buf
            .iter()
            .take_while(|byte| **byte != 0 && **byte != b'\n')
            .copied()
            .collect();
        let action = match text.as_slice() {
            b"add" => "add",
            b"remove" => "remove",
            b"change" => "change",
            _ => return Err(VfsError::Invalid),
        };
        crate::uevent::publish(&Event {
            action,
            devpath: self.event.devpath.clone(),
            subsystem: self.event.subsystem,
            devname: self.event.devname.clone(),
            major: self.event.major,
            minor: self.event.minor,
            extra: self.event.extra,
        });
        Ok(buf.len())
    }
}

/// A device's `uevent` attribute: readable, and *writable*.
///
/// The write is the whole reason this file exists. Userland that started
/// after the device appeared - an initramfs, always - has no way to hear the
/// event that announced it; writing the action here makes the kernel say it
/// again, and the listener's netlink socket is the audience.
struct SysEventAttr {
    ino: Ino,
    event: Event,
}

impl Inode for SysEventAttr {
    fn metadata(&self) -> Metadata {
        Metadata {
            ino: self.ino,
            kind: NodeKind::File,
            // The length is not known before the contents are, and asking is
            // the work: report zero, as Linux does for generated attributes.
            size: 0,
        }
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> VfsResult<usize> {
        // The keys a listener reads, in the spelling Linux prints them.
        let content = alloc::format!(
            "ACTION={}\nDEVPATH={}\nSUBSYSTEM={}\nDEVNAME={}\nMAJOR={}\nMINOR={}\n",
            self.event.action,
            self.event.devpath,
            self.event.subsystem,
            self.event.devname,
            self.event.major,
            self.event.minor,
        );
        let bytes = content.as_bytes();
        let start = (offset as usize).min(bytes.len());
        let n = buf.len().min(bytes.len() - start);
        buf[..n].copy_from_slice(&bytes[start..start + n]);
        Ok(n)
    }

    fn write_at(&self, _offset: u64, buf: &[u8]) -> VfsResult<usize> {
        // What was written is the action, NUL or newline terminated: one of
        // the three the kernel speaks. Anything else is refused, as Linux
        // refuses an action no device has.
        let text: Vec<u8> = buf
            .iter()
            .take_while(|byte| **byte != 0 && **byte != b'\n')
            .copied()
            .collect();
        let action = match text.as_slice() {
            b"add" => "add",
            b"remove" => "remove",
            b"change" => "change",
            _ => return Err(VfsError::Invalid),
        };
        crate::uevent::publish(&Event {
            action,
            devpath: self.event.devpath.clone(),
            subsystem: self.event.subsystem,
            devname: self.event.devname.clone(),
            major: self.event.major,
            minor: self.event.minor,
            extra: self.event.extra,
        });
        Ok(buf.len())
    }
}

/// The filesystem itself: the device tree, built once.
pub struct SysFs {
    root: Arc<SysDir>,
}

impl SysFs {
    /// The tree for the machine as it is: the boot disk, its partitions, and
    /// every PCI function the boot found.
    pub fn new() -> Arc<SysFs> {
        SysFs::with(Numbering::of_boot_disk(), crate::pci::devices())
    }

    /// The tree for a given set of block devices, and no PCI bus. The host has
    /// no disk and no bus, and the walk a listener does is worth testing
    /// without either.
    pub fn with_devices(numbering: Numbering) -> Arc<SysFs> {
        SysFs::with(numbering, Vec::new())
    }

    /// The tree for a given set of devices: the same walk the kernel's own
    /// tree is built by, with the devices handed in rather than looked up, so
    /// a host test can put a machine of its own choosing behind it.
    pub fn with(numbering: Numbering, pci_devices: Vec<Device>) -> Arc<SysFs> {
        let fs = Arc::new(SysFs {
            root: SysDir::new(0),
        });

        // `/bus` and `/class` are the two places a listener walks to find the
        // devices that are already there: `/class` names them by what they
        // are, `/bus` by what they are attached to.
        let bus = fs.root.add_dir("bus", 1);
        let class = fs.root.add_dir("class", 2);
        let block = class.add_dir("block", 3);

        let mut next = 4;
        // `/sys/devices`: the tree every device really lives in - the input
        // devices under `virtual/`, the PCI functions under `pci0000:00/`.
        // Built *once*: `add_dir` names a directory in its parent, so a second
        // call with the same name is the first directory being replaced, and
        // everything it held dropped without a word.
        let devices = fs.root.add_dir("devices", next);
        next += 1;
        // `/sys/devices/virtual`: the home of every device in this tree that no
        // bus discovered. Created once, here, for exactly the reason the line
        // above gives - the input devices and the DRM nodes both hang under it,
        // and a second `add_dir("virtual")` would replace the first directory
        // and drop everything inside it without a word.
        let virtual_root = devices.add_dir("virtual", next);
        next += 1;
        // Every device number in this tree, with the `/sys`-relative path of
        // the device that carries it and which of `/sys/dev`'s two directories
        // it belongs in. Collected as the classes are built because `/sys/dev`
        // below is the one place they are needed together, and a second walk of
        // the tree to find them would be a second definition of the same fact.
        //
        // The path is the *device tree's*, not the class entry's, wherever the
        // device has one - Linux's driver core points `/sys/dev/char/M:m` at
        // the device itself (`../../devices/...`), and a reader that resolves
        // the link is expected to land on the device, not on another link to
        // it. libudev resolves exactly one hop, and libinput leans on the
        // answer: `evdev_device_have_same_syspath` re-derives the device from
        // its own `fstat` devnum and refuses the device unless the two
        // *strings* are equal. With a class-shaped link they are
        // `/devices/virtual/input/input0/event0` and `/class/input/event0`,
        // the check fails silently, and weston reports "failed to create input
        // device" for a keyboard that is otherwise perfectly well tagged.
        //
        // Block devices are the exception, and honestly so: they have no
        // directory under `/sys/devices` in this tree, so their class entry is
        // where they really live.
        let mut devnodes: Vec<(&'static str, String, u32, u32)> = Vec::new();
        for (name, major, minor, _whole) in numbering.devices.iter() {
            let dir = block.add_dir(name, next);
            next += 1;
            devnodes.push((
                "block",
                alloc::format!("class/block/{}", name),
                *major,
                *minor,
            ));
            // The numbers, as `/dev` nodes are made from them.
            let numbers = alloc::format!("{}:{}\n", major, minor);
            let numbers: &'static str = Box::leak(numbers.into_boxed_str());
            dir.add(
                "dev",
                Arc::new(SysFile {
                    ino: next,
                    content: numbers,
                }),
            );
            next += 1;
            dir.add(
                "uevent",
                Arc::new(SysEventAttr {
                    ino: next,
                    event: Event {
                        action: "add",
                        devpath: alloc::format!("/devices/virtio/block/{}", name),
                        subsystem: "block",
                        devname: name.clone(),
                        major: *major,
                        minor: *minor,
                        extra: &[],
                    },
                }),
            );
            next += 1;
        }

        // `/class/input`: the evdev nodes, and - under `/sys/devices` - the
        // input devices they belong to. This is how udev and `libinput`
        // enumerate the keyboard and the pointer without scanning `/dev` by
        // hand, and the shape is Linux's rather than a convenient one:
        //
        //   /sys/devices/virtual/input/inputN/           the input device
        //       capabilities/ev, capabilities/key, ...   what it can report
        //       eventN/                                  the node it reports through
        //   /sys/class/input/inputN -> that directory
        //   /sys/class/input/eventN -> .../inputN/eventN
        //
        // The indirection is load-bearing. udev's `input_id` builtin decides a
        // device *is* an input device by walking up from the node to the first
        // parent carrying a `capabilities/ev` attribute, and libudev learns a
        // device's subsystem from the `subsystem` link beside it - so a flat
        // `/sys/class/input/eventN` directory is a node with no parent, no
        // subsystem and no capability: a keyboard that nothing will ever
        // classify as one.
        #[cfg(target_os = "none")]
        {
            use crate::drivers::virtio::input::{CAPABILITY_BYTES, CAPABILITY_TYPES};

            let input = class.add_dir("input", next);
            next += 1;
            let input_root = virtual_root.add_dir("input", next);
            next += 1;

            // The subsystem links point back at the class directory they hang
            // under, which is what makes libudev call these devices "input".
            let to_class = |depth: usize| alloc::format!("{}class/input", "../".repeat(depth));
            let text = |s: String| -> &'static str { Box::leak(s.into_boxed_str()) };

            for (index, node) in crate::drivers::virtio::input::nodes().iter().enumerate() {
                let name = alloc::format!("event{index}");
                let device_name = alloc::format!("input{index}");
                let major = crate::drivers::virtio::input::INPUT_MAJOR;
                let minor = crate::drivers::virtio::input::input_minor(index);

                // The input device: everything that describes *what* it is.
                let device = input_root.add_dir(&device_name, next);
                next += 1;
                device.add(
                    "subsystem",
                    Arc::new(SysSymlink {
                        ino: next,
                        target: to_class(4),
                    }),
                );
                next += 1;
                device.add(
                    "name",
                    Arc::new(SysFile {
                        ino: next,
                        content: text(alloc::format!("{}\n", node.name())),
                    }),
                );
                next += 1;
                // Where the device sits. The rule that groups a device with
                // its siblings matches on this being present at all.
                device.add(
                    "phys",
                    Arc::new(SysFile {
                        ino: next,
                        content: text(alloc::format!("virtio{index}/input{index}\n")),
                    }),
                );
                next += 1;
                device.add(
                    "properties",
                    Arc::new(SysFile {
                        ino: next,
                        content: text(alloc::format!("{}\n", bitmap_attr(node.prop_bits(), 32))),
                    }),
                );
                next += 1;

                // Every path under `/sys/devices` needs a `uevent` file, and
                // this one is not decoration either: `udev_device_new_from_syspath`
                // stats `<path>/uevent` for anything under `/devices/` and gives
                // up when it is not there. Without it the input device is not a
                // device at all to udev - so the walk from an `eventN` node up
                // to the thing that reports through it finds nothing, and the
                // node is never tagged as input.
                device.add(
                    "uevent",
                    Arc::new(SysEventAttr {
                        ino: next,
                        event: Event {
                            action: "add",
                            devpath: alloc::format!("/devices/virtual/input/{}", device_name),
                            subsystem: "input",
                            // No node of its own: the events are what the
                            // device reports through, and they are the nodes.
                            devname: String::new(),
                            major: 0,
                            minor: 0,
                            extra: &[],
                        },
                    }),
                );
                next += 1;

                let id = node.id();
                let ids = device.add_dir("id", next);
                next += 1;
                for (attr, value) in [
                    ("bustype", id[0]),
                    ("vendor", id[1]),
                    ("product", id[2]),
                    ("version", id[3]),
                ] {
                    ids.add(
                        attr,
                        Arc::new(SysFile {
                            ino: next,
                            content: text(alloc::format!("{:04x}\n", value)),
                        }),
                    );
                    next += 1;
                }

                let capabilities = device.add_dir("capabilities", next);
                next += 1;
                let mut scratch = [0u8; CAPABILITY_BYTES];
                for (attr, evtype, declared) in CAPABILITY_TYPES {
                    let len = node.eviocgbit(*evtype, &mut scratch);
                    capabilities.add(
                        attr,
                        Arc::new(SysFile {
                            ino: next,
                            content: text(alloc::format!(
                                "{}\n",
                                bitmap_attr(&scratch[..len], *declared)
                            )),
                        }),
                    );
                    next += 1;
                }

                // The node itself, a child of the device it reports for.
                let event = device.add_dir(&name, next);
                next += 1;
                // `device`: the link to the parent that every class device's
                // directory carries, and the one `udev`'s `input_id` builtin
                // follows to reach the `capabilities/*` that say *what* this
                // node is. Without it the node classifies as nothing, so no
                // `ID_INPUT_KEYBOARD` tag is set, `libinput` walks past every
                // keyboard on the machine, and weston comes up saying "no input
                // devices found" - a compositor with a screen and no keys.
                event.add(
                    "device",
                    Arc::new(SysSymlink {
                        ino: next,
                        target: alloc::string::String::from(".."),
                    }),
                );
                next += 1;
                event.add(
                    "subsystem",
                    Arc::new(SysSymlink {
                        ino: next,
                        target: to_class(5),
                    }),
                );
                next += 1;
                event.add(
                    "name",
                    Arc::new(SysFile {
                        ino: next,
                        content: text(alloc::format!("{}\n", node.name())),
                    }),
                );
                next += 1;
                event.add(
                    "dev",
                    Arc::new(SysFile {
                        ino: next,
                        content: text(alloc::format!("{}:{}\n", major, minor)),
                    }),
                );
                next += 1;
                event.add(
                    "uevent",
                    Arc::new(SysEventAttr {
                        ino: next,
                        event: Event {
                            action: "add",
                            // The node's own path, as Linux spells it: the
                            // event is a child of the device, so a listener
                            // that walks up from it finds the device.
                            devpath: alloc::format!(
                                "/devices/virtual/input/{}/{}",
                                device_name,
                                name
                            ),
                            subsystem: "input",
                            devname: alloc::format!("input/{}", name),
                            major,
                            minor,
                            extra: &[],
                        },
                    }),
                );
                next += 1;

                // The class entries, as links into the device tree above.
                input.add(
                    &device_name,
                    Arc::new(SysSymlink {
                        ino: next,
                        target: alloc::format!("../../devices/virtual/input/{}", device_name),
                    }),
                );
                next += 1;
                input.add(
                    &name,
                    Arc::new(SysSymlink {
                        ino: next,
                        target: alloc::format!(
                            "../../devices/virtual/input/{}/{}",
                            device_name,
                            name
                        ),
                    }),
                );
                next += 1;

                devnodes.push((
                    "input",
                    alloc::format!("devices/virtual/input/{}/{}", device_name, name),
                    major,
                    minor,
                ));
            }
        }

        // `/class/drm`: the KMS card and its render node, announced the way
        // the DRM class spells them - `card0` with the `dev` number
        // 226:0 and a `DEVNAME` under `dri/`. This is how weston's DRM
        // backend finds a device at all: it enumerates the `drm` subsystem
        // through libudev, matching the sysname against `card[0-9]*`, and
        // opens what answers as its primary GPU. A tree without this class
        // is a machine with no display, however good /dev/dri is; eudev
        // reads exactly the two attributes below.
        //
        // The GPU the two nodes belong to is built beside them, because in
        // Linux a DRM node is a child of the device it drives rather than a
        // device of its own. A reader holding nothing but an open card
        // descriptor reaches that parent in two hops out of the node -
        // `device`, then the parent's `drm/`, whose entries are the node
        // *names* of one GPU - and it is an identity check, not a
        // convenience: the Rust `drm` crate's `is_device_drm` is a `stat` of
        // `/sys/dev/char/226:N/device/drm`, and its `dev_path` then matches
        // the entries below by prefix (`card`, `renderD`) to find the sibling
        // node. Without the directory, a card whose `/dev` node and `st_rdev`
        // are both correct is refused as "the provided file descriptor does
        // not refer to a DRM node" - which is where niri died, before it had
        // drawn anything, with libdrm's different test (`fstat` plus a look
        // for `/dev/dri/card*`) happily passing.
        //
        // Nothing here asks the hardware, so this is not gated on
        // `target_os`: a tree the host cannot build is a tree nothing pins,
        // and the chain above is exactly the kind of shape that rots.
        {
            let drm = class.add_dir("drm", next);
            next += 1;
            let drm_root = virtual_root.add_dir("drm", next);
            next += 1;
            let gpu = virtual_root.add_dir("virtio-gpu", next);
            next += 1;
            let gpu_drm = gpu.add_dir("drm", next);
            next += 1;
            for (name, minor, devnode) in [
                ("card0", 0u32, "dri/card0"),
                ("renderD128", 128, "dri/renderD128"),
            ] {
                // The device itself under `/sys/devices/virtual/drm`, and the
                // class entry as a link to it - the shape Linux's driver core
                // produces, and the shape `udev` needs: a uevent names its
                // device by `/sys/<DEVPATH>`, so a `DEVPATH` that is only the
                // class symlink's target *in name* is an event the daemon can
                // resolve to nothing. It then keeps the entry it opened, writes
                // no properties into it, and every client that reads the
                // database - libudev, and through it weston's GPU selection -
                // sees a card with no subsystem and no tags.
                let dir = drm_root.add_dir(name, next);
                next += 1;
                devnodes.push((
                    "drm",
                    alloc::format!("devices/virtual/drm/{}", name),
                    226,
                    minor,
                ));
                // DRM's major, fixed as a convention since XFree86; the
                // minors are the card index and 128 + index.
                let numbers = alloc::format!("226:{}\n", minor);
                let numbers: &'static str = Box::leak(numbers.into_boxed_str());
                dir.add(
                    "dev",
                    Arc::new(SysFile {
                        ino: next,
                        content: numbers,
                    }),
                );
                next += 1;
                dir.add(
                    "uevent",
                    Arc::new(SysEventAttr {
                        ino: next,
                        event: Event {
                            action: "add",
                            devpath: alloc::format!("/devices/virtual/drm/{}", name),
                            subsystem: "drm",
                            devname: String::from(devnode),
                            major: 226,
                            minor,
                            extra: &[],
                        },
                    }),
                );
                next += 1;
                dir.add(
                    "subsystem",
                    Arc::new(SysSymlink {
                        ino: next,
                        target: String::from("../../../class/drm"),
                    }),
                );
                next += 1;
                // The GPU this node belongs to. The parent is the same
                // directory for both nodes - one card carries both names - so
                // the link is what makes them siblings rather than two trees
                // that agree by hand.
                dir.add(
                    "device",
                    Arc::new(SysSymlink {
                        ino: next,
                        target: String::from("../../virtio-gpu"),
                    }),
                );
                next += 1;
                // And the node under the parent's `drm/`, by name, as a link
                // back to the node itself: that listing is the whole answer to
                // "which other names does this GPU have", and a copy of the
                // node here would be a second device to keep in step.
                gpu_drm.add(
                    name,
                    Arc::new(SysSymlink {
                        ino: next,
                        target: alloc::format!("../../drm/{}", name),
                    }),
                );
                next += 1;
                drm.add(
                    name,
                    Arc::new(SysSymlink {
                        ino: next,
                        target: alloc::format!("../../devices/virtual/drm/{}", name),
                    }),
                );
                next += 1;
            }
        }

        // `/sys/dev/char` and `/sys/dev/block`: the reverse index from a device
        // *number* to the device that carries it. Linux keeps it because a
        // number is all some callers ever have - `udevadm info
        // --device-id-of-file=/`, which the udev cold-plug runs to learn what
        // the root filesystem is mounted from, resolves through
        // `/sys/dev/block`, and libdrm turns a card's file descriptor into a
        // device the same way. A tree without it is a tree in which a device
        // node cannot be identified at all.
        //
        let dev = fs.root.add_dir("dev", next);
        next += 1;
        let chars = dev.add_dir("char", next);
        next += 1;
        let blocks = dev.add_dir("block", next);
        next += 1;
        for (class_name, where_it_lives, major, minor) in devnodes {
            let numbers = alloc::format!("{}:{}", major, minor);
            let target = alloc::format!("../../{}", where_it_lives);
            let dir = if class_name == "block" {
                &blocks
            } else {
                &chars
            };
            dir.add(&numbers, Arc::new(SysSymlink { ino: next, target }));
            next += 1;
        }

        // The PCI bus, and the devices on it. The inode numbers are handed out
        // in one sequence and a device carries many attributes, so this comes
        // after the classes - nothing but "no two nodes share a number" is a
        // contract here.
        let pci = bus.add_dir("pci", next);
        next += 1;
        next = build_pci(&pci, &devices, pci_devices, next);

        let kernel = fs.root.add_dir("kernel", next);
        next += 1;
        kernel.add(
            "ostype",
            Arc::new(SysFile {
                ino: next,
                content: env!("CARGO_PKG_NAME"),
            }),
        );

        fs
    }
}

/// One capability bitmap, printed the way `/sys` prints a kernel bitmap:
/// `unsigned long` words, most significant first, space-separated, in hex.
///
/// The format is the kernel's `%*pb`, and `udev` parses exactly it (`strrchr`
/// on spaces, `strtoul(..., 16)` per word, filling a `unsigned long` array).
/// So the words have to be 64 bits wide and the *high* word has to come first:
/// `input_id` reads `test_bit(EV_ABS, ...)` and `test_bit(ABS_X, ...)` out of
/// these words, and a bitmap printed the other way round classifies a pointer
/// as something else entirely.
///
/// The declared bit count is the *type's*, not the device's - `EV_KEY` covers
/// `KEY_MAX` whatever a keyboard implements - and it is what decides how many
/// words are printed.
fn bitmap_attr(bits: &[u8], declared: usize) -> String {
    let mut out = String::new();
    for word in (0..declared.div_ceil(64)).rev() {
        if !out.is_empty() {
            out.push(' ');
        }
        let mut value = 0u64;
        for byte in 0..8 {
            let at = word * 8 + byte;
            if at < bits.len() {
                value |= (bits[at] as u64) << (byte * 8);
            }
        }
        out.push_str(&alloc::format!("{:x}", value));
    }
    out
}

/// Build `/sys/bus/pci` and the devices under `/sys/devices`, and return the
/// next free inode number.
///
/// The layout is Linux's, and its shape is not arbitrary. `/sys/bus/pci` is an
/// index of links; `/sys/devices/pci0000:00/...` is the tree they point into,
/// with a bridge's children underneath the bridge. Both halves have to agree
/// about where a device really lives, because the reader that cares - the one
/// drawing `lspci -t` - gets a device's parent by resolving the link and
/// reading the name it lands on.
fn build_pci(
    pci_dir: &Arc<SysDir>,
    devices_root: &Arc<SysDir>,
    pci: Vec<Device>,
    mut next: Ino,
) -> Ino {
    let root_bus = devices_root.add_dir("pci0000:00", next);
    next += 1;
    let links = pci_dir.add_dir("devices", next);
    next += 1;
    let drivers = pci_dir.add_dir("drivers", next);
    next += 1;

    // Which directory holds the devices of each bus, and where that directory
    // sits under `/sys/devices`. Bus 0 is the root bus's own; any other bus is
    // reached through a bridge, and the enumeration walks buses in order, so a
    // device's bridge has always been placed by the time the device is. A bus
    // whose bridge is somehow missing falls back to the root rather than
    // vanishing.
    let mut buses: Vec<(u8, Arc<SysDir>, String)> =
        alloc::vec![(0u8, root_bus.clone(), String::from("pci0000:00"))];

    for device in &pci {
        let (parent, prefix) = buses
            .iter()
            .find(|(bus, _, _)| *bus == device.bus)
            .map(|(_, dir, path)| (dir.clone(), path.clone()))
            .unwrap_or_else(|| (root_bus.clone(), String::from("pci0000:00")));
        let name = device.sysname();
        let path = alloc::format!("{}/{}", prefix, name);
        let dir = parent.add_dir(&name, next);
        next += 1;

        // The link that makes the device reachable by address, which is where
        // every reader looks for it. Relative, as Linux's is, and the same
        // three levels up however deep the device sits: the target is a path
        // *inside* `/sys/devices`, not a count of bridges.
        links.add(
            &name,
            Arc::new(SysSymlink {
                ino: next,
                target: alloc::format!("../../../devices/{}", path),
            }),
        );
        next += 1;

        next = add_pci_attributes(&dir, &drivers, device, &path, next);

        if let Some(secondary) = device.bridge {
            buses.push((secondary, dir, path));
        }
    }
    next
}

/// Hang one device's attributes in its own directory, and return the next free
/// inode number.
///
/// Which attributes exist is a decision, not a copy: Linux has many more
/// (`numa_node`, `iommu_group`, `label`, `msi_irqs`, the per-BAR `resourceN`
/// files) and every one of them is optional to a reader - pciutils opens each
/// and carries on when it is not there. What is here is what can be answered
/// honestly: identity, resources, the live configuration space, and the
/// event.
fn add_pci_attributes(
    dir: &Arc<SysDir>,
    drivers: &Arc<SysDir>,
    device: &Device,
    path: &str,
    mut next: Ino,
) -> Ino {
    let attributes = [
        ("vendor", device.vendor_attr()),
        ("device", device.device_attr()),
        ("subsystem_vendor", device.subsystem_vendor_attr()),
        ("subsystem_device", device.subsystem_device_attr()),
        ("revision", device.revision_attr()),
        ("class", device.class_attr()),
        ("irq", device.irq_attr()),
        ("modalias", device.modalias_attr()),
        ("resource", device.resource_attr()),
        // `enable` is the driver's reference count in Linux, and this kernel
        // has no such count: a device is on because the firmware turned it on,
        // and nothing here ever turns one off. A read-only zero says that. A
        // writable one would claim a power management this kernel does not
        // have, and a tool that wrote it would believe it.
        ("enable", String::from("0\n")),
        ("power_state", String::from("D0\n")),
    ];
    for (name, content) in attributes {
        let content: &'static str = Box::leak(content.into_boxed_str());
        dir.add(name, Arc::new(SysFile { ino: next, content }));
        next += 1;
    }

    dir.add(
        "config",
        Arc::new(SysConfig {
            ino: next,
            bus: device.bus,
            dev: device.dev,
            func: device.func,
        }),
    );
    next += 1;

    dir.add(
        "uevent",
        Arc::new(SysPciEvent {
            ino: next,
            keys: device.uevent_attr(),
            event: Event {
                action: "add",
                devpath: alloc::format!("/devices/{}", path),
                subsystem: "pci",
                devname: String::new(),
                major: 0,
                minor: 0,
                extra: &[],
            },
        }),
    );
    next += 1;

    // `driver` - the link `lspci -k` follows to name the driver in use. Its
    // target is relative to the device's own directory, so how far up to go
    // depends on how deep the device sits: one more level for every bridge
    // between it and the root bus. And the directory it points at has to
    // exist, because `realpath` resolves it - a link to nothing names nothing.
    if let Some(driver) = device.driver {
        let up = path.matches('/').count() + 2;
        if drivers.lookup(driver).is_err() {
            drivers.add_dir(driver, next);
            next += 1;
        }
        dir.add(
            "driver",
            Arc::new(SysSymlink {
                ino: next,
                target: alloc::format!("{}bus/pci/drivers/{}", "../".repeat(up), driver),
            }),
        );
        next += 1;
    }
    next
}

impl FileSystem for SysFs {
    fn root(&self) -> Arc<dyn Inode> {
        self.root.clone()
    }

    fn name(&self) -> &'static str {
        "sysfs"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fs() -> Arc<SysFs> {
        // A machine with a disk, since the host has none: the same shape the
        // kernel sees, so the tree and its attributes are exercised here.
        SysFs::with_devices(Numbering {
            devices: alloc::vec![
                (String::from("vda"), 254, 0, true),
                (String::from("vda1"), 254, 1, false),
                (String::from("vda2"), 254, 2, false),
            ],
        })
    }

    fn block_dir(fs: &Arc<SysFs>) -> Arc<dyn Inode> {
        fs.root().lookup("class").unwrap().lookup("block").unwrap()
    }

    #[test]
    fn the_tree_is_walkable_the_way_a_listener_walks_it() {
        // /class, then /class/block, then a device, then its attributes -
        // which is `trigger_path("/class", NULL, 2)` in nlplug-findfs.
        let fs = fs();
        let block = block_dir(&fs);
        for (name, kind) in block.read_dir().expect("a listing") {
            assert_eq!(kind, NodeKind::Dir, "{} is a device directory", name);
            let device = block.lookup(&name).expect("the device");
            device.lookup("dev").expect("dev");
            device.lookup("uevent").expect("uevent");
        }
    }

    #[test]
    fn a_dev_attribute_reads_as_the_numbers_dev_nodes_are_made_of() {
        let fs = fs();
        let block = block_dir(&fs);
        for (name, _kind) in block.read_dir().unwrap() {
            let dev = block.lookup(&name).unwrap().lookup("dev").unwrap();
            let mut buf = [0u8; 16];
            let n = dev.read_at(0, &mut buf).expect("read");
            let text = core::str::from_utf8(&buf[..n]).unwrap().trim();
            let (major, minor) = text.split_once(':').expect("major:minor");
            assert_eq!(major, "254", "virtio-blk is major 254");
            assert!(minor.parse::<u32>().is_ok());
        }
    }

    #[test]
    fn writing_uevent_announces_the_device_again() {
        let _bus = crate::uevent::test_bus::take();
        // The point of the attribute: a listener that started late writes
        // "add" and gets an event it would never have heard.
        let fs = fs();
        let watcher = crate::uevent::open();
        crate::uevent::bind(watcher, 0, 1, 0).expect("a fresh socket binds");
        let block = block_dir(&fs);
        let (name, _) = block.read_dir().unwrap().into_iter().next().unwrap();
        let uevent = block.lookup(&name).unwrap().lookup("uevent").unwrap();
        assert_eq!(uevent.write_at(0, b"add").unwrap(), 3);

        let event = crate::uevent::recv(watcher)
            .expect("the re-announcement")
            .bytes;
        let text = String::from_utf8(event.to_vec()).unwrap();
        assert!(text.starts_with("add@"));
        assert!(text.contains(&alloc::format!("DEVNAME={}", name)));

        // An action the kernel does not speak is refused, and nothing goes out.
        assert!(uevent.write_at(0, b"frobnicate").is_err());
        assert!(!crate::uevent::ready(watcher));
        crate::uevent::close(watcher);
    }

    /// One PCI function, with nothing assigned to its BARs.
    fn a_function(bus: u8, dev: u8, func: u8) -> Device {
        Device::fixture(bus, dev, func)
    }

    /// A machine with a disk *and* a PCI bus: a virtio function on the root
    /// bus with a driver bound to it, and a second function behind a bridge -
    /// so the tree has both a flat level and a nested one to place, which is
    /// the difference the `driver` link's depth is made of.
    fn pci_fs() -> Arc<SysFs> {
        let mut disk = a_function(0, 3, 0);
        disk.driver = Some("virtio-pci");
        let mut bridge = a_function(0, 1, 2);
        bridge.vendor = 0x1b36;
        bridge.device = 0x000c;
        bridge.class = 0x06;
        bridge.subclass = 0x04;
        bridge.header_type = 0x01;
        bridge.bridge = Some(0x01);
        let behind = a_function(1, 0, 0);

        SysFs::with(
            Numbering {
                devices: alloc::vec![(String::from("vda"), 254, 0, true)],
            },
            alloc::vec![disk, bridge, behind],
        )
    }

    /// Resolve an absolute path the way a real reader does: `/sys` mounted on
    /// a root of its own, links and mount edge and all. The mount is not
    /// decoration - a reader's path crosses it, and the links inside the tree
    /// are relative to directories on the far side of it.
    fn resolve(fs: &Arc<SysFs>, path: &str) -> Arc<dyn Inode> {
        let vfs = crate::vfs::mount::Vfs::new();
        let root = crate::vfs::ramfs::RamFs::new();
        vfs.mount_root(&*root);
        root.root().create("sys", NodeKind::Dir).unwrap();
        vfs.mount("/sys", &**fs).unwrap();
        vfs.resolve(path)
            .unwrap_or_else(|e| panic!("{path} did not resolve: {e:?}"))
    }

    /// The same walk on a root that has `fillers` other directories in it.
    ///
    /// Which is the difference between this tree and a machine: the guest's
    /// root filesystem carries 483 directories at depth four or less, so a walk
    /// that answers `..` by *searching* the tree for each parent spends its
    /// budget before it reaches anything this deep - and every relative link
    /// target in `/sys` names its parent with `..`.
    fn resolve_on_a_full_root(fs: &Arc<SysFs>, path: &str, fillers: usize) -> Arc<dyn Inode> {
        let vfs = crate::vfs::mount::Vfs::new();
        let root = crate::vfs::ramfs::RamFs::new();
        vfs.mount_root(&*root);
        root.root().create("sys", NodeKind::Dir).unwrap();
        for i in 0..fillers {
            root.root()
                .create(&alloc::format!("d{i:04}"), NodeKind::Dir)
                .unwrap();
        }
        vfs.mount("/sys", &**fs).unwrap();
        vfs.resolve(path)
            .unwrap_or_else(|e| panic!("{path} did not resolve: {e:?}"))
    }

    fn text(node: &Arc<dyn Inode>) -> String {
        let mut buf = [0u8; 512];
        let n = node.read_at(0, &mut buf).expect("a read");
        String::from_utf8(buf[..n].to_vec()).expect("utf-8")
    }

    #[test]
    #[test]
    fn a_capability_bitmap_prints_as_linux_prints_one() {
        // udev's `input_id` reads these attributes with `strtoul(..., 16)`
        // per space-separated word, filling a `unsigned long` array - so the
        // words are 64 bits wide and the high word comes first. A keyboard's
        // event-type bitmap (EV_SYN|EV_KEY|EV_MSC|EV_LED|EV_REP) is one word,
        // and this is the value the kernel prints for it.
        assert_eq!(bitmap_attr(&[0x13, 0x00, 0x12, 0x00], 32), "120013");
        // A key bitmap is twelve words wide whatever the device implements,
        // and the *last* word is the low bits - `test_key` looks at word 0 for
        // the keys below BTN_MISC, so a bitmap printed the other way round
        // would have it read a bit of something else.
        let mut keys = [0u8; 96];
        keys[3] = 0xff; // KEY_24..KEY_31: the top of the low word
        let text = bitmap_attr(&keys, 0x300);
        let words: Vec<&str> = text.split(' ').collect();
        assert_eq!(words.len(), 12, "{text:?}");
        assert_eq!(words[11], "ff000000", "the low word prints last");
        assert_eq!(words[0], "0");
        // An empty bitmap is still the type's width, and prints as a zero.
        assert_eq!(bitmap_attr(&[], 32), "0");
    }

    #[test]
    fn the_pci_tree_is_walkable_the_way_lspci_walks_it() {
        // pciutils opens /sys/bus/pci (its detect), readdirs `devices`, and
        // parses each name as a bus address - which is the walk below.
        let fs = pci_fs();
        let devices = resolve(&fs, "/sys/bus/pci/devices");
        assert_eq!(devices.metadata().kind, NodeKind::Dir);

        let mut listed: Vec<String> = devices
            .read_dir()
            .expect("a listing")
            .into_iter()
            .map(|(name, kind)| {
                assert_eq!(kind, NodeKind::Symlink, "{name} is a link, as Linux's is");
                name
            })
            .collect();
        listed.sort();
        assert_eq!(
            listed,
            alloc::vec![
                String::from("0000:00:01.2"),
                String::from("0000:00:03.0"),
                String::from("0000:01:00.0"),
            ]
        );
    }

    #[test]
    fn a_device_link_resolves_to_its_real_home() {
        // The link is what carries the topology: a reader resolves
        // `<bdf>/..` and parses the name it lands on to find the parent.
        let fs = pci_fs();
        let devices = resolve(&fs, "/sys/bus/pci/devices");
        for (name, _) in devices.read_dir().unwrap() {
            let link = devices.lookup(&name).unwrap();
            let target = link.readlink().unwrap();
            assert!(
                target.starts_with("../../../devices/pci0000:00/"),
                "{target}"
            );
            assert!(target.ends_with(&name), "{target} names {name}");
            // And it really resolves: the attributes are behind it.
            assert_eq!(
                resolve(&fs, &alloc::format!("/sys/bus/pci/devices/{name}/vendor"))
                    .metadata()
                    .kind,
                NodeKind::File
            );
        }
        // A device behind the bridge hangs under the bridge, not beside it -
        // one level deeper, which is what its `driver` link's extra `../`
        // is for.
        assert_eq!(
            devices.lookup("0000:01:00.0").unwrap().readlink().unwrap(),
            "../../../devices/pci0000:00/0000:00:01.2/0000:01:00.0"
        );
        // And its parent really is the bridge: `realpath("<bdf>/..")` has to
        // land there, because that is the name `lspci -t` parses an edge out
        // of.
        let bridge = resolve(&fs, "/sys/devices/pci0000:00/0000:00:01.2");
        let parent = resolve(&fs, "/sys/devices/pci0000:00/0000:00:01.2/0000:01:00.0/..");
        assert!(Arc::ptr_eq(&bridge, &parent));
    }

    #[test]
    fn the_attributes_read_as_linux_spells_them() {
        let fs = pci_fs();
        let at = |name: &str| {
            text(&resolve(
                &fs,
                &alloc::format!("/sys/bus/pci/devices/0000:00:03.0/{name}"),
            ))
        };
        assert_eq!(at("vendor"), "0x1af4\n");
        assert_eq!(at("device"), "0x1001\n");
        assert_eq!(at("subsystem_vendor"), "0x1af4\n");
        assert_eq!(at("subsystem_device"), "0x0002\n");
        assert_eq!(at("class"), "0x010000\n");
        assert_eq!(at("revision"), "0x00\n");
        assert_eq!(at("irq"), "11\n");
        assert_eq!(
            at("modalias"),
            "pci:v00001AF4d00001001sv00001AF4sd00000002bc01sc00i00\n"
        );
        assert_eq!(at("resource").lines().count(), 7);
        assert!(at("uevent").contains("PCI_ID=1AF4:1001\n"));
        // busybox's `lspci` reads the uevent file and nothing else, so these
        // keys are the whole of what it knows about a device.
        assert!(at("uevent").contains("PCI_SLOT_NAME=0000:00:03.0\n"));
        assert!(at("uevent").contains("PCI_CLASS=10000\n"));
    }

    #[test]
    fn the_config_attribute_is_the_size_mechanism_one_can_address() {
        // 256 bytes, and not a byte more: the address register of mechanism 1
        // has eight bits of offset. Reporting 4096 would invite a reader to
        // read PCIe extended capabilities that cannot be reached.
        let fs = pci_fs();
        let config = resolve(&fs, "/sys/bus/pci/devices/0000:00:03.0/config");
        assert_eq!(config.metadata().kind, NodeKind::File);
        assert_eq!(config.metadata().size, 256);
    }

    #[test]
    fn a_claimed_device_has_a_driver_link_that_resolves() {
        let fs = pci_fs();
        let driver = resolve(&fs, "/sys/bus/pci/devices/0000:00:03.0/driver");
        // The link resolves to the driver's own directory, which is what
        // `lspci -k` takes the basename of.
        assert_eq!(driver.metadata().kind, NodeKind::Dir);
        assert!(driver.lookup("nonexistent").is_err());
        assert!(
            resolve(&fs, "/sys/bus/pci/drivers/virtio-pci")
                .metadata()
                .kind
                == NodeKind::Dir
        );
        // Three levels up from a device on the root bus.
        assert_eq!(
            resolve(&fs, "/sys/bus/pci/devices/0000:00:03.0")
                .lookup("driver")
                .unwrap()
                .readlink()
                .unwrap(),
            "../../../bus/pci/drivers/virtio-pci"
        );
        // An unclaimed device has no link at all, which is how `lspci -k`
        // tells "no driver" from "a driver named nothing".
        let unclaimed = resolve(&fs, "/sys/bus/pci/devices/0000:01:00.0");
        assert!(unclaimed.lookup("driver").is_err());
        assert!(unclaimed.lookup("vendor").is_ok());
    }

    #[test]
    fn a_device_number_resolves_back_to_its_device() {
        // `/sys/dev/<type>/<major>:<minor>` is the reverse index udev and
        // libdrm use: a number is all some callers ever have, and this is how
        // they turn it back into a device. `udevadm info
        // --device-id-of-file=/` - which the udev cold-plug runs to learn what
        // the root filesystem is mounted from - resolves the root's number
        // through `/sys/dev/block`.
        let fs = pci_fs();
        assert_eq!(resolve(&fs, "/sys/dev").metadata().kind, NodeKind::Dir);
        let blocks = resolve(&fs, "/sys/dev/block");
        let link = blocks.lookup("254:0").expect("the disk's number");
        assert_eq!(link.metadata().kind, NodeKind::Symlink);
        assert_eq!(link.readlink().unwrap(), "../../class/block/vda");
        // And it resolves to the device itself, not to a link to nowhere.
        assert_eq!(
            resolve(&fs, "/sys/dev/block/254:0").metadata().kind,
            NodeKind::Dir
        );
        // A number nothing carries is not there: an index that invented an
        // entry would hand a caller a device that does not exist.
        assert!(blocks.lookup("254:9").is_err());
    }

    #[test]
    fn a_card_leads_to_the_node_names_the_drm_crate_reads() {
        // The identity chain the Rust `drm` crate walks from an open card, and
        // the one niri panicked on: `DrmNode::from_path("/dev/dri/card0")`
        // stats the node, is handed 226:0, and then has only sysfs left to ask
        // - whether `/sys/dev/char/226:0/device/drm` exists at all (its
        // `is_device_drm`), and which names it lists (its `dev_path`, matching
        // by prefix to find the sibling node). A tree with everything else
        // right and this directory missing answers the first question "no":
        // "the provided file descriptor does not refer to a DRM node".
        let fs = fs();
        for number in ["226:0", "226:128"] {
            let parent = resolve(&fs, &alloc::format!("/sys/dev/char/{number}/device"));
            assert_eq!(parent.metadata().kind, NodeKind::Dir);
            let nodes = resolve(&fs, &alloc::format!("/sys/dev/char/{number}/device/drm"));
            assert_eq!(nodes.metadata().kind, NodeKind::Dir);
            let names: Vec<String> = nodes
                .read_dir()
                .expect("a listing")
                .into_iter()
                .map(|(name, _kind)| name)
                .collect();
            // The rule `dev_path` applies: the first entry whose name starts
            // with the type's prefix, taken only if the `/dev` node it names
            // exists. `/dev/dri/card0` and `/dev/dri/renderD128` are the two
            // the device registry registers, so both prefixes must answer -
            // and answer with the name that node really has, since that name
            // is the path the crate hands back.
            assert_eq!(
                names
                    .iter()
                    .find(|name| name.starts_with("card"))
                    .map(String::as_str),
                Some("card0")
            );
            assert_eq!(
                names
                    .iter()
                    .find(|name| name.starts_with("renderD"))
                    .map(String::as_str),
                Some("renderD128")
            );
            // No third name: an entry whose `/dev` node does not exist is a
            // node the crate finds and then rejects, which reads to a
            // compositor as a GPU with no render node at all.
            assert_eq!(names.len(), 2, "one GPU, two names: {names:?}");
        }

        // One card under three names - the device tree's directory, the class
        // entry, and the number's index - and they have to be the same node: a
        // compositor picks the card up by class and then identifies it by
        // number, and two nodes would be two cards.
        assert_eq!(
            resolve(&fs, "/sys/dev/char/226:0").metadata().ino,
            resolve(&fs, "/sys/class/drm/card0").metadata().ino,
            "the class entry and the device number are one card"
        );
        // And one parent for both nodes, which is what makes them siblings: a
        // compositor asks the card it opened for the *name* of its render
        // node through exactly this directory.
        assert_eq!(
            resolve(&fs, "/sys/dev/char/226:0/device").metadata().ino,
            resolve(&fs, "/sys/dev/char/226:128/device").metadata().ino,
            "both nodes hang from the one GPU"
        );
    }

    #[test]
    fn the_cards_identity_chain_survives_a_full_root() {
        // The same chain, on a root with a machine's number of directories in
        // it. niri hit this one *after* the chain above existed: the `device`
        // link was in the kernel and the card was still refused as "the
        // provided file descriptor does not refer to a DRM node", because the
        // `..` in the link's own target could not be resolved on a tree this
        // size (see `a_links_target_climbs_the_chain_rather_than_searching_for_it`).
        // The 600 directories are the guest's 483 in miniature.
        let fs = fs();
        let drm = resolve_on_a_full_root(&fs, "/sys/dev/char/226:0/device/drm", 600);
        assert_eq!(drm.metadata().kind, NodeKind::Dir);
        // The names `dev_path` matches by prefix, read on the same full root:
        // this is what hands the compositor its sibling node, so a chain that
        // resolves to the wrong directory is as bad as one that does not.
        let names: Vec<String> = drm
            .read_dir()
            .expect("a listing")
            .into_iter()
            .map(|(name, _kind)| name)
            .collect();
        assert_eq!(
            names
                .iter()
                .find(|name| name.starts_with("renderD"))
                .map(String::as_str),
            Some("renderD128")
        );
        for number in ["226:0", "226:128"] {
            let parent =
                resolve_on_a_full_root(&fs, &alloc::format!("/sys/dev/char/{number}/device"), 600);
            assert_eq!(parent.metadata().kind, NodeKind::Dir);
        }
    }

    #[test]
    fn no_two_nodes_in_the_tree_share_an_inode_number() {
        // The numbers are handed out by a counter threaded through the whole
        // build, and a device carries a dozen attributes each - so the one
        // thing that can go wrong quietly is a second counter starting where
        // the first left off. Nothing else notices: `stat` reports the number,
        // and a reader that believed two nodes were one would be wrong.
        let fs = pci_fs();
        let mut seen: Vec<Ino> = Vec::new();
        let mut queue = alloc::vec![fs.root()];
        while let Some(dir) = queue.pop() {
            seen.push(dir.metadata().ino);
            for (name, kind) in dir.read_dir().unwrap_or_default() {
                // A link is not followed: its target is already in the walk,
                // and following one here would visit the same nodes twice.
                if kind == NodeKind::Symlink {
                    continue;
                }
                let child = dir.lookup(&name).expect("a listed name resolves");
                if kind == NodeKind::Dir {
                    queue.push(child);
                } else {
                    seen.push(child.metadata().ino);
                }
            }
        }
        let mut unique = seen.clone();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(
            unique.len(),
            seen.len(),
            "{} nodes, {} distinct numbers",
            seen.len(),
            unique.len()
        );
        // And the walk reached the whole tree rather than a corner of it.
        assert!(seen.len() > 30, "only {} nodes walked", seen.len());
    }

    #[test]
    fn writing_the_pci_uevent_announces_the_device_again() {
        let _bus = crate::uevent::test_bus::take();
        let fs = pci_fs();
        let watcher = crate::uevent::open();
        crate::uevent::bind(watcher, 0, 1, 0).expect("a fresh socket binds");
        let uevent = resolve(&fs, "/sys/bus/pci/devices/0000:00:03.0/uevent");
        assert_eq!(uevent.write_at(0, b"add").unwrap(), 3);

        let event = crate::uevent::recv(watcher)
            .expect("the re-announcement")
            .bytes;
        let text = String::from_utf8(event.to_vec()).unwrap();
        assert!(text.starts_with("add@/devices/pci0000:00/0000:00:03.0"));
        assert!(text.contains("SUBSYSTEM=pci"));
        // No device node: a PCI function has none, and an event claiming one
        // would have a listener create it.
        assert!(!text.contains("DEVNAME="));

        assert!(uevent.write_at(0, b"frobnicate").is_err());
        assert!(!crate::uevent::ready(watcher));
        crate::uevent::close(watcher);
    }
}
