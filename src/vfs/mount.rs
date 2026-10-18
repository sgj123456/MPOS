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
//! trees; the kernel's own table lives behind [`machine_table`], which every free function here
//! routes through — the caller's own table when its task has unshared one.

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
/// A mount as the *table* holds it, copied out before anything that walks the
/// tree runs: naming a mountpoint takes the table's lock, so the walk happens
/// after the copy rather than under it.
struct MountEntry {
    at: Arc<dyn Inode>,
    mount_id: u64,
    dev: u64,
    source: alloc::string::String,
    fstype: alloc::string::String,
    read_only: bool,
    bind: bool,
}

/// The per-mount options column, as both `/proc/mounts` and mountinfo spell it.
///
/// Only what this kernel can be asked for is reported: a caller that reads `ro`
/// here knows a write will be refused, which is the one thing the column is
/// for. A bind says so too, because that is how a reader tells a second name
/// from a real second filesystem - `findmnt` and OpenRC's scan for the mount
/// under `/run` both key on it.
fn mount_options(read_only: bool, bind: bool) -> alloc::string::String {
    alloc::string::String::from(match (read_only, bind) {
        (true, true) => "ro,bind",
        (true, false) => "ro",
        (false, true) => "rw,bind",
        (false, false) => "rw",
    })
}

/// Whether the mountpoint `mountpoint` covers `path`: equal to it, or a
/// directory prefix of it on a name boundary.
///
/// The boundary is the whole of it - `/mntx` is not under `/mnt` - and it is
/// spelled once because two readers ask it: which mount serves a path, and
/// which mount a mount hangs under.
fn mountpoint_covers(mountpoint: &str, path: &str) -> bool {
    if mountpoint == "/" {
        return true;
    }
    path == mountpoint
        || (path.starts_with(mountpoint) && path[mountpoint.len()..].starts_with('/'))
}

/// One mount, as `/proc/mounts` and `/proc/self/mountinfo` report it.
///
/// Both files are renderings of this. They are renderings of one fact, "what is
/// mounted where", and two walks of the mount table that each derived it would
/// be two answers to the same question. The fields a single file does not use
/// are here rather than in a second accessor: `mounts` wants only four of them,
/// while `mountinfo` wants the ids and the device as well, and the walk that
/// names each mountpoint is the expensive part.
pub struct MountRecord {
    /// This mount's own id, unique per attachment and never reused - mountinfo's
    /// first field. It is *not* [`Mount::id`]: that names the serving instance,
    /// which a bind alias shares, and a reader that built a tree out of two
    /// records carrying one id would fold two mounts into one.
    pub mount_id: u64,
    /// The id of the mount this one hangs under: the deepest mountpoint that
    /// prefixes it, or this mount's own id for the root, which hangs under
    /// nothing - mountinfo's second field.
    pub parent_id: u64,
    /// What `stat` reports as `st_dev` for paths on this mount, packed by
    /// [`crate::uevent::makedev`] - mountinfo's `maj:min` column.
    pub dev: u64,
    /// The name `mount(2)` was given for the filesystem, or "none" for one
    /// mounted without a source.
    pub source: alloc::string::String,
    /// Where it is mounted, named from the root.
    pub path: alloc::string::String,
    pub fstype: alloc::string::String,
    /// The per-mount options: exactly what this kernel can be asked for, so a
    /// reader that sees `ro` knows a write will be refused.
    pub options: alloc::string::String,
}

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
    /// This attachment's own id, unique in the table and never reused: what a
    /// mount is *named* by when one mount has to be told from another, which is
    /// what mountinfo's id and parent-id columns are for. Distinct from both
    /// numbers beside it: a bind's own id is its own while its `id` and `dev`
    /// belong to the instance it aliases, and two names for one filesystem are
    /// still two mounts.
    mount_id: u64,
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
    /// The filesystem this record attaches - and it is an `Arc` rather than an
    /// `Option` because everything downstream asks *which* filesystem serves a
    /// node: `statfs`'s numbers, `remount`'s read-only promise, and the
    /// identity test `linkat`'s `EXDEV` is decided by. A record that attached a
    /// filesystem without keeping it (this field used to be `None` for the
    /// kernel's own attaches) left every one of those questions unanswerable,
    /// so `mount_root` and `mount` now take the instance rather than borrowing
    /// it.
    fs: Arc<dyn FileSystem>,
    /// Whether this record is a bind alias rather than an attached
    /// filesystem: `root` is a node that another record - or the root slot -
    /// already serves, and the `id`, `fs` and `fstype` below are *borrowed*
    /// from that serving instance so one filesystem keeps one identity under
    /// both of its names.
    bind: bool,
}

/// A set of mounts with a root, and the resolver over them.
pub struct MountTable {
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
    /// The root's own [`Mount::mount_id`], for mountinfo's first two fields:
    /// the root is a mount like any other to a reader of that file, and the one
    /// that hangs under nothing.
    root_mount_id: Mutex<RefCell<u64>>,
    /// The source of mount ids: one per attachment, never reused.
    mount_seq: Mutex<RefCell<u64>>,
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
    /// The number this namespace is known by (`mnt:[<id>]`), minted from
    /// `crate::ns`'s one pool the first time somebody asks.
    ///
    /// Lazily, because `new()` is `const` - the machine's table is built in a
    /// `static` - and a mint is not. A first-read mint is still an *identity*:
    /// every later read of the same table answers the same number, and a copy
    /// (`unshare`, `clone` with the flag) starts at zero again and so gets a
    /// number of its own, which is what makes it a namespace rather than a
    /// name for one.
    ns_id: Mutex<RefCell<u64>>,
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

impl MountTable {
    pub const fn new() -> MountTable {
        MountTable {
            root: Mutex::new(RefCell::new(None)),
            mounts: Mutex::new(RefCell::new(Vec::new())),
            root_fstype: Mutex::new(RefCell::new(None)),
            root_source: Mutex::new(RefCell::new(None)),
            root_fs: Mutex::new(RefCell::new(None)),
            root_read_only: Mutex::new(RefCell::new(false)),
            root_dev: Mutex::new(RefCell::new(0)),
            root_mount_id: Mutex::new(RefCell::new(0)),
            mount_seq: Mutex::new(RefCell::new(1)),
            fs_seq: Mutex::new(RefCell::new(1)),
            anon_seq: Mutex::new(RefCell::new(1)),
            mount_points: Mutex::new(RefCell::new(Vec::new())),
            ns_id: Mutex::new(RefCell::new(0)),
        }
    }

    /// The root this namespace resolves absolute paths against. `None` before
    /// `mount_root`/`pivot_root` has installed one, which in practice is only
    /// a table built and not yet given a filesystem.
    ///
    /// `setns` is the caller that needs it: joining a mount namespace moves the
    /// caller's root to the *namespace's*, because an absolute path means "from
    /// this namespace's root" and a caller left holding the old one would be
    /// jailed in a tree the namespace it just joined does not have.
    pub fn ns_root(&self) -> Option<Arc<dyn super::Inode>> {
        critical_section::with(|cs| self.root.borrow(cs).borrow().clone())
    }

    /// The number this namespace is known by, minted on first ask and stable
    /// from then on (`/proc/<pid>/ns/mnt` reads `mnt:[<this>]`).
    pub fn ns_id(&self) -> u64 {
        critical_section::with(|cs| {
            let mut slot = self.ns_id.borrow(cs).borrow_mut();
            if *slot == 0 {
                *slot = crate::vfs::ns::mint();
            }
            *slot
        })
    }

