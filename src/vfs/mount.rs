//! Mounts and path resolution.
//!
//! Resolution walks the tree one component at a time, keeping the logical
//! chain from the root in a stack. The stack is what makes `..` correct across
//! mounts: entering `/mnt` (a mountpoint) pushes the mounted filesystem's
//! root, so `..` from inside that filesystem pops straight back to the
//! mountpoint's parent without any special case - the entry that would have
//! needed a cross-filesystem parent pointer is simply not on the stack.
//!
//! The table is a plain instance, not a global, so tests can build isolated
//! trees; the kernel's own table lives in [`STATIC`] behind the free functions.

use alloc::collections::VecDeque;
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::cell::RefCell;

use critical_section::Mutex;

use super::path;
use super::{FileSystem, Inode, NodeKind, VfsError, VfsResult, resolve_opt};

/// How many links one resolution may follow before it is called a cycle.
/// Linux's own limit is 40.
const SYMLINK_HOPS: usize = 40;

/// One mount as `/proc/mounts` and `/proc/self/mountinfo` print it: the mount
/// point's node, the source and filesystem name `mount(2)` was given, and the
/// two flags that decide how the line reads.
type MountRow = (
    Arc<dyn Inode>,
    alloc::string::String,
    alloc::string::String,
    bool,
    bool,
);

/// One active mount: `at` is the directory inode in the parent filesystem,
/// `root` is the root of the mounted one.
struct Mount {
    at: Arc<dyn Inode>,
    root: Arc<dyn Inode>,
    /// The name `mount(2)` was given for this filesystem, and the source it
    /// named - "/dev/vda3" for an ext2 partition, "proc" for procfs. Both are
    /// what /proc/mounts reports, and /proc/mounts is how userland finds out
    /// what is mounted where, which is the only record there is.
    fstype: alloc::string::String,
    source: alloc::string::String,
    /// Which filesystem instance this is: a number distinct per mounted
    /// instance, minted in mounting order. It is this table's own identity for
    /// a mount - two names for one filesystem share it (a bind borrows it) and
    /// two attachments of one device do not - and it is what `EXDEV` and
    /// `switch_root`'s comparison are decided by.
    ///
    /// It is *not* what `stat`'s `st_dev` reports. See [`Mount::dev`].
    id: u64,
    /// What `stat`'s `st_dev` reports for every path this filesystem serves.
    ///
    /// Linux's answer, and not the same thing as `id`: for a filesystem with
    /// storage behind it this is that *device's* number (`254:2` for a root
    /// mounted from `/dev/vda2`), and for a pseudo-filesystem - procfs, tmpfs,
    /// a ramfs root - it is an anonymous number, which is what Linux hands
    /// those too. The two questions a caller asks of the field are both
    /// answered by it: "is this the same filesystem as that?" (compare two
    /// paths' `st_dev`) and "which device is this on?" (match it against the
    /// numbers `/sys/class/block` publishes) - and the second is the one that
    /// only a device's own number can answer. `lsblk`'s `MOUNTPOINTS` column
    /// and `df`'s `Filesystem` column are that match, and both are blank
    /// without it.
    dev: u64,
    /// Whether `mount -o ro` was asked for. The flag is reported in
    /// /proc/mounts and passed on to the filesystem, which enforces it.
    read_only: bool,
    /// The filesystem itself, when the caller had one to give. `remount`
    /// needs it: changing the read-only state is a request to the filesystem,
    /// not a note in this table.
    fs: Option<Arc<dyn FileSystem>>,
    /// Whether this record is a bind alias rather than an attached
    /// filesystem: `root` is a node that another record - or the root slot -
    /// already serves, and the `id`, `fs` and `fstype` below are *borrowed*
    /// from that serving instance so one filesystem keeps one identity under
    /// both of its names.
    bind: bool,
}

/// A set of mounts with a root, and the resolver over them.
pub struct Vfs {
    root: Mutex<RefCell<Option<Arc<dyn Inode>>>>,
    mounts: Mutex<RefCell<Vec<Mount>>>,
    /// What `/` is, for /proc/mounts. The root is not in the mount table - it
    /// is reached directly - so its own record lives here, filled in by
    /// `pivot_root` from the mount that carried the new root in.
    root_fstype: Mutex<RefCell<Option<alloc::string::String>>>,
    root_source: Mutex<RefCell<Option<alloc::string::String>>>,
    /// The filesystem that *is* `/`, once `pivot_root` has promoted one, and
    /// its read-only promise. `mount -o remount,rw /` names the root, which is
    /// not in the mount table to find - these two fields are where that call
    /// lands - and `/proc/mounts` reports the promise, which a hardcoded "rw"
    /// turned into a lie the moment an init mounted the root read-only.
    root_fs: Mutex<RefCell<Option<Arc<dyn FileSystem>>>>,
    root_read_only: Mutex<RefCell<bool>>,
    /// What `stat`'s `st_dev` reports for paths on the root filesystem, on the
    /// same terms as [`Mount::dev`]: the device behind it when it was mounted
    /// from one, else an anonymous number. It lives outside the table for the
    /// same reason `root_fstype` does - the root is reached directly and is not
    /// an entry to look itself up in.
    root_dev: Mutex<RefCell<u64>>,
    /// The source of filesystem instance ids. Ids start at 1: 0 is the root's.
    fs_seq: Mutex<RefCell<u64>>,
    /// The source of *anonymous device numbers*, for a filesystem with no
    /// device behind it. A separate counter from `fs_seq` because the two are
    /// different facts about a mount - an attachment order and a reported
    /// number - and one mount may have the first without the second being
    /// expressible by it.
    anon_seq: Mutex<RefCell<u64>>,
    /// Mountpoint path -> which filesystem serves it, refreshed whenever the
    /// table changes. `stat` answers per path, and recomputing the mountpoint
    /// paths on every stat would be a tree walk per call.
    mount_points: Mutex<RefCell<Vec<MountPoint>>>,
}

/// One mountpoint, as the readers that answer by path need it.
#[derive(Clone)]
struct MountPoint {
    path: alloc::string::String,
    /// The instance's identity, for `EXDEV`-style comparisons.
    id: u64,
    /// The number that instance reports as `st_dev`.
    dev: u64,
    fstype: alloc::string::String,
}

impl Vfs {
    pub const fn new() -> Vfs {
        Vfs {
            root: Mutex::new(RefCell::new(None)),
            mounts: Mutex::new(RefCell::new(Vec::new())),
            root_fstype: Mutex::new(RefCell::new(None)),
            root_source: Mutex::new(RefCell::new(None)),
            root_fs: Mutex::new(RefCell::new(None)),
            root_read_only: Mutex::new(RefCell::new(false)),
            root_dev: Mutex::new(RefCell::new(0)),
            fs_seq: Mutex::new(RefCell::new(1)),
            anon_seq: Mutex::new(RefCell::new(1)),
            mount_points: Mutex::new(RefCell::new(Vec::new())),
        }
    }

    /// Install the root filesystem. Once, at boot: a second call is a bug.
    pub fn mount_root(&self, fs: &dyn FileSystem) {
        // Drawn before the lock is taken: it is its own counter, and a number
        // minted under this section would be one more thing nested inside it.
        let dev = self.next_anon_dev();
        critical_section::with(|cs| {
            let mut root = self.root.borrow(cs).borrow_mut();
            assert!(root.is_none(), "the root filesystem is already mounted");
            *root = Some(fs.root());
            // The root's own type has to be on record from the start: init's
            // `statfs("/")` reads it, and busybox `switch_root` refuses to run
            // unless the tree it stands in answers ramfs/tmpfs magic.
            *self.root_fstype.borrow(cs).borrow_mut() =
                Some(alloc::string::String::from(fs.name()));
            *self.root_dev.borrow(cs).borrow_mut() = dev;
        });
    }

    /// The root inode, for the one caller that needs to start a walk from
    /// "the beginning" without a path: a task whose cwd was never set.
    pub fn root_inode(&self) -> Arc<dyn Inode> {
        critical_section::with(|cs| self.root.borrow(cs).borrow().clone())
            .expect("no root filesystem is mounted")
    }

    /// The next anonymous device number, for a filesystem with no device
    /// behind it.
    ///
    /// Linux hands a pseudo-filesystem one of these - major 0, a minor counted
    /// up from 1 - and reports it as `st_dev` exactly as it reports a disk's
    /// number. The value being *real* matters more than its being meaningful: a
    /// caller comparing two paths' `st_dev` to ask "one filesystem or two"
    /// reads the same field whichever kind of filesystem it is, and 0 is not a
    /// candidate answer, because a `dev_t` of zero means "no device" and two of
    /// those compare equal whatever serves them.
    ///
    /// No device number this machine has can collide with one: a major of 0 is
    /// not a block driver's, and virtio's disks are numbered under 254 and SATA's
    /// under 8.
    fn next_anon_dev(&self) -> u64 {
        critical_section::with(|cs| {
            let seq = self.anon_seq.borrow(cs);
            let mut seq = seq.borrow_mut();
            let minor = *seq;
            *seq += 1;
            crate::uevent::makedev(0, minor as u32)
        })
    }

    /// Mount `fs` at the directory named by `path`.
    ///
    /// The mountpoint is found through its parent, not by resolving `path`
    /// outright: resolving would walk through whatever is already mounted
    /// there and lose the directory that is supposed to be mounted on.
    pub fn mount(&self, path: &str, fs: &dyn FileSystem) -> VfsResult<()> {
        if critical_section::with(|cs| self.root.borrow(cs).borrow().is_none()) {
            return Err(VfsError::NoRoot);
        }
        let trimmed = path.trim_end_matches('/');
        let (parent_path, name) = match trimmed.rfind('/') {
            Some(i) => (&path[..=i], &trimmed[i + 1..]),
            // `/` itself: always mounted, and roots do not stack here yet.
            None if trimmed.is_empty() => return Err(VfsError::Busy),
            // Relative with no cwd to resolve it against.
            None => return Err(VfsError::Invalid),
        };
        if name.is_empty() || name == "." || name == ".." {
            return Err(VfsError::Invalid);
        }

        let parent = self.resolve(parent_path)?;
        let at = parent.lookup(name)?;
        if at.metadata().kind != NodeKind::Dir {
            return Err(VfsError::NotDir);
        }
        critical_section::with(|cs| {
            let mut mounts = self.mounts.borrow(cs).borrow_mut();
            if mounts.iter().any(|m| Arc::ptr_eq(&m.at, &at)) {
                return Err(VfsError::Busy);
            }
            let id = self.next_fs_id();
            let dev = self.next_anon_dev();
            mounts.push(Mount {
                at,
                root: fs.root(),
                fstype: alloc::string::String::from(fs.name()),
                source: alloc::string::String::from("none"),
                id,
                dev,
                read_only: false,
                fs: None,
                bind: false,
            });
            Ok(())
        })?;
        self.refresh_mount_points();
        Ok(())
    }

