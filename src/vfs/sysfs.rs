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
        SysFs::with(Numbering::of_this_machine(), crate::pci::devices())
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
        // `/sys/block`: the *whole* disks and nothing else, each a link to its
        // entry in the class directory - Linux's shape, where `/sys/block/<disk>`
        // is a link into the device tree and a partition is deliberately not
        // listed here.
        //
        // It is not a second spelling of `/sys/class/block`. A reader that wants
        // every block device walks the class directory; a reader that wants to
        // know what the machine has *storage* on walks this one, because a
        // partition is not a disk. `lsblk` builds its table from exactly that
        // distinction: it lists `/sys/block`, then looks inside each entry for
        // the partitions, which is why the partitions are real directories
        // under the disk (below) and not entries of their own.
        let block_root = fs.root.add_dir("block", next);
        next += 1;
        // `/sys/bus/usb/devices`: the USB bus, with nothing on it.
        //
        // Empty, and deliberately so. Linux registers the USB bus whether or not
        // the machine has a host controller, so a machine with no USB has an
        // empty `devices` directory rather than no directory: `lsusb` opens this
        // exact path and lists what it finds, and an empty directory is the true
        // answer - "no USB devices" - where the absent one that used to be here
        // was an error it reported and exited non-zero on. This kernel has no
        // xHCI and no driver for one, so nothing will ever appear here; the
        // directory says what is true about the tree, not what is hoped for.
        let usb = bus.add_dir("usb", next);
        next += 1;
        usb.add_dir("devices", next);
        next += 1;
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
        let text = |s: String| -> &'static str { Box::leak(s.into_boxed_str()) };
        // The disk the partitions that follow belong to. `BlockEntry::of_disk`
        // contributes a disk and then its own partitions, one disk at a time,
        // and that order is the whole of what says which partition is whose -
        // there is no parent to read, and a name does not say it either, since a
        // partition is named `<disk><n>` and a disk may be called `sda1`.
        let mut disk_home: Option<(alloc::string::String, Arc<SysDir>)> = None;
        for entry in numbering.devices.iter() {
            let name = &entry.name;
            // Where this device's attributes really live: one home per file.
            // A whole disk's home is its own class entry; a partition's is
            // *inside* its disk's, which is where Linux puts it - the partition
            // is a child of the disk in the device tree - and where a reader
            // that walks a disk looking for its partitions finds it. The class
            // entry of a partition is then a link to that one home rather than a
            // second copy of the same three attributes.
            let (dir, home) = if entry.whole {
                let dir = block.add_dir(name, next);
                next += 1;
                block_root.add(
                    name,
                    Arc::new(SysSymlink {
                        ino: next,
                        target: alloc::format!("../class/block/{name}"),
                    }),
                );
                next += 1;
                disk_home = Some((name.clone(), dir.clone()));
                (dir, alloc::format!("class/block/{name}"))
            } else {
                let (disk_name, parent) = disk_home
                    .clone()
                    .expect("a partition is numbered after its own disk");
                let dir = parent.add_dir(name, next);
                next += 1;
                block.add(
                    name,
                    Arc::new(SysSymlink {
                        ino: next,
                        target: alloc::format!("{disk_name}/{name}"),
                    }),
                );
                next += 1;
                (dir, alloc::format!("class/block/{disk_name}/{name}"))
            };
            devnodes.push(("block", home, entry.major, entry.minor));
            // The numbers, as `/dev` nodes are made from them.
            dir.add(
                "dev",
                Arc::new(SysFile {
                    ino: next,
                    content: text(alloc::format!("{}:{}\n", entry.major, entry.minor)),
                }),
            );
            next += 1;
            // What the device is worth, in 512-byte sectors - the unit sysfs
            // uses, and the one `entry.sectors` is in. A reader that wants bytes
            // multiplies; `lsblk` prints `SIZE` from this attribute and would
            // print a partition as empty without it.
            dir.add(
                "size",
                Arc::new(SysFile {
                    ino: next,
                    content: text(alloc::format!("{}\n", entry.sectors)),
                }),
            );
            next += 1;
            // How a partition is told from a disk, and which partition it is.
            // The attribute exists on partitions only - a whole disk has no
            // `partition` file, and its *absence* is what says "disk" - and its
            // value is the partition's number in its table, which is the number
            // its own name ends with and the minor it was given.
            if !entry.whole {
                dir.add(
                    "partition",
                    Arc::new(SysFile {
                        ino: next,
                        content: text(alloc::format!("{}\n", entry.minor)),
                    }),
                );
                next += 1;
            }
            dir.add(
                "uevent",
                Arc::new(SysEventAttr {
                    ino: next,
                    event: Event {
                        action: "add",
                        // The tree position comes from the disk's own driver,
                        // so a SATA disk is filed under the SATA controller
                        // rather than under virtio with the rest.
                        devpath: alloc::format!("{}/{}", entry.devpath, name),
                        subsystem: "block",
                        devname: name.clone(),
                        major: entry.major,
                        minor: entry.minor,
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
            // The PCI bus, and the devices on it. Built here rather than with
            // the rest of the classes below, because the DRM nodes hang off
            // the function that drives them and its directory has to exist
            // first.
            let pci = bus.add_dir("pci", next);
            next += 1;
            let (after_pci, display) = build_pci(&pci, &devices, pci_devices, next);
            next = after_pci;

            let drm = class.add_dir("drm", next);
            next += 1;
            let drm_root = virtual_root.add_dir("drm", next);
            next += 1;
            // Where the card's parent lives. In Linux a DRM node is a child of
            // the device that drives it, and the caller that matters asks that
            // parent three questions: `vendor`, `device`, and - resolved, not
            // read - `subsystem`, which is how it learns *which bus* it is
            // looking at before it parses anything else. Measured on a working
            // card and recorded in `PLAN.md` §7.4: libdrm's device parse reads
            // exactly those three, then the parent's `drm/` listing and its
            // `uevent`. A card hung from `/sys/devices/virtual/virtio-gpu` - a
            // directory with no attributes at all - answers none of them,
            // which is why a machine with 3D behind the card still reported a
            // software renderer and no renderer at all when GBM asked.
            //
            // A machine whose bus has no display function keeps the older
            // shape: there is no parent to name, and inventing one would be a
            // claim about a bus nothing found.
            //
            // The climb is three levels - `card0` to `drm`, to `virtual`, to
            // `devices` - and the function's own path is relative to
            // `/sys/devices`, which is the name a `PCI_SLOT_NAME` carries.
            let (gpu, path) = match display {
                Some((dir, path)) => (dir, path),
                None => {
                    let dir = virtual_root.add_dir("virtio-gpu", next);
                    next += 1;
                    (dir, String::from("virtual/virtio-gpu"))
                }
            };
            let parent = alloc::format!("../../../{}", path);
            // The `drm/` listing hangs under the *parent*, so the links inside
            // it climb from there rather than from the node - one level deeper
            // for every name in the parent's path, plus `devices` and `sys`.
            // Getting this wrong leaves the listing's entries dangling, which
            // is worse than an empty listing: a reader that walks `/sys` - udev
            // does, at coldplug - follows them into nothing.
            let listing_up = path.matches('/').count() + 2;
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
                        target: parent.clone(),
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
                        target: alloc::format!("{}virtual/drm/{}", "../".repeat(listing_up), name),
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

        // The PCI bus was built above, before the DRM class: the card hangs
        // off the function that drives it, so the function's directory has to
        // exist first. A device carries many attributes and the inode numbers
        // are handed out in one sequence, but nothing but "no two nodes share
        // a number" is a contract here.

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
) -> (Ino, Option<(Arc<SysDir>, String)>) {
    let root_bus = devices_root.add_dir("pci0000:00", next);
    next += 1;
    let links = pci_dir.add_dir("devices", next);
    next += 1;
    let drivers = pci_dir.add_dir("drivers", next);
    next += 1;

    // The display function, if the machine has one, and the path it lives at.
    // The DRM class is built from this: a DRM node hangs off the device that
    // drives it, so the node's `device` link has to name this directory, and
    // the tree is walked once rather than searched a second time.
    let mut display: Option<(Arc<SysDir>, String)> = None;

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

        // The display-class function is where a DRM node belongs - but a
        // machine can carry more than one, and only one of them is this
        // kernel's GPU. Measured, on the very first GL boot after this was
        // built: q35 puts the firmware's own VGA beside the `virtio-gpu` the
        // machine was given, class `0x03` both, and picking by class alone
        // hung the card off the VGA. Mesa then read it and said so in its own
        // words - `pci id for fd 3: 1234:1111, driver (null)`, the id of a
        // display adapter it has no driver for, where the card it was holding
        // is a virtio-gpu (`1af4:1050`).
        //
        // So the choice is the function a driver bound to. The firmware's VGA
        // has none, because this kernel has no driver for it; the GPU's
        // function carries `virtio-pci`, which is the driver that made the
        // card. **The honest shape is the driver telling sysfs where its own
        // device is, rather than sysfs guessing from class and driver** - and
        // that is recorded in `PLAN.md` §7.4 as the part still to do. What
        // this does not do is pick one of two live display drivers by
        // enumeration order, because this kernel has only the one.
        if device.class == PCI_CLASS_DISPLAY && device.driver.is_some() && display.is_none() {
            display = Some((dir.clone(), path.clone()));
        }

        if let Some(secondary) = device.bridge {
            buses.push((secondary, dir, path));
        }
    }
    (next, display)
}

/// The base class code of a display controller - `0x03`, Linux's
/// `PCI_CLASS_DISPLAY_VGA`/`_3D`/`_OTHER` family. The class byte is the base
/// class and the bits below it are the subclass, so this is a comparison on
/// the byte rather than on [`Device::class_code`].
const PCI_CLASS_DISPLAY: u8 = 0x03;

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

    // How far up a relative link has to climb to reach `/sys`: one level for
    // every name in the device's path, plus `devices` and `sys`. Both links
    // below are relative to the device's own directory, so how deep the device
    // sits decides it - one more level for each bridge between it and the root
    // bus.
    let up = path.matches('/').count() + 2;

    // `subsystem` - which bus the device is on. This is not advice for a
    // reader: libdrm resolves this link to learn which bus it is looking at
    // *before* it parses the bus-specific attributes, and gives up when it is
    // missing. Measured on a working card (`PLAN.md` §7.4): it answers
    // `../../../../bus/pci`, and a DRM node whose parent has no such link is a
    // device no caller can classify.
    dir.add(
        "subsystem",
        Arc::new(SysSymlink {
            ino: next,
            target: alloc::format!("{}bus/pci", "../".repeat(up)),
        }),
    );
    next += 1;

    // `driver` - the link `lspci -k` follows to name the driver in use. The
    // directory it points at has to exist, because `realpath` resolves it - a
    // link to nothing names nothing.
    if let Some(driver) = device.driver {
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
    use crate::uevent::BlockEntry;

    fn fs() -> Arc<SysFs> {
        // A machine with a disk, since the host has none: the same shape the
        // kernel sees, so the tree and its attributes are exercised here.
        SysFs::with_devices(Numbering {
            devices: alloc::vec![
                BlockEntry {
                    name: String::from("vda"),
                    major: 254,
                    minor: 0,
                    whole: true,
                    devpath: "/devices/virtio/block",
                    sectors: 20 * 1024 * 1024,
                },
                BlockEntry {
                    name: String::from("vda1"),
                    major: 254,
                    minor: 1,
                    whole: false,
                    devpath: "/devices/virtio/block",
                    sectors: 1024,
                },
                BlockEntry {
                    name: String::from("vda2"),
                    major: 254,
                    minor: 2,
                    whole: false,
                    devpath: "/devices/virtio/block",
                    sectors: 20 * 1024 * 1024 - 1024,
                },
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
        //
        // The device is reached by *path*, and that is the part worth stating:
        // a disk's class entry is a directory while a partition's is a link to
        // the directory inside its disk (Linux's shape, and see
        // `a_partition_is_a_directory_under_its_disk_and_a_link_beside_it`), so
        // a walk that went straight through the listing would have to resolve
        // the link itself. Everything that reads this tree - udev, libmount,
        // `lsblk` - resolves it, which is what this asserts.
        let fs = fs();
        let block = block_dir(&fs);
        let mut seen = 0;
        for (name, kind) in block.read_dir().expect("a listing") {
            assert!(
                matches!(kind, NodeKind::Dir | NodeKind::Symlink),
                "{name} is a device entry, not a {kind:?}"
            );
            let device = resolve(&fs, &alloc::format!("/sys/class/block/{name}"));
            device.lookup("dev").expect("dev");
            device.lookup("uevent").expect("uevent");
            seen += 1;
        }
        assert_eq!(seen, 3, "vda and both of its partitions");
    }

    #[test]
    fn a_dev_attribute_reads_as_the_numbers_dev_nodes_are_made_of() {
        let fs = fs();
        let block = block_dir(&fs);
        for (name, _kind) in block.read_dir().unwrap() {
            let dev = resolve(&fs, &alloc::format!("/sys/class/block/{name}"))
                .lookup("dev")
                .unwrap();
            let mut buf = [0u8; 16];
            let n = dev.read_at(0, &mut buf).expect("read");
            let text = core::str::from_utf8(&buf[..n]).unwrap().trim();
            let (major, minor) = text.split_once(':').expect("major:minor");
            assert_eq!(major, "254", "virtio-blk is major 254");
            assert!(minor.parse::<u32>().is_ok());
        }
    }

    /// The block shape the tools read, verified against one of them.
    ///
    /// Measured, not derived: busybox 1.38's `lsblk` - which is what this image's
    /// `/bin/lsblk` is - was run against mockups of this tree on the host.
    ///
    /// * A partition's entry under its disk that is a **directory** holding its
    ///   own `dev` (and, for `lsblk` to print a size, its own `size`): the full
    ///   table, `vda`, `vda1`, `vda2` with the mount point of the one that is
    ///   mounted, `vda3`.
    /// * The same entry as a **symlink** to the class entry: `lsblk` printed
    ///   `vda` and no partitions at all, having opened
    ///   `/sys/block/vda/vda1/dev` and found nothing it accepted as a partition.
    ///   So the entry under the disk has to be a directory, which is Linux's
    ///   shape as well - on Linux `/sys/block/vda/vda1` is the partition's own
    ///   device directory, and it is `/sys/class/block/vda1` that is the link.
    ///
    /// Both facts hold at once here because each file has exactly *one* home:
    /// the directory lives inside the disk, and the class entry is a link to it,
    /// so `dev` is written once and read by both names.
    #[test]
    fn a_partition_is_a_directory_under_its_disk_and_a_link_beside_it() {
        let fs = fs();
        let block = block_dir(&fs);

        // The disk is a directory with its own attributes, and its partitions are
        // directories inside it - which is what a walk of `/sys/block/<disk>`
        // finds.
        let disk = resolve(&fs, "/sys/class/block/vda");
        assert_eq!(disk.metadata().kind, NodeKind::Dir);
        assert_eq!(at(&fs, "/sys/class/block/vda/dev"), "254:0\n");
        let disk_listing = disk
            .read_dir()
            .expect("a listing")
            .into_iter()
            .collect::<Vec<_>>();
        // The partitions are the *directories* in the disk's entry - its other
        // children are its own attributes - which is exactly the distinction a
        // walk looking for partitions makes.
        let partitions = disk_listing
            .iter()
            .filter(|(_, kind)| *kind == NodeKind::Dir)
            .map(|(name, _)| name.as_str())
            .collect::<Vec<_>>();
        assert_eq!(partitions, ["vda1", "vda2"], "both of its partitions");

        // A partition's class entry is a link, and it resolves to the *same*
        // inode as the entry under the disk - one home, two names, which is how
        // a reader can be sure the two are one device.
        let beside = resolve(&fs, "/sys/class/block/vda1");
        let nested = resolve(&fs, "/sys/class/block/vda/vda1");
        assert_eq!(
            beside.metadata().ino,
            nested.metadata().ino,
            "the class entry and the nested one are one directory"
        );
        assert_eq!(beside.metadata().kind, NodeKind::Dir);

        // And the attributes a tool asks a partition for are all reachable
        // through either name: its numbers, its size in 512-byte sectors, which
        // partition it is, and - the one that tells it from a disk - the fact
        // that `partition` is there at all.
        assert_eq!(at(&fs, "/sys/class/block/vda1/dev"), "254:1\n");
        assert_eq!(at(&fs, "/sys/class/block/vda/vda1/dev"), "254:1\n");
        assert_eq!(at(&fs, "/sys/class/block/vda1/partition"), "1\n");
        assert_eq!(at(&fs, "/sys/class/block/vda2/partition"), "2\n");
        // `size` is in 512-byte sectors, which is sysfs's unit and the one the
        // numbering carries - so it is the fixture's `20 MiB less one KiB`
        // exactly, not a rounded or byte-scaled version of it.
        assert_eq!(at(&fs, "/sys/class/block/vda2/size"), "20970496\n");
        assert!(
            resolve(&fs, "/sys/class/block/vda")
                .lookup("partition")
                .is_err(),
            "a whole disk is not a partition: the attribute is absent, and its              absence is what says so"
        );

        // `/sys/block` holds the disk and only the disk - a partition is not
        // something the machine has storage on.
        let storage = resolve(&fs, "/sys/block").read_dir().expect("a listing");
        assert_eq!(
            storage
                .iter()
                .map(|(name, _)| name.as_str())
                .collect::<Vec<_>>(),
            ["vda"],
            "the whole disks, and no partitions among them"
        );
        assert_eq!(at(&fs, "/sys/block/vda/dev"), "254:0\n");
        assert_eq!(at(&fs, "/sys/block/vda/vda1/dev"), "254:1\n");

        // And `/sys/dev/block/<maj>:<min>` reaches the same place, which is how
        // a tool that holds a device number gets to the device's directory.
        assert_eq!(at(&fs, "/sys/dev/block/254:2/dev"), "254:2\n");
        assert_eq!(at(&fs, "/sys/dev/block/254:2/partition"), "2\n");
    }

    /// `/sys/bus/usb/devices` exists and is empty.
    ///
    /// The counterfactual is the failure this fixed: busybox's `lsusb` opens
    /// this one path, and with it absent it printed
    /// `/sys/bus/usb/devices: No such file or directory` and exited 1. With it
    /// present and empty it prints nothing and exits 0 - which is what `lsusb`
    /// does on a Linux machine with no host controller, because the bus is
    /// registered whether or not anything is on it. A test that asserted the
    /// directory *has* something in it would be asserting a USB driver this
    /// kernel does not have.
    #[test]
    fn the_usb_bus_is_present_and_empty() {
        let fs = fs();
        let devices = resolve(&fs, "/sys/bus/usb/devices");
        assert_eq!(devices.metadata().kind, NodeKind::Dir);
        assert!(
            devices.read_dir().expect("a listing").is_empty(),
            "nothing is plugged into a bus this machine does not have"
        );
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

    /// A second disk is numbered under *its own* name, major and tree position.
    ///
    /// This is what the numbering being a list of the machine's disks buys, and
    /// the counterfactual is exact: the devpath used to be derived from the
    /// name - `/devices/virtio/block/<name>` in three files - so this fixture's
    /// SATA disk would have been filed under the virtio tree, and a device event
    /// would have pointed a listener at a bus the disk is not on.
    #[test]
    fn a_second_disk_is_numbered_under_its_own_name_and_tree() {
        let _bus = crate::uevent::test_bus::take();
        let fs = SysFs::with_devices(Numbering {
            devices: alloc::vec![
                BlockEntry {
                    name: String::from("vda"),
                    major: 254,
                    minor: 0,
                    whole: true,
                    devpath: "/devices/virtio/block",
                    sectors: 20_971_520,
                },
                BlockEntry {
                    name: String::from("sda"),
                    major: 8,
                    minor: 0,
                    whole: true,
                    devpath: "/devices/ahci/block",
                    sectors: 4_194_304,
                },
            ],
        });
        let block = block_dir(&fs);
        let sda = block.lookup("sda").expect("the SATA disk is in the tree");

        // The numbers, which is how userland tells the two disks apart: a
        // virtio disk is major 254 and a SATA one is major 8.
        let mut buf = [0u8; 16];
        let n = sda.lookup("dev").unwrap().read_at(0, &mut buf).unwrap();
        assert_eq!(core::str::from_utf8(&buf[..n]).unwrap().trim(), "8:0");

        // And the tree position, which is the part a name cannot say.
        let watcher = crate::uevent::open();
        crate::uevent::bind(watcher, 0, 1, 0).expect("a fresh socket binds");
        assert_eq!(
            sda.lookup("uevent").unwrap().write_at(0, b"add").unwrap(),
            3
        );
        let event = crate::uevent::recv(watcher).expect("the event").bytes;
        let text = String::from_utf8(event.to_vec()).unwrap();
        assert!(text.contains("DEVPATH=/devices/ahci/block/sda"), "{text}");
        assert!(text.contains("DEVNAME=sda"), "{text}");
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
                devices: alloc::vec![BlockEntry {
                    name: String::from("vda"),
                    major: 254,
                    minor: 0,
                    whole: true,
                    devpath: "/devices/virtio/block",
                    sectors: 20 * 1024 * 1024,
                }],
            },
            alloc::vec![disk, bridge, behind],
        )
    }

    /// A machine with a disk, a *display* function, and the firmware's own
    /// display adapter beside it - the shape a DRM card hangs from, and one
    /// `pci_fs` deliberately does not have: display functions there would
    /// change the device list that fixture's own test asserts, so the two
    /// machines stay separate.
    ///
    /// The two display functions are both class `0x03` and the VGA comes
    /// **first**, which is the whole point: this is a q35 machine, and the
    /// bug this fixture exists for was hanging the card off the VGA because
    /// it matched the class first.
    fn gpu_fs() -> Arc<SysFs> {
        let mut vga = a_function(0, 1, 0);
        vga.vendor = 0x1234;
        vga.device = 0x1111;
        vga.class = 0x03;
        vga.subclass = 0x00;
        vga.prog_if = 0x00;
        // No driver: this kernel has none for it, which is what tells the two
        // apart.

        let mut gpu = a_function(0, 2, 0);
        gpu.vendor = 0x1af4;
        gpu.device = 0x1050;
        gpu.class = 0x03;
        gpu.subclass = 0x00;
        gpu.prog_if = 0x00;
        gpu.driver = Some("virtio-pci");

        SysFs::with(
            Numbering {
                devices: alloc::vec![BlockEntry {
                    name: String::from("vda"),
                    major: 254,
                    minor: 0,
                    whole: true,
                    devpath: "/devices/virtio/block",
                    sectors: 20 * 1024 * 1024,
                }],
            },
            alloc::vec![vga, gpu],
        )
    }

    #[test]
    fn a_card_hangs_off_the_function_that_drives_it() {
        // What libdrm's device parse reads, measured on a working card and
        // recorded in `PLAN.md` §7.4: given the descriptor it holds, `vendor`
        // and `device` off the node's *parent*, the `subsystem` link resolved
        // to learn which bus it is looking at, the parent's `drm/` listing,
        // and the parent's `uevent`. This tree carried all of it except the
        // parent: `device` landed on `virtual/virtio-gpu`, a directory with one
        // child and no attributes at all, and a machine with 3D behind the card
        // still reported a software renderer because of it.
        // Built before the card's own tree, because the helper and the local
        // cannot both be called `fs`.
        let bare = fs();
        let fs = gpu_fs();

        for number in ["226:0", "226:128"] {
            let parent = resolve(&fs, &alloc::format!("/sys/dev/char/{number}/device"));
            assert_eq!(parent.metadata().kind, NodeKind::Dir, "a parent to ask");

            // The two attributes it opens, as the function spells them.
            assert_eq!(text(&parent.lookup("vendor").expect("vendor")), "0x1af4\n");
            assert_eq!(text(&parent.lookup("device").expect("device")), "0x1050\n");

            // And that it really is the function: the keys `lspci` reads are
            // the proof that this is not a directory that merely answers the
            // two names.
            let uevent = text(&parent.lookup("uevent").expect("uevent"));
            assert!(uevent.contains("PCI_ID=1AF4:1050"), "{uevent}");

            // The link it resolves before it parses anything else. A device
            // whose bus cannot be named is a device it refuses, which is what
            // happens when the parent is a `virtual` directory.
            assert_eq!(
                resolve(
                    &fs,
                    &alloc::format!("/sys/dev/char/{number}/device/subsystem")
                )
                .metadata()
                .kind,
                NodeKind::Dir,
                "the subsystem link resolves to the bus"
            );

            // And the listing that makes the two nodes siblings, which the
            // DRM crate reads to hand a compositor its render node. The
            // entries are *resolved*, not merely listed: the listing hangs
            // under the parent, so its links have one more level to climb than
            // the node's own, and a listing whose entries dangle is worse than
            // an empty one - a reader walking `/sys` (udev, at coldplug)
            // follows them into nothing, and the boot stalls on it. Names
            // alone passed a version of this test that shipped exactly that.
            for (name, expected) in [("card0", "226:0"), ("renderD128", "226:128")] {
                let node = resolve(
                    &fs,
                    &alloc::format!("/sys/dev/char/{number}/device/drm/{name}"),
                );
                assert_eq!(node.metadata().kind, NodeKind::Dir, "{name} resolves");
                assert_eq!(
                    text(&node.lookup("dev").expect("dev")),
                    alloc::format!("{expected}\n"),
                    "{name} is the node it names"
                );
            }
        }

        // A machine with no display function keeps the older shape rather than
        // pointing at nothing - there is no bus to name, and the card still has
        // to be a device. Its listing resolves too, with a different number of
        // levels to climb than the one above: the fallback parent is a name
        // under `virtual`, the function is a name under `pci0000:00`, and both
        // are two deep.
        assert_eq!(
            resolve(&bare, "/sys/dev/char/226:0/device").metadata().kind,
            NodeKind::Dir
        );
        let fallback = resolve(&bare, "/sys/dev/char/226:0/device/drm/renderD128");
        assert_eq!(fallback.metadata().kind, NodeKind::Dir);
        assert_eq!(
            text(&fallback.lookup("dev").expect("dev")),
            String::from("226:128\n")
        );
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

    /// The contents of the file at a `/sys` path, with any link on the way
    /// resolved - which is how every reader of this tree reads an attribute.
    fn at(fs: &Arc<SysFs>, path: &str) -> String {
        text(&resolve(fs, path))
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