    /// A **copy** of this table: the same mounts, the same records, fresh
    /// counters. This is `unshare(CLONE_NEWNS)`'s namespace — Linux's
    /// `copy_mnt_ns` + `copy_tree` of the mount list, and the copy is the whole
    /// feature: a caller handed an *empty* table would lose every filesystem
    /// under its feet at the instant of the call, which is not what asking for
    /// your own namespace means.
    ///
    /// The three counters are re-minted from 1 rather than carried over, because
    /// they are *this table's* facts: a mount id names a mount within one table
    /// (mountinfo's id/parent-id columns only ever compare ids of one reader's
    /// own tree), and two tables that shared a counter source would hand out two
    /// mounts claiming to be one attachment. The instance ids (`fs_seq`) and the
    /// anonymous device numbers (`anon_seq`) restart too, and that restart is
    /// honest for the same reason [`Self::fs_of_path`] already answers per-table:
    /// "which instance serves this path" is asked of one table at a time, and no
    /// caller on this kernel compares an id across two of them. What a copy does
    /// *not* get is a fresh identity for the served filesystems themselves — the
    /// `Arc<dyn FileSystem>` values travel by reference, so a file written
    /// through one table's mount is visible through the other's, exactly as it
    /// is across a fork today. Namespaces isolate names, not data.
    ///
    /// `owner` is the user namespace the namespace belongs to - the one its
    /// maker stands in (see [`crate::vfs::ns::Namespace::owner`]). It is given
    /// here rather than at the first read of [`Self::ns_id`] because by then the
    /// maker is not on the call stack any more: the table would have to guess
    /// who made it, and a `setns` into it would be asked about the wrong
    /// namespace.
    pub fn copy(&self, owner: Arc<crate::userns::UserNs>) -> Arc<MountTable> {
        let fresh = Arc::new(MountTable::new());
        critical_section::with(|cs| {
            // Minted now, and *owned* now, for the reason above.
            *fresh.ns_id.borrow(cs).borrow_mut() = crate::vfs::ns::mint_owned(owner);
            *fresh.root.borrow(cs).borrow_mut() = self.root.borrow(cs).borrow().clone();
            *fresh.mounts.borrow(cs).borrow_mut() = self
                .mounts
                .borrow(cs)
                .borrow()
                .iter()
                .map(|m| Mount {
                    at: m.at.clone(),
                    root: m.root.clone(),
                    fstype: m.fstype.clone(),
                    source: m.source.clone(),
                    id: m.id,
                    mount_id: m.mount_id,
                    dev: m.dev,
                    read_only: m.read_only,
                    fs: m.fs.clone(),
                    bind: m.bind,
                })
                .collect();
            *fresh.root_fstype.borrow(cs).borrow_mut() =
                self.root_fstype.borrow(cs).borrow().clone();
            *fresh.root_source.borrow(cs).borrow_mut() =
                self.root_source.borrow(cs).borrow().clone();
            *fresh.root_fs.borrow(cs).borrow_mut() = self.root_fs.borrow(cs).borrow().clone();
            *fresh.root_read_only.borrow(cs).borrow_mut() =
                *self.root_read_only.borrow(cs).borrow();
            *fresh.root_dev.borrow(cs).borrow_mut() = *self.root_dev.borrow(cs).borrow();
            *fresh.root_mount_id.borrow(cs).borrow_mut() = *self.root_mount_id.borrow(cs).borrow();
            // The copied records keep their ids; the counters start above every
            // id in the copy so a later mount cannot mint one already spoken for.
            let max_mount_id = fresh
                .mounts
                .borrow(cs)
                .borrow()
                .iter()
                .map(|m| m.mount_id)
                .max()
                .unwrap_or(0);
            let max_id = fresh
                .mounts
                .borrow(cs)
                .borrow()
                .iter()
                .map(|m| m.id)
                .max()
                .unwrap_or(0);
            let max_anon = fresh
                .mounts
                .borrow(cs)
                .borrow()
                .iter()
                .filter(|m| crate::uevent::major_of(m.dev) == 0)
                .map(|m| crate::uevent::minor_of(m.dev) as u64)
                .max()
                .unwrap_or(0);
            *fresh.mount_seq.borrow(cs).borrow_mut() =
                core::cmp::max(max_mount_id, *self.root_mount_id.borrow(cs).borrow()) + 1;
            *fresh.fs_seq.borrow(cs).borrow_mut() = max_id + 1;
            *fresh.anon_seq.borrow(cs).borrow_mut() = max_anon + 1;
            *fresh.mount_points.borrow(cs).borrow_mut() =
                self.mount_points.borrow(cs).borrow().clone();
        });
        fresh
    }

