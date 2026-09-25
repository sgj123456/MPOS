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
            self.entries
                .borrow(cs)
                .borrow_mut()
                .insert(String::from(name), node.clone());
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
        });
        Ok(buf.len())
    }
}

/// The filesystem itself: the device tree, built once.
pub struct SysFs {
    root: Arc<SysDir>,
}

impl SysFs {
    /// The tree for the machine as it is: the boot disk and its partitions.
    pub fn new() -> Arc<SysFs> {
        SysFs::with_devices(Numbering::of_boot_disk())
    }

    /// The tree for a given set of devices. The host has no disk, and the
    /// walk a listener does is worth testing without one.
    pub fn with_devices(numbering: Numbering) -> Arc<SysFs> {
        let fs = Arc::new(SysFs {
            root: SysDir::new(0),
        });

        // `/bus` and `/class` are the two places a listener walks to find the
        // devices that are already there. Only `/class` has anything in it
        // here: this kernel publishes no buses, and inventing one would be a
        // directory that lies about the machine.
        let _bus = fs.root.add_dir("bus", 1);
        let class = fs.root.add_dir("class", 2);
        let block = class.add_dir("block", 3);

        let mut next = 4;
        for (name, major, minor, _whole) in numbering.devices.iter() {
            let dir = block.add_dir(name, next);
            next += 1;
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
                    },
                }),
            );
            next += 1;
        }

        // `/class/input`: the evdev nodes, each carrying the same `dev`
        // number `/dev/input/eventN` is announced with. This is how udev and
        // `libinput` enumerate the keyboard and the pointer without scanning
        // `/dev` by hand - they read this tree, open the node the number
        // names, then interrogate it through the evdev ioctls. The input
        // driver only exists on the kernel target, so this half of the tree
        // is built there alone.
        #[cfg(target_os = "none")]
        {
            let input = class.add_dir("input", next);
            next += 1;
            for index in 0..crate::drivers::virtio::input::nodes().len() {
                let name = alloc::format!("event{index}");
                let major = crate::drivers::virtio::input::INPUT_MAJOR;
                let minor = crate::drivers::virtio::input::input_minor(index);
                let dir = input.add_dir(&name, next);
                next += 1;
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
                            devpath: alloc::format!("/devices/virtual/input/input{index}"),
                            subsystem: "input",
                            devname: alloc::format!("input/{}", name),
                            major,
                            minor,
                        },
                    }),
                );
                next += 1;
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
        #[cfg(target_os = "none")]
        {
            let drm = class.add_dir("drm", next);
            next += 1;
            for (name, minor, devnode) in [
                ("card0", 0u32, "dri/card0"),
                ("renderD128", 128, "dri/renderD128"),
            ] {
                let dir = drm.add_dir(name, next);
                next += 1;
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
                        },
                    }),
                );
                next += 1;
            }
        }

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
        crate::uevent::bind(watcher, 1);
        let block = block_dir(&fs);
        let (name, _) = block.read_dir().unwrap().into_iter().next().unwrap();
        let uevent = block.lookup(&name).unwrap().lookup("uevent").unwrap();
        assert_eq!(uevent.write_at(0, b"add").unwrap(), 3);

        let event = crate::uevent::recv(watcher).expect("the re-announcement");
        let text = String::from_utf8(event).unwrap();
        assert!(text.starts_with("add@"));
        assert!(text.contains(&alloc::format!("DEVNAME={}", name)));

        // An action the kernel does not speak is refused, and nothing goes out.
        assert!(uevent.write_at(0, b"frobnicate").is_err());
        assert!(!crate::uevent::ready(watcher));
        crate::uevent::close(watcher);
    }
}