    /// The next filesystem instance id. Ids start at 1: 0 is the root's.
    fn next_fs_id(&self) -> u64 {
        critical_section::with(|cs| {
            let seq = self.fs_seq.borrow(cs);
            let mut seq = seq.borrow_mut();
            let id = *seq;
            *seq += 1;
            id
        })
    }

    /// Recompute the mountpoint table: each mount's point named from the root,
    /// plus the root itself as entry 0. `stat`'s `st_dev`, `statfs`'s type and
    /// the `st_dev`-by-path comparisons all come from here, by longest-prefix
    /// match on the path.
    fn refresh_mount_points(&self) {
        // The table is copied out first: naming each mount point walks the
        // tree with `path_of`, and that walk takes the same lock.
        let (root_fstype, root_dev, entries) = critical_section::with(|cs| {
            let fstype = self.root_fstype.borrow(cs).borrow().clone();
            let dev = *self.root_dev.borrow(cs).borrow();
            let entries: Vec<(Arc<dyn Inode>, u64, u64, alloc::string::String)> = self
                .mounts
                .borrow(cs)
                .borrow()
                .iter()
                .map(|m| (m.at.clone(), m.id, m.dev, m.fstype.clone()))
                .collect();
            (fstype, dev, entries)
        });
        let mut points: Vec<MountPoint> = Vec::new();
        if let Some(fstype) = root_fstype {
            points.push(MountPoint {
                path: alloc::string::String::from("/"),
                id: 0,
                dev: root_dev,
                fstype,
            });
        }
        for (at, id, dev, fstype) in entries {
            if let Some(path) = self.path_of(&at) {
                points.push(MountPoint {
                    path,
                    id,
                    dev,
                    fstype,
                });
            }
        }
        critical_section::with(|cs| {
            *self.mount_points.borrow(cs).borrow_mut() = points;
        });
    }

    /// Which filesystem serves `path`, by the deepest mountpoint that prefixes
    /// it. The root slot is the answer when nothing else matches, which is what
    /// makes one function the whole of "what serves this path".
    fn serving_point(&self, path: &str) -> MountPoint {
        let trimmed = super::path::normalize(path);
        let points = critical_section::with(|cs| self.mount_points.borrow(cs).borrow().clone());
        let mut best: Option<MountPoint> = None;
        for point in points.iter() {
            let mp = point.path.as_str();
            let inside = if mp == "/" {
                true
            } else {
                trimmed == mp || (trimmed.starts_with(mp) && trimmed[mp.len()..].starts_with('/'))
            };
            if inside
                && best
                    .as_ref()
                    .is_none_or(|b| point.path.len() > b.path.len())
            {
                best = Some(point.clone());
            }
        }
        best.unwrap_or_else(|| MountPoint {
            path: alloc::string::String::from("/"),
            id: 0,
            dev: critical_section::with(|cs| *self.root_dev.borrow(cs).borrow()),
            fstype: alloc::string::String::from("unknown"),
        })
    }

    /// Which filesystem *instance* serves `path`, and its type. The instance is
    /// this table's identity for a mount, which is what `EXDEV` and
    /// `rename`'s "same filesystem?" test ask about; what `stat` reports for
    /// the path is [`Vfs::dev_of_path`], and for a filesystem with a device
    /// behind it the two are deliberately different numbers.
    pub fn fs_of_path(&self, path: &str) -> (u64, alloc::string::String) {
        let point = self.serving_point(path);
        (point.id, point.fstype)
    }

    /// What `stat`'s `st_dev` reports for `path`: the number of the device
    /// behind the filesystem that serves it, or an anonymous one for a
    /// pseudo-filesystem.
    ///
    /// This is the function `stat` answers with, and the one a mount record's
    /// `maj:min` column has to agree with - a tool that finds a mount's device
    /// by `stat`ing its mountpoint and then looks that number up in
    /// `/proc/self/mountinfo` reads the same value from both only because both
    /// come from here.
    pub fn dev_of_path(&self, path: &str) -> u64 {
        self.serving_point(path).dev
    }

    /// What `stat`'s `st_dev` reports for a descriptor's inode, which has no
    /// path to look the answer up by: a mount's own root names that mount;
    /// anything else is served by the root filesystem.
    pub fn dev_of_inode(&self, inode: &Arc<dyn Inode>) -> u64 {
        critical_section::with(|cs| {
            self.mounts
                .borrow(cs)
                .borrow()
                .iter()
                .find(|m| Arc::ptr_eq(&m.root, inode))
                .map(|m| m.dev)
                .unwrap_or_else(|| *self.root_dev.borrow(cs).borrow())
        })
    }

    /// The filesystem instance that serves `path` - the same longest-prefix
    /// winner `fs_of_path` names, but the instance itself, so `statfs(2)` can
    /// ask it for real block numbers. The root slot (id 0) serves from
    /// `root_fs`; a table entry serves from its own `fs`.
    pub fn fs_serving_path(&self, path: &str) -> Option<alloc::sync::Arc<dyn super::FileSystem>> {
        let (id, _) = self.fs_of_path(path);
        critical_section::with(|cs| {
            if id == 0 {
                self.root_fs.borrow(cs).borrow().clone()
            } else {
                self.mounts
                    .borrow(cs)
                    .borrow()
                    .iter()
                    .find(|m| m.id == id)
                    .and_then(|m| m.fs.clone())
            }
        })
    }

    /// The filesystem instance that serves `inode`: a mount's own root names
    /// that mount; anything else is served by the root filesystem - the same
    /// rule `fstype_of_inode` spells by type.
    pub fn fs_serving_inode(
        &self,
        inode: &alloc::sync::Arc<dyn super::Inode>,
    ) -> Option<alloc::sync::Arc<dyn super::FileSystem>> {
        critical_section::with(|cs| {
            self.mounts
                .borrow(cs)
                .borrow()
                .iter()
                .find(|m| alloc::sync::Arc::ptr_eq(&m.root, inode))
                .and_then(|m| m.fs.clone())
                .or_else(|| self.root_fs.borrow(cs).borrow().clone())
        })
    }

    /// Make `inode` the global root, dropping every mount. `chroot(2)`'s
    /// shape: the caller has already decided what the root is, and this
    /// kernel has one namespace.
    pub fn set_global_root(&self, inode: Arc<dyn Inode>) {
        critical_section::with(|cs| {
            *self.root.borrow(cs).borrow_mut() = Some(inode);
        });
        self.refresh_mount_points();
    }

    /// `mount(2)`'s MS_MOVE: the filesystem mounted at `at` becomes the root.
    /// The mount entry moves out of the table and into the root slot.
    ///
    /// The mounts that survive are the ones whose mount points live *inside*
    /// the promoted tree - an initramfs has already moved `/proc`, `/sys` and
    /// `/dev` onto directories under the new root before `switch_root` runs,
    /// and those names are exactly what the new root's processes expect to
    /// find. The rest are anchored in the old tree, which is about to be
    /// unmounted out from under them; keeping them would leave entries
    /// pointing at nodes nothing resolves any more.
    pub fn move_root(&self, at: &Arc<dyn Inode>) -> VfsResult<()> {
        // Named outside the table lock: `path_of` walks the tree and takes
        // the same lock this edit needs, so every mount point is named first
        // and judged afterwards.
        let promoted = self.path_of(at);
        let named: alloc::vec::Vec<(Arc<dyn Inode>, Option<alloc::string::String>)> =
            critical_section::with(|cs| {
                self.mounts
                    .borrow(cs)
                    .borrow()
                    .iter()
                    .map(|m| (m.at.clone(), self.path_of(&m.at)))
                    .collect()
            });
        let follows = |path: &Option<alloc::string::String>| match (&promoted, path) {
            (Some(base), Some(p)) => {
                p == base.as_str() || p.starts_with(&alloc::format!("{base}/"))
            }
            // A mount point the current tree cannot name - or a promotion of
            // a node with no name - stays behind with the old tree.
            _ => false,
        };
        critical_section::with(|cs| {
            let mut mounts = self.mounts.borrow(cs).borrow_mut();
            let pos = mounts
                .iter()
                .position(|m| Arc::ptr_eq(&m.at, at) || Arc::ptr_eq(&m.root, at))
                .ok_or(VfsError::Invalid)?;
            let m = mounts.remove(pos);
            *self.root.borrow(cs).borrow_mut() = Some(m.root.clone());
            *self.root_fstype.borrow(cs).borrow_mut() = Some(m.fstype.clone());
            *self.root_source.borrow(cs).borrow_mut() = Some(m.source.clone());
            // The promoted filesystem keeps its own promise about writes: an
            // init that mounted its root read-only hands a read-only root on,
            // and the `root` service's later remount,rw is what lifts it.
            *self.root_fs.borrow(cs).borrow_mut() = m.fs.clone();
            *self.root_read_only.borrow(cs).borrow_mut() = m.read_only;
            // And its device number travels with it for the same reason: `/` is
            // the ext4 partition the init mounted, so this is where the root's
            // `st_dev` stops being an anonymous number and becomes `254:2`.
            *self.root_dev.borrow(cs).borrow_mut() = m.dev;
            mounts.retain(|e| {
                named
                    .iter()
                    .find(|(node, _)| Arc::ptr_eq(node, &e.at))
                    .is_some_and(|(_, path)| follows(path))
            });
            Ok(())
        })?;
        self.refresh_mount_points();
        Ok(())
    }