    /// Install the root filesystem. Once, at boot: a second call is a bug.
    /// The *filesystem itself* is recorded, not only its name and its device
    /// number, because the root slot is where a machine's every later question
    /// about "which filesystem serves this node" lands: `fs_serving_inode`
    /// answers with `root_fs` for anything that is not some mount's own root -
    /// which is every file, every directory and every `O_TMPFILE` inode on the
    /// machine - so a root recorded without one turns `statfs(2)`,
    /// `remount,rw /` and `linkat`'s `EXDEV` identity into "no filesystem" for
    /// the whole tree. The second machine shipped that way: its own boot
    /// attached the disk through the name-only form below and then promoted it
    /// to `/`, and every `apk update` on it answered `EXDEV` about two inodes
    /// of one ext4.
    pub fn mount_root(&self, fs: Arc<dyn FileSystem>) {
        // Drawn before the lock is taken: they are their own counters, and
        // numbers minted under this section would be two more things nested
        // inside it.
        let dev = self.next_anon_dev();
        let mount_id = self.next_mount_id();
        critical_section::with(|cs| {
            let mut root = self.root.borrow(cs).borrow_mut();
            assert!(root.is_none(), "the root filesystem is already mounted");
            *root = Some(fs.root());
            // The root's own type has to be on record from the start: init's
            // `statfs("/")` reads it, and busybox `switch_root` refuses to run
            // unless the tree it stands in answers ramfs/tmpfs magic.
            *self.root_fstype.borrow(cs).borrow_mut() =
                Some(alloc::string::String::from(fs.name()));
            *self.root_fs.borrow(cs).borrow_mut() = Some(fs);
            *self.root_dev.borrow(cs).borrow_mut() = dev;
            *self.root_mount_id.borrow(cs).borrow_mut() = mount_id;
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

    /// The next mount id, for mountinfo's first column: one per *attachment*,
    /// and never reused, so no two records in any two readings of the file can
    /// be confused for one another. A bind gets its own here even though it
    /// borrows the instance's `id` and `dev` - it is a second mount, and
    /// mountinfo is a list of mounts.
    fn next_mount_id(&self) -> u64 {
        critical_section::with(|cs| {
            let seq = self.mount_seq.borrow(cs);
            let mut seq = seq.borrow_mut();
            let id = *seq;
            *seq += 1;
            id
        })
    }

    /// Mount `fs` at the directory named by `path`.
    ///
    /// The mountpoint is found through its parent, not by resolving `path`
    /// outright: resolving would walk through whatever is already mounted
    /// there and lose the directory that is supposed to be mounted on.
    ///
    /// The filesystem instance is kept (§ [`Mount::fs`]), which is why this
    /// takes an `Arc` rather than a borrow: a caller that has one to give must
    /// not lose it, and a caller's borrowed `&dyn FileSystem` has no answer to
    /// the questions above at all.
    pub(crate) fn mount(&self, path: &str, fs: Arc<dyn FileSystem>) -> VfsResult<()> {
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

        let parent = self.resolve_caller(parent_path, self.root_inode())?;
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
            let mount_id = self.next_mount_id();
            mounts.push(Mount {
                at,
                root: fs.root(),
                fstype: alloc::string::String::from(fs.name()),
                source: alloc::string::String::from("none"),
                id,
                mount_id,
                dev,
                read_only: false,
                fs,
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
    pub(crate) fn fs_of_path(&self, path: &str) -> (u64, alloc::string::String) {
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
    pub(crate) fn dev_of_path(&self, path: &str) -> u64 {
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
                    .map(|m| m.fs.clone())
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
                .map(|m| m.fs.clone())
                .or_else(|| self.root_fs.borrow(cs).borrow().clone())
        })
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
    ///
    /// "Inside" is asked by *name*, and that is why a hand-over that mounts one
    /// device twice needs care: a mount whose point lives in the second tree is
    /// reached by a name the promoted one does not prefix (`/mnt/share` while
    /// the disk is handed over from `/sysroot`), and this test drops it. The
    /// initramfs's script mounts the host share *onto the new root* for exactly
    /// that reason (`assets/initramfs/core/init`).
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
            *self.root_fs.borrow(cs).borrow_mut() = Some(m.fs.clone());
            *self.root_read_only.borrow(cs).borrow_mut() = m.read_only;
            // And its device number travels with it for the same reason: `/` is
            // the ext4 partition the init mounted, so this is where the root's
            // `st_dev` stops being an anonymous number and becomes `254:2`.
            *self.root_dev.borrow(cs).borrow_mut() = m.dev;
            // The root is that mount now, and mountinfo names it by that id
            // rather than by a number of its own.
            *self.root_mount_id.borrow(cs).borrow_mut() = m.mount_id;
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
            let mount_id = self.next_mount_id();
            mounts.push(Mount {
                at,
                root: fs.root(),
                fstype: alloc::string::String::from(fstype),
                source: alloc::string::String::from(source),
                id,
                mount_id,
                dev,
                read_only,
                fs,
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
        // A bind borrows the serving instance, so a source path with no
        // filesystem behind it has nothing to alias. That is only reachable
        // before `mount_root`, which is not a state this table serves.
        let fs = self.fs_serving_path(source_path).ok_or(VfsError::NoRoot)?;
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
            let mount_id = self.next_mount_id();
            mounts.push(Mount {
                at,
                root: source,
                fstype,
                source: alloc::string::String::from(source_path),
                id,
                mount_id,
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
            entry.fs.set_read_only(read_only);
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
    /// The directory `path` names as a mountpoint, resolved for a caller: the
    /// last component looked up in its parent rather than walked through, so a
    /// bind's target may be a name that does not exist yet in the sense of a
    /// mount - but must still be a directory. `None` start means the machine's
    /// tree (`/` answers this table's root); `Some` means the caller's cwd.
    pub(crate) fn mountpoint_at(
        &self,
        path: &str,
        start: Option<Arc<dyn Inode>>,
    ) -> VfsResult<Arc<dyn Inode>> {
        let trimmed = path.trim_end_matches('/');
        if trimmed.is_empty() {
            // The target of a bind can never be `/` itself (the kernel has
            // one root slot, and no mountpoint inode names it), but the
            // mountpoint of a path *is* wherever the walk starts for "/".
            // The caller's root, not the machine's: a chrooted task's "/" is
            // its own jail, and `mount --bind x /` inside that chroot names it.
            return self.caller_root().ok_or(VfsError::NoRoot);
        }
        let (parent_path, name) = split_last(trimmed);
        let parent = match start {
            Some(s) => self.resolve_caller(parent_path, s)?,
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
    pub(crate) fn unmount(&self, path: &str) -> VfsResult<()> {
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
    pub(crate) fn pivot_root(&self, new_root: &str, put_old: &str) -> VfsResult<()> {
        let old_root = critical_section::with(|cs| self.root.borrow(cs).borrow().clone())
            .ok_or(VfsError::NoRoot)?;
        // Resolved against the *caller*, as `mount(2)` resolves its target: an
        // initramfs's pid 1 runs this with absolute paths from its own cwd, and
        // today that lands on the same root either way. The jail form is what
        // keeps the promise honest once the caller can hold a different view.
        let new_root_inode =
            self.resolve_caller(new_root.trim_end_matches('/'), old_root.clone())?;

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

        // What the tree being replaced is served by, read before the promotion
        // below overwrites the slot: the entry parked at `put_old` stands for
        // exactly that tree, so it carries this instance. `mount_root` is what
        // puts one there, and a pivot with no root filesystem is a pivot with
        // nothing to park.
        let demoted_fs = critical_section::with(|cs| self.root_fs.borrow(cs).borrow().clone())
            .ok_or(VfsError::NoRoot)?;

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
                        m.mount_id,
                    )
                })
            {
                *self.root_fstype.borrow(cs).borrow_mut() = Some(entry.0);
                *self.root_source.borrow(cs).borrow_mut() = Some(entry.1);
                *self.root_read_only.borrow(cs).borrow_mut() = entry.2;
                *self.root_fs.borrow(cs).borrow_mut() = Some(entry.3);
                *self.root_dev.borrow(cs).borrow_mut() = entry.4;
                *self.root_mount_id.borrow(cs).borrow_mut() = entry.5;
            }
            // Every mount in the table was attached somewhere in the old tree -            // /dev, /tmp, the new root itself - and none of them can be reached
            // once the root is replaced. Keeping them would leave entries
            // anchored at nodes nothing resolves to, holding whole filesystems
            // alive for good. The new tree starts with the one mount below plus
            // whatever the caller attaches next.
            mounts.clear();
            let id = self.next_fs_id();
            let dev = self.next_anon_dev();
            let mount_id = self.next_mount_id();
            mounts.push(Mount {
                at: put_old_inode.clone(),
                root: old_root,
                fstype: alloc::string::String::from("ramfs"),
                source: alloc::string::String::from("initramfs"),
                id,
                mount_id,
                dev,
                read_only: false,
                fs: demoted_fs.clone(),
                bind: false,
            });
            *self.root.borrow(cs).borrow_mut() = Some(new_root_inode);
        });
        self.refresh_mount_points();
        Ok(())
    }

    /// Walk `path` from the root, crossing mounts as they are met.
    pub(crate) fn resolve(&self, path: &str) -> VfsResult<Arc<dyn Inode>> {
        let root = self.root_opt().ok_or(VfsError::NoRoot)?;
        self.resolve_walk(path, root.clone(), Some(root), self)
    }

    /// The root inode as it stands, or `None` before a filesystem is mounted -
    /// the graceful form every walk wants, where [`Self::root_inode`] panics for
    /// the one caller that cannot run without a root.
    fn root_opt(&self) -> Option<Arc<dyn Inode>> {
        critical_section::with(|cs| self.root.borrow(cs).borrow().clone())
    }

    /// The root this **caller** is jailed to: its own `chroot` root when it has
    /// one, else this table's root. One function because there is one question
    /// - "where does an absolute path begin for whoever is asking" - and two
    /// answers by context: a syscall arrives with a task whose slot may carry a
    /// re-homed directory (`sched::current_root`, which itself falls back to the
    /// machine's root), and everything else - boot code running before any task
    /// exists, a host test driving a bare tree - means the table's own.
    ///
    /// This is the jail half of every resolution; the *table* half is separate
    /// by design. The mount set is still the machine's single table here, so a
    /// chrooted task's walk crosses mounts exactly as an unjailed one does:
    /// `pivot_root`/`switch_root` can only move `/` from outside any chroot, and
    /// making the crossing follow the chroot alone would hand a chrooted pid 1 a
    /// live view of mounts its tree no longer contains. When the table itself
    /// becomes per-task (the MNT namespace step), the two halves meet at this
    /// one call site - the task's context carries both its root and its table.
    fn caller_root(&self) -> Option<Arc<dyn Inode>> {
        process_fs_context().1.or_else(|| self.root_opt())
    }

    /// Resolve `path` against a caller's start, jailed to *that table's* root.
    /// The three sibling walks below differ only in what they hand back; all
    /// three take their jail here rather than through [`Vfs::resolve_from`],
    /// because `resolve_from` is the machine-rooted walk - the shape a boot step
    /// or a host test on an explicit tree means - and these are the shapes a
    /// syscall reaches with its caller's cwd. Keeping the two apart is what
    /// makes "resolved against the caller" mean the same thing everywhere.
    pub fn resolve_caller(&self, path: &str, start: Arc<dyn Inode>) -> VfsResult<Arc<dyn Inode>> {
        self.resolve_walk(path, start, self.caller_root(), self)
    }

    /// [`Vfs::resolve_caller`] handing back the directory the last name lives
    /// in and the name itself - Linux's `user_path_parent` shape, the one every
    /// make-or-remove call resolves through. See
    /// [`Vfs::resolve_parent_from`] for why the walk stops before the name.
    pub fn resolve_caller_parent(
        &self,
        path: &str,
        start: Arc<dyn Inode>,
    ) -> VfsResult<(Arc<dyn Inode>, String)> {
        let (parent_path, name) = split_last(path);
        if name.is_empty() {
            return Err(VfsError::Invalid);
        }
        // A trailing `.` or `..` names the directory itself, which is ordinary
        // resolution (`unlink(".")` is what Linux refuses as EISDIR later).
        if name == "." || name == ".." {
            let dir = self.resolve_caller(path, start)?;
            return Ok((dir, String::from(name)));
        }
        let parent = self.resolve_caller(parent_path, start)?;
        // The directory the name will land in, checked to *be* one: a bind or
        // an unlink under a plain file is `ENOTDIR` from here, never the
        // `EACCES` a search through a file's absent execute bit would give.
        crate::vfs::require_directory(&parent)?;
        Ok((parent, String::from(name)))
    }

    /// [`Vfs::resolve_caller`] without following a symbolic link in the **last**
    /// component - what `lstat`, `readlink` and `symlink` need. Same rules as
    /// [`Vfs::resolve_no_follow`], jailed like every other caller-shaped walk.
    pub fn resolve_caller_no_follow(
        &self,
        path: &str,
        start: Arc<dyn Inode>,
    ) -> VfsResult<Arc<dyn Inode>> {
        let (parent_path, name) = split_last(path);
        if name.is_empty() || name == "." || name == ".." {
            return self.resolve_caller(path, start);
        }
        let parent = self.resolve_caller(parent_path, start)?;
        if parent.metadata().kind != NodeKind::Dir {
            return Err(VfsError::NotDir);
        }
        parent.lookup(name)
    }

    /// Walk `path` from `start` instead of the root - which is what a
    /// relative path means, and why a process carrying a cwd needs this
    /// rather than [`resolve`](Self::resolve).
    ///
    /// An absolute path ignores `start` entirely and begins at the global root,
    /// as `/` must. [`resolve_walk`](Self::resolve_walk) is the same walk with
    /// the root it starts from, its `..` clamp, and its absolute-symlink restart
    /// all named by one `jail` argument; this is that walk with the jail being
    /// the machine's own root, which is what a caller with no process context -
    /// the mount bookkeeping, a host test on a bare tree - means.
    pub(crate) fn resolve_from(
        &self,
        path: &str,
        start: Arc<dyn Inode>,
    ) -> VfsResult<Arc<dyn Inode>> {
        self.resolve_walk(path, start, self.root_opt(), self)
    }

    /// Walk `path` from `start`, treating `jail` as the root: the directory an
    /// absolute path begins at, the node whose `..` is itself, and where an
    /// absolute symlink target restarts from.
    ///
    /// `jail` is a *process's* root, kept apart from the machine's so a
    /// `chroot(2)` re-homes only the calling task - see the mount-crossing note
    /// below, which is unchanged by it. `None` is "no root is mounted": the
    /// graceful answer is `NoRoot` wherever the walk needs a root to begin, and
    /// a `..` with nothing to clamp against behaves as it did before a root
    /// existed.
    ///
    /// The mount crossing is the same walk as ever, one node deeper in the tree.
    /// The walk itself. `jail` says where an absolute path begins and what `..`
    /// clamps against; `table` is *which mount set the crossing consults* — the
    /// caller's own once it has unshared one, this table otherwise. They are two
    /// arguments because they are two facts a task carries together
    /// (`process_fs_context`) and a resolution that crossed mounts in the
    /// machine's table while jailing to a private root would show one namespace's
    /// mounts inside another's tree.
    fn resolve_walk(
        &self,
        path: &str,
        start: Arc<dyn Inode>,
        jail: Option<Arc<dyn Inode>>,
        table: &MountTable,
    ) -> VfsResult<Arc<dyn Inode>> {
        let absolute = path.starts_with('/');
        let start = if absolute {
            jail.clone().ok_or(VfsError::NoRoot)?
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
                        // The *last* component is not walked through,
                        // though - Linux's `link_path_walk` looks nothing up
                        // once the names run out, which is why
                        // `stat("/etc/hostname/..")` answers the parent while
                        // `open("/etc/hostname/../x")` reads ENOTDIR. A
                        // trailing `..` therefore leaves the chain standing
                        // where it is rather than asking to leave it; the
                        // node itself gets searched when a name follows.
                        if !todo.is_empty()
                            && let Some(here) = stack.last()
                        {
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
                        } else if !jail.as_ref().is_some_and(|j| Arc::ptr_eq(j, &stack[0])) {
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
                                let mounts = table.mounts.borrow(cs).borrow();
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
                            // A link that carries a *payload* is followed to
                            // what it denotes rather than spliced by its
                            // target text, because its target is a report and
                            // not a path to reopen. A descriptor's second name
                            // - `/proc/<pid>/fd/N`, and the `/dev/std*` links
                            // into it - denotes the file the descriptor holds,
                            // and that is Linux's own resolution: measured on a
                            // current host (`.workbuddy/probe_fdlink.c`),
                            // `stat` of such a name reports the file where
                            // `lstat` reports the link, `truncate`/`chmod`/
                            // `utimensat`/`access` act on the file, a name
                            // *inside* a path continues through it
                            // (`/proc/self/fd/<dirfd>/<name>` names the child of
                            // the directory the descriptor holds), and a
                            // directory's second name is a directory to
                            // `chdir`. The walk is where that belongs: every
                            // one of those doors comes through here, and a door
                            // that had to ask separately would be a second
                            // answer to one question.
                            //
                            // A *namespace* link is the second payload, and what
                            // it denotes is the namespace object itself - so the
                            // walk resolves it here and hands on the
                            // namespace-shaped file that *pins* it
                            // ([`crate::vfs::ns::NsFile`]): the object is
                            // resolved once, at the door that asked for it, and
                            // every later reader of that descriptor (a `setns`
                            // above all) acts on the namespace that was named
                            // rather than on wherever the process is by then.
                            // This is Linux's own split between the two doors:
                            // `lstat`/`readlink` stop at the procfs link and keep
                            // answering live, while `stat` and `open` see the
                            // nsfs file (measured, `.workbuddy/probe_ns_fd_stat.c`).
                            if let Some(fd) = child.as_fd() {
                                let denoted = crate::vfs::file::descriptor_inode(fd)?;
                                stack.push(denoted);
                                continue;
                            }
                            if let Some(ns) = child.as_namespace() {
                                stack.push(crate::vfs::ns::NsFile::opened(ns));
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
                                // An absolute target names a path from the
                                // walk's root, so the names after it are looked
                                // up there and the walk to the link - and any
                                // bind alias it came through - is behind it.
                                stack.truncate(1);
                                stack[0] = jail.clone().ok_or(VfsError::NoRoot)?;
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

    /// Every mount, as `/proc/mounts` and `/proc/self/mountinfo` report it: the
    /// root first, then the table in mounting order.
    ///
    /// One walk serves both files. They are two renderings of one fact - what
    /// is mounted where - and a second walk that derived the same thing would
    /// be a second answer to the same question; `mounts` prints four of these
    /// fields and mountinfo wants the ids and the device as well.
    pub fn mount_records(&self) -> Vec<MountRecord> {
        // The table is copied out first: naming each mount point walks the
        // tree, and that walk takes the same lock. Nested takes of the one
        // critical section are not something to lean on.
        let (root_fstype, root_source, root_read_only, root_dev, root_mount_id, entries) =
            critical_section::with(|cs| {
                let entries: Vec<MountEntry> = self
                    .mounts
                    .borrow(cs)
                    .borrow()
                    .iter()
                    .map(|m| MountEntry {
                        at: m.at.clone(),
                        mount_id: m.mount_id,
                        dev: m.dev,
                        source: m.source.clone(),
                        fstype: m.fstype.clone(),
                        read_only: m.read_only,
                        bind: m.bind,
                    })
                    .collect();
                (
                    self.root_fstype.borrow(cs).borrow().clone(),
                    self.root_source.borrow(cs).borrow().clone(),
                    *self.root_read_only.borrow(cs).borrow(),
                    *self.root_dev.borrow(cs).borrow(),
                    *self.root_mount_id.borrow(cs).borrow(),
                    entries,
                )
            });
        let mut records: Vec<MountRecord> = Vec::new();
        if let Some(fstype) = root_fstype {
            // The root's source, when no mount named one: Linux prints the
            // filesystem's own *type* there, so an initramfs's `/` reads
            // `rootfs / rootfs rw 0 0`. A root the kernel installed was never
            // given a device or a tag to be named by, and leaving the line out
            // - which is what requiring a source here did - tells every reader
            // of both files that `/` is not mounted at all. `switch_root`'s own
            // `grep` for the root in `/proc/mounts` is one of them.
            let source = root_source.unwrap_or_else(|| fstype.clone());
            records.push(MountRecord {
                mount_id: root_mount_id,
                // Filled in below, with every other mount's.
                parent_id: root_mount_id,
                dev: root_dev,
                source,
                path: alloc::string::String::from("/"),
                fstype,
                options: mount_options(root_read_only, false),
            });
        }
        for entry in entries {
            if let Some(path) = self.path_of(&entry.at) {
                records.push(MountRecord {
                    mount_id: entry.mount_id,
                    parent_id: entry.mount_id,
                    dev: entry.dev,
                    source: entry.source,
                    path,
                    fstype: entry.fstype,
                    options: mount_options(entry.read_only, entry.bind),
                });
            }
        }
        // A mount hangs under the deepest *other* mountpoint that covers it;
        // the root, which none of them does, hangs under itself - which is how
        // Linux reports the root of the namespace.
        let parents: Vec<u64> = (0..records.len())
            .map(|i| {
                let me = &records[i];
                records
                    .iter()
                    .filter(|other| {
                        other.mount_id != me.mount_id
                            && mountpoint_covers(&other.path, &me.path)
                            && other.path.len() < me.path.len()
                    })
                    .max_by_key(|other| other.path.len())
                    .map_or(me.mount_id, |other| other.mount_id)
            })
            .collect();
        for (record, parent) in records.iter_mut().zip(parents) {
            record.parent_id = parent;
        }
        records
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
    /// The directory `target` sits in, found by walking down from this table's
    /// root — the one parent question the chain cannot answer for itself (a
    /// `..` at a walk's base). Bounded below; see the constant.
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
        // A name cannot be made or removed in something that is not a
        // directory, and the answer is the tree's rather than the caller's -
        // see [`require_directory`](crate::vfs::require_directory).
        crate::vfs::require_directory(&parent)?;
        Ok((parent, String::from(name)))
    }

    /// [`resolve_parent`](Self::resolve_parent) with a caller-supplied start -
    /// the same "the last component is a name, not a step" walk that `unlink`
    /// needs through a process's cwd. Linux's `user_path_parent` resolves the
    /// directory and hands the final name to the operation, so
    /// `unlink("/etc/hostname/x")` answers ENOTDIR from the lookup of `x`
    /// rather than EACCES from searching the plain file it walked through.
    pub fn resolve_parent_from(
        &self,
        path: &str,
        start: Arc<dyn Inode>,
    ) -> VfsResult<(Arc<dyn Inode>, String)> {
        let (parent_path, name) = split_last(path);
        if name.is_empty() {
            return Err(VfsError::Invalid);
        }
        // A trailing `.` or `..` names the directory itself, which is ordinary
        // resolution (`unlink(".")` is what Linux refuses as EISDIR later).
        if name == "." || name == ".." {
            let dir = self.resolve_from(path, start)?;
            return Ok((dir, String::from(name)));
        }
        let parent = self.resolve_from(parent_path, start)?;
        // The directory the name will land in, checked to *be* one: a bind or
        // an unlink under a plain file is `ENOTDIR` from here, never the
        // `EACCES` a search through a file's absent execute bit would give.
        crate::vfs::require_directory(&parent)?;
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

impl Default for MountTable {
    fn default() -> MountTable {
        MountTable::new()
    }
}

/// The machine's one mount table — the namespace every task starts in until it
/// unshares one of its own. A value rather than a bare `static` because a task's
/// slot holds an `Arc` to *some* table, and the machine's has to be an `Arc` too
/// for the two to be interchangeable; everything that used to reach `&'static`
/// (`global()`) keeps working, since the inner table is still reachable through
/// the handle.
/// The machine's table, behind a handle rather than in a bare `static`: a
/// task's namespace slot holds an `Arc<MountTable>`, and the machine's own has
/// to be an `Arc` too for the two to answer the same questions. The value is
/// built on first use and never replaced; cloning the handle shares the state,
/// which is exactly what fork inheritance means.
///
/// One critical section for the whole slot because the lock discipline this
/// module keeps everywhere else is `critical_section::with` — a mutex on both
/// targets, reentrant-kind on the host — and a second kind of lock here would be
/// a second thing to reason about for a value that is written exactly once.
static STATIC: Mutex<RefCell<Option<Arc<MountTable>>>> = Mutex::new(RefCell::new(None));

/// The machine's table, initializing it on first use. Every free function here
/// answers "the caller's table, or this one" through this call, which is what
/// makes the namespace routing have exactly one decision point.
///
/// `spin::Once` is the repo's one-time-init primitive (the IDT and the GDT use
/// it for hardware); here it holds the *value* the machine table is, and every
/// call hands out an `Arc` over the same inner state. Cloning the `Arc` is the
/// cheap half; cloning the table would be the bug — two handles to one set of
/// mounts is a shared namespace, which is what fork inheritance means.
pub fn machine_table() -> Arc<MountTable> {
    critical_section::with(|cs| {
        let mut slot = STATIC.borrow(cs).borrow_mut();
        if slot.is_none() {
            *slot = Some(Arc::new(MountTable::new()));
        }
        slot.as_ref().expect("just filled").clone()
    })
}

/// The kernel's one instance, for callers that want the machine's table by
/// borrow rather than by handle — the initramfs unpacker, a host test naming
/// the fallback. A task's *own* table is reached through the free functions,
/// which route by `process_fs_context`; nothing outside this module should
/// resolve against `global()` directly from syscall context.
pub fn global() -> &'static MountTable {
    // The `Arc` is leaked to a `'static` borrow on first use, once. A handle
    // that lived only inside the slot would dangle the moment it was returned,
    // and the callers of this function (`tar::unpack_into`, host tests naming
    // the fallback table) want the same shape they always had: a borrow of the
    // machine's one table. Leaking it is honest — the machine's namespace is
    // never torn down while the kernel runs.
    &*alloc::boxed::Box::leak(alloc::boxed::Box::new(machine_table()))
}

/// Install the kernel's root filesystem. Once, at boot.
///
/// The instance travels, not just a borrow of it: see [`Vfs::mount_root`] for
/// what a root recorded without one costs.
pub fn mount_root(fs: Arc<dyn FileSystem>) {
    machine_table().mount_root(fs);
}

/// Mount `fs` at the directory named by `path` in the kernel's tree.
pub fn mount(path: &str, fs: Arc<dyn FileSystem>) -> VfsResult<()> {
    caller_table().mount(path, fs)
}

/// Unmount the filesystem mounted at `path`.
///
/// The path must name a mountpoint - a directory that has a filesystem mounted
/// on it. The mount is removed from the mount table, and subsequent accesses
/// to files under `path` will see the underlying directory in the parent
/// filesystem instead.
pub fn unmount(path: &str) -> VfsResult<()> {
    caller_table().unmount(path)
}

/// Replace the root filesystem: move `new_root` to `/`, and move the old root
/// to `put_old` inside the new root, so it can be unmounted afterward.
///
/// This is the kernel's pivot_root(2): after this call, `/` refers to what was
/// previously `new_root`, and `put_old` holds the old root filesystem's
/// contents, ready to be unmounted.
pub fn pivot_root(new_root: &str, put_old: &str) -> VfsResult<()> {
    caller_table().pivot_root(new_root, put_old)
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
    caller_table().mount_at(at, fs, fstype, source, read_only, dev)
}

/// `mount -o remount,...`: change a mounted filesystem's flags.
pub fn remount(target: &str, start: Option<Arc<dyn Inode>>, read_only: bool) -> VfsResult<()> {
    let at = resolve_opt(target, start)?;
    caller_table().remount(&at, read_only)
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
    let at = caller_table().mountpoint_at(target, start)?;
    caller_table().bind_mount(at, src, source, read_only)
}

/// Unmount the filesystem at `path`, resolved against a caller's cwd.
pub fn unmount_opt(path: &str, start: Option<Arc<dyn Inode>>) -> VfsResult<()> {
    let resolved = resolve_opt(path, start)?;
    caller_table().unmount_resolved(&resolved)
}

/// Every mount, as `/proc/mounts` and `/proc/self/mountinfo` report it.
pub fn mount_records() -> Vec<MountRecord> {
    caller_table().mount_records()
}

/// The filesystem instance id serving `path`, with its type. This is the
/// instance's *identity*; what `stat` reports for the path is [`dev_of_path`].
pub fn fs_of_path(path: &str) -> (u64, alloc::string::String) {
    caller_table().fs_of_path(path)
}

/// What `stat`'s `st_dev` reports for `path`: the number of the device behind
/// the filesystem that serves it, or an anonymous number for a pseudo-filesystem.
pub fn dev_of_path(path: &str) -> u64 {
    caller_table().dev_of_path(path)
}

/// The same, for an inode with no path - `fstat(2)`'s descriptor shape.
pub fn dev_of_inode(inode: &Arc<dyn Inode>) -> u64 {
    caller_table().dev_of_inode(inode)
}

/// The space accounting of the filesystem that serves `path`, straight from
/// the instance. `None` when the serving filesystem has no real numbers
/// (ramfs and friends answer through the syscall layer instead).
pub fn fs_stats_of_path(path: &str) -> Option<super::FsStats> {
    caller_table()
        .fs_serving_path(path)
        .and_then(|fs| fs.statfs())
}

/// The same, for an inode with no path - `fstatfs(2)`'s descriptor shape.
pub fn fs_stats_of_inode(inode: &Arc<dyn Inode>) -> Option<super::FsStats> {
    caller_table()
        .fs_serving_inode(inode)
        .and_then(|fs| fs.statfs())
}

/// The type of the filesystem a node belongs to, by identity: a mount's own
/// root names that mount; anything else names the root filesystem.
pub fn fstype_of_inode(inode: &Arc<dyn Inode>) -> alloc::string::String {
    // The caller's table first: which filesystem *type* a node belongs to is a
    // fact about the mounts that name it, and two tables can name one inode
    // differently (a mount in one namespace, plain directory in another).
    let table = process_fs_context().0.unwrap_or_else(machine_table);
    let (root_fstype, hits) = critical_section::with(|cs| {
        let fstype = table
            .root_fstype
            .borrow(cs)
            .borrow()
            .clone()
            .unwrap_or_else(|| alloc::string::String::from("unknown"));
        let hits: Vec<alloc::string::String> = table
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
    let table = process_fs_context().0.unwrap_or_else(machine_table);
    critical_section::with(|cs| {
        table
            .mounts
            .borrow(cs)
            .borrow()
            .iter()
            .find(|m| Arc::ptr_eq(&m.root, inode))
            .map(|m| m.id)
            .unwrap_or(0)
    })
}

/// The directory `chroot(2)` names, resolved against the caller's current root
/// and checked to be a directory. Making it the caller's root is the handler's
/// job (a per-process view held in `sched`); this only answers *which* inode and
/// refuses a non-directory, so it can name nothing outside the process's tree.
pub fn chroot_target(path: &str, cwd: &Arc<dyn Inode>) -> VfsResult<Arc<dyn Inode>> {
    // Resolved against the caller's *current* root explicitly — `resolve_caller`
    // with the task's own jail — because this is the one walk whose answer
    // re-homes the task: it must see what the task sees today, not whatever the
    // fallback would pick. The handler checks CAP_SYS_CHROOT before calling and
    // installs the returned inode through `sched::set_root`.
    let (table, root) = process_fs_context();
    let table = table.unwrap_or_else(machine_table);
    let jail = root.or_else(|| table.root_opt());
    let inode = table.resolve_walk(path, cwd.clone(), jail, &table)?;
    if inode.metadata().kind != NodeKind::Dir {
        return Err(VfsError::NotDir);
    }
    Ok(inode)
}

/// `mount(2)`'s MS_MOVE, free-function spelling: the filesystem mounted at
/// `at` becomes the root.
pub fn move_root(at: &Arc<dyn Inode>) -> VfsResult<()> {
    caller_table().move_root(at)
}

/// Re-home the mount serving `source` on the directory `at`.
pub fn move_mount(source: &Arc<dyn Inode>, at: &Arc<dyn Inode>) -> VfsResult<()> {
    caller_table().move_mount(source, at)
}

/// Resolve `path` in the kernel's (machine's) tree. The boot code, which runs
/// before any task exists and so has no per-process root, starts every walk
/// here; a syscall resolves against its caller's root through `resolve_at`.
pub fn resolve(path: &str) -> VfsResult<Arc<dyn Inode>> {
    caller_table().resolve(path)
}

/// Resolve `path` starting from `start` - a process's cwd - and jailed to that
/// process's root. An absolute path ignores the start and begins at the caller's
/// `chroot` root (the machine's root for any task that never called one), as `/`
/// must, and a `..` at that root stays there rather than climbing out.
pub fn resolve_at(path: &str, start: Arc<dyn Inode>) -> VfsResult<Arc<dyn Inode>> {
    // The caller's own table when it has one; the machine's otherwise. Same
    // rule as [`resolve`] below for every other free function here.
    let (table, _) = process_fs_context();
    let table = table.unwrap_or_else(machine_table);
    table.resolve_caller(path, start)
}

/// The directory `path` names its last component in, resolved through
/// [`resolve_at`] so a `dirfd`-relative call sees the caller's tree. See
/// [`Vfs::resolve_parent_from`] for why the walk stops before the name.
pub fn resolve_parent_at(path: &str, start: Arc<dyn Inode>) -> VfsResult<(Arc<dyn Inode>, String)> {
    let (table, _) = process_fs_context();
    let table = table.unwrap_or_else(machine_table);
    table.resolve_caller_parent(path, start)
}

/// Resolve `path` from `start` **without following the last name**, through the
/// caller's table.
///
/// The free twin of [`Vfs::resolve_caller_no_follow`], and the one a syscall
/// must use: this module's header states the rule — *nothing outside it resolves
/// against [`global`] from syscall context* — and the reason is a namespace. A
/// task that unshared its mounts has mounts the machine's table does not, so a
/// probe that asked the machine's table would answer about a tree the caller is
/// not standing in. That is not hypothetical: a dozen path-taking handlers asked
/// `global()` here, and the divergence is sharp rather than subtle, because the
/// *following* walks were caller-routed already. Inside a namespace with a
/// filesystem mounted, `stat("/mnt/f")` saw the mount and `lstat("/mnt/f")` did
/// not — the same path, two answers, and the wrong one for every `ls -l` in a
/// container.
pub fn resolve_no_follow_at(path: &str, start: Arc<dyn Inode>) -> VfsResult<Arc<dyn Inode>> {
    let (table, _) = process_fs_context();
    let table = table.unwrap_or_else(machine_table);
    table.resolve_caller_no_follow(path, start)
}

/// The filesystem serving `inode` **in the caller's table** - the identity test
/// `fstatfs` and `linkat`'s `EXDEV` ask, on the same rule as
/// [`resolve_no_follow_at`] above.
pub fn fs_serving_inode_of(inode: &Arc<dyn Inode>) -> Option<Arc<dyn FileSystem>> {
    caller_table().fs_serving_inode(inode)
}

/// The calling task's mount context: `(table, root)`, both `None` when the
/// caller has neither — which is every boot step (running before any task
/// exists), every kernel-side walk, and every host test driving an explicit
/// table. On bare metal this reads the running task's slots once; on the host it
/// answers `(None, None)` because a host resolution has no per-task namespace to
/// consult and each test already names the table it walks.
///
/// One call for two facts because one resolution needs both: the table decides
/// which mounts exist to cross into, and the root decides where an absolute path
/// begins and what `..` clamps against. Keeping them apart was fine while every
/// caller shared the machine's table; with namespaces they move together or a
/// chrooted task would jail its paths into one table while crossing mounts in
/// another.
#[cfg(target_os = "none")]
pub(crate) fn process_fs_context() -> (Option<Arc<MountTable>>, Option<Arc<dyn Inode>>) {
    crate::sched::current_fs_context()
}

#[cfg(not(target_os = "none"))]
pub(crate) fn process_fs_context() -> (Option<Arc<MountTable>>, Option<Arc<dyn Inode>>) {
    (None, None)
}

/// The table a resolution or a mount operation answers to: the calling task's
/// own when it has unshared one, the machine's otherwise (`None` off any task —
/// boot code, and every host test driving an explicit tree). One function because
/// that question is asked by every free function below, and a namespace step
/// that answered it in twelve places is how two of them start disagreeing about
/// which table a syscall saw.
pub(crate) fn caller_table() -> Arc<MountTable> {
    process_fs_context().0.unwrap_or_else(machine_table)
}

#[cfg(all(test, not(target_os = "none")))]
mod tests {
    use alloc::sync::Arc;

    use super::{MountTable, split_last};
    use crate::vfs::ramfs::RamFs;
    use crate::vfs::{FileSystem, NodeKind, VfsError};

    fn tree_with_root() -> Arc<MountTable> {
        let vfs = Arc::new(MountTable::new());
        vfs.mount_root(RamFs::new());
        vfs
    }

    #[test]
    fn two_tables_do_not_see_each_others_mounts() {
        // The mount table is a *value*, and this is the counterfactual that says
        // so: with one shared table, `a`'s mount answers `b`'s walk and both
        // records lists. Every later per-task namespace step rests on this being
        // true of the type itself rather than of a caller's discipline — a task
        // holding its own table cannot leak through any path the free functions
        // take, because the free functions all name the one table they forward
        // to. That forwarding list is what makes "which table does a syscall see"
        // answerable in one place, so it is pinned here too: `STATIC` is the
        // kernel's table, and nothing else exists behind the free functions.
        let a = tree_with_root();
        let b = tree_with_root();
        // `mount(2)` names an existing directory, so both trees carry the
        // mountpoint before either takes a mount — the same precondition every
        // other mount test in this module builds by hand.
        for vfs in [&a, &b] {
            vfs.resolve("/")
                .unwrap()
                .create("mnt", NodeKind::Dir)
                .expect("the shared mountpoint directory");
        }
        let tmp_a = RamFs::new();
        tmp_a
            .root()
            .create("from-a", NodeKind::File)
            .expect("the mounted fs is writable");
        a.mount("/mnt", tmp_a).unwrap();

        assert!(
            a.resolve("/mnt/from-a").is_ok(),
            "the mounting table sees its own mount"
        );
        assert_eq!(
            b.resolve("/mnt/from-a").err(),
            Some(VfsError::NoEntry),
            "the other table does not"
        );
        assert_eq!(a.mount_records().len(), 2, "one root slot + one mount");
        assert_eq!(
            b.mount_records().len(),
            1,
            "and the second table never grew a record from the first's mount"
        );
        assert_ne!(
            a.fs_of_path("/mnt").0,
            b.fs_of_path("/mnt").0,
            "instance identity is per-table: the same name serves different facts"
        );
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
        // The walk below climbs out of a mount and back through it, and every
        // `..` asks the search bit of the directory it leaves. A real root
        // arrives with traversable bits - the tar unpack stamps them, an ext4
        // superblock carries them - so give this fixture's two hand-built
        // roots the same ones instead of leaning on "unset means unpoliced"
        // for directories the test walks *through*.
        vfs.resolve("/").unwrap().set_mode(0o755);
        let mnt_dir = vfs
            .resolve("/")
            .unwrap()
            .create("mnt", NodeKind::Dir)
            .unwrap();
        // The mountpoint is walked *through* on the way back in, and a
        // directory created by hand keeps no mode until something says one.
        mnt_dir.set_mode(0o755);
        let inner = RamFs::new();
        // The mounted root is a filesystem built by hand too, and the walk
        // below passes *through* it on its way out of the mount (`..` asks
        // the search bit of every directory it leaves). A real mount's root
        // arrives traversable - an ext4 superblock's `/`, a tar's unpacked
        // top - so give this one the same bits rather than lean on "unset
        // means unpoliced" for a directory the test walks through.
        inner.root().set_mode(0o755);
        inner.root().create("hello", NodeKind::File).unwrap();
        vfs.mount("/mnt", inner.clone()).unwrap();

        let mnt = vfs.resolve("/mnt").unwrap();
        assert!(Arc::ptr_eq(&mnt, &inner.root()));
        assert!(Arc::ptr_eq(
            &vfs.resolve("/mnt/hello").unwrap(),
            &inner.root().lookup("hello").unwrap()
        ));

        // `..` from inside the mount lands on the mountpoint's parent, then
        // walks back in through the mount edge. The walk leaves a file by
        // searching it - Linux asks the execute bit of every node it passes
        // *through*, and `hello` is a plain file with no bits for anyone, so
        // leaving it is what says Access here, not the mount. Give it the
        // traversable shape and the climb answers as the machine does: out to
        // the mounted root, up to the outer one, back in through the mount
        // edge. (On a real boot a relative path starts at a process's cwd -
        // always a directory - and no walk ever leaves a file.)
        let hello = vfs.resolve("/mnt/hello").unwrap();
        hello.set_mode(0o755);
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
        vfs.mount("/run", live.clone()).unwrap();
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

        vfs.mount("/d", inner.clone()).unwrap();
        assert_eq!(vfs.mount("/d", inner.clone()).unwrap_err(), VfsError::Busy);
        assert_eq!(
            vfs.mount("/f", inner.clone()).unwrap_err(),
            VfsError::NotDir
        );
    }

    #[test]
    fn resolving_before_a_root_exists_is_noroot() {
        let vfs = MountTable::new();
        assert_eq!(vfs.resolve("/").err(), Some(VfsError::NoRoot));
        assert_eq!(vfs.mount("/", RamFs::new()).err(), Some(VfsError::NoRoot));
    }
    /// Build `/bin/busybox` as a file and `/bin/ls`, `/bin/sh` as links to it -
    /// the shape a busybox rootfs is made of.
    fn tree_with_busybox() -> Arc<MountTable> {
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

    /// The directory a name is made or removed in has to *be* a directory.
    ///
    /// The counterfactual is the one the guest sweep found: `bind` under a
    /// plain file reached `may_create_in` first and answered `EACCES` - the
    /// search of a file carrying no execute bit - where Linux answers
    /// `ENOTDIR`. The walk refuses a component that cannot hold children
    /// *before* any permission is asked, and this pins that order for the
    /// split every make-or-remove goes through.
    #[test]
    fn a_parent_that_is_not_a_directory_is_not_dir() {
        let vfs = tree_with_root();
        let root = vfs.resolve("/").unwrap();
        root.create("plain", NodeKind::File).unwrap();
        // Absolute resolution, the door `bind` and `unlink` come through.
        assert_eq!(vfs.resolve_parent("/plain/x").err(), Some(VfsError::NotDir));
        // And the cwd-relative door a `dirfd` call uses, which resolves the
        // parent from the caller's directory rather than the root.
        assert_eq!(
            vfs.resolve_parent_from("/plain/x", root.clone()).err(),
            Some(VfsError::NotDir),
            "a name cannot be made inside a plain file"
        );
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
        vfs.mount("/mnt", RamFs::new()).unwrap();
        // `usb` is made *inside* the mount, which is the only place it can be
        // seen once the filesystem is on top of `/mnt`.
        let mnt = vfs.resolve("/mnt").unwrap();
        mnt.create("usb", NodeKind::Dir).unwrap();
        vfs.mount("/mnt/usb", RamFs::new()).unwrap();

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
        vfs.mount("/mnt", RamFs::new()).unwrap();

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
        vfs.mount("/mnt", RamFs::new()).unwrap();

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
        let at = vfs.mountpoint_at("/mnt", None).unwrap();
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
            let at = vfs.mountpoint_at(path, None).unwrap();
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

    /// The record `/proc/self/mountinfo` prints carries the same number `stat`
    /// reports for that mount's paths.
    ///
    /// This is the one thing the file is *for* - a tool that finds a mount's
    /// device by `stat`ing its mountpoint, then looks that number up in
    /// mountinfo to find where the device is mounted - and two walks of the
    /// table that could disagree are how it stops being true. They come from one
    /// place, and this is the test that says so.
    #[test]
    fn a_mount_records_device_is_the_one_stat_reports_there() {
        let vfs = tree_with_root();
        vfs.resolve("/")
            .unwrap()
            .create("mnt", NodeKind::Dir)
            .unwrap();
        let at = vfs.mountpoint_at("/mnt", None).unwrap();
        vfs.mount_at(
            at,
            RamFs::new(),
            "ext4",
            "/dev/vda2",
            false,
            Some(crate::uevent::makedev(254, 2)),
        )
        .unwrap();

        // The root is among them, and it is the one mount here that no
        // `mount(2)` named: its source is its own type, which is what Linux
        // prints for a root the kernel installed (`rootfs / rootfs rw 0 0`).
        // Omitting the line because nothing named a source - which is what this
        // did - tells every reader of both files that `/` is not mounted.
        let records = vfs.mount_records();
        assert_eq!(records.len(), 2, "the root, and the one mount made here");
        assert_eq!(records[0].path, "/");
        assert_eq!(records[0].source, "ramfs", "its type, as Linux prints it");
        assert_eq!(
            records[0].parent_id, records[0].mount_id,
            "the root is its own parent"
        );
        assert_eq!(records[1].path, "/mnt");
        for record in &records {
            assert_eq!(
                record.dev,
                vfs.dev_of_path(&record.path),
                "the record for {} must agree with stat there",
                record.path
            );
            assert_eq!(
                record.dev,
                vfs.dev_of_inode(&vfs.resolve(&record.path).unwrap()),
                "and with the descriptor door, for a mount's own root"
            );
        }
        // And the two records really are two numbers, so the agreement above is
        // not everything being one number.
        assert_ne!(
            records[0].dev, records[1].dev,
            "the root is the ramfs, and /mnt the device"
        );
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
        let at = vfs.mountpoint_at("/new", None).unwrap();
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

    use super::MountTable;
    use crate::vfs::NodeKind;
    use crate::vfs::ramfs::RamFs;

    #[test]
    fn a_relative_path_starts_at_the_given_node() {
        let vfs = Arc::new(MountTable::new());
        vfs.mount_root(RamFs::new());
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