    /// `mount(2)`'s MS_MOVE in its ordinary form: the filesystem mounted at
    /// `source` is re-homed on the directory `at`, and every other mount stays
    /// exactly where it was. This is what an initramfs does with the
    /// directories it mounted before finding the real root - `/proc`, `/sys`
    /// and `/dev` move under it rather than being mounted there a second
    /// time, which is both impossible (the names are taken) and wrong (the
    /// new root would see empty directories where its processes expect the
    /// devices and the process tables).
    ///
    /// `source` is named the way a caller names a mounted directory - a path
    /// that resolves *into* the mount - so the entry is found by the root it
    /// serves, the way `umount` finds its.
    pub fn move_mount(&self, source: &Arc<dyn Inode>, at: &Arc<dyn Inode>) -> VfsResult<()> {
        if at.metadata().kind != NodeKind::Dir {
            return Err(VfsError::NotDir);
        }
        critical_section::with(|cs| {
            let mut mounts = self.mounts.borrow(cs).borrow_mut();
            if mounts.iter().any(|m| Arc::ptr_eq(&m.at, at)) {
                return Err(VfsError::Busy);
            }
            let entry = mounts
                .iter_mut()
                .find(|m| Arc::ptr_eq(&m.root, source))
                .ok_or(VfsError::Invalid)?;
            entry.at = at.clone();
            Ok(())
        })?;
        self.refresh_mount_points();
        Ok(())
    }

    /// Mount `fs` on an already-resolved directory, as `mount(2)` does: the
    /// syscall has the mountpoint in its hand and the filesystem type and
    /// source as the caller spelled them, and both go into the record that
    /// /proc/mounts later reports.
    ///
    /// `dev` is what `stat` will report as `st_dev` for everything under this
    /// mount: the number of the device the source resolved to, when there was
    /// one, and `None` for a filesystem with no storage behind it, which takes
    /// an anonymous number. It is a *parameter* rather than something looked up
    /// here because resolving a source to a device is the syscall layer's job -
    /// it is the layer that had the source as a path and walked it to a
    /// [`crate::blk::BlockDev`] in the first place.
    pub fn mount_at(
        &self,
        at: Arc<dyn Inode>,
        fs: Arc<dyn FileSystem>,
        fstype: &str,
        source: &str,
        read_only: bool,
        dev: Option<u64>,
    ) -> VfsResult<()> {
        if at.metadata().kind != NodeKind::Dir {
            return Err(VfsError::NotDir);
        }
        // The filesystem is told first: a mount that records "read-only" while
        // the filesystem goes on accepting writes would be a promise nobody
        // keeps.
        fs.set_read_only(read_only);
        critical_section::with(|cs| {
            let mut mounts = self.mounts.borrow(cs).borrow_mut();
            if mounts.iter().any(|m| Arc::ptr_eq(&m.at, &at)) {
                return Err(VfsError::Busy);
            }
            let id = self.next_fs_id();
            let dev = match dev {
                Some(dev) => dev,
                None => self.next_anon_dev(),
            };
            mounts.push(Mount {
                at,
                root: fs.root(),
                fstype: alloc::string::String::from(fstype),
                source: alloc::string::String::from(source),
                id,
                dev,
                read_only,
                fs: Some(fs),
                bind: false,
            });
            Ok(())
        })?;
        self.refresh_mount_points();
        Ok(())
    }

    /// `mount --bind`: hang an already-existing directory tree at a second
    /// name, sharing everything about the instance that serves it.
    ///
    /// A bind is not a filesystem; it is a second edge into one. The new
    /// record borrows the serving instance's id, type, device number, filesystem
    /// handle and write promise rather than minting its own, because the point
    /// of the operation is that the two names are the *same* storage: a file
    /// written through one appears under the other, and `stat` answers one
    /// `st_dev` from either. The resolver already swaps a mountpoint child for
    /// its mount's root, and `root` here is simply a node that is not a freshly
    /// attached filesystem's - so the crossing itself needs no new machinery.
    ///
    /// What is not here: binding a *file* (the resolver models directory
    /// crossings only, and no boot caller binds anything else), and `MS_REC`'s
    /// carrying of the subtree's other mounts - the handler refuses that flag
    /// rather than silently binding less than was asked.
    pub fn bind_mount(
        &self,
        at: Arc<dyn Inode>,
        source: Arc<dyn Inode>,
        source_path: &str,
        read_only: bool,
    ) -> VfsResult<()> {
        if at.metadata().kind != NodeKind::Dir {
            return Err(VfsError::NotDir);
        }
        if source.metadata().kind != NodeKind::Dir {
            return Err(VfsError::Invalid);
        }
        // What serves the source is what serves the bind: the same questions
        // `stat`'s `st_dev` and `statfs` already answer by path, asked of the
        // name the caller gave.
        let (id, fstype) = self.fs_of_path(source_path);
        // The device number is borrowed with the identity, and it is the reason
        // a bind is not a filesystem of its own: `/mnt/root` and `/` are one
        // storage mounted twice, so they answer one `st_dev` - which is exactly
        // how a tool that lists mounts tells a second name from a second device.
        let dev = self.dev_of_path(source_path);
        let fs = self.fs_serving_path(source_path);
        let serving_read_only = critical_section::with(|cs| {
            if id == 0 {
                *self.root_read_only.borrow(cs).borrow()
            } else {
                self.mounts
                    .borrow(cs)
                    .borrow()
                    .iter()
                    .rev()
                    .find(|m| m.id == id)
                    .is_some_and(|m| m.read_only)
            }
        });
        critical_section::with(|cs| {
            let mut mounts = self.mounts.borrow(cs).borrow_mut();
            if mounts.iter().any(|m| Arc::ptr_eq(&m.at, &at)) {
                return Err(VfsError::Busy);
            }
            mounts.push(Mount {
                at,
                root: source,
                fstype,
                source: alloc::string::String::from(source_path),
                id,
                dev,
                // A bind's own `MS_RDONLY` is a promise about writes *through
                // this name*; it does not reach the shared filesystem, which
                // the other name may still write through. Nothing in this
                // boot asks; the distinction is recorded so that when it does,
                // the record tells which one it was.
                read_only: serving_read_only || read_only,
                fs,
                bind: true,
            });
            Ok(())
        })?;
        self.refresh_mount_points();
        Ok(())
    }

    /// `mount -o remount,...`: change the flags on a filesystem that is
    /// already mounted, without mounting anything.
    pub fn remount(&self, at: &Arc<dyn Inode>, read_only: bool) -> VfsResult<()> {
        // The root slot first. The filesystem that *is* `/` never entered the
        // mount table - it is reached directly - and `mount -o remount,rw /`
        // is exactly how an init's `root` service asks to have it writable.
        let is_root = critical_section::with(|cs| {
            self.root
                .borrow(cs)
                .borrow()
                .as_ref()
                .is_some_and(|root| Arc::ptr_eq(root, at))
        });
        if is_root {
            let fs = critical_section::with(|cs| self.root_fs.borrow(cs).borrow().clone());
            if let Some(fs) = fs {
                fs.set_read_only(read_only);
            }
            critical_section::with(|cs| {
                *self.root_read_only.borrow(cs).borrow_mut() = read_only;
            });
            return Ok(());
        }
        critical_section::with(|cs| {
            let mut mounts = self.mounts.borrow(cs).borrow_mut();
            let entry = mounts
                .iter_mut()
                // The mountpoint's own inode is what `mount(2)` recorded, but
                // a caller that resolved the path (`remount dev /dev`) holds
                // the mounted *root* - the resolver crossed into the mount.
                // Either name is the same filesystem to remount.
                .find(|m| Arc::ptr_eq(&m.at, at) || Arc::ptr_eq(&m.root, at))
                .ok_or(VfsError::NoEntry)?;
            if let Some(fs) = entry.fs.clone() {
                fs.set_read_only(read_only);
            }
            entry.read_only = read_only;
            Ok(())
        })
    }

    /// The *mountpoint's own* directory for `path`: the parent resolved
    /// through the table as it stands, and the final name looked up in it
    /// **without** the crossing. A caller mounting on a directory that is
    /// already someone's mountpoint must meet that directory, not the tree
    /// currently covering it - resolving all the way would re-aim the mount
    /// into the existing mount's filesystem, and its "is this point taken?"
    /// check at that answer is a question about the wrong inode.
    fn mountpoint_inode(
        &self,
        path: &str,
        start: Option<Arc<dyn Inode>>,
    ) -> VfsResult<Arc<dyn Inode>> {
        let trimmed = path.trim_end_matches('/');
        if trimmed.is_empty() {
            // The target of a bind can never be `/` itself (the kernel has
            // one root slot, and no mountpoint inode names it), but the
            // mountpoint of a path *is* wherever the walk starts for "/".
            return Ok(self.root_inode());
        }
        let (parent_path, name) = split_last(trimmed);
        let parent = match start {
            Some(s) => self.resolve_from(parent_path, s)?,
            None => self.resolve(parent_path)?,
        };
        if parent.metadata().kind != NodeKind::Dir {
            return Err(VfsError::NotDir);
        }
        parent.lookup(name)
    }

    /// Unmount whatever is mounted on `target`, given the mountpoint's own
    /// inode - the shape `umount2(2)` arrives in, having resolved the path
    /// against its caller's cwd.
    pub fn unmount_resolved(&self, target: &Arc<dyn Inode>) -> VfsResult<()> {
        critical_section::with(|cs| {
            let mut mounts = self.mounts.borrow(cs).borrow_mut();
            // The *last* record wins, the way a stack of mounts on one
            // directory peels from the top: a bind shares its source's tree -
            // the source's own mount entry, or the root - and unmounting the
            // bind's name must remove the bind, not the mount underneath it.
            // Bind entries are pushed after what they alias, so newest-first
            // is the right search order.
            if let Some(pos) = mounts.iter().rposition(|m| Arc::ptr_eq(&m.root, target)) {
                mounts.remove(pos);
                Ok(())
            } else {
                Err(VfsError::Busy)
            }
        })?;
        self.refresh_mount_points();
        Ok(())
    }

    /// Unmount the filesystem mounted at `path`.
    ///
    /// The path must name a mountpoint - a directory that has a filesystem
    /// mounted on it. The mount is removed from the mount table, and subsequent
    /// accesses to files under `path` will see the underlying directory in the
    /// parent filesystem instead.
    pub fn unmount(&self, path: &str) -> VfsResult<()> {
        if critical_section::with(|cs| self.root.borrow(cs).borrow().is_none()) {
            return Err(VfsError::NoRoot);
        }
        let trimmed = path.trim_end_matches('/');
        if trimmed.is_empty() || trimmed == "." {
            return Err(VfsError::Invalid);
        }

        // Resolve the path to find the mountpoint's inode. We need to find
        // the mount entry that corresponds to this path.
        let target = self.resolve(trimmed)?;

        critical_section::with(|cs| {
            let mut mounts = self.mounts.borrow(cs).borrow_mut();
            if let Some(pos) = mounts.iter().position(|m| Arc::ptr_eq(&m.root, &target)) {
                mounts.remove(pos);
                Ok(())
            } else {
                Err(VfsError::Busy)
            }
        })
    }

    /// Replace the root filesystem: move `new_root` to `/`, and move the old
    /// root to `put_old` inside the new root, so it can be unmounted afterward.
    ///
    /// `put_old` has to be an existing directory in the new root, as Linux
    /// requires: the point of the operation is that nothing is left in the
    /// air, and a mount point invented on the caller's behalf would be a
    /// directory nobody knows to remove.
    ///
    /// The old root is *mounted* at `put_old` rather than merely dropped. That
    /// keeps it reachable until the caller says otherwise, and it is the one
    /// reference that decides when the initramfs's memory can be given back.
    /// Dropping it here instead would leave the tree alive in whatever still
    /// pointed into it, with no name left to reach it by - a leak with no
    /// handle. Detaching it is the caller's next step, and a deliberate one.
    pub fn pivot_root(&self, new_root: &str, put_old: &str) -> VfsResult<()> {
        let old_root = critical_section::with(|cs| self.root.borrow(cs).borrow().clone())
            .ok_or(VfsError::NoRoot)?;
        let new_root_inode = self.resolve(new_root.trim_end_matches('/'))?;

        // `put_old` is walked *inside* the new root: it names a directory in
        // the tree that is about to become `/`, not one in the tree being
        // replaced. The canonical spelling - `pivot_root /new_root
        // /new_root/old_root` - names it from the caller's tree, so the new
        // root's own prefix comes off before the walk.
        let new_root_trimmed = new_root.trim_end_matches('/');
        let put_old = put_old.trim_start_matches('/');
        let put_old = match put_old.strip_prefix(new_root_trimmed.trim_start_matches('/')) {
            Some(rest) => rest.trim_start_matches('/'),
            None => put_old,
        };
        let mut put_old_inode = new_root_inode.clone();
        if !put_old.is_empty() {
            for component in put_old.split('/') {
                if component.is_empty() || component == "." {
                    continue;
                }
                put_old_inode = put_old_inode.lookup(component)?;
            }
        }
        if put_old_inode.metadata().kind != NodeKind::Dir {
            return Err(VfsError::NotDir);
        }

        critical_section::with(|cs| {
            let mut mounts = self.mounts.borrow(cs).borrow_mut();
            // The root's own identity comes from the mount that made
            // `new_root` reachable: after the pivot that mount is gone, but
            // /proc/mounts still has to say what `/` is and what it came from.
            // Its write promise travels too - a read-only mount read-only
            // stays - together with the filesystem itself, so a later
            // `remount,rw /` can reach the thing that refuses the writes, and
            // with its device number, which is what `/`'s `st_dev` becomes.
            if let Some(entry) = mounts
                .iter()
                .find(|m| Arc::ptr_eq(&m.root, &new_root_inode))
                .map(|m| {
                    (
                        m.fstype.clone(),
                        m.source.clone(),
                        m.read_only,
                        m.fs.clone(),
                        m.dev,
                    )
                })
            {
                *self.root_fstype.borrow(cs).borrow_mut() = Some(entry.0);
                *self.root_source.borrow(cs).borrow_mut() = Some(entry.1);
                *self.root_read_only.borrow(cs).borrow_mut() = entry.2;
                *self.root_fs.borrow(cs).borrow_mut() = entry.3;
                *self.root_dev.borrow(cs).borrow_mut() = entry.4;
            }
            // Every mount in the table was attached somewhere in the old tree -
            // /dev, /tmp, the new root itself - and none of them can be reached
            // once the root is replaced. Keeping them would leave entries
            // anchored at nodes nothing resolves to, holding whole filesystems
            // alive for good. The new tree starts with the one mount below plus
            // whatever the caller attaches next.
            mounts.clear();
            let id = self.next_fs_id();
            let dev = self.next_anon_dev();
            mounts.push(Mount {
                at: put_old_inode.clone(),
                root: old_root,
                fstype: alloc::string::String::from("ramfs"),
                source: alloc::string::String::from("initramfs"),
                id,
                dev,
                read_only: false,
                fs: None,
                bind: false,
            });
            *self.root.borrow(cs).borrow_mut() = Some(new_root_inode);
        });
        self.refresh_mount_points();
        Ok(())
    }

    /// Walk `path` from the root, crossing mounts as they are met.
    pub fn resolve(&self, path: &str) -> VfsResult<Arc<dyn Inode>> {
        let start = critical_section::with(|cs| self.root.borrow(cs).borrow().clone())
            .ok_or(VfsError::NoRoot)?;
        self.resolve_from(path, start)
    }

    /// Walk `path` from `start` instead of the root - which is what a
    /// relative path means, and why a process carrying a cwd needs this
    /// rather than [`resolve`](Self::resolve).
    ///
    /// An absolute path ignores `start` entirely: `/` means the root, not
    /// "wherever this happens to be". The mount crossing is the same walk as
    /// ever, one node deeper in the tree.
    pub fn resolve_from(&self, path: &str, start: Arc<dyn Inode>) -> VfsResult<Arc<dyn Inode>> {
        let absolute = path.starts_with('/');
        let start = if absolute {
            critical_section::with(|cs| self.root.borrow(cs).borrow().clone())
                .ok_or(VfsError::NoRoot)?
        } else {
            start
        };
        // A relative path is walked as the absolute path it names from where
        // it starts, so the one `components` - which splits absolute paths -
        // covers both. The joined form is owned, and lives for the walk.
        let joined;
        let path = if absolute {
            path
        } else {
            joined = format!("/{path}");
            joined.as_str()
        };

        // The names still to walk. Owned, because meeting a link rewrites what
        // is left: the link's target goes in front of the names that followed
        // it, and the walk continues from there.
        let mut todo: VecDeque<String> = path::components(path)?
            .map(String::from)
            .collect::<Vec<_>>()
            .into();
        // The walk so far, and the only record of where it has been: `stack[0]`
        // is where it started - the root for an absolute path, the caller's cwd
        // for a relative one - and the last entry is the directory the next
        // name is looked up in. The chain is what answers `..`, which is why it
        // outlives a link: a relative target is spliced *beside* the link it
        // came from, so the names already walked are exactly the ancestors the
        // target's own `..` has to reach. A walk that dropped the chain would
        // have to *search* for each of those parents instead - one bounded
        // whole-tree search per `..`, `parent_of` - and a search that gives up
        // leaves the `..` standing still.
        let mut stack: Vec<Arc<dyn Inode>> = alloc::vec![start];
        let mut hops = 0;
        // Whether the walk's current context is inside a *bind* alias. Linux
        // copies no child mounts into a plain bind - only `MS_REC` carries
        // the subtree's mounts along, and this kernel refuses that flag -
        // so from inside a bind's copy no further mount crossing exists: the
        // copy shows the aliased filesystem's own directories, exactly where
        // the originals are mounted *over*. OpenRC's bootmisc depends on
        // this precisely: `mount --bind / $dir && rm -rf $dir/run/*` is its
        // way to clean the *underlying* /run, and a crossing that re-appeared
        // through the alias would hand that `rm -rf` to the live tmpfs
        // instead - deleting the softlevel file every later service checks.
        let mut bound = false;

        loop {
            let mut met_link = false;

            while let Some(comp) = todo.pop_front() {
                match comp.as_str() {
                    "." => {}
                    // POSIX: `/..` is `/`, so the last entry never pops. Every
                    // other `..` is one step back along the chain the walk
                    // built on its way down.
                    ".." => {
                        // Leaving a directory is a search of it as much as
                        // entering one is: Linux asks for the execute bit
                        // here too, and without it `../secret` reaches a
                        // file through a directory the caller cannot walk.
                        if let Some(here) = stack.last() {
                            crate::vfs::may_search(here)?;
                        }
                        if stack.len() > 1 {
                            stack.pop();
                        } else if stack[0].metadata().kind != NodeKind::Dir {
                            // `file/..` is not "the parent of a file": a name
                            // that is not a directory has no `..` to go to, and
                            // an ordinary walk would have said so by looking a
                            // name up in it.
                            return Err(VfsError::NotDir);
                        } else if !self.is_root(&stack[0]) {
                            // A chain of one means the walk has not descended
                            // from where it started, so the parent above it is
                            // the one thing the chain cannot supply: an
                            // absolute path starts at the root, whose `..` is
                            // itself, and anything else is a caller's cwd, whose
                            // ancestry the kernel is not holding. That is the
                            // only parent search left, and it is bounded.
                            if let Some(parent) = self.parent_of(&stack[0]) {
                                stack[0] = parent;
                            }
                        }
                    }
                    name => {
                        let here = stack.last().expect("the stack is never empty").clone();
                        // A name that is not a directory has no children to
                        // look up - and Linux says so *before* it asks the
                        // permission questions, so `cat /etc/hostname/x`
                        // reads as ENOTDIR whatever the file's bits are.
                        if here.metadata().kind != NodeKind::Dir {
                            return Err(VfsError::NotDir);
                        }
                        // Walking *through* a directory is what the execute
                        // bit is for: without it a caller can name a file it
                        // may not reach, and the name is all a directory
                        // listing ever needed. Linux checks every component
                        // as it goes, so the check belongs in the walk rather
                        // than at the open - the walk is what reaches the
                        // file.
                        crate::vfs::may_search(&here)?;
                        let child = here.lookup(name)?;
                        let child = if bound {
                            // Inside a bind alias: the table's other mounts
                            // are not part of this copy's subtree (see the
                            // note on `bound` above), so the directory the
                            // lookup found is the answer as it stands.
                            child
                        } else {
                            critical_section::with(|cs| {
                                let mounts = self.mounts.borrow(cs).borrow();
                                // The newest mount on the point wins, the
                                // way a stack of mounts peels from the top.
                                match mounts.iter().rposition(|m| Arc::ptr_eq(&m.at, &child)) {
                                    Some(pos) => {
                                        bound = mounts[pos].bind;
                                        mounts[pos].root.clone()
                                    }
                                    None => child,
                                }
                            })
                        };
                        if child.metadata().kind == NodeKind::Symlink {
                            // A descriptor's second name - `/proc/<pid>/fd/N`
                            // and the `/dev/std*` links into it - is a link
                            // that must not be spliced. Its target is a
                            // *report* about a descriptor, not a path to
                            // reopen: the walk hands the link itself back, and
                            // the opener reads `as_fd` off it and clones the
                            // descriptor, position and flags and all. Splicing
                            // would open a fresh description of whatever the
                            // descriptor once named - a different file
                            // position, or nothing at all once the name has
                            // gone.
                            if child.as_fd().is_some() {
                                stack.push(child);
                                continue;
                            }
                            // Splice the target in front of whatever was still
                            // to come. `here`, not the link, is what a relative
                            // target is relative to - and `here` is what the
                            // chain ends at, so a relative splice keeps every
                            // ancestor the walk has walked.
                            let target = child.readlink()?;
                            // `components` only splits absolute paths, so a
                            // relative target is walked as the absolute path
                            // it names from `here` - which is what makes it
                            // relative to the link's own directory.
                            let owned;
                            let walk = if target.starts_with('/') {
                                target.as_str()
                            } else {
                                owned = format!("/{target}");
                                owned.as_str()
                            };
                            let mut next: VecDeque<String> = path::components(walk)?
                                .map(String::from)
                                .collect::<Vec<_>>()
                                .into();
                            next.extend(todo);
                            todo = next;
                            if target.starts_with('/') {
                                // An absolute target names a path from the real
                                // root, so the names after it are looked up
                                // there and the walk to the link - and any bind
                                // alias it came through - is behind it.
                                stack.truncate(1);
                                stack[0] = critical_section::with(|cs| {
                                    self.root.borrow(cs).borrow().clone()
                                })
                                .ok_or(VfsError::NoRoot)?;
                                bound = false;
                            }
                            met_link = true;
                            break;
                        }
                        stack.push(child);
                    }
                }
            }

            if !met_link {
                return Ok(stack.pop().expect("the stack is never empty"));
            }
            // ELOOP: a link chain this long is a cycle, and walking it any
            // further would just be a slower way to hang.
            hops += 1;
            if hops > SYMLINK_HOPS {
                return Err(VfsError::Invalid);
            }
        }
    }

    /// The path by which `target` is reached, found by walking down from the
    /// root. The same search [`Self::parent_of`] makes, carrying the names it
    /// came by - which is what /proc/mounts needs, because a mount is recorded
    /// by the inode it sits on and /proc reports it by name.
    ///
    /// `"/"` for the root itself, `None` for a node the tree cannot name: a
    /// mounted filesystem's root reached from outside its mount point, or a
    /// node whose other name the caller did not use.
    ///
    /// The walk asks every directory it meets to list itself, so a listing
    /// that answers by searching the tree would start this search again from
    /// the top, one stack deeper each time. The one directory that used to do
    /// that is `/proc/<pid>/fd`, which names each open descriptor by the path
    /// it was opened through; it now answers a listing with the descriptor
    /// numbers alone, and computes a name only when `readlink` asks for one.
    pub fn path_of(&self, target: &Arc<dyn Inode>) -> Option<alloc::string::String> {
        const VISITS_MAX: usize = 512;

        let root = critical_section::with(|cs| self.root.borrow(cs).borrow().clone())?;
        if Arc::ptr_eq(&root, target) {
            return Some(alloc::string::String::from("/"));
        }
        let mut queue: VecDeque<(Arc<dyn Inode>, alloc::string::String)> =
            VecDeque::from([(root, alloc::string::String::new())]);
        let mut visits = 0usize;
        while let Some((dir, prefix)) = queue.pop_front() {
            visits += 1;
            if visits > VISITS_MAX {
                return None;
            }
            let Ok(entries) = dir.read_dir() else {
                continue;
            };
            for (name, kind) in entries {
                if name == "." || name == ".." {
                    continue;
                }
                let Ok(child) = dir.lookup(&name) else {
                    continue;
                };
                let reached = critical_section::with(|cs| {
                    let mounts = self.mounts.borrow(cs).borrow();
                    mounts
                        .iter()
                        .find(|m| Arc::ptr_eq(&m.at, &child))
                        .map(|m| m.root.clone())
                        .unwrap_or_else(|| child.clone())
                });
                let path = alloc::format!("{prefix}/{name}");
                // Two names for one place: `target` may be the directory the
                // mount sits on - how the mount table asks - or the mounted
                // filesystem's own root, how a walk through the mount sees
                // it. Both spell `/proc` here.
                if Arc::ptr_eq(&child, target) || Arc::ptr_eq(&reached, target) {
                    return Some(path);
                }
                if kind == NodeKind::Dir {
                    queue.push_back((reached, path));
                }
            }
        }
        None
    }

    /// Every mount, as /proc/mounts reports them: source, mountpoint, type.
    /// The root comes first, then the table in mounting order.
    ///
    /// This is the only record of what is mounted where that userland can
    /// read, and the thing `mountinfo` and `mount -a` are built on.
    pub fn mount_table(
        &self,
    ) -> Vec<(
        alloc::string::String,
        alloc::string::String,
        alloc::string::String,
        alloc::string::String,
    )> {
        // The table is copied out first: naming each mount point walks the
        // tree, and that walk takes the same lock. Nested takes of the one
        // critical section are not something to lean on.
        let (root_fstype, root_source, root_read_only, entries) = critical_section::with(|cs| {
            let fstype = self.root_fstype.borrow(cs).borrow().clone();
            let source = self.root_source.borrow(cs).borrow().clone();
            let ro = *self.root_read_only.borrow(cs).borrow();
            let entries: Vec<MountRow> = self
                .mounts
                .borrow(cs)
                .borrow()
                .iter()
                .map(|m| {
                    (
                        m.at.clone(),
                        m.source.clone(),
                        m.fstype.clone(),
                        m.read_only,
                        m.bind,
                    )
                })
                .collect();
            (fstype, source, ro, entries)
        });
        let mut lines = Vec::new();
        if let (Some(fstype), Some(source)) = (root_fstype, root_source) {
            lines.push((
                source,
                alloc::string::String::from("/"),
                fstype,
                alloc::string::String::from(if root_read_only { "ro" } else { "rw" }),
            ));
        }
        for (at, source, fstype, read_only, bind) in entries {
            if let Some(path) = self.path_of(&at) {
                // The options column. Only what this kernel can be asked for
                // is reported: a caller that reads "ro" here knows a write
                // will be refused, which is the one thing the column is for.
                // A bind says so too, because that is how a reader tells a
                // second name from a real second filesystem - `findmnt` and
                // OpenRC's scan for the mount under `/run` both key on it.
                let options = match (read_only, bind) {
                    (true, true) => alloc::string::String::from("ro,bind"),
                    (true, false) => alloc::string::String::from("ro"),
                    (false, true) => alloc::string::String::from("rw,bind"),
                    (false, false) => alloc::string::String::from("rw"),
                };
                lines.push((source, path, fstype, options));
            }
        }
        lines
    }

    /// Whether `node` is the root of this tree - which answers `..` without a
    /// search, because `/..` is `/`: the root is the one node whose parent the
    /// walk must *not* go looking for. Nothing names the root, so the search
    /// would spend its whole budget learning what this comparison knows.
    fn is_root(&self, node: &Arc<dyn Inode>) -> bool {
        critical_section::with(|cs| self.root.borrow(cs).borrow().clone())
            .is_some_and(|root| Arc::ptr_eq(&root, node))
    }

    /// The directory that names `target`, found by walking down from the root.
    ///
    /// A node does not know its parent. A directory is a *name edge* in
    /// something else and nothing points back up, so the only way to go up from
    /// an inode is to look for the edge that names it - which is a walk of the
    /// tree. It is what makes `..` mean anything from a cwd, since a relative
    /// path starts at an inode and not at a name.
    ///
    /// This is the *fallback*, for a `..` whose ancestors no walk has seen:
    /// everything a path walked from the root brings its own chain, and so does
    /// a symlink's target, which is spliced beside the link it came from
    /// ([`Self::resolve_from`]). What is left is a path relative to a cwd,
    /// whose parent the caller's inode cannot name.
    ///
    /// A mounted filesystem is handled by the edge that carries it: the search
    /// steps into the mount and compares against the mounted root, so `..` out
    /// of `/dev` lands on `/` rather than nowhere.
    ///
    /// `None` for the root (nothing names it), for a node reached by one of
    /// several names that the caller did not use, and for a tree too large to
    /// look through. The first is what POSIX asks for; the others leave `..`
    /// where it was, which is the honest answer when the tree cannot say.
    fn parent_of(&self, target: &Arc<dyn Inode>) -> Option<Arc<dyn Inode>> {
        // Bounded: the answer is near the top for every cwd a program actually
        // holds, and searching a large tree without limit to resolve one `..`
        // is a worse trade than a `..` that does not move.
        const VISITS_MAX: usize = 512;

        let root = critical_section::with(|cs| self.root.borrow(cs).borrow().clone())?;
        let mut queue: VecDeque<Arc<dyn Inode>> = VecDeque::from([root]);
        let mut visits = 0usize;
        while let Some(dir) = queue.pop_front() {
            visits += 1;
            if visits > VISITS_MAX {
                return None;
            }
            let Ok(entries) = dir.read_dir() else {
                continue;
            };
            for (name, kind) in entries {
                if name == "." || name == ".." {
                    continue;
                }
                let Ok(child) = dir.lookup(&name) else {
                    continue;
                };
                // What resolving `name` here would actually give: the mount's
                // root when the name is a mount point, the child otherwise.
                let reached = critical_section::with(|cs| {
                    let mounts = self.mounts.borrow(cs).borrow();
                    mounts
                        .iter()
                        .find(|m| Arc::ptr_eq(&m.at, &child))
                        .map(|m| m.root.clone())
                        .unwrap_or_else(|| child.clone())
                });
                if Arc::ptr_eq(&reached, target) {
                    return Some(dir.clone());
                }
                if kind == NodeKind::Dir {
                    queue.push_back(reached);
                }
            }
        }
        None
    }

    /// The directory holding `path`, and the name within it - without following
    /// a link the name turns out to name.
    ///
    /// This is the shape `readlink`, `symlink`, `unlink` and `mount` need: they
    /// all act on the *name*, and a resolver that followed it would hand back
    /// what the name points at instead of the name itself.
    ///
    /// The two halves are separated rather than returned as a node because a
    /// caller that is about to *make* it needs the directory, and one that is
    /// about to *read* it needs the name it will look up.
    pub fn resolve_parent(&self, path: &str) -> VfsResult<(Arc<dyn Inode>, String)> {
        let (parent_path, name) = split_last(path);
        if name.is_empty() {
            return Err(VfsError::Invalid);
        }
        let parent = self.resolve(parent_path)?;
        Ok((parent, String::from(name)))
    }

    /// Resolve `path` without following a symbolic link in its **last**
    /// component - what `lstat`, `readlink` and `symlink` need, and the one
    /// thing the ordinary walk cannot give them.
    ///
    /// Everything before the last name is walked normally, links and all, so
    /// `a/b/c` really looks inside the `b` that `a` leads to. The last name is
    /// then looked up in its parent and handed back unfollowed. That
    /// distinction is the whole point: `lstat` on a link must answer for the
    /// link, and `symlink` onto a name that is already a link must report the
    /// name as taken rather than quietly writing beside its target.
    ///
    /// `.` and `..` are the two names that are not entries of the directory
    /// they appear in - `.` *is* that directory, and neither can be a link -
    /// so resolving them is ordinary resolution. They are also the names every
    /// directory-listing tool asks for, which is why treating them as
    /// unnameable broke `ls -l`.
    pub fn resolve_no_follow(
        &self,
        path: &str,
        start: Arc<dyn Inode>,
    ) -> VfsResult<Arc<dyn Inode>> {
        let (parent_path, name) = split_last(path);
        if name.is_empty() || name == "." || name == ".." {
            return self.resolve_from(path, start);
        }
        let parent = self.resolve_from(parent_path, start)?;
        if parent.metadata().kind != NodeKind::Dir {
            return Err(VfsError::NotDir);
        }
        parent.lookup(name)
    }
}

/// Split `/a/b/c` into `/a/b` and `c`; `/a` into `/` and `a`.
fn split_last(path: &str) -> (&str, &str) {
    let trimmed = path.trim_end_matches('/');
    match trimmed.rfind('/') {
        Some(i) => (if i == 0 { "/" } else { &trimmed[..i] }, &trimmed[i + 1..]),
        None => (".", trimmed),
    }
}

impl Default for Vfs {
    fn default() -> Vfs {
        Vfs::new()
    }
}

static STATIC: Vfs = Vfs::new();

/// The kernel's one instance, for callers the free functions do not cover -
/// the initramfs unpacker, which walks and creates through a `&Vfs`.
pub fn global() -> &'static Vfs {
    &STATIC
}

/// Install the kernel's root filesystem. Once, at boot.
pub fn mount_root(fs: &dyn FileSystem) {
    STATIC.mount_root(fs);
}

/// Mount `fs` at the directory named by `path` in the kernel's tree.
pub fn mount(path: &str, fs: &dyn FileSystem) -> VfsResult<()> {
    STATIC.mount(path, fs)
}

/// Unmount the filesystem mounted at `path`.
///
/// The path must name a mountpoint - a directory that has a filesystem mounted
/// on it. The mount is removed from the mount table, and subsequent accesses
/// to files under `path` will see the underlying directory in the parent
/// filesystem instead.
pub fn unmount(path: &str) -> VfsResult<()> {
    STATIC.unmount(path)
}

/// Replace the root filesystem: move `new_root` to `/`, and move the old root
/// to `put_old` inside the new root, so it can be unmounted afterward.
///
/// This is the kernel's pivot_root(2): after this call, `/` refers to what was
/// previously `new_root`, and `put_old` holds the old root filesystem's
/// contents, ready to be unmounted.
pub fn pivot_root(new_root: &str, put_old: &str) -> VfsResult<()> {
    STATIC.pivot_root(new_root, put_old)
}

/// Mount `fs` on the directory `target` already resolves to, recording the
/// type and source as the caller spelled them. This is the shape `mount(2)`
/// arrives in, and the reason it exists as a syscall at all: which filesystems
/// are mounted where is userland's decision to make.
///
/// `dev` is what `stat` will report as `st_dev` under this mount - the resolved
/// source device's number, or `None` for a filesystem with no storage behind it.
pub fn mount_at(
    target: &str,
    fs: Arc<dyn FileSystem>,
    fstype: &str,
    source: &str,
    dev: Option<u64>,
) -> VfsResult<()> {
    mount_at_opt(target, None, fs, fstype, source, false, dev)
}

/// [`mount_at`](fn.mount_at.html) resolved against a caller's cwd.
pub fn mount_at_opt(
    target: &str,
    start: Option<Arc<dyn Inode>>,
    fs: Arc<dyn FileSystem>,
    fstype: &str,
    source: &str,
    read_only: bool,
    dev: Option<u64>,
) -> VfsResult<()> {
    let at = resolve_opt(target, start)?;
    STATIC.mount_at(at, fs, fstype, source, read_only, dev)
}

/// `mount -o remount,...`: change a mounted filesystem's flags.
pub fn remount(target: &str, start: Option<Arc<dyn Inode>>, read_only: bool) -> VfsResult<()> {
    let at = resolve_opt(target, start)?;
    STATIC.remount(&at, read_only)
}

/// `mount --bind source target`: an existing tree, a second name. Both paths
/// resolve the way `mount(2)` resolves them - against the caller's cwd - and
/// the source's name is what the record reports as its source afterward.
pub fn bind_mount(
    source: &str,
    target: &str,
    start: Option<Arc<dyn Inode>>,
    read_only: bool,
) -> VfsResult<()> {
    let src = resolve_opt(source, start.clone())?;
    let at = STATIC.mountpoint_inode(target, start)?;
    STATIC.bind_mount(at, src, source, read_only)
}

/// Unmount the filesystem at `path`, resolved against a caller's cwd.
pub fn unmount_opt(path: &str, start: Option<Arc<dyn Inode>>) -> VfsResult<()> {
    let resolved = resolve_opt(path, start)?;
    STATIC.unmount_resolved(&resolved)
}

/// Every mount, as /proc/mounts reports it.
pub fn mount_table() -> Vec<(
    alloc::string::String,
    alloc::string::String,
    alloc::string::String,
    alloc::string::String,
)> {
    STATIC.mount_table()
}

/// The filesystem instance id serving `path`, with its type. This is the
/// instance's *identity*; what `stat` reports for the path is [`dev_of_path`].
pub fn fs_of_path(path: &str) -> (u64, alloc::string::String) {
    STATIC.fs_of_path(path)
}

/// What `stat`'s `st_dev` reports for `path`: the number of the device behind
/// the filesystem that serves it, or an anonymous number for a pseudo-filesystem.
pub fn dev_of_path(path: &str) -> u64 {
    STATIC.dev_of_path(path)
}

/// The same, for an inode with no path - `fstat(2)`'s descriptor shape.
pub fn dev_of_inode(inode: &Arc<dyn Inode>) -> u64 {
    STATIC.dev_of_inode(inode)
}

/// The space accounting of the filesystem that serves `path`, straight from
/// the instance. `None` when the serving filesystem has no real numbers
/// (ramfs and friends answer through the syscall layer instead).
pub fn fs_stats_of_path(path: &str) -> Option<super::FsStats> {
    STATIC.fs_serving_path(path).and_then(|fs| fs.statfs())
}

/// The same, for an inode with no path - `fstatfs(2)`'s descriptor shape.
pub fn fs_stats_of_inode(inode: &Arc<dyn Inode>) -> Option<super::FsStats> {
    STATIC.fs_serving_inode(inode).and_then(|fs| fs.statfs())
}

/// The type of the filesystem a node belongs to, by identity: a mount's own
/// root names that mount; anything else names the root filesystem.
pub fn fstype_of_inode(inode: &Arc<dyn Inode>) -> alloc::string::String {
    let (root_fstype, hits) = critical_section::with(|cs| {
        let fstype = STATIC
            .root_fstype
            .borrow(cs)
            .borrow()
            .clone()
            .unwrap_or_else(|| alloc::string::String::from("unknown"));
        let hits: Vec<alloc::string::String> = STATIC
            .mounts
            .borrow(cs)
            .borrow()
            .iter()
            .filter(|m| Arc::ptr_eq(&m.root, inode))
            .map(|m| m.fstype.clone())
            .collect();
        (fstype, hits)
    });
    hits.into_iter().next().unwrap_or(root_fstype)
}

/// The filesystem instance id a node belongs to, by identity: a mount's own
/// root names that mount; anything else names the root filesystem.
///
/// This is the *identity* of the instance - the key two files' lock records
/// and a socket's address are filed under, and the thing `EXDEV` is decided
/// by - and not what `stat` reports. A descriptor's `st_dev` is
/// [`dev_of_inode`], which is a different number whenever the filesystem has a
/// device behind it.
pub fn fs_id_of_inode(inode: &Arc<dyn Inode>) -> u64 {
    critical_section::with(|cs| {
        STATIC
            .mounts
            .borrow(cs)
            .borrow()
            .iter()
            .find(|m| Arc::ptr_eq(&m.root, inode))
            .map(|m| m.id)
            .unwrap_or(0)
    })
}

/// Make `inode` the root of the namespace, as `chroot(2)` asks. The kernel
/// has one namespace, so this is a whole-namespace answer rather than a
/// per-process one - which is exactly what an initramfs's `switch_root`
/// wants, being pid 1 and alone.
pub fn chroot(path: &str, cwd: &Arc<dyn Inode>) -> VfsResult<()> {
    let inode = resolve_opt(path, Some(cwd.clone()))?;
    if inode.metadata().kind != NodeKind::Dir {
        return Err(VfsError::NotDir);
    }
    STATIC.set_global_root(inode);
    Ok(())
}

/// `mount(2)`'s MS_MOVE, free-function spelling: the filesystem mounted at
/// `at` becomes the root.
pub fn move_root(at: &Arc<dyn Inode>) -> VfsResult<()> {
    STATIC.move_root(at)
}

/// Re-home the mount serving `source` on the directory `at`.
pub fn move_mount(source: &Arc<dyn Inode>, at: &Arc<dyn Inode>) -> VfsResult<()> {
    STATIC.move_mount(source, at)
}

/// Resolve `path` in the kernel's tree.
pub fn resolve(path: &str) -> VfsResult<Arc<dyn Inode>> {
    STATIC.resolve(path)
}

/// Resolve `path` starting from `start` - a process's cwd. An absolute path
/// ignores the start and begins at the root, as `/` must.
pub fn resolve_at(path: &str, start: Arc<dyn Inode>) -> VfsResult<Arc<dyn Inode>> {
    STATIC.resolve_from(path, start)
}

#[cfg(all(test, not(target_os = "none")))]
mod tests {
    use alloc::sync::Arc;

    use super::{Vfs, split_last};
    use crate::vfs::ramfs::RamFs;
    use crate::vfs::{FileSystem, NodeKind, VfsError};

    fn tree_with_root() -> Arc<Vfs> {
        let vfs = Arc::new(Vfs::new());
        vfs.mount_root(&*RamFs::new());
        vfs
    }

    #[test]
    fn resolution_walks_names_and_dots() {
        let vfs = tree_with_root();
        vfs.resolve("/")
            .unwrap()
            .create("a", NodeKind::Dir)
            .unwrap();
        vfs.resolve("/a")
            .unwrap()
            .create("f", NodeKind::File)
            .unwrap();

        let f = vfs.resolve("/a/./f").unwrap();
        let again = vfs.resolve("/a/../a/f").unwrap();
        assert!(Arc::ptr_eq(&f, &again));
        // `/..` at the root stays at the root, as POSIX demands.
        let root = vfs.resolve("/").unwrap();
        assert!(Arc::ptr_eq(&vfs.resolve("/..").unwrap(), &root));

        assert_eq!(vfs.resolve("/a/missing").err(), Some(VfsError::NoEntry));
        // A file in the middle of a path is not a directory.
        assert_eq!(vfs.resolve("/a/f/x").err(), Some(VfsError::NotDir));
    }

    #[test]
    fn a_links_target_climbs_the_chain_rather_than_searching_for_it() {
        let vfs = tree_with_root();
        let root = vfs.resolve("/").unwrap();
        // A root with enough in it that looking for a parent *by searching the
        // tree* gives up - which is what a real root filesystem looks like, and
        // why the search is not something a `..` can lean on. The count is the
        // budget's own: `parent_of` stops after `VISITS_MAX` directories.
        for i in 0..600 {
            root.create(&alloc::format!("d{i:04}"), NodeKind::Dir)
                .unwrap();
        }
        let a = root.create("a", NodeKind::Dir).unwrap();
        let b = a.create("b", NodeKind::Dir).unwrap();
        let target = b.create("target", NodeKind::Dir).unwrap();
        target.create("file", NodeKind::File).unwrap();
        let c = b.create("c", NodeKind::Dir).unwrap();
        let deep = c.create("deep", NodeKind::Dir).unwrap();
        // `/a/b/c/deep/link` names `/a/b/target`, which is the shape every
        // `/sys/dev/char/N:M/device` link has: a target that climbs out of the
        // directory the link sits in. Walking it needs the two ancestors `../..`
        // names, and the walk has them - it came down through them - so nothing
        // has to be looked for.
        deep.symlink("link", "../../target").unwrap();

        assert!(
            Arc::ptr_eq(&vfs.resolve("/a/b/c/deep/link").unwrap(), &target),
            "a relative target resolves beside the link it came from"
        );
        // And the walk continues *through* the link, which is where a failed
        // climb shows up as a name not found in the wrong directory.
        vfs.resolve("/a/b/c/deep/link/file").unwrap();
        assert_eq!(
            vfs.resolve("/a/b/c/deep/link/missing").err(),
            Some(VfsError::NoEntry)
        );
    }

    #[test]
    fn mounts_replace_the_child_with_the_mounted_root() {
        let vfs = tree_with_root();
        vfs.resolve("/")
            .unwrap()
            .create("mnt", NodeKind::Dir)
            .unwrap();
        let inner = RamFs::new();
        inner.root().create("hello", NodeKind::File).unwrap();
        vfs.mount("/mnt", &*inner).unwrap();

        let mnt = vfs.resolve("/mnt").unwrap();
        assert!(Arc::ptr_eq(&mnt, &inner.root()));
        assert!(Arc::ptr_eq(
            &vfs.resolve("/mnt/hello").unwrap(),
            &inner.root().lookup("hello").unwrap()
        ));

        // `..` from inside the mount lands on the mountpoint's parent, then
        // walks back in through the mount edge.
        let back = vfs.resolve("/mnt/hello/../../mnt").unwrap();
        assert!(Arc::ptr_eq(&back, &mnt));
    }

    /// A plain bind aliases one filesystem; the mounts attached *inside* it
    /// are children of the original mount, and Linux copies none of them
    /// into the alias. OpenRC's bootmisc is the case this exists for:
    /// `mount --bind / $dir && rm -rf $dir/run/*` is meant to clean the
    /// underlying `/run` - the directory the tmpfs covers - and a crossing
    /// that re-appeared through the alias would run that `rm -rf` against
    /// the live tmpfs and delete the very `/run/openrc` state the boot is
    /// built on.
    #[test]
    fn a_bind_alias_carries_no_mounts_of_the_aliased_tree() {
        let vfs = tree_with_root();
        let under = vfs
            .resolve("/")
            .unwrap()
            .create("run", NodeKind::Dir)
            .unwrap();
        under.create("stale", NodeKind::File).unwrap();
        let live = RamFs::new();
        live.root().create("openrc", NodeKind::Dir).unwrap();
        vfs.mount("/run", &*live).unwrap();
        vfs.resolve("/")
            .unwrap()
            .create("b", NodeKind::Dir)
            .unwrap();
        let b = vfs.resolve("/b").unwrap();
        vfs.bind_mount(b, vfs.resolve("/").unwrap(), "/", false)
            .unwrap();

        // Through the alias, `run` is the root tree's own directory: it
        // holds what is under the mount and none of what is mounted.
        let aliased = vfs.resolve("/b/run").unwrap();
        assert!(Arc::ptr_eq(&aliased, &under));
        assert!(vfs.resolve("/b/run/stale").is_ok());
        assert_eq!(vfs.resolve("/b/run/openrc").err(), Some(VfsError::NoEntry));

        // The original context keeps its crossings; the alias is the only
        // place the mounts stop showing.
        assert!(Arc::ptr_eq(&vfs.resolve("/run").unwrap(), &live.root()));
        // And both names are the same filesystem instance, st_dev and all:
        // the record the alias adds borrows the root's id.
        assert_eq!(vfs.fs_of_path("/b/run").0, vfs.fs_of_path("/run/..").0);
    }

    #[test]
    fn mounting_twice_at_one_point_is_busy_and_files_are_not_mountpoints() {
        let vfs = tree_with_root();
        vfs.resolve("/")
            .unwrap()
            .create("d", NodeKind::Dir)
            .unwrap();
        vfs.resolve("/")
            .unwrap()
            .create("f", NodeKind::File)
            .unwrap();
        let inner = RamFs::new();

        vfs.mount("/d", &*inner).unwrap();
        assert_eq!(vfs.mount("/d", &*inner).unwrap_err(), VfsError::Busy);
        assert_eq!(vfs.mount("/f", &*inner).unwrap_err(), VfsError::NotDir);
    }

    #[test]
    fn resolving_before_a_root_exists_is_noroot() {
        let vfs = Vfs::new();
        assert_eq!(vfs.resolve("/").err(), Some(VfsError::NoRoot));
        assert_eq!(vfs.mount("/", &*RamFs::new()).err(), Some(VfsError::NoRoot));
    }
    /// Build `/bin/busybox` as a file and `/bin/ls`, `/bin/sh` as links to it -
    /// the shape a busybox rootfs is made of.
    fn tree_with_busybox() -> Arc<Vfs> {
        let vfs = tree_with_root();
        let bin = vfs
            .resolve("/")
            .unwrap()
            .create("bin", NodeKind::Dir)
            .unwrap();
        let busybox = bin.create("busybox", NodeKind::File).unwrap();
        busybox.write_at(0, b"busybox").unwrap();
        bin.symlink("ls", "/bin/busybox").unwrap();
        bin.symlink("sh", "busybox").unwrap();
        vfs
    }

    #[test]
    fn resolution_follows_an_absolute_and_a_relative_link() {
        let vfs = tree_with_busybox();
        let busybox = vfs.resolve("/bin/busybox").unwrap();

        let ls = vfs.resolve("/bin/ls").unwrap();
        assert!(
            Arc::ptr_eq(&ls, &busybox),
            "an absolute link was not followed"
        );
        // A relative link is relative to the directory holding it, not to the
        // root and not to the caller.
        let sh = vfs.resolve("/bin/sh").unwrap();
        assert!(
            Arc::ptr_eq(&sh, &busybox),
            "a relative link was not followed"
        );
        assert_eq!(ls.metadata().kind, NodeKind::File);
    }

    #[test]
    fn a_link_in_the_middle_of_a_path_is_followed() {
        let vfs = tree_with_busybox();
        vfs.resolve("/")
            .unwrap()
            .symlink("binlink", "/bin")
            .unwrap();
        let busybox = vfs.resolve("/bin/busybox").unwrap();
        // The names after the link still have to be walked once it resolves.
        assert!(Arc::ptr_eq(&vfs.resolve("/binlink/ls").unwrap(), &busybox));
        assert!(vfs.resolve("/binlink/../bin/sh").is_ok());
    }

    #[test]
    fn a_link_cycle_is_an_error_and_not_a_hang() {
        let vfs = tree_with_root();
        let root = vfs.resolve("/").unwrap();
        root.symlink("a", "b").unwrap();
        root.symlink("b", "a").unwrap();
        assert_eq!(vfs.resolve("/a").err(), Some(VfsError::Invalid));
    }

    #[test]
    fn resolve_parent_names_the_link_itself() {
        let vfs = tree_with_busybox();
        let (parent, name) = vfs.resolve_parent("/bin/ls").unwrap();
        assert_eq!(name, "ls");
        // The link, not what it points at: this is what readlink needs.
        let link = parent.lookup(&name).unwrap();
        assert_eq!(link.metadata().kind, NodeKind::Symlink);
        assert_eq!(link.readlink().unwrap(), "/bin/busybox");
    }

    #[test]
    fn readlink_on_a_file_that_is_not_a_link_is_an_error() {
        let vfs = tree_with_busybox();
        let (parent, name) = vfs.resolve_parent("/bin/busybox").unwrap();
        assert_eq!(
            parent.lookup(&name).unwrap().readlink().err(),
            Some(VfsError::Invalid)
        );
    }

    /// The parent/name split every create and remove goes through. A trailing
    /// slash is not a name, and a bare `/` has no name to split off at all -
    /// which comes out as the caller's own directory and an empty name, the
    /// one shape no caller actually asks for.
    #[test]
    fn split_last_names_the_parent_and_the_last_name() {
        assert_eq!(split_last("/a/b"), ("/a", "b"));
        assert_eq!(split_last("/a"), ("/", "a"));
        assert_eq!(split_last("/a/b/"), ("/a", "b"));
        assert_eq!(split_last("/a//b"), ("/a/", "b"));
        assert_eq!(split_last("/"), (".", ""));
    }

    /// `statfs(2)` and `stat`'s `st_dev` both ask which filesystem serves a
    /// path, and the answer is the *deepest* mountpoint that prefixes it -
    /// `/mnt/usb/f` is served by the filesystem on `/mnt/usb`, not the one on
    /// `/mnt`, and not the root.
    #[test]
    fn the_deepest_mountpoint_serves_the_path_under_it() {
        let vfs = tree_with_root();
        let root = vfs.resolve("/").unwrap();
        root.create("mnt", NodeKind::Dir).unwrap();
        vfs.mount("/mnt", &*RamFs::new()).unwrap();
        // `usb` is made *inside* the mount, which is the only place it can be
        // seen once the filesystem is on top of `/mnt`.
        let mnt = vfs.resolve("/mnt").unwrap();
        mnt.create("usb", NodeKind::Dir).unwrap();
        vfs.mount("/mnt/usb", &*RamFs::new()).unwrap();

        // Ids are handed out in mount order, and 0 is the root's.
        assert_eq!(vfs.fs_of_path("/mnt/usb/f").0, 2);
        assert_eq!(vfs.fs_of_path("/mnt/usb").0, 2);
        assert_eq!(vfs.fs_of_path("/mnt/f").0, 1);
        assert_eq!(vfs.fs_of_path("/").0, 0);
        assert_eq!(vfs.fs_of_path("/elsewhere").0, 0);
    }

    /// The prefix has to end on a name boundary: `/mntx` is a sibling of
    /// `/mnt`, not something under it, and a match that stopped at the string
    /// would report every filesystem as serving its neighbours' names.
    #[test]
    fn a_mountpoint_does_not_serve_a_name_it_only_prefixes() {
        let vfs = tree_with_root();
        let root = vfs.resolve("/").unwrap();
        root.create("mnt", NodeKind::Dir).unwrap();
        root.create("mntx", NodeKind::Dir).unwrap();
        vfs.mount("/mnt", &*RamFs::new()).unwrap();

        assert_eq!(vfs.fs_of_path("/mnt").0, 1);
        assert_eq!(vfs.fs_of_path("/mnt/f").0, 1);
        assert_eq!(vfs.fs_of_path("/mntx").0, 0);
        assert_eq!(vfs.fs_of_path("/mntx/f").0, 0);
    }

    /// The path is normalized before the match, so a spelling with `..` or a
    /// doubled slash in it lands on the same filesystem as the plain one -
    /// otherwise `statfs` would answer by how a program happened to write the
    /// path rather than by where the file is.
    #[test]
    fn a_path_is_normalized_before_the_mountpoint_match() {
        let vfs = tree_with_root();
        let root = vfs.resolve("/").unwrap();
        root.create("mnt", NodeKind::Dir).unwrap();
        vfs.mount("/mnt", &*RamFs::new()).unwrap();

        for spelling in [
            "/mnt/f",
            "/mnt//f",
            "/mnt/./f",
            "/other/../mnt/f",
            "/mnt/f/",
        ] {
            assert_eq!(
                vfs.fs_of_path(spelling).0,
                1,
                "{spelling} is served by /mnt"
            );
        }
    }

    /// `st_dev` is the *device's* number when the filesystem has a device
    /// behind it, and that is the whole reason a mount records two numbers.
    ///
    /// The counterfactual is the failing case this replaced: with one number
    /// doing both jobs, a root mounted from `/dev/vda2` reported the mount
    /// table's own attachment counter as `st_dev`, so nothing that reads that
    /// field to find a device - `lsblk`'s `MOUNTPOINTS` column, `df`'s
    /// `Filesystem` column, `findmnt` - could ever match a mount back to the
    /// partition it is on, and every one of them printed a blank instead.
    #[test]
    fn a_mounted_device_reports_the_devices_own_number() {
        let vfs = tree_with_root();
        vfs.resolve("/")
            .unwrap()
            .create("mnt", NodeKind::Dir)
            .unwrap();
        let at = vfs.mountpoint_inode("/mnt", None).unwrap();
        vfs.mount_at(
            at,
            RamFs::new(),
            "ext4",
            "/dev/vda2",
            false,
            Some(crate::uevent::makedev(254, 2)),
        )
        .unwrap();

        assert_eq!(
            vfs.dev_of_path("/mnt"),
            0xfe02,
            "254:2 as libc packs it - the number /sys/class/block/vda2 publishes"
        );
        assert_eq!(vfs.dev_of_path("/mnt/anything/under/it"), 0xfe02);
        assert_ne!(
            vfs.dev_of_path("/mnt"),
            vfs.fs_of_path("/mnt").0,
            "which is not the attachment counter, and must not be"
        );

        // The root filesystem is the pseudo-filesystem in this tree, so it gets
        // an anonymous number - Linux's answer for a mount with no device, and
        // not the attachment counter's 0 either.
        assert_eq!(crate::uevent::major_of(vfs.dev_of_path("/")), 0);
        assert_ne!(vfs.dev_of_path("/"), 0, "0 is `no device`, not a number");
        assert_ne!(
            vfs.dev_of_path("/"),
            vfs.dev_of_path("/mnt"),
            "two filesystems, two numbers"
        );
    }

    /// Two pseudo-filesystems are two numbers, not one.
    ///
    /// "Anonymous" must not mean "the same": a caller that compares two paths'
    /// `st_dev` to decide whether a rename can be a rename, or whether a tree
    /// walks off its own filesystem, is answered by this field, and two
    /// unrelated mounts sharing a number turns a refusal into silent
    /// corruption. Linux draws each one from its own counter for exactly this.
    #[test]
    fn two_pseudo_filesystems_get_two_anonymous_numbers() {
        let vfs = tree_with_root();
        let root = vfs.resolve("/").unwrap();
        root.create("proc", NodeKind::Dir).unwrap();
        root.create("sys", NodeKind::Dir).unwrap();
        for (path, fstype) in [("/proc", "proc"), ("/sys", "sysfs")] {
            let at = vfs.mountpoint_inode(path, None).unwrap();
            vfs.mount_at(at, RamFs::new(), fstype, fstype, false, None)
                .unwrap();
        }

        let (root_dev, proc_dev, sys_dev) = (
            vfs.dev_of_path("/"),
            vfs.dev_of_path("/proc"),
            vfs.dev_of_path("/sys"),
        );
        assert_ne!(proc_dev, sys_dev, "one number each");
        assert_ne!(proc_dev, root_dev);
        assert_ne!(proc_dev, 0);
        // All three are anonymous, i.e. major 0 - which is what keeps them from
        // ever colliding with a real disk's number.
        for dev in [root_dev, proc_dev, sys_dev] {
            assert_eq!(crate::uevent::major_of(dev), 0);
        }
    }

    /// Promoting a mount to `/` moves its device number with it.
    ///
    /// This is the initramfs's last act: it mounts the real root somewhere,
    /// calls `pivot_root`, and from that moment `stat("/")` is the number of
    /// the partition the machine really lives on. A root that kept the ramfs's
    /// anonymous number would leave every tool that reads `st_dev` by path
    /// unable to find the root's device - `df /` would be blank on the one
    /// filesystem everything is on.
    #[test]
    fn the_root_keeps_the_device_number_of_the_mount_it_came_from() {
        let vfs = tree_with_root();
        vfs.resolve("/")
            .unwrap()
            .create("new", NodeKind::Dir)
            .unwrap();
        let at = vfs.mountpoint_inode("/new", None).unwrap();
        vfs.mount_at(
            at.clone(),
            RamFs::new(),
            "ext4",
            "/dev/vda2",
            false,
            Some(crate::uevent::makedev(254, 2)),
        )
        .unwrap();
        assert_eq!(
            crate::uevent::major_of(vfs.dev_of_path("/")),
            0,
            "still ramfs"
        );

        vfs.move_root(&vfs.resolve("/new").unwrap()).unwrap();
        assert_eq!(
            vfs.dev_of_path("/"),
            0xfe02,
            "the root is the vda2 mount now, and says so"
        );
        assert_eq!(
            vfs.dev_of_inode(&vfs.root_inode()),
            0xfe02,
            "and the descriptor door agrees with the path door"
        );
    }
}

#[cfg(all(test, not(target_os = "none")))]
mod cwd_tests {
    use alloc::sync::Arc;

    use super::Vfs;
    use crate::vfs::NodeKind;
    use crate::vfs::ramfs::RamFs;

    #[test]
    fn a_relative_path_starts_at_the_given_node() {
        let vfs = Arc::new(Vfs::new());
        vfs.mount_root(&*RamFs::new());
        vfs.resolve("/")
            .unwrap()
            .create("bin", NodeKind::Dir)
            .unwrap();
        vfs.resolve("/bin")
            .unwrap()
            .create("cat", NodeKind::File)
            .unwrap();
        vfs.resolve("/")
            .unwrap()
            .create("etc", NodeKind::Dir)
            .unwrap();

        let bin = vfs.resolve("/bin").unwrap();
        // "." from /bin is /bin.
        let here = vfs.resolve_from(".", bin.clone()).unwrap();
        assert!(Arc::ptr_eq(&here, &bin));

        // A file relative to /bin resolves; one relative to the root does not
        // exist there.
        let cat = vfs.resolve_from("cat", bin.clone()).unwrap();
        assert_eq!(cat.metadata().kind, NodeKind::File);
        assert!(vfs.resolve_from("etc", bin).is_err(), "etc is not in /bin");
    }
}
