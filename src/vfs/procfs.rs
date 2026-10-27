//! `/proc`: the kernel's own facts, spelled as files.
//!
//! Nothing here is storage. Every read runs a generator that asks the kernel
//! what is true *now* - which is the whole point of the filesystem: a program
//! that wants to know how long the machine has been up, or what is mounted
//! where, reads a file for it, and the answer is current by construction.
//!
//! It is also the filesystem OpenRC's `procfs` service mounts first, and the
//! one half of userland (`mountinfo`, `ps`, anything reading `/proc/self`)
//! quietly assumes is there. A missing `/proc/mounts` is not one service
//! failing; it is every tool that asks "what is mounted" being told nothing.
//!
//! Half of this tree is *not* fixed. `/proc/<pid>` exists because a process
//! does, `/proc/<pid>/fd/3` because a descriptor does, and `/proc/self` is a
//! different directory depending on who reads it. Those directories are built
//! on demand and answered by asking the scheduler, so a listing of `/proc` is
//! a list of the processes running at that moment rather than a shape decided
//! when the filesystem was mounted.

use alloc::borrow::Cow;
use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::cell::RefCell;
use core::sync::atomic::{AtomicU64, Ordering};

use critical_section::Mutex;

use super::{FileSystem, Ino, Inode, Metadata, NodeKind, VfsError, VfsResult};
use crate::mm::vm::MapFlags;

/// What a generated file reads back as: a function of the moment.
///
/// `None` is not "empty", it is "the thing this file reports no longer
/// exists" - a process that exited between the listing that named it and the
/// read that opened it. That is `ENOENT` and not a zero-byte file, because a
/// `ps` that read an empty `/proc/57/stat` would print a process with no name
/// and no parent rather than skipping one that has already gone.
type Generator = Box<dyn Fn() -> Option<String> + Send + Sync>;

/// What a writable generated file does with a write: how `sysctl -w` and
/// OpenRC's `hostname` service name the system.
type Writer = Box<dyn Fn(&str) -> VfsResult<()> + Send + Sync>;

/// A directory's fixed children: each entry with the kind it was added with.
type Entries = Mutex<RefCell<BTreeMap<String, (Arc<dyn Inode>, NodeKind)>>>;

/// One generated file. Its size is its content's, recomputed on every
/// `stat` - a file whose contents are "how long has the machine been up"
/// does not have a size that stays still, and pretending otherwise is the
/// kind of lie a `cp` trips over.
struct ProcFile {
    ino: Ino,
    generate: Generator,
    /// The permission bits Linux gives *this* file, when they are not the
    /// read-only default every generated file here carries. Zero means that
    /// default, which is what every file but `/proc/<pid>/environ` reports.
    mode: u32,
    /// The file's own `st_size`, when the answer is known before the file is
    /// read. Zero means the default: Linux reports no size for a file whose
    /// contents are generated on the read, because the length is not known
    /// until it is asked for and asking is the work.
    size: u64,
}

impl Inode for ProcFile {
    fn metadata(&self) -> Metadata {
        // Size zero, as Linux reports for the files it generates: the answer
        // is not known until it is asked for, and asking is the work. A
        // generator that ran here would run during every directory listing -
        // and /proc/mounts' generator walks the tree, which walks /proc. The
        // one file whose length *is* known when the tree is built says so
        // instead: `/proc/cmdline` is the command line the machine booted
        // with, and Linux reports its length (measured on a current host: 86
        // bytes, the file's own content including its newline).
        Metadata {
            ino: self.ino,
            kind: NodeKind::File,
            size: self.size,
        }
    }

    fn mode(&self) -> u32 {
        if self.mode == 0 {
            PROC_RO_FILE_MODE
        } else {
            self.mode
        }
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> VfsResult<usize> {
        let content = (self.generate)().ok_or(VfsError::NoEntry)?;
        let bytes = content.as_bytes();
        let start = offset.min(bytes.len() as u64) as usize;
        let end = (start + buf.len()).min(bytes.len());
        buf[..end - start].copy_from_slice(&bytes[start..end]);
        Ok(end - start)
    }

    // No `write_at`: the default answers `Unsupported`, and that is the
    // correct answer for everything here except the sysctls, which carry
    // their own.
}

/// A generated file whose contents can be replaced by writing them.
///
/// This is what makes `/proc/sys/kernel/hostname` a sysctl rather than a
/// read-only fact: `sysctl -w` and OpenRC's `hostname` service both set the
/// system's name by writing the file, and a file that refused the write would
/// leave the reader to conclude the write had not taken.
struct ProcCtl {
    ino: Ino,
    read: Generator,
    write: Writer,
}

impl Inode for ProcCtl {
    fn metadata(&self) -> Metadata {
        Metadata {
            ino: self.ino,
            kind: NodeKind::File,
            size: 0,
        }
    }

    fn mode(&self) -> u32 {
        PROC_RW_FILE_MODE
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> VfsResult<usize> {
        let content = (self.read)().ok_or(VfsError::NoEntry)?;
        let bytes = content.as_bytes();
        let start = offset.min(bytes.len() as u64) as usize;
        let end = (start + buf.len()).min(bytes.len());
        buf[..end - start].copy_from_slice(&bytes[start..end]);
        Ok(end - start)
    }

    fn write_at(&self, _offset: u64, buf: &[u8]) -> VfsResult<usize> {
        // Trailing newlines are how every tool writes these: `sysctl -w`
        // writes the value and a newline, and a hostname with one inside it
        // would then be a hostname `hostname` prints with a line break.
        let text = core::str::from_utf8(buf).map_err(|_| VfsError::Invalid)?;
        (self.write)(text.trim_end_matches(['\n', '\r']))?;
        Ok(buf.len())
    }
}

/// A link whose target is a fact rather than a stored string.
///
/// `/proc/<pid>/exe` names the image the process came from and
/// `/proc/<pid>/cwd` the directory it is in, and both are answers about a
/// process that can go away. `None` is `ENOENT`: a link to a process that is
/// no longer there names nothing.
struct ProcLink {
    ino: Ino,
    target: Generator,
    /// The process whose descriptor this link is a second name for.
    pid: usize,
    /// The descriptor this link is a second name for, when it is one: every
    /// name under `/proc/<pid>/fd` is, and opening one has to open the
    /// descriptor rather than the link.
    fd: Option<usize>,
    /// The namespace this link names, when it names one: every name under
    /// `/proc/<pid>/ns` is. The second kind of payload a link can carry, and
    /// asked for the same way - the walk must not splice this link either,
    /// because its target is a report (`mnt:[<id>]`) rather than a path.
    ns: Option<(usize, crate::vfs::ns::Entry)>,
    /// The `st_size` of a link that is **not** a descriptor's second name.
    ///
    /// Linux's `proc_symlink(name, parent, target)` stores the text and sets the
    /// inode's size to its length *at build time*, so the number follows the
    /// text that was registered rather than the text `readlink` later answers -
    /// and the two are not the same thing for a name whose answer depends on who
    /// reads it. Measured on a current host
    /// (`.workbuddy/probe_proc_self_links.c`): `/proc/self` and
    /// `/proc/thread-self` are **0**, because they were registered with an empty
    /// target whatever number they hand back, while `/proc/mounts` is **11** -
    /// the length of `self/mounts`, the only target in this tree that was
    /// registered as a path. `exe`, `cwd`, `root` and `ns/*` are 0 too.
    registered: u64,
}

impl Inode for ProcLink {
    fn metadata(&self) -> Metadata {
        Metadata {
            ino: self.ino,
            kind: NodeKind::Symlink,
            // The size Linux gives this link's own `stat`, not the length of
            // the text `readlink` answers: measured on a current host, an
            // `/proc/<pid>/fd/<N>` link is **64 bytes** whatever it names - a
            // 9-byte `/dev/null` entry, a pathless socket's `socket:[…]` and a
            // 33-byte `exe`-style target alike. A link that is not a descriptor's
            // second name reports what its name was *registered* with, which is
            // zero for every live target here and the text's own length for the
            // one fixed one (`/proc/mounts`).
            size: if self.fd.is_some() {
                FD_LINK_SIZE
            } else {
                self.registered
            },
        }
    }

    fn mode(&self) -> u32 {
        // The link's own permission bits, which is the half `stat` never sees:
        // `stat` follows the name to the file (item 24's whole point) and
        // `lstat` stops here. Linux gives a descriptor's second name the
        // direction the descriptor was opened in and every other magic link
        // `0777` (measured, `.workbuddy/probe_proclink_mode.c`).
        match self.fd {
            Some(fd) => {
                fd_link_mode(fd_access(self.pid, fd).unwrap_or(crate::vfs::file::ACC_RDONLY))
            }
            None => 0o777,
        }
    }

    fn readlink(&self) -> VfsResult<String> {
        (self.target)().ok_or(VfsError::NoEntry)
    }

    fn as_fd(&self) -> Option<usize> {
        // Another process's descriptor is not ours to clone: opening
        // `/proc/<other>/fd/3` from here must not hand back *our* fd 3. The
        // shared-position door opens only for the caller's own group.
        #[cfg(target_os = "none")]
        if crate::sched::current_tgid().0 != self.pid {
            return None;
        }
        self.fd
    }

    fn as_namespace(&self) -> Option<crate::vfs::ns::Namespace> {
        // Asked when the object is wanted, and answered with the namespace the
        // process is in *at that instant*: a process that unshared a moment ago
        // is in a different namespace now, and a link that answered with what
        // it was built from would be naming the one the process left. `None`
        // when the process is gone - the same answer `readlink` gives, one
        // instant later. What the walk then hands on is a
        // [`crate::vfs::ns::NsFile`] holding *this* answer, so the pinning is
        // the resolution's and not this method's: the link stays live, the
        // descriptor it was opened through does not.
        let (pid, entry) = self.ns?;
        namespace_of(pid, entry)
    }

    fn read_at(&self, _offset: u64, _buf: &mut [u8]) -> VfsResult<usize> {
        // A namespace is not bytes, and Linux says so with `EINVAL` rather than
        // by returning zero: measured on a current host, `read` on an opened
        // `/proc/self/ns/mnt` answers `-1`/`EINVAL` (errno 22). A descriptor's
        // second name is a different answer - it *is* a file descriptor, and
        // the opener clones it rather than reading the link - so this applies
        // only to the namespace payload.
        if self.ns.is_some() {
            return Err(VfsError::Invalid);
        }
        Err(VfsError::Unsupported)
    }

    fn seekable(&self) -> bool {
        // `lseek` on a namespace descriptor is `ESPIPE`, measured with the same
        // probe. A descriptor's second name keeps the seekability of the
        // descriptor it clones, so the answer is the namespace's alone.
        self.ns.is_none()
    }
}

/// Names a directory has that are not in its table, because they come and go
/// while the machine runs: one per process, one per open descriptor, one per
/// thread.
///
/// A table cannot hold them - it would have to be rebuilt on every fork,
/// every `open` and every exit - so the directory asks, at each lookup and
/// each listing, what exists now. Everything a name means here is a question
/// about live state, and the answer has to be given at the moment it is asked.
trait Dynamic: Send + Sync {
    /// The names that exist right now, with the kind each is listed as, in
    /// the order they should be listed. The kind is part of the answer
    /// because `getdents` reports one: an entry under `/proc/<pid>/fd` is a
    /// symlink, and a tool told it was a directory would try to walk into it.
    fn names(&self) -> Vec<(String, NodeKind)>;
    /// The node one of those names names, or `None` if it went away between
    /// [`names`](Self::names) and here - which is the ordinary case when a
    /// process exits while a tool is walking `/proc`.
    fn node(&self, name: &str) -> Option<Arc<dyn Inode>>;
}

/// A directory of generated files and other directories, by name.
struct ProcDir {
    ino: Ino,
    /// Each entry with its kind, recorded as it is added: a listing must not
    /// have to ask a node what it is, because asking a generated file is what
    /// generates it.
    entries: Entries,
    /// The children that are not in `entries` because they are not fixed.
    dynamic: Option<Arc<dyn Dynamic>>,
    /// The permission bits Linux gives *this* directory, when they are not the
    /// tree's own (`0555`). Zero means the tree's, which is what every proc
    /// directory but `/proc/<pid>/fd` and `/proc/<pid>/ns` reports here.
    mode: u32,
    /// Whether the directory's own size is the number of entries it lists,
    /// which is Linux's rule for `/proc/<pid>/fd` alone (`st_size` there is
    /// the descriptor count) and no other directory in this tree.
    size_is_count: bool,
}

impl ProcDir {
    fn new(dynamic: Option<Arc<dyn Dynamic>>) -> Arc<ProcDir> {
        ProcDir::own(dynamic, 0, false)
    }

    /// A directory whose own metadata is Linux's rather than the tree's
    /// default: its permission bits, and whether its size is the number of
    /// entries it lists.
    fn own(dynamic: Option<Arc<dyn Dynamic>>, mode: u32, size_is_count: bool) -> Arc<ProcDir> {
        Arc::new(ProcDir {
            ino: next_ino(),
            entries: Mutex::new(RefCell::new(BTreeMap::new())),
            dynamic,
            mode,
            size_is_count,
        })
    }

    fn add(&self, name: &str, kind: NodeKind, node: Arc<dyn Inode>) {
        critical_section::with(|cs| {
            self.entries
                .borrow(cs)
                .borrow_mut()
                .insert(String::from(name), (node, kind));
        });
    }
}

impl Inode for ProcDir {
    fn metadata(&self) -> Metadata {
        Metadata {
            ino: self.ino,
            kind: NodeKind::Dir,
            // Linux gives a directory its own size only where the number of
            // things it holds is the whole of what it is - measured for
            // `/proc/<pid>/fd` alone (3 descriptors answer 3, 23 answer 23,
            // and a hole is a *number* rather than a span, so fds 0 and 200
            // with one more open answer 3): every other proc directory reports
            // zero, and the tree's own permission word rather than the kind's.
            size: if self.size_is_count {
                self.read_dir()
                    .map(|listed| listed.len() as u64)
                    .unwrap_or(0)
            } else {
                0
            },
        }
    }

    fn mode(&self) -> u32 {
        if self.mode == 0 {
            PROC_DIR_MODE
        } else {
            self.mode
        }
    }

    fn lookup(&self, name: &str) -> VfsResult<Arc<dyn Inode>> {
        let fixed = critical_section::with(|cs| {
            self.entries
                .borrow(cs)
                .borrow()
                .get(name)
                .map(|(node, _kind)| node.clone())
        });
        match (fixed, &self.dynamic) {
            (Some(node), _) => Ok(node),
            (None, Some(live)) => live.node(name).ok_or(VfsError::NoEntry),
            (None, None) => Err(VfsError::NoEntry),
        }
    }

    fn read_dir(&self) -> VfsResult<Vec<(String, NodeKind)>> {
        let mut listed: Vec<(String, NodeKind)> = critical_section::with(|cs| {
            self.entries
                .borrow(cs)
                .borrow()
                .iter()
                .map(|(name, (_node, kind))| (name.clone(), *kind))
                .collect()
        });
        if let Some(live) = &self.dynamic {
            for (name, kind) in live.names() {
                // A live name that is also a fixed one - `self` beside the
                // pid directories - is listed once, by whoever holds it.
                let already = critical_section::with(|cs| {
                    self.entries.borrow(cs).borrow().contains_key(&name)
                });
                if !already {
                    listed.push((name, kind));
                }
            }
        }
        // Sorted, as Linux lists them: a `ps` that walks /proc in directory
        // order sees processes in pid order rather than in whatever order the
        // task table happens to hold them.
        listed.sort_by(|(a, _), (b, _)| a.cmp(b));
        Ok(listed)
    }
}

/// Inode numbers, shared by the whole filesystem rather than kept per mount.
///
/// `/proc`'s nodes are made on demand - a directory per process, built when
/// the name is looked up and thrown away when nothing names it - so there is
/// no fixed set to count and no instance to keep the count on.
static NEXT_INO: AtomicU64 = AtomicU64::new(1);

fn next_ino() -> Ino {
    NEXT_INO.fetch_add(1, Ordering::Relaxed)
}

/// A file that always has an answer.
fn file(generate: impl Fn() -> String + Send + Sync + 'static) -> Arc<dyn Inode> {
    Arc::new(ProcFile {
        ino: next_ino(),
        generate: Box::new(move || Some(generate())),
        mode: 0,
        size: 0,
    })
}

/// The body of `/proc/cpuinfo`.
///
/// Its *shape* is the machine's rather than the kernel's, and the two renderings
/// do not overlap: Linux on x86 reports `model name`/`vendor_id`, while on ARM64
/// it reports `CPU implementer`/`CPU part` and leaves the reader to map the part
/// to a core name. A reader picks its parser from the machine it runs on -
/// `fastfetch`'s `detectCPUOthers` has no row for `model name` at all, so an
/// x86-shaped file answers "Unknown CPU" on the second machine. The id fields
/// are read from `MIDR_EL1` rather than invented, so the name a reader derives
/// is the core the machine is actually running on.
#[cfg(target_arch = "aarch64")]
fn cpuinfo_text() -> String {
    let midr: u64;
    let pfr0: u64;
    let isar0: u64;
    // SAFETY: the `MIDR_EL1` and `ID_AA64*` identification registers are
    // readable at EL1 with no side effects, and EL1 is the only level this
    // kernel runs at.
    unsafe {
        core::arch::asm!("mrs {}, midr_el1", out(reg) midr, options(nomem, nostack));
        core::arch::asm!("mrs {}, id_aa64pfr0_el1", out(reg) pfr0, options(nomem, nostack));
        core::arch::asm!("mrs {}, id_aa64isar0_el1", out(reg) isar0, options(nomem, nostack));
    }
    let implementer = (midr >> 24) & 0xff;
    let variant = (midr >> 20) & 0xf;
    let part = (midr >> 4) & 0xfff;
    let revision = midr & 0xf;

    // `Features` is derived from the same registers Linux reads rather than
    // written out: a core that lacks a block must not be told it has one.
    let mut features = String::new();
    if pfr0 & 0xf != 0xf {
        features.push_str("fp ");
    }
    if (pfr0 >> 4) & 0xf != 0xf {
        features.push_str("asimd ");
    }
    let aes = (isar0 >> 4) & 0xf;
    if aes != 0 {
        features.push_str("aes ");
    }
    if aes >= 0b0010 {
        features.push_str("pmull ");
    }
    if (isar0 >> 8) & 0xf != 0 {
        features.push_str("sha1 ");
    }
    if (isar0 >> 12) & 0xf != 0 {
        features.push_str("sha2 ");
    }
    if (isar0 >> 16) & 0xf != 0 {
        features.push_str("crc32 ");
    }
    let features = features.trim_end();

    alloc::format!(
        "processor\t: 0\n\
         Features\t: {features}\n\
         CPU implementer\t: {implementer:#04x}\n\
         CPU architecture: 8\n\
         CPU variant\t: 0x{variant:x}\n\
         CPU part\t: 0x{part:03x}\n\
         CPU revision\t: {revision}\n\
         \n"
    )
}

/// `/proc/cpuinfo` on a machine that is not AArch64. See the AArch64 twin above
/// for why the shape belongs to the machine.
#[cfg(not(target_arch = "aarch64"))]
fn cpuinfo_text() -> String {
    alloc::format!(
        "processor\t: 0\ncpu family\t: x86_64\nmodel name\t: {} x86_64 processor\n\n",
        env!("CARGO_PKG_NAME")
    )
}

/// A file that reports something which can go away.
fn maybe_file(generate: impl Fn() -> Option<String> + Send + Sync + 'static) -> Arc<dyn Inode> {
    Arc::new(ProcFile {
        ino: next_ino(),
        generate: Box::new(generate),
        mode: 0,
        size: 0,
    })
}

/// The one read-only file whose own permission word is narrower than the
/// tree's: `/proc/<pid>/environ`, which Linux makes `0400` because a process's
/// environment can carry secrets and the file belongs to its owner.
///
/// A [`maybe_file`] whose word is said out loud rather than a knob on every
/// node: the generator answers nothing for a process that has gone, exactly as
/// its siblings' do, so this file's *shape* is ordinary and only the bits it
/// reports are its own. Measured on a current host, beside
/// `/proc/<pid>/cmdline`'s `0444`.
fn env_file(generate: impl Fn() -> Option<String> + Send + Sync + 'static) -> Arc<dyn Inode> {
    Arc::new(ProcFile {
        ino: next_ino(),
        generate: Box::new(generate),
        mode: 0o400,
        size: 0,
    })
}

/// A file whose length is known when the tree is built rather than when it is
/// read.
///
/// `/proc/cmdline` is the one node of that shape: the command line the machine
/// booted with cannot change, so its length is a fact the node can report -
/// and Linux reports it (a current host answers 86 for its own, which is the
/// file's whole content including its newline). Every other generated file
/// leaves the size at zero, because a generator that ran to measure it would
/// run during every directory listing.
fn file_of_size(
    size: u64,
    generate: impl Fn() -> String + Send + Sync + 'static,
) -> Arc<dyn Inode> {
    Arc::new(ProcFile {
        ino: next_ino(),
        generate: Box::new(move || Some(generate())),
        mode: 0,
        size,
    })
}

/// A writable file, for the handful of facts that are set by writing them.
fn ctl(
    read: impl Fn() -> String + Send + Sync + 'static,
    write: impl Fn(&str) -> VfsResult<()> + Send + Sync + 'static,
) -> Arc<dyn Inode> {
    Arc::new(ProcCtl {
        ino: next_ino(),
        read: Box::new(move || Some(read())),
        write: Box::new(write),
    })
}

/// A sysctl knob whose value the kernel reports but does not enforce.
///
/// Each of these is a real Linux default, honestly labelled: the read answers
/// what Linux would answer, and a write is accepted and ignored - the same
/// deal `domainname` makes. This is not `sysctl -a` shrugging; a key that
/// does not exist here is a key the kernel refuses (ENOENT on a plain open,
/// EOPNOTSUPP when `sysctl -w` asks `open(O_CREAT)` to invent it), and a
/// stub is a knob whose *policy* this kernel has no machinery to act on.
/// Writing one changes what the next read says, not how the kernel behaves.
fn stub(value: &'static str) -> Arc<dyn Inode> {
    ctl(move || alloc::format!("{}\n", value), |_| Ok(()))
}

/// A link to something that can go away.
fn link(target: impl Fn() -> Option<String> + Send + Sync + 'static) -> Arc<dyn Inode> {
    Arc::new(ProcLink {
        ino: next_ino(),
        target: Box::new(target),
        pid: 0,
        fd: None,
        ns: None,
        registered: 0,
    })
}

/// A link whose target is a **fixed path**, made the way Linux makes one.
///
/// `proc_symlink(name, parent, target)` stores the text and sets `st_size` to its
/// length, so this link reports 11 where a live target reports 0 - measured, and
/// the reason `/proc/mounts`' size is 11 while `/proc/self`'s is 0
/// (`.workbuddy/probe_proc_self_links.c`).
fn fixed_link(target: &'static str) -> Arc<dyn Inode> {
    Arc::new(ProcLink {
        ino: next_ino(),
        target: Box::new(move || Some(alloc::string::String::from(target))),
        pid: 0,
        fd: None,
        ns: None,
        registered: target.len() as u64,
    })
}

/// A link that is also a descriptor's second name, as every entry in
/// `/proc/<pid>/fd` is.
fn fd_link(
    pid: usize,
    fd: usize,
    target: impl Fn() -> Option<String> + Send + Sync + 'static,
) -> Arc<dyn Inode> {
    Arc::new(ProcLink {
        ino: next_ino(),
        target: Box::new(target),
        pid,
        fd: Some(fd),
        ns: None,
        registered: 0,
    })
}

/// A directory of fixed entries.
fn dir() -> Arc<ProcDir> {
    ProcDir::new(None)
}

// ---------------------------------------------------------------------------
// /proc/net/{tcp,udp} - the socket table.
//
// The rows are resolved to plain byte fields (`NetRow`) before they reach the
// formatter, so the byte-order and layout of the `/proc/net` text - the part a
// tool like `ss` parses - is unit-testable here without a live network stack;
// only collecting the rows touches smoltcp, and that is the target-only half.
// ---------------------------------------------------------------------------

/// One listening or connected socket, already reduced to the numbers the text
/// format needs: IPv4 octets and port for the two ends, the numeric TCP state
/// (0 for UDP), and the inode the descriptor answers with.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct NetRow {
    pub local: ([u8; 4], u16),
    pub remote: ([u8; 4], u16),
    pub state: u8,
    pub uid: u32,
    pub inode: u64,
}

/// An IPv4 address as `/proc/net` prints it: the octets packed into a u32 read
/// little-endian, so `127.0.0.1` is `0100007F`.
fn net_ip_word(octets: &[u8; 4]) -> u32 {
    (octets[0] as u32)
        | (octets[1] as u32) << 8
        | (octets[2] as u32) << 16
        | (octets[3] as u32) << 24
}

/// Render the socket rows in the `/proc/net/tcp` shape.
fn format_net(rows: &[NetRow]) -> String {
    let mut out = String::from(
        "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode\n",
    );
    for (i, r) in rows.iter().enumerate() {
        out.push_str(&alloc::format!(
            "  {:2}: {:08X}:{:04X} {:08X}:{:04X} {:02X} {:08X}:{:08X} 00:00000000 00000000 {:>7} 0 {:>8} 1 0 0 0\n",
            i,
            net_ip_word(&r.local.0),
            r.local.1,
            net_ip_word(&r.remote.0),
            r.remote.1,
            r.state,
            0u32,
            0u32,
            r.uid,
            r.inode,
        ));
    }
    out
}

/// The live rows for one protocol. Host builds have no stack, so they are
/// empty - the `/proc/net` files then show just the header.
fn net_rows(tcp: bool) -> Vec<NetRow> {
    #[cfg(target_os = "none")]
    {
        crate::net::collect_net_sockets(tcp)
    }
    #[cfg(not(target_os = "none"))]
    {
        let _ = tcp;
        Vec::new()
    }
}

/// Render `/proc/net/route` for a statically-configured interface: the default
/// route (destination `0.0.0.0` via `gw`, `RTF_UP|RTF_GATEWAY`) and the
/// directly-reachable /24 link route. `fastfetch`'s LocalIp and `ip route`
/// parse this text to name the default interface, so it is a real requirement,
/// not decoration. Addresses are the little-endian `%08X` form the file uses.
fn format_route_table(local: &[u8; 4], gw: &[u8; 4]) -> String {
    let network = [local[0], local[1], local[2], 0];
    alloc::format!(
        "Iface\tDestination\tGateway \tFlags\tRefCnt\tUse\tMetric\tMask\t\tMTU\tWindow\tIRTT\n\
         eth0\t{:08X}\t{:08X}\t0003\t0\t0\t0\t{:08X}\t0\t0\t0\n\
         eth0\t{:08X}\t{:08X}\t0001\t0\t0\t0\t{:08X}\t0\t0\t0\n",
        0u32,
        net_ip_word(gw),
        0u32,
        net_ip_word(&network),
        0u32,
        net_ip_word(&[255, 255, 255, 0]),
    )
}

/// The `/proc/net/route` text. Host builds have no stack, so they report the
/// same static configuration the kernel boots with.
fn route_table_text() -> String {
    #[cfg(target_os = "none")]
    {
        format_route_table(&crate::net::LOCAL_IP, &crate::net::GATEWAY_IP)
    }
    #[cfg(not(target_os = "none"))]
    {
        format_route_table(&[10, 0, 2, 15], &[10, 0, 2, 2])
    }
}

/// `/proc/net/dev`: every interface and its frame counters, in Linux's column
/// layout.
///
/// The file is the canonical door for what §10.4's instrument counts - rx/tx
/// packets and bytes per interface - which is why this kernel puts the numbers
/// here rather than behind a private protocol: an ordinary boot answers them,
/// no boot word arms them, and a reader that parses by position (`ifconfig`,
/// `ss -i`, procps' own tools) finds every standard column where Linux puts
/// it. The one divergence is named in [`net_dev_row`]: two words appended past
/// the last standard column, which a positional reader never reaches.
///
/// Both header lines are copied byte for byte from what Linux prints -
/// measured on the host 2026-10-11, and pinned as literals by the test beside
/// [`net_dev_row`] - because the file is parsed by *position*: the column line
/// is where readers find each field, and a program that reads it is not asked
/// to be forgiving. Two header lines, then one row per interface.
fn net_dev_text() -> String {
    let mut out = String::from(
        "Inter-|   Receive                                                |  Transmit\n\
         \x20face |bytes    packets errs drop fifo frame compressed multicast|bytes    packets errs drop fifo colls carrier compressed\n",
    );
    for row in net_dev_rows() {
        out.push_str(&net_dev_row(&row));
    }
    out
}

/// One `/proc/net/dev` row, rendered from a [`NetDevRow`].
///
/// Split from [`net_dev_text`] so the host test hands it hand-built numbers and
/// checks the columns byte for byte - the same shape `partitions_text` uses,
/// and for the same reason: the rendering is the contract, and a machine that
/// has no stack cannot be the only witness to it.
fn net_dev_row(row: &NetDevRow) -> String {
    // The widths below are *measured*, not guessed: real Linux (host,
    // 2026-10-11) right-aligns every value into a fixed advance - the name in
    // six columns plus its colon and one space, then rx bytes/packets at 8,
    // errs/drop/fifo at 5, frame at 6, compressed at 11, multicast at 10, tx
    // bytes at 9, tx packets at 8, errs/drop/fifo at 5, colls at 6, carrier at
    // 8, compressed at 11. A field wider than its column pushes only itself
    // (`%llu` has no truncation), which is what the host's own rows show for a
    // ten-digit byte count - and the test beside this function pins both the
    // aligned shape and that overflow case.
    //
    // The zeros are honest: this kernel's seam does not count errors, drops,
    // fifos or collisions - a frame either crosses or does not - and zero is
    // what Linux itself reports for a driver that keeps no such counter.
    //
    // The two appended words are this kernel's own, *after* the last standard
    // column: `mtu` because `ip -s link` reads the link's current MTU and this
    // kernel's is settable (`ip link set eth0 mtu N`), and `pool=<hits>:<misses>`
    // because §10.4's instrument state has no Linux column to live in — inventing
    // one *inside* the table would shift every reader's positions, which is the
    // wrong-shape answer, while extra words at the tail are invisible to a
    // parser that stops at `compressed`. Divergence disclosed here, in
    // `docs/uapi/`, and read by `t-netstats`.
    alloc::format!(
        "{:>6}: {:>7} {:>7} {:>4} {:>4} {:>4} {:>5} {:>10} {:>9} {:>8} {:>7} {:>4} {:>4} {:>4} {:>5} {:>7} {:>10} mtu {mtu} pool={hits}:{misses}\n",
        row.name,
        row.rx_bytes,
        row.rx_packets,
        0u64,
        0u64,
        0u64,
        0u64,
        0u64,
        0u64,
        row.tx_bytes,
        row.tx_packets,
        0u64,
        0u64,
        0u64,
        0u64,
        0u64,
        0u64,
        mtu = row.mtu,
        hits = row.pool_hits,
        misses = row.pool_misses,
    )
}

/// A `/proc/net/dev` row as plain numbers, so the renderer is testable without
/// a stack and the collector stays a translation layer.
pub(crate) struct NetDevRow {
    pub name: &'static str,
    pub rx_bytes: u64,
    pub rx_packets: u64,
    pub tx_bytes: u64,
    pub tx_packets: u64,
    pub mtu: usize,
    pub pool_hits: u64,
    pub pool_misses: u64,
}

/// The rows, from the seam's counters on the machine.
///
/// The same two-body rule as [`net_rows`], but with nothing to stub on the
/// host: the *renderer*'s claims are pinned against hand-built [`NetDevRow`]
/// values by the tests below, and a host build has no seam to count - so the
/// empty answer here is the honest one, not a fixture dressed up as data. The
/// stats type lives in `crate::net::pool`, inside the machine-gated stack - the
/// host build never names it, only its own [`NetDevRow`] twin.
fn net_dev_rows() -> Vec<NetDevRow> {
    #[cfg(target_os = "none")]
    {
        crate::net::net_dev_rows()
            .into_iter()
            .map(Into::into)
            .collect()
    }
    #[cfg(not(target_os = "none"))]
    Vec::new()
}

#[cfg(target_os = "none")]
impl From<crate::net::pool::NetDevStats> for NetDevRow {
    fn from(s: crate::net::pool::NetDevStats) -> NetDevRow {
        NetDevRow {
            name: s.name,
            rx_bytes: s.rx_bytes,
            rx_packets: s.rx_packets,
            tx_bytes: s.tx_bytes,
            tx_packets: s.tx_packets,
            mtu: s.mtu,
            pool_hits: s.pool_hits,
            pool_misses: s.pool_misses,
        }
    }
}

// ---------------------------------------------------------------------------
// The questions /proc asks the scheduler.
//
// Host builds have no scheduler, and this filesystem is compiled into them so
// its shapes are unit-testable - so each of these answers emptily there
// rather than failing to compile. A /proc with no processes is what a machine
// that has not started any would report.
// ---------------------------------------------------------------------------

#[cfg(target_os = "none")]
fn current_pid() -> usize {
    crate::sched::current_tgid().0
}

#[cfg(not(target_os = "none"))]
fn current_pid() -> usize {
    1
}

/// The number the **reader** sees for a task, given the slot it lives in.
///
/// `/proc` is one tree that two readers can walk into two different answers,
/// because a pid namespace gives one task two numbers. The rule is the whole
/// feature in one line - *the number is the reader's* - and it is applied at
/// the two funnels below (which names a directory has, and which directory a
/// name resolves to) plus at every place this file *prints* an id, so that a
/// `/proc/<pid>/status` and the directory it came from cannot disagree.
///
/// Falls back to the slot for a task the reader's namespace does not contain,
/// which cannot happen for a task the reader just found in its own tree: the
/// fallback is here so a disagreement answers a number rather than stopping the
/// machine.
fn reader_number(slot: usize) -> usize {
    crate::sched::pid_number_of(crate::sched::TaskId(slot)).map_or(slot, |n| n as usize)
}

/// The `NSpid`-style chain of numbers for a slot - every level of the task's own
/// namespace chain, outermost first, tab-separated, which is what `status`'s
/// `NStgid`/`NSpid`/`NSpgid` carry.
///
/// Deliberately not [`reader_number`]: the *reader's* number is what `Pid:`/`Tgid:`
/// answer, while these fields list the task "in each of the PID namespaces of
/// which it is a member", so the same file line reads the same for every reader.
/// See `sched::pid_numbers_of` for the shape and its measurement.
fn number_chain(slot: usize) -> String {
    let mut out = String::new();
    for n in crate::sched::pid_numbers_of(crate::sched::TaskId(slot)) {
        if !out.is_empty() {
            out.push('\t');
        }
        out.push_str(&alloc::format!("{n}"));
    }
    if out.is_empty() {
        // Nothing answers for that id any more - a session leader the task
        // outlived, or a task that exited between the snapshot and this render.
        // The number itself is what Linux keeps in that case (an id outlives the
        // process it named), and it is the same fallback [`reader_number`] makes.
        out.push_str(&alloc::format!("{slot}"));
    }
    out
}

/// The slot a number written by the **reader** names, if it names a live task
/// at all - the inverse of [`reader_number`], and where a number from another
/// namespace stops being a name.
fn reader_task(number: usize) -> Option<usize> {
    crate::sched::resolve_pid(number as i64).map(|id| id.0)
}

/// Every process id, in the order the task table holds them. Threads are left
/// out: a process *is* its leader's id, and listing a thread's tid beside it
/// would be two names for one process.
#[cfg(target_os = "none")]
fn process_ids() -> Vec<usize> {
    crate::sched::snapshots()
        .into_iter()
        .filter(|task| task.pid == task.tid)
        .map(|task| reader_number(task.pid))
        .collect()
}

#[cfg(not(target_os = "none"))]
fn process_ids() -> Vec<usize> {
    Vec::new()
}

/// Whether a task this tree still holds has a tid that names a directory.
///
/// Linux unhooks a task from the namespace it was numbered in **at its exit**,
/// unless it leads its thread group - `__unhash_process` asks
/// `thread_group_leader`, which is `pid == tgid`. That one clause is why a dead
/// *process* keeps its `/proc/<pid>` until it is reaped (a `ps` reads a
/// zombie's record, and item 63's pin reads a zombie's byte through it) while a
/// dead *thread*'s tid names nothing at all. Measured on a current host
/// (`.workbuddy/probe_tid_dir.c`, Linux 7.2.11): a thread that returns from its
/// start routine - joined or not, and in the same instant - is gone from
/// `/proc/<pid>/task`, and `/proc/<tid>` is `ENOENT`, with its leader still
/// listed beside it.
///
/// The question is asked of the *state letter* because that is what a snapshot
/// carries: `snapshot_of` writes `Z` for `exited` and nothing else does.
///
/// This kernel keeps a dead thread's slot until the slot is reused (its retired
/// record is what holds it), which is a fact about the task table rather than
/// about `/proc` - so the clause is applied at the two funnels that *name* a
/// task ([`process_dir`] and [`thread_ids`]) instead of being left to the
/// table's own liveness.
fn tid_has_a_directory(state: char, tid: usize, pid: usize) -> bool {
    !(state == 'Z' && tid != pid)
}

/// The ids of every thread in the process `pid`, leader included - and, like
/// Linux's own listing, the *live* ones: a thread that has left takes its tid
/// out of the group with it ([`tid_has_a_directory`]).
#[cfg(target_os = "none")]
fn thread_ids(pid: usize) -> Vec<usize> {
    crate::sched::thread_snapshots(pid)
        .into_iter()
        .filter(|task| tid_has_a_directory(task.state, task.tid, task.pid))
        .map(|task| task.tid)
        .collect()
}

#[cfg(not(target_os = "none"))]
fn thread_ids(_pid: usize) -> Vec<usize> {
    Vec::new()
}

/// The children a task's `children` file lists: the tasks **that task**
/// forked.
///
/// Keyed on the *forking task* and not on the process, which is Linux's own
/// link (`real_parent`) and the whole of what item 64's residual named.
/// Measured on a current host (`.workbuddy/probe_children_owner.c`, Linux
/// 7.2.11, a live thread of a live group): a child forked by a **non-leader
/// thread** is listed under `/proc/<pid>/task/<tid>/children` alone - the
/// group's file held the leader's own child and not the thread's, and the
/// thread's file the other way round, in the same instant.
///
/// Two clauses belong to the file rather than to the link:
///
///   * a **thread is not a child**: a `CLONE_THREAD` task goes on no
///     `children` list at all (Linux's `copy_process` asks
///     `thread_group_leader`), which is why the filter wants a leader - and
///     it is measured: the leader's file listed its forked child while a
///     `pthread_create`d thread of the same group stood beside it, nowhere on
///     the list.
///   * a **corpse stays**: a task that has exited but has not been reaped is
///     still its parent's child, and leaves the list at the reap rather than at
///     the exit (measured: an unreaped child that had exited was listed beside
///     the live ones, and the reading after its `waitpid` no longer carried it).
fn child_slots(tasks: &[Process], slot: usize) -> Vec<usize> {
    let mut ids: Vec<usize> = tasks
        .iter()
        .filter(|task| task.pid == task.tid && task.parent == Some(slot))
        .map(|task| task.pid)
        .collect();
    ids.sort_unstable();
    ids
}

/// The children of the task in `slot`, as `children` reports them - the rule
/// above over the live task table.
#[cfg(target_os = "none")]
fn child_ids(slot: usize) -> Vec<usize> {
    child_slots(&crate::sched::snapshots(), slot)
}

#[cfg(not(target_os = "none"))]
fn child_ids(_slot: usize) -> Vec<usize> {
    Vec::new()
}

/// The descriptor numbers `pid` has open.
/// The descriptor numbers `pid` has open - what `/proc/<pid>/fd` lists.
///
/// The numbers only, deliberately: naming a descriptor is a search of the
/// filesystem tree, and this is a *listing*, which a tree search is itself
/// made of. Asking for a name here would put the search inside its own
/// listing - `/proc/<pid>/fd` again, one stack deeper, without end - so a
/// name is computed where one is actually wanted, by `readlink` on an entry.
#[cfg(target_os = "none")]
fn fd_numbers(pid: usize) -> Vec<String> {
    crate::sched::descriptor_numbers(pid)
        .into_iter()
        .map(|fd| alloc::format!("{fd}"))
        .collect()
}

#[cfg(not(target_os = "none"))]
fn fd_numbers(_pid: usize) -> Vec<String> {
    Vec::new()
}

/// What the descriptor `fd` of `pid` is reported as: the path it was opened
/// by, or the anonymous spelling a pipe or a socket falls back to. `None`
/// only when the descriptor is not open.
#[cfg(target_os = "none")]
fn fd_target(pid: usize, fd: usize) -> Option<String> {
    crate::sched::descriptors_of(pid)
        .into_iter()
        .find(|(number, _name)| *number == fd)
        .map(|(_number, name)| name)
}

#[cfg(not(target_os = "none"))]
fn fd_target(_pid: usize, _fd: usize) -> Option<String> {
    None
}

/// The permission bits a proc directory reports about itself.
///
/// Not the kind's `0755`. Measured whole on a current host
/// (`.workbuddy/probe_proc_node_modes.c`, Linux 7.2.9, `lstat` through
/// `syscall(2)`, over every path this tree builds): `/proc`, `/proc/sys`,
/// `/proc/sys/kernel`, `/proc/net`, `/proc/bus`, `/proc/bus/pci`, every
/// `/proc/<pid>` and `/proc/<pid>/task` answer `0555` - a reader of the tree
/// may look inside it and may not write to it, and the `w` bit is absent from
/// all three triads. The two directories whose own word is narrower are the
/// two named where they are built: `/proc/<pid>/fd` (`0500`) and
/// `/proc/<pid>/ns` (`0511`).
const PROC_DIR_MODE: u32 = 0o555;

/// The permission bits a generated file that cannot be written reports.
///
/// Linux creates a proc file with `S_IRUGO` when it names no mode of its own,
/// and that is most of them: measured on a current host, `/proc/version`,
/// `/proc/meminfo`, `/proc/self/status`, `/proc/net/tcp` and
/// `/proc/sys/kernel/random/poolsize` all answer `0444` where this kernel
/// reported its kind's `0644`. The word follows the node's *shape* here - a
/// file nothing can write is `0444` - which is why the handful of files Linux
/// made writable and this kernel did not (`/proc/sys/kernel/pid_max`,
/// `/proc/<pid>/comm`, ...) report the read-only word: the file refuses the
/// write, and the word says so (`docs/uapi/` §5.2).
const PROC_RO_FILE_MODE: u32 = 0o444;

/// The permission bits a file that can be read *and* written reports: the
/// sysctls, the map files, and the process's own `/proc/bus/pci/<bus>/<dev>`
/// configuration space.
///
/// `0644` is Linux's word for every one of them - measured on a current host
/// for `/proc/sys/kernel/hostname`, `/proc/sys/fs/protected_hardlinks`,
/// `/proc/<pid>/oom_score_adj`, `/proc/<pid>/uid_map`, `/proc/net/ipv4/...`
/// and `/proc/bus/pci/00/00.0`, each of which this kernel builds as a
/// `ProcCtl` or a `ProcConfig`.
const PROC_RW_FILE_MODE: u32 = 0o644;

/// The size an `/proc/<pid>/fd/<N>` link reports about itself.
///
/// 64 on Linux, for every descriptor and whatever it names - measured on a
/// current host with `.workbuddy/probe_proclink_mode.c` (`lstat` through
/// `syscall(2)`, Linux 7.2.9): the read-only link to a 9-byte `/dev/null`, the
/// `O_RDWR` one, and one whose target is a pathless socket all answer 64, while
/// `readlink` answers the target's *text* and the same call on `exe`/`cwd`/
/// `root` answers 0. It is procfs's own constant rather than a property of the
/// file the descriptor holds, which is why it lives here and not on the
/// description.
const FD_LINK_SIZE: u64 = 64;

/// The permission bits an `/proc/<pid>/fd/<N>` link reports about itself, from
/// the direction its descriptor was opened in.
///
/// Measured on a current host (`.workbuddy/probe_proclink_mode.c`, `lstat`
/// through `syscall(2)`, one descriptor per direction): `0500` read-only,
/// `0300` write-only, `0700` both - the two halves are *bits*, so a descriptor
/// opened both ways carries both - and `0` for one opened with neither, which is
/// what an `O_PATH` descriptor reports. This kernel has no `O_PATH` (its `open`
/// drops the bit and serves the open it was asked for), so the zero case is
/// named in `docs/uapi/` §5.2 rather than reachable here.
///
/// A descriptor that is not open answers the read half. That is a default
/// rather than a measurement: by the time a caller could see it the name is
/// `ENOENT`, because the entry is minted per lookup from the descriptor list.
fn fd_link_mode(access: u32) -> u32 {
    use crate::vfs::file::{ACC_RDONLY, ACC_WRONLY};
    match access {
        ACC_WRONLY => 0o300,
        ACC_RDONLY => 0o500,
        _ => 0o700,
    }
}

/// The direction the descriptor `fd` of `pid` was opened in, or `None` when it
/// is not open.
#[cfg(target_os = "none")]
fn fd_access(pid: usize, fd: usize) -> Option<u32> {
    crate::sched::descriptor_access(pid, fd)
}

/// The host build has no task table, so no descriptor has a direction; the
/// shape tests that reach here get the read-only word.
#[cfg(not(target_os = "none"))]
fn fd_access(_pid: usize, _fd: usize) -> Option<u32> {
    None
}

/// The facts about a process that the files below are built from.
#[cfg(target_os = "none")]
pub type Process = crate::sched::TaskSnapshot;

/// The same shape on a build with no scheduler.
///
/// It carries the fields because the generators below are compiled on the host
/// too - that is what makes their file shapes testable - and every one of them
/// answers `None` there, since none of them ever finds a process.
#[cfg(not(target_os = "none"))]
pub struct Process {
    pub comm: String,
    pub exe: String,
    pub argv: alloc::sync::Arc<Vec<String>>,
    pub env: alloc::sync::Arc<Vec<String>>,
    pub cwd: String,
    pub state: char,
    pub pid: usize,
    pub tid: usize,
    pub parent: Option<usize>,
    pub ppid: usize,
    pub pgid: usize,
    pub session: usize,
    pub threads: usize,
    pub rss_pages: usize,
    pub exit_signal: u8,
}

/// One process, by pid. `None` when nothing answers to it.
#[cfg(target_os = "none")]
fn task(pid: usize) -> Option<Process> {
    crate::sched::snapshot(pid)
}

#[cfg(not(target_os = "none"))]
fn task(_pid: usize) -> Option<Process> {
    None
}

// ---------------------------------------------------------------------------
// /proc/<pid>: the files a process is described by.
// ---------------------------------------------------------------------------

/// `/proc/<pid>/stat`'s field 38, `exit_signal`: the signal this task's death
/// reports, as Linux prints it.
///
/// Linux stores the byte `clone` carried on the task, rewrites that byte to
/// **`-1`** for a task created with `CLONE_THREAD` (its `copy_process`: a
/// thread never notifies anyone of its death), and prints what it stored - so
/// the field is a *signed* word carrying a number no signal byte can. Measured
/// whole on a current host first (`.workbuddy/probe_exit_signal_field.c`, Linux
/// 7.2.11), the byte the only variable: a `fork`'s child reports `17`, a raw
/// `clone(0)` child `0`, a `clone(SIGTERM)` child `15` (its own number, which
/// is what makes this a report rather than a constant), a **thread** `-1`
/// through both `/proc/<pid>/task/<tid>/stat` and the tid's own
/// `/proc/<tid>/stat`, the *leader* of a threaded process its own `17`, and a
/// zombie keeps the byte it died with. The file is `stat` and not `status`:
/// `/proc/<pid>/status` has no such line at all on Linux, which the same run
/// measured and which corrects the name `docs/ROADMAP.md` and `docs/uapi/` §5.2
/// carried for this field (`docs/uapi/` §5.1 item 63 is the landing).
///
/// This kernel's threads **store** `SIGCHLD` instead of `-1`, deliberately: the
/// one place a thread does leave a record is its group's `Retired`, and a
/// `wait` that named that slot must not read it as a "clone" child's (the
/// creation site in `sched::runtime::child_exit` says so in full). So the
/// report is derived from the task's own shape rather than read off the stored
/// byte, and the caller asks that shape the way Linux does: a task whose id is
/// not its group's is a thread, which is Linux's own `p->pid != p->tgid`.
/// `wchan`'s number: the wait the task last slept in. The host build has no
/// task table and nothing there sleeps in a kernel park, so it answers the `0`
/// that means "nothing to name" - the same split `vfs::ext4`'s `claim_park`
/// takes, and for the same reason: the field's *shape* is host-testable and the
/// number is the machine's.
fn wchan_of(tid: usize) -> u8 {
    #[cfg(target_os = "none")]
    return crate::sched::park_site(tid);
    #[cfg(not(target_os = "none"))]
    {
        let _ = tid;
        0
    }
}

fn exit_signal_field(exit_signal: u8, is_thread: bool) -> i32 {
    if is_thread {
        -1
    } else {
        i32::from(exit_signal)
    }
}

/// The one line `/proc/<pid>/stat` is, in Linux's own field order.
///
/// Fifty-two fields, because that is what the file is: a `ps` reads field 4
/// for the parent and field 3 for the state, and a tool that stops reading
/// after the ones it wants still lands on the right ones. Fields this kernel
/// does not account - page faults, CPU time, wchan - are zero, which is what
/// a machine that never counted them would honestly report.
fn stat_line(pid: usize, tid: usize) -> Option<String> {
    let task = task(tid)?;
    let shown = if tid == task.pid { task.pid } else { tid };
    let shown = reader_number(shown);
    let kb = task.rss_pages * 4096 / 1024;
    let fields: [alloc::borrow::Cow<'_, str>; 52] = [
        Cow::Owned(alloc::format!("{}", shown)),
        Cow::Owned(alloc::format!("({})", task.comm)),
        Cow::Owned(alloc::format!("{}", task.state)),
        Cow::Owned(alloc::format!("{}", reader_number(task.ppid))),
        Cow::Owned(alloc::format!("{}", reader_number(task.pgid))),
        // session: no `setsid` has ever been answered with a group of its
        // own, so the process group is the closest thing there is.
        Cow::Owned(alloc::format!("{}", reader_number(task.pgid))),
        // tty_nr: 0, "no controlling terminal". Everything on this machine
        // types at the one console, and claiming a terminal number that no
        // entry in /dev carries would send `ps` looking for a name it cannot
        // find.
        Cow::Borrowed("0"),
        Cow::Borrowed("-1"), // tpgid
        Cow::Borrowed("0"),  // flags
        Cow::Borrowed("0"),  // minflt
        Cow::Borrowed("0"),  // cminflt
        Cow::Borrowed("0"),  // majflt
        Cow::Borrowed("0"),  // cmajflt
        Cow::Borrowed("0"),  // utime
        Cow::Borrowed("0"),  // stime
        Cow::Borrowed("0"),  // cutime
        Cow::Borrowed("0"),  // cstime
        Cow::Borrowed("20"), // priority
        Cow::Borrowed("0"),  // nice
        Cow::Owned(alloc::format!("{}", task.threads)),
        Cow::Borrowed("0"), // itrealvalue
        // starttime: no per-task record of when a process began, so every
        // process claims to have started at boot. A `ps` that reads it prints
        // a start time rather than crashing on an absent one.
        Cow::Borrowed("0"),
        Cow::Owned(alloc::format!("{}", kb * 1024)), // vsize
        Cow::Owned(alloc::format!("{}", task.rss_pages)), // rss
        Cow::Borrowed("0"),                          // rsslim
        Cow::Borrowed("0"),                          // startcode
        Cow::Borrowed("0"),                          // endcode
        Cow::Borrowed("0"),                          // startstack
        Cow::Borrowed("0"),                          // kstkesp
        Cow::Borrowed("0"),                          // kstkeip
        Cow::Borrowed("0"),                          // signal
        Cow::Borrowed("0"),                          // blocked
        Cow::Borrowed("0"),                          // sigignore
        Cow::Borrowed("0"),                          // sigcatch
        // wchan: which wait the task last slept in, as a number the way
        // Linux's field is one (an address there). 0 is "never parked"; the
        // values name `sched::task::PARK_SITES`' entries, so a stuck task can
        // say which park holds it without a debugger.
        Cow::Owned(alloc::format!("{}", wchan_of(task.tid))),
        Cow::Borrowed("0"), // nswap
        Cow::Borrowed("0"), // cnswap
        // exit_signal: this task's own byte, and `-1` where it is a thread -
        // see `exit_signal_field` for the measurement and for why the report is
        // derived rather than read off the stored byte.
        Cow::Owned(alloc::format!(
            "{}",
            exit_signal_field(task.exit_signal, task.tid != task.pid)
        )),
        Cow::Borrowed("0"), // processor
        Cow::Borrowed("0"), // rt_priority
        Cow::Borrowed("0"), // policy
        Cow::Borrowed("0"), // delayacct_blkio_ticks
        Cow::Borrowed("0"), // guest_time
        Cow::Borrowed("0"), // cguest_time
        Cow::Borrowed("0"), // start_data
        Cow::Borrowed("0"), // end_data
        Cow::Borrowed("0"), // start_brk
        Cow::Borrowed("0"), // arg_start
        Cow::Borrowed("0"), // arg_end
        Cow::Borrowed("0"), // env_start
        Cow::Borrowed("0"), // env_end
        Cow::Borrowed("0"), // exit_code
    ];
    let mut out = String::new();
    for (index, field) in fields.iter().enumerate() {
        if index > 0 {
            out.push(' ');
        }
        out.push_str(field);
    }
    out.push('\n');
    // `pid` is unused on the `/proc/<pid>` spelling because the file is
    // already named for it; it is carried so the thread spelling can ask for
    // a tid that is not the process's own.
    let _ = pid;
    Some(out)
}

/// `/proc/<pid>/statm`: the same memory numbers as `stat`, in pages, as
/// seven fields - total, resident, shared, text, library, data, dirty.
fn statm_line(pid: usize) -> Option<String> {
    let task = task(pid)?;
    Some(alloc::format!(
        "{} {} 0 0 0 {} 0\n",
        task.rss_pages,
        task.rss_pages,
        task.rss_pages
    ))
}

/// The column a mapping's name starts in, on Linux.
///
/// Measured, not recalled: over process images with 12- and 16-digit addresses
/// and with inodes of several widths, every named line of `/proc/self/maps` on
/// Linux 7.2.7 puts its first name byte at offset 73, and an unnamed line ends
/// after the single space that would have preceded one. That is `seq_pad`'s
/// fixed column, and it is what makes the file readable by `awk '{print $6}'`
/// whatever the address width.
const MAPS_NAME_COLUMN: usize = 73;

/// What a mapping's *caller* may do with it - the permission both files'
/// permission column and `smaps`' `VmFlags` `rd`/`wr`/`ex` words are about.
///
/// A VMA's `READ` bit is "this range is present", not "the caller may read it":
/// `mmap` and `mprotect` both record `READ` and add the `USER` bit only when the
/// caller asked for a non-empty protection (`syscall::linux::proc`'s two
/// conversions). `USER` is what makes a page reachable from ring 3 - it is the
/// first thing `Vma::permits` asks, and every mapping the loader, the stack
/// installer or an ordinary `mmap` makes sets it.
///
/// So the report is the grant, and the difference is one a reader acts on:
/// printed from the internal bit, a `PROT_NONE` reservation read `r--p` where
/// Linux prints `---p` (measured over a two-page reservation and a
/// `PROT_NONE`/`rw-p`/`PROT_NONE` span, `.workbuddy/probe_maps_prot.c`), and a
/// tool that decided it could read the range on that word would take the fault
/// Linux's own answer tells it not to try.
fn granted(prot: MapFlags) -> MapFlags {
    if prot.contains(MapFlags::USER) {
        prot & (MapFlags::READ | MapFlags::WRITE | MapFlags::EXEC)
    } else {
        MapFlags::empty()
    }
}

/// One line of `/proc/<pid>/maps`, with every field already resolved.
///
/// The fields are resolved *before* the formatting because the two halves
/// belong on different sides of the host boundary: this struct and
/// [`format_maps`] are the ABI, and they are compiled for the host so a unit
/// test can pin the exact bytes. Gathering them asks the scheduler and the
/// inode layer, which only exist on the machine.
pub struct MapLine {
    pub start: usize,
    pub end: usize,
    /// The permissions *granted* to the caller, as [`granted`] resolves them,
    /// which is what Linux's four permission characters name.
    pub prot: MapFlags,
    /// `MAP_SHARED`, which is the fourth permission character.
    pub shared: bool,
    /// Byte offset into the backing object.
    pub offset: u64,
    /// The st_dev pair to print. Both zero for a mapping with no file behind
    /// it, and for a file on this kernel - `stat` reports zero there too, so
    /// the two agree rather than disagreeing.
    pub dev: (u32, u32),
    pub inode: u64,
    /// What goes in the last column, or `None` for a blank one.
    pub name: Option<String>,
}

/// Render `/proc/<pid>/maps`: one line per mapping, in address order.
///
/// Linux's layout exactly - `start-end perms offset dev inode name` - because
/// the file is parsed rather than read: `pmap`, `lsof`-style tools and every
/// crash reporter split it on whitespace and index into it.
///
/// ```
/// use mpos::vfs::procfs::{MapLine, format_maps};
/// use mpos_vm::MapFlags;
///
/// let line = MapLine {
///     start: 0x55_5555_4000,
///     end: 0x55_5555_8000,
///     prot: MapFlags::READ | MapFlags::EXEC,
///     shared: false,
///     offset: 0x1000,
///     dev: (0x08, 0x01),
///     inode: 12345,
///     name: Some(String::from("/bin/sh")),
/// };
/// let text = format_maps(&[line]);
/// assert!(text.starts_with("5555554000-5555558000 r-xp 00001000 08:01 12345 "));
/// // The name column starts at a fixed offset, which is what a parser indexes.
/// assert_eq!(text.find("/bin/sh"), Some(73));
/// assert!(text.ends_with('\n'));
/// ```
pub fn format_maps(lines: &[MapLine]) -> String {
    format_map_lines(lines.iter())
}

/// The body of [`format_maps`] and of every `smaps` record's header.
///
/// An iterator of borrows rather than a slice, so the file that *owns* its
/// records (`smaps`, whose field lines are rendered from the same records) does
/// not clone them to print their headers.
fn format_map_lines<'a>(lines: impl Iterator<Item = &'a MapLine>) -> String {
    let mut out = String::new();
    for line in lines {
        let perms = [
            if line.prot.contains(MapFlags::READ) {
                'r'
            } else {
                '-'
            },
            if line.prot.contains(MapFlags::WRITE) {
                'w'
            } else {
                '-'
            },
            if line.prot.contains(MapFlags::EXEC) {
                'x'
            } else {
                '-'
            },
            if line.shared { 's' } else { 'p' },
        ];
        // `%08lx` with a 64-bit value: eight digits at least, and every digit a
        // full-width address needs - which is why a line's address field is
        // 12 or 16 characters long rather than fixed.
        let mut rendered = alloc::format!(
            "{:08x}-{:08x} {} {:08x} {:02x}:{:02x} {} ",
            line.start,
            line.end,
            perms.iter().collect::<String>(),
            line.offset,
            line.dev.0,
            line.dev.1,
            line.inode,
        );
        // An unnamed mapping is the prefix and the space that would have
        // preceded a name - not a padded line with an empty column, which is
        // what a reader that counts spaces would trip over.
        if let Some(name) = &line.name {
            while rendered.len() < MAPS_NAME_COLUMN {
                rendered.push(' ');
            }
            rendered.push_str(name);
        }
        out.push_str(&rendered);
        out.push('\n');
    }
    out
}

/// One record of `/proc/<pid>/smaps`: the [`MapLine`] both files' headers are,
/// plus the two facts only this file's field lines need.
///
/// The fields are resolved before the formatting for the reason `MapLine`'s
/// are: [`format_smaps`] is the ABI and is compiled for the host, while
/// gathering them asks the page tables, which only exist on the machine.
pub struct SmapsLine {
    pub map: MapLine,
    /// Resident pages of the range, counted from the page tables - the same
    /// fact `mincore` reads, per mapping instead of per caller's buffer.
    pub rss_pages: usize,
    /// Whether the pages belong to this address space alone: true for a range
    /// with no object behind it (an anonymous VMA, or a file the kernel read
    /// in), false for one whose pages a `memfd`/dma-buf/device owns and this
    /// mapping only aliases.
    ///
    /// This is Linux's `Anonymous` question - "is the page's content held
    /// anywhere but here" - and it is answered by the mapping's *kind* rather
    /// than by a count, because this kernel keeps no mapcount. Measured on a
    /// current host: a shared anonymous mapping and a `memfd`'s are both 0 kB
    /// anonymous (their page belongs to the object), a forked private anon page
    /// is 4 kB (`probe_smaps_shapes.c`).
    pub anonymous: bool,
}

/// Render `/proc/<pid>/smaps`: for each mapping, the `maps` line, then Linux's
/// field lines.
///
/// The field set, its order and its layout are Linux's `show_smap` exactly -
/// twenty-four `label: value` lines and a `VmFlags` word list, in the order
/// measured on a current host (`.workbuddy/probe_smaps.c`), with every value
/// right-aligned so that it ends in column 24 whatever its width. A parser that
/// reads this file as a map (one `name -> value` per record) is the whole
/// reason the layout is not this kernel's own.
///
/// What is not Linux's, and is named in `docs/uapi/` §5.2 with the measurement
/// beside it, is where three of the accounting numbers come from. This kernel
/// keeps no mapcount, no dirty bit and no page cache, so Linux's four
/// shared/private buckets and its proportional set size are not derivable; the
/// rule this file applies instead is one sentence - **every resident page is
/// this mapping's own, and it is dirty** - which makes `Pss`, `Pss_Dirty` and
/// `Private_Dirty` all `Rss`, and `Shared_*`/`Private_Clean` zero. It is the
/// only rule this kernel's model can state: a page becomes resident because
/// this address space faulted it in (`mm`'s demand paging), a private frame is
/// nobody else's until a `fork` shares it, and whether anything wrote a file
/// copy is not recorded anywhere.
///
/// The remaining lines are zero because the thing behind them does not exist
/// here: no KSM, no huge pages, no soft-dirty tracking's `THPeligible` page, no
/// pkeys, no swap, and no locked page - `mlock` is served as the no-op it is,
/// so `Locked` is the truth rather than a placeholder.
///
/// ```
/// use mpos::vfs::procfs::{MapLine, SmapsLine, format_smaps};
/// use mpos_vm::MapFlags;
///
/// let line = SmapsLine {
///     map: MapLine {
///         start: 0x7f_0000_0000,
///         end: 0x7f_0000_4000,
///         prot: MapFlags::READ | MapFlags::WRITE,
///         shared: false,
///         offset: 0,
///         dev: (0, 0),
///         inode: 0,
///         name: None,
///     },
///     rss_pages: 1,
///     anonymous: true,
/// };
/// let text = format_smaps(&[line]);
/// assert!(text.starts_with("7f00000000-7f00004000 rw-p 00000000 00:00 0 \nSize:"));
/// assert!(text.contains("\nSize:                 16 kB\n"));
/// assert!(text.contains("\nRss:                   4 kB\n"));
/// assert!(text.contains("\nPrivate_Dirty:         4 kB\n"));
/// assert!(text.contains("\nAnonymous:             4 kB\n"));
/// assert!(text.ends_with("VmFlags: rd wr mr mw me \n"));
/// ```
pub fn format_smaps(lines: &[SmapsLine]) -> String {
    let mut out = String::new();
    for line in lines {
        out.push_str(&format_map_lines(core::iter::once(&line.map)));
        let rss_kb = line.rss_pages as u64 * PAGE_KB;
        // The range's length in kilobytes, the way Linux's `show_smap` computes
        // it - `(vm_end - vm_start) / 1024`, not a page count times the frame
        // size, because a range need not be a whole number of frames.
        let size_kb = (line.map.end - line.map.start) as u64 / 1024;
        smaps_kb(&mut out, "Size:", size_kb);
        smaps_kb(&mut out, "KernelPageSize:", PAGE_KB);
        smaps_kb(&mut out, "MMUPageSize:", PAGE_KB);
        smaps_kb(&mut out, "Rss:", rss_kb);
        smaps_kb(&mut out, "Pss:", rss_kb);
        smaps_kb(&mut out, "Pss_Dirty:", rss_kb);
        smaps_kb(&mut out, "Shared_Clean:", 0);
        smaps_kb(&mut out, "Shared_Dirty:", 0);
        smaps_kb(&mut out, "Private_Clean:", 0);
        smaps_kb(&mut out, "Private_Dirty:", rss_kb);
        smaps_kb(&mut out, "Referenced:", rss_kb);
        smaps_kb(
            &mut out,
            "Anonymous:",
            if line.anonymous { rss_kb } else { 0 },
        );
        for absent in [
            "KSM:",
            "LazyFree:",
            "AnonHugePages:",
            "ShmemPmdMapped:",
            "FilePmdMapped:",
            "Shared_Hugetlb:",
            "Private_Hugetlb:",
            "Swap:",
            "SwapPss:",
            "Locked:",
        ] {
            smaps_kb(&mut out, absent, 0);
        }
        // These two are numbers rather than "kB" counts - `THPeligible` a flag
        // and `ProtectionKey` a pkey - and Linux prints them without a unit.
        smaps_number(&mut out, "THPeligible:", 0);
        smaps_number(&mut out, "ProtectionKey:", 0);
        // Linux writes the label, a space, and then every word *followed* by a
        // space - so the line ends in one ("VmFlags: rd mr mw me \n"), which is
        // the shape a reader splitting on spaces sees.
        out.push_str("VmFlags: ");
        for word in vma_flags(&line.map) {
            out.push_str(word);
            out.push(' ');
        }
        out.push('\n');
    }
    out
}

/// The frame size in kilobytes, for the three lines that report it.
///
/// Both machines' frames are 4096 bytes (`mpos_vm`'s `PAGE_SIZE`, which is the
/// one definition), and the renderer is compiled for the host as well as for
/// the machine, so the number comes from that leaf crate rather than from the
/// port.
const PAGE_KB: u64 = (crate::mm::pframe::PAGE_SIZE / 1024) as u64;

/// One `label: value kB` line, laid out the way Linux's `seq_printf` lays it
/// out: the label padded to sixteen columns and the number ending in column 24.
///
/// Measured rather than assumed (`probe_smaps.c`): `Size:` prints `8` and
/// `136` with both ending in the same column, which is what makes the file
/// readable by a fixed-column reader.
fn smaps_kb(out: &mut String, label: &str, kb: u64) {
    out.push_str(&alloc::format!("{label:<16}{kb:>8} kB\n"));
}

/// The same layout for the two lines Linux prints a bare number on.
fn smaps_number(out: &mut String, label: &str, value: u64) {
    out.push_str(&alloc::format!("{label:<16}{value:>8}\n"));
}

/// The `VmFlags` words this kernel can stand behind, in Linux's order.
///
/// Linux's vocabulary is its `vm_flags` bitset's, and most of it describes
/// features this kernel does not have (huge pages, soft-dirty, pkeys, mlock,
/// accounting, write-notify). What is printed here is the subset whose
/// condition this kernel really meets, each with its reason:
///
/// * `rd`/`wr`/`ex` are the mapping's *protection* as the caller was granted it
///   (`granted` resolves the VMA's "present" bit into the grant), which is the
///   permission Linux's `VM_READ`/`VM_WRITE`/`VM_EXEC` also describe;
/// * `sh` is `MAP_SHARED`, which is the same bit `/proc/<pid>/maps` prints as
///   the fourth permission character;
/// * `mr`/`mw`/`me` are Linux's `VM_MAY*` - what the mapping *could* be given -
///   and they are unconditional here because `mprotect` asks no may-question:
///   any mapping this kernel has can be given any protection (measured against
///   this kernel's `VmaTable::set_prot`, which has no may-check to fail). A
///   `PROT_NONE` guard page therefore prints `mr mw me` exactly as Linux's does;
/// * `ms` is `VM_MAYSHARE`, which is the same sharing bit for the same reason
///   as `sh`: nothing here can turn a private mapping shared or the reverse.
///
/// The words that are *absent* are the named half of this file's divergence
/// (`docs/uapi/` §5.2): `sd` (soft-dirty) would claim a state nothing here
/// records, `ac` (accounted) an overcommit accounting that does not exist, and
/// `lo` (locked) a lock `mlock` does not take.
fn vma_flags(line: &MapLine) -> alloc::vec::Vec<&'static str> {
    let mut words = alloc::vec::Vec::new();
    if line.prot.contains(MapFlags::READ) {
        words.push("rd");
    }
    if line.prot.contains(MapFlags::WRITE) {
        words.push("wr");
    }
    if line.prot.contains(MapFlags::EXEC) {
        words.push("ex");
    }
    if line.shared {
        words.push("sh");
    }
    words.push("mr");
    words.push("mw");
    words.push("me");
    if line.shared {
        words.push("ms");
    }
    words
}

/// Render `/proc/<pid>/numa_maps`: one line per mapping - the policy, the name,
/// and the page counts per node.
///
/// Linux's line is `<start> <policy>[ file=<path>| heap| stack] anon=N dirty=N
/// mapped=N mapmax=N active=N N<node>=N kernelpagesize_kB=4`, and every one of
/// those was measured on a current host before this file was written
/// (`.workbuddy/probe_numamaps.c`, `probe_numamaps2.c`, Linux 7.2.11, one node).
/// What the measurement says, and what this kernel can stand behind:
///
/// * **the address is the start alone** - no `-end`, unlike a `maps` line - and
///   it is followed by the policy word, which is `default` for every mapping
///   here because `set_mempolicy`/`mbind` are not served and `default` is the
///   policy a task has when it never asked for one;
/// * **the name is `file=<path>` when a path is known, `heap` for the heap, and
///   nothing at all otherwise** - `[vdso]` and `[vvar_vclock]` print no name on
///   Linux either, measured; a space in the path is `\040` and a tab `\011`
///   (`seq_file_path`'s octal escape, measured with a path carrying both);
/// * **a range with no resident page prints the policy and the name and nothing
///   else** (measured: an untouched mapping, and a read-only anonymous one whose
///   read fault Linux satisfied with the shared zero page);
/// * **`anon=` and `dirty=` are the whole count** - every resident page here is
///   this mapping's own and dirty, which is the same rule `smaps` applies
///   (`format_smaps`) for the same reason: no mapcount, no dirty bit, no page
///   cache. Linux prints `anon=N` only for a range whose pages are anonymous and
///   `dirty=N` for any dirty ones, which is what this kernel's own two kinds map
///   onto;
/// * **`active=0`** - the count is the pages on Linux's active LRU list and this
///   kernel keeps no LRU, so the number it can state is zero, and Linux prints
///   the field whenever that count differs from the total, which is every range
///   that holds a page;
/// * **`N0=` is the node's count** and there is one node, so it is the resident
///   count; `kernelpagesize_kB` is the frame size, 4 kB on both machines.
///
/// Two of Linux's fields are **not** printed, and both are named rather than
/// guessed: `mapped=` (Linux prints it where its `anon`+`file` counts do not add
/// up to the total - a clean file-backed range - and this kernel cannot tell a
/// clean page from a written one) and `mapmax=` (the highest mapcount of any page
/// in the range, and there is no mapcount - the same fact `smaps`' `Pss` names).
/// Linux's `stack` word is absent too: this kernel names the heap and not the
/// stack, so the stack's range prints no name where Linux prints `stack`.
///
/// ```
/// use mpos::vfs::procfs::{MapLine, SmapsLine, format_numa_maps};
/// use mpos_vm::MapFlags;
///
/// let anon = SmapsLine {
///     map: MapLine {
///         start: 0x7f_0000_0000,
///         end: 0x7f_0000_4000,
///         prot: MapFlags::READ | MapFlags::WRITE,
///         shared: false,
///         offset: 0,
///         dev: (0, 0),
///         inode: 0,
///         name: None,
///     },
///     rss_pages: 2,
///     anonymous: true,
/// };
/// let untouched = SmapsLine {
///     map: MapLine {
///         start: 0x55_5555_4000,
///         end: 0x55_5555_8000,
///         prot: MapFlags::READ,
///         shared: false,
///         offset: 0,
///         dev: (0, 0),
///         inode: 7,
///         name: Some(String::from("/bin/sh")),
///     },
///     rss_pages: 0,
///     anonymous: false,
/// };
/// let text = format_numa_maps(&[anon, untouched]);
/// assert_eq!(text.lines().next().unwrap(),
///            "7f00000000 default anon=2 dirty=2 active=0 N0=2 kernelpagesize_kB=4");
/// // A range with no resident page says the name and the policy, and no number.
/// assert_eq!(text.lines().nth(1).unwrap(), "5555554000 default file=/bin/sh");
/// ```
pub fn format_numa_maps(lines: &[SmapsLine]) -> String {
    let mut out = String::new();
    for line in lines {
        // `%08lx`: eight digits at least, and every digit the address needs.
        out.push_str(&alloc::format!("{:08x} default", line.map.start));
        match line.map.name.as_deref() {
            // The heap's spelling is a word of its own, not a path - measured.
            Some("[heap]") => out.push_str(" heap"),
            Some(path) if path.starts_with('/') => {
                out.push_str(" file=");
                push_escaped_path(&mut out, path);
            }
            // A name that is not a path (`[vdso]`, `[vvar]`) is not printed at
            // all, which is what Linux prints for those too.
            _ => {}
        }
        let pages = line.rss_pages;
        if pages > 0 {
            if line.anonymous {
                out.push_str(&alloc::format!(" anon={pages}"));
            }
            out.push_str(&alloc::format!(" dirty={pages}"));
            out.push_str(" active=0");
            out.push_str(&alloc::format!(" N0={pages}"));
            out.push_str(" kernelpagesize_kB=4");
        }
        out.push('\n');
    }
    out
}

/// A path with the four characters `seq_file_path` escapes written as `\NNN`.
///
/// Measured rather than taken from the header: a path carrying a space comes
/// back `a\040b`, one carrying a tab `a\011b`, and a backslash is left alone.
fn push_escaped_path(out: &mut String, path: &str) {
    for ch in path.chars() {
        match ch {
            ' ' => out.push_str("\\040"),
            '\t' => out.push_str("\\011"),
            '\n' => out.push_str("\\012"),
            other => out.push(other),
        }
    }
}

/// Render `/proc/<pid>/smaps_rollup`: one record for the whole address space.
///
/// The same accounting `smaps` prints, summed, under a header that is the
/// *bounds* of the space rather than one mapping's - `<first start>-<last end>
/// ---p 00000000 00:00 0` and then `[rollup]` in the name column, at the same
/// column a `maps` line names its mapping (73, measured). It exists because
/// `smaps` costs a walk of every mapping and a reader that only wants the total
/// - Android's low-memory killer, a compositor asking what its clients hold -
/// was reading the whole file to add it up.
///
/// Its field list is `smaps`' with three taken out and three put in (measured on
/// a current host, `.workbuddy/probe_rollup.c`): out go the three that belong to
/// one mapping (`Size`, `KernelPageSize`, `MMUPageSize`) and the three that are
/// per-mapping flags (`THPeligible`, `ProtectionKey`, `VmFlags`); in come
/// `Pss_Anon`, `Pss_File` and `Pss_Shmem`, the split of the proportional set
/// size. Twenty-two lines, in Linux's order, in `smaps`' layout.
///
/// The numbers are the sums of the same numbers `smaps` prints, and they are
/// therefore this kernel's own answers to the same three questions
/// (`format_smaps` carries the measurement): `Pss` and `Pss_Dirty` are `Rss`,
/// the four shared/private buckets collapse into `Private_Dirty`, and
/// `Referenced` is `Rss`. Of the three that `smaps` has no room for, `Pss_Anon`
/// is the sum over the ranges that are anonymous and `Pss_File` the sum over the
/// rest - which is the one split this kernel *can* make, since it is the
/// question `Anonymous` already answers - and `Pss_Shmem` is **zero**, because a
/// `memfd`'s pages are an object's here and Linux's shmem/file distinction is a
/// distinction about its page cache. That is named rather than approximated.
///
/// ```
/// use mpos::vfs::procfs::{MapLine, SmapsLine, format_smaps_rollup};
/// use mpos_vm::MapFlags;
///
/// let line = |start: usize, end: usize, pages: usize, anonymous: bool| SmapsLine {
///     map: MapLine {
///         start, end,
///         prot: MapFlags::READ | MapFlags::WRITE,
///         shared: false, offset: 0, dev: (0, 0), inode: 0, name: None,
///     },
///     rss_pages: pages,
///     anonymous,
/// };
/// let text = format_smaps_rollup(&[
///     line(0x1000, 0x2000, 1, true),
///     line(0x2000, 0x3000, 2, false),
/// ]);
/// // The header is the bounds of the whole space, not one mapping's.
/// assert!(text.starts_with("00001000-00003000 ---p 00000000 00:00 0 "));
/// assert_eq!(text.find("[rollup]"), Some(73));
/// assert!(text.contains("\nRss:                  12 kB\n"));
/// assert!(text.contains("\nPss_Anon:              4 kB\n"));
/// assert!(text.contains("\nPss_File:              8 kB\n"));
/// // The three per-mapping lines are gone, and so are the three flags.
/// assert!(!text.contains("Size:"));
/// assert!(!text.contains("THPeligible:"));
/// assert!(!text.contains("VmFlags:"));
/// ```
pub fn format_smaps_rollup(lines: &[SmapsLine]) -> String {
    let mut out = String::new();
    // The header is the space's bounds: the first range's start and the last
    // one's end, which is what Linux prints (measured against `maps`' own first
    // and last line). An address space with no mapping at all has no bounds,
    // and Linux answers the empty string for one.
    if let (Some(first), Some(last)) = (lines.first(), lines.last()) {
        let header = MapLine {
            start: first.map.start,
            end: last.map.end,
            prot: MapFlags::empty(),
            shared: false,
            offset: 0,
            dev: (0, 0),
            inode: 0,
            name: Some(String::from("[rollup]")),
        };
        out.push_str(&format_map_lines(core::iter::once(&header)));
    }

    let mut rss_kb = 0u64;
    let mut anon_kb = 0u64;
    let mut file_kb = 0u64;
    for line in lines {
        let kb = line.rss_pages as u64 * PAGE_KB;
        rss_kb += kb;
        if line.anonymous {
            anon_kb += kb;
        } else {
            file_kb += kb;
        }
    }
    smaps_kb(&mut out, "Rss:", rss_kb);
    smaps_kb(&mut out, "Pss:", rss_kb);
    smaps_kb(&mut out, "Pss_Dirty:", rss_kb);
    smaps_kb(&mut out, "Pss_Anon:", anon_kb);
    smaps_kb(&mut out, "Pss_File:", file_kb);
    smaps_kb(&mut out, "Pss_Shmem:", 0);
    smaps_kb(&mut out, "Shared_Clean:", 0);
    smaps_kb(&mut out, "Shared_Dirty:", 0);
    smaps_kb(&mut out, "Private_Clean:", 0);
    smaps_kb(&mut out, "Private_Dirty:", rss_kb);
    smaps_kb(&mut out, "Referenced:", rss_kb);
    smaps_kb(&mut out, "Anonymous:", anon_kb);
    for absent in [
        "KSM:",
        "LazyFree:",
        "AnonHugePages:",
        "ShmemPmdMapped:",
        "FilePmdMapped:",
        "Shared_Hugetlb:",
        "Private_Hugetlb:",
        "Swap:",
        "SwapPss:",
        "Locked:",
    ] {
        smaps_kb(&mut out, absent, 0);
    }
    out
}

/// Every range of `pid`'s address space, as both `maps` and `smaps` report it.
///
/// Three sources, because the address space is recorded in three places: the
/// VMA table holds what `mmap`, `mprotect` and `brk` put there, the loader's
/// recorded ranges hold what `execve` and a private file mapping filled in
/// eagerly (neither leaves a VMA - see [`crate::mm::vma::MappedRange`]), and the
/// program break is what names the heap.
///
/// The residency a [`SmapsLine`] carries is left at zero here: `maps` does not
/// report it, and counting it is one page-table walk per mapped page, so the
/// caller that wants it asks for it.
///
/// What this cannot report, and does not pretend to: a mapping's `offset` and
/// `inode` when the pages are a *copy* of a file rather than the file's own
/// pages. This kernel reads such a file in rather than paging it, so there is
/// no object left to name - the mapping gets the file's name and zeroes for the
/// two columns, and `docs/notes/` §7.4 records the gap.
#[cfg(target_os = "none")]
fn mapping_lines(pid: usize) -> Option<Vec<SmapsLine>> {
    use crate::mm::vma::{Vma, VmaKind};

    let facts = crate::sched::map_facts(pid)?;

    // `[heap]` is a reporting convention rather than a mapping's own name, so
    // it is applied here, by range: Linux prints it for the break's VMA, and
    // the break's VMA here is an ordinary anonymous one (see `sched::brk`).
    let heap_name = |vma: &Vma| -> Option<String> {
        let (start, end) = facts.heap?;
        (vma.start >= start && vma.end <= end && vma.name.is_none()).then(|| String::from("[heap]"))
    };

    let mut lines: Vec<SmapsLine> = Vec::new();
    for vma in facts.vmas.iter() {
        let (offset, dev, inode, shared) = match &vma.kind {
            VmaKind::Anon => (0, (0, 0), 0, false),
            VmaKind::Frames {
                object,
                offset,
                shared,
            } => {
                // A device node reports the device it is; a file reports the
                // filesystem it is on, and this kernel's `stat` says zero for
                // every filesystem, so the two files that read the same fact do
                // not disagree about it. Both halves come from the object's own
                // answer (`mm::vma::MappedObject::identity`), which is the same
                // pair the object compares itself by.
                let (dev, inode) = object.identity();
                (*offset as u64, dev, inode, *shared)
            }
        };
        let name = match &vma.name {
            Some(name) => Some(String::from(&**name)),
            None => heap_name(vma),
        };
        lines.push(SmapsLine {
            map: MapLine {
                start: vma.start,
                end: vma.end,
                prot: granted(vma.prot),
                shared,
                offset,
                dev,
                inode,
                name,
            },
            rss_pages: 0,
            anonymous: matches!(&vma.kind, VmaKind::Anon),
        });
    }
    for range in &facts.eager {
        lines.push(SmapsLine {
            map: MapLine {
                start: range.start,
                end: range.end,
                prot: granted(range.prot),
                // Eagerly-filled ranges are a private copy or an image: `private`,
                // which is what Linux prints for both.
                shared: false,
                offset: 0,
                dev: (0, 0),
                inode: 0,
                name: range.name.as_deref().map(String::from),
            },
            rss_pages: 0,
            // An eager range is a file's bytes read into this process's own
            // frames, with nothing of the file left behind: the pages are
            // nobody else's, which is the question `Anonymous` asks.
            anonymous: true,
        });
    }

    lines.sort_by_key(|line| (line.map.start, line.map.end));
    Some(lines)
}

/// `/proc/<pid>/maps`: every range of the address space, one line each.
#[cfg(target_os = "none")]
fn maps_of(pid: usize) -> Option<String> {
    Some(format_map_lines(
        mapping_lines(pid)?.iter().map(|line| &line.map),
    ))
}

/// `/proc/<pid>/smaps`: the same records with per-mapping accounting.
///
/// `maps` with a field block under each line, which is Linux's file and the one
/// every memory-reporting tool reads (`smem`, `ps_mem`, and a compositor asking
/// how much of its client's buffer is resident). The header of each record is
/// the `maps` line byte for byte - measured on a current host over a process
/// whose mappings cover every shape (`probe_smaps_shapes.c`: 34 of 34 records
/// agreed) - so a reader may compare the two files.
///
/// The residency is counted page by page in `sched::resident_pages`, which is
/// the only way it can be counted here: this kernel's resident set *is* the page
/// tables.
/// The records with their residency counted - what the three files that are
/// `maps` plus accounting all read.
///
/// Counting is one page-table walk per mapped page, which is why `maps` does not
/// do it: `numa_maps` and the two `smaps` spellings are the only callers.
#[cfg(target_os = "none")]
fn resident_lines(pid: usize) -> Option<Vec<SmapsLine>> {
    let mut lines = mapping_lines(pid)?;
    for line in lines.iter_mut() {
        line.rss_pages = crate::sched::resident_pages(pid, line.map.start, line.map.end);
    }
    Some(lines)
}

#[cfg(target_os = "none")]
fn smaps_of(pid: usize) -> Option<String> {
    Some(format_smaps(&resident_lines(pid)?))
}

/// `/proc/<pid>/numa_maps`: the policy and the per-node counts, one line per
/// mapping. The counts are the same residency `smaps` reports; see
/// [`format_numa_maps`] for what is and is not Linux's.
#[cfg(target_os = "none")]
fn numa_maps_of(pid: usize) -> Option<String> {
    Some(format_numa_maps(&resident_lines(pid)?))
}

/// `/proc/<pid>/smaps_rollup`: one record for the whole address space, for the
/// reader that wants the total and not the walk. See [`format_smaps_rollup`].
#[cfg(target_os = "none")]
fn smaps_rollup_of(pid: usize) -> Option<String> {
    Some(format_smaps_rollup(&resident_lines(pid)?))
}

#[cfg(not(target_os = "none"))]
fn maps_of(_pid: usize) -> Option<String> {
    None
}

#[cfg(not(target_os = "none"))]
fn smaps_of(_pid: usize) -> Option<String> {
    None
}

#[cfg(not(target_os = "none"))]
fn numa_maps_of(_pid: usize) -> Option<String> {
    None
}

#[cfg(not(target_os = "none"))]
fn smaps_rollup_of(_pid: usize) -> Option<String> {
    None
}

/// `/proc/<pid>/cmdline`: the argument vector, NUL-separated, with a NUL
/// after the last one.
///
/// Empty - not absent - for a process that was started without one, which is
/// what Linux reports for a kernel thread and what makes `ps` fall back to the
/// name in brackets rather than print a blank command.
fn cmdline_of(pid: usize) -> Option<String> {
    let task = task(pid)?;
    let mut out = String::new();
    for arg in task.argv.iter() {
        out.push_str(arg);
        out.push('\0');
    }
    Some(out)
}

/// `/proc/<pid>/environ`: the environment, NUL-separated, with a NUL after the
/// last `KEY=VALUE`.
///
/// Empty - not absent - for a process that was entered without one, which is
/// what Linux reports for a kernel thread and what a reader must be able to
/// tell from a missing file.
///
/// The file is not decoration, and the boot that lacked it says so. OpenRC's
/// `init.sh` decides whether `/proc` still needs mounting by reading *this*
/// file twice under two different values of a variable it sets on the command
/// line - `VAR=a md5sum /proc/self/environ` against `VAR=b md5sum ...` - and
/// treating two *different* digests as proof that a live procfs is already
/// there (identical digests mean the path is stale cruft in the root
/// filesystem, which is the case the test exists to catch). With the name
/// absent, `[ -e /proc/self/environ ]` fails, so the check never runs, OpenRC
/// concludes "not mounted" and mounts over the procfs our disk `/init` had
/// already mounted: `mount: mounting proc on /proc failed: Resource busy`,
/// with the `/proc` service reporting `[ !! ]` on the Alpine boot.
fn environ_of(pid: usize) -> Option<String> {
    let task = task(pid)?;
    let mut out = String::new();
    for var in task.env.iter() {
        out.push_str(var);
        out.push('\0');
    }
    Some(out)
}

/// `/proc/<pid>/status`: the same facts `stat` carries, in the long form a
/// human or a `top` reads.
///
/// `slot` is the task the file is about, and the two id lines are the pair
/// Linux prints rather than one number twice: `Tgid` is the *group's* and
/// `Pid` is the *task's*, so a thread's own directory reports the tid where its
/// leader's reports the pid. Measured on a current host
/// (`.workbuddy/probe_tid_dir.c`, Linux 7.2.11) for a live thread of a live
/// group: `Tgid 16084 / Pid 16085 / NStgid 16084 / NSpid 16085 / NSpgid 16082 /
/// NSsid 16075`, with the `NS*` chains running the same way - `NStgid` the
/// leader's chain and `NSpid` the task's. `Name` is the task's own `comm`,
/// which a thread may have set for itself (`PR_SET_NAME`: the probe's thread
/// says `wdthread` where its leader says `probe_tid_dir`), and `Threads` is the
/// group's either way. A leader is the case where the two ids coincide, which
/// is why this was invisible through `/proc/<pid>` alone.
fn status_of(slot: usize) -> Option<String> {
    let task = task(slot)?;
    let leader = task.pid;
    let kb = task.rss_pages * 4096 / 1024;
    let state_name = match task.state {
        'R' => "running",
        'S' => "sleeping",
        'Z' => "zombie",
        _ => "unknown",
    };
    Some(alloc::format!(
        "Name:\t{name}\n\
         Umask:\t0022\n\
         State:\t{state} ({state_name})\n\
         Tgid:\t{tgid}\n\
         Ngid:\t0\n\
         Pid:\t{pid}\n\
         PPid:\t{ppid}\n\
         TracerPid:\t0\n\
         Uid:\t0\t0\t0\t0\n\
         Gid:\t0\t0\t0\t0\n\
         FDSize:\t64\n\
         Groups:\t\n\
         NStgid:\t{nstgid}\n\
         NSpid:\t{nspid}\n\
         NSpgid:\t{nspgid}\n\
         NSsid:\t{nssid}\n\
         VmPeak:\t   {kb} kB\n\
         VmSize:\t   {kb} kB\n\
         VmLck:\t       0 kB\n\
         VmPin:\t       0 kB\n\
         VmHWM:\t   {kb} kB\n\
         VmRSS:\t   {kb} kB\n\
         RssAnon:\t   {kb} kB\n\
         RssFile:\t       0 kB\n\
         RssShmem:\t       0 kB\n\
         VmData:\t   {kb} kB\n\
         VmStk:\t       0 kB\n\
         VmExe:\t       0 kB\n\
         VmLib:\t       0 kB\n\
         VmPTE:\t       0 kB\n\
         VmSwap:\t       0 kB\n\
         HugetlbPages:\t       0 kB\n\
         CoreDumping:\t0\n\
         Threads:\t{threads}\n\
         SigQ:\t0/64\n\
         SigPnd:\t0000000000000000\n\
         ShdPnd:\t0000000000000000\n\
         SigBlk:\t0000000000000000\n\
         SigIgn:\t0000000000000000\n\
         SigCgt:\t0000000000000000\n\
         CapInh:\t0000000000000000\n\
         CapPrm:\t000001ffffffffff\n\
         CapEff:\t000001ffffffffff\n\
         CapBnd:\t000001ffffffffff\n\
         CapAmb:\t0000000000000000\n\
         NoNewPrivs:\t0\n\
         Seccomp:\t0\n\
         Seccomp_filters:\t0\n\
         Cpus_allowed:\t1\n\
         Cpus_allowed_list:\t0\n\
         Mems_allowed:\t1\n\
         Mems_allowed_list:\t0\n\
         voluntary_ctxt_switches:\t0\n\
         nonvoluntary_ctxt_switches:\t0\n",
        name = task.comm,
        state = task.state,
        tgid = reader_number(leader),
        pid = reader_number(slot),
        ppid = reader_number(task.ppid),
        // The `NS*` fields are the *chain* the task is known by rather than the
        // reader's one number: `4231` for a task of the machine's namespace, and
        // `4231\t1` for one a namespace in - Linux's leftmost entry is the
        // namespace the procfs was mounted in, and this procfs is the machine's.
        // `NStgid` chains the *group* and `NSpid` the task, exactly as the
        // reader's pair above does.
        nstgid = number_chain(leader),
        nspid = number_chain(slot),
        nspgid = number_chain(task.pgid),
        // The session's chain rather than a hard `0`: Linux carries the session
        // id in each namespace too, and a task in no session has the leader id 0,
        // whose chain is the one zero this prints.
        nssid = number_chain(task.session),
        threads = task.threads,
    ))
}

/// `/proc/<pid>/task/<tid>/children`: the children **this task** forked, space
/// separated, as `pstree` reads it to walk the tree without scanning /proc.
///
/// The file belongs to the *thread* set, so it is reached through `task/` and
/// nowhere else: measured on a current host
/// (`.workbuddy/probe_children_owner.c`, Linux 7.2.11), `/proc/<pid>/children`
/// and `/proc/<tid>/children` are both `ENOENT` even for a task that has
/// children - `children` is one of the names Linux's `tid_base_stuff` carries
/// and `base_stuff` does not - which is why [`process_dir`] adds none, and why
/// the tid's own directory ([`process_dir`]'s set) has none either.
///
/// The text is Linux's exactly, byte for byte: `<pid> ` per child and
/// **nothing at all** for a task that has forked nobody. Measured in the same
/// run: two children answered `"42205 42206 "` - each id followed by a space -
/// one child `"42208 "`, and a childless task a **zero-byte** file rather than
/// a blank line.
fn children_of(slot: usize) -> Option<String> {
    // The gate is the *task's* own, the same clause its directory answers
    // ([`tid_has_a_directory`]) rather than the leader's `pid_exists`: a thread
    // that has exited takes its tid's directory with it (item 64), so its
    // `children` file has to leave with it too - and a zombie *process* keeps
    // both, which is the half `wait` reads a record out of.
    let this = task(slot)?;
    if !tid_has_a_directory(this.state, this.tid, this.pid) {
        return None;
    }
    let mut out = String::new();
    for id in child_ids(slot) {
        out.push_str(&alloc::format!("{} ", reader_number(id)));
    }
    Some(out)
}

/// `/proc/<pid>/limits`: the `RLIMIT_*` table `setrlimit` writes, rendered the
/// way Linux does - a header, then one named row per resource with the soft
/// and hard ceilings, `unlimited` for `RLIM_INFINITY`. `ulimit` reads it.
#[cfg(target_os = "none")]
fn limits_of_pid(pid: usize) -> Option<String> {
    let rl = crate::sched::rlimits_of(pid)?;
    let table: &[(&str, &str)] = &[
        ("Max cpu time", "seconds"),
        ("Max file size", "bytes"),
        ("Max data size", "bytes"),
        ("Max stack size", "bytes"),
        ("Max core file size", "bytes"),
        ("Max resident set", "bytes"),
        ("Max processes", "processes"),
        ("Max open files", "files"),
        ("Max locked memory", "bytes"),
        ("Max address space", "bytes"),
        ("Max file locks", "locks"),
        ("Max pending signals", "signals"),
        ("Max msgqueue size", "bytes"),
        ("Max nice priority", ""),
        ("Max realtime priority", ""),
        ("Max realtime timeout", "us"),
    ];
    let fmt = |v: u64| -> String {
        if v == u64::MAX {
            String::from("unlimited")
        } else {
            alloc::format!("{}", v)
        }
    };
    let mut out = String::from(
        "Limit                     Soft Limit           Hard Limit           Units     \n",
    );
    for (i, (name, unit)) in table.iter().enumerate() {
        let r = rl.get(i)?;
        out.push_str(&alloc::format!(
            "{:<26} {:<20} {:<20} {}\n",
            name,
            fmt(r.cur),
            fmt(r.max),
            unit
        ));
    }
    Some(out)
}

/// `/proc/<pid>/task/<tid>`: one thread's directory, and a *narrower* set than
/// the tid's own `/proc/<tid>` answers with.
///
/// The same files a process has, named for the thread, plus `children` - the
/// one name this set carries and the *process* set does not, which is Linux's
/// own split (`base_stuff` against `tid_base_stuff`) rather than a statement
/// about whose children they are. It is the *thread's* list, and its doc
/// carries the measurement: a child forked by a non-leader thread is listed
/// here and on the leader's file nowhere. Measured whole on a current host
/// (`.workbuddy/probe_tid_dir.c`, `.workbuddy/probe_children_owner.c`, Linux
/// 7.2.11): the `task/` spelling carries `children` where `/proc/<tid>` does
/// not, and the other way round lacks `autogroup`, `coredump_filter`,
/// `map_files`, `mountstats`, `task`, `timens_offsets`, `timers` and
/// `timerslack_ns`. Which is why the tid's own directory is not built here:
/// [`process_dir`] is that set.
fn thread_dir(pid: usize, tid: usize) -> Arc<dyn Inode> {
    let node = dir();
    node.add(
        "stat",
        NodeKind::File,
        maybe_file(move || stat_line(pid, tid)),
    );
    node.add(
        "comm",
        NodeKind::File,
        maybe_file(move || Some(alloc::format!("{}\n", task(tid)?.comm))),
    );
    node.add(
        "cmdline",
        NodeKind::File,
        maybe_file(move || cmdline_of(tid)),
    );
    // Same file as the process's, under the thread's own name - a thread
    // shares its process's environment, and Linux answers it here too, with
    // the same `0400`.
    node.add("environ", NodeKind::File, env_file(move || environ_of(tid)));
    node.add("status", NodeKind::File, maybe_file(move || status_of(tid)));
    node.add(
        "children",
        NodeKind::File,
        maybe_file(move || children_of(tid)),
    );
    // The process's control-group membership, the v2 rendering - one line,
    // `0::<path>`, because the hierarchy has no controllers to name. It is
    // the cgroup tree's answer, asked per task: a thread of a moved group
    // reports it here the same moment its leader does.
    node.add(
        "cgroup",
        NodeKind::File,
        maybe_file(move || crate::vfs::cgroupfs::cgroup_line(tid)),
    );
    // A thread sees the same mounts its process does, and asks at its own
    // name: `/proc/thread-self/mountinfo` is where a threaded program looks.
    node.add("mounts", NodeKind::File, file(mounts_text));
    node.add("mountinfo", NodeKind::File, file(mountinfo_text));
    node
}

/// The range Linux lets `oom_score_adj` take - `OOM_SCORE_ADJ_MIN` and
/// `OOM_SCORE_ADJ_MAX`, the two ends of the scale the OOM killer scores on.
#[cfg(target_os = "none")]
const OOM_SCORE_ADJ_MIN: i32 = -1000;
#[cfg(target_os = "none")]
const OOM_SCORE_ADJ_MAX: i32 = 1000;

/// `/proc/<pid>/oom_score_adj`: the score adjustment itself, with the floor
/// Linux's `oom_score_adj_write` keeps beside it.
///
/// Three answers a caller can tell apart, and each is Linux's: a value that is
/// not a number or is outside the scale is `EINVAL` - `kstrtoint` refuses both
/// and the man page says the range outright. A process that has gone is
/// `ENOENT`, which is why the read is a generator that can fail rather than a
/// string: a write that races an exit has to report the vanished file, not
/// quietly succeed. And going below the floor the process has already been
/// given needs `CAP_SYS_RESOURCE`; this kernel has no capability set, so uid 0
/// is the reading of it - the same rule every other DAC check here already
/// applies, and a non-root writer hears `EACCES`.
#[cfg(target_os = "none")]
fn oom_score_adj_ctl(pid: usize) -> Arc<dyn Inode> {
    Arc::new(ProcCtl {
        ino: next_ino(),
        read: Box::new(move || {
            crate::sched::oom_score_adj_of(pid).map(|(value, _)| alloc::format!("{value}\n"))
        }),
        write: Box::new(move |text: &str| {
            let Ok(value) = text.parse::<i32>() else {
                return Err(VfsError::Invalid);
            };
            if !(OOM_SCORE_ADJ_MIN..=OOM_SCORE_ADJ_MAX).contains(&value) {
                return Err(VfsError::Invalid);
            }
            let Some((_, floor)) = crate::sched::oom_score_adj_of(pid) else {
                return Err(VfsError::NoEntry);
            };
            if value < floor && crate::sched::current_owner().0 != 0 {
                return Err(VfsError::Access);
            }
            if !crate::sched::set_oom_score_adj(pid, value) {
                return Err(VfsError::NoEntry);
            }
            Ok(())
        }),
    })
}

/// `/proc/<pid>/timens_offsets`: the offsets the process's **children** will
/// read their monotonic clocks through.
///
/// The file is the *children's* namespace's and not the process's own, which is
/// the whole reason it is useful: `unshare(CLONE_NEWTIME)` does not move the
/// caller, so a program that wants its children in a namespace with a different
/// clock has exactly one moment to say by how much - after the unshare and
/// before the fork - and this is the door it says it through. Measured on a real
/// kernel (7.2): the file of a task that has never unshared belongs to the
/// machine's namespace, reads two zero lines, and refuses the write with
/// `EACCES`, because `init` is in that namespace and its offsets are therefore
/// already frozen.
///
/// Three answers a caller can tell apart, and each is Linux's:
///
/// * **`EACCES`** for a writer that is not root. The measured file is `0644`
///   `root:root`, so the mode is the gate and not a capability - which with one
///   user namespace reads as "the owner must be 0". The same reading
///   [`oom_score_adj_ctl`] applies to its own `CAP_SYS_RESOURCE` floor, and the
///   same divergence rides with it: procfs here has no permission bits to check
///   at `open`, so a non-root opener gets its descriptor and hears the refusal
///   from the write rather than from the open.
/// * **`EACCES`** again once anything is in the namespace, which is the freeze
///   [`crate::timens::TimeNs::write_offsets`] keeps. That is a *different*
///   EACCES from the one above and Linux spells both the same way.
/// * **`EINVAL`** for text the grammar does not accept, and `ENOENT` for a
///   process that has gone - which is why the read can fail rather than handing
///   back a string, exactly as `oom_score_adj` reads.
#[cfg(target_os = "none")]
fn timens_offsets_ctl(pid: usize) -> Arc<dyn Inode> {
    /// The slot this file reads and writes: the `ForChildren` one, by the same
    /// name `/proc/<pid>/ns/time_for_children` lists.
    const SLOT: crate::vfs::ns::Entry = crate::vfs::ns::Entry {
        kind: crate::vfs::ns::Kind::Time,
        slot: crate::vfs::ns::Slot::ForChildren,
    };
    /// The namespace, asked of the live task rather than recorded when the file
    /// was built - a `unshare` a moment ago has to be visible to the next read.
    fn ns(pid: usize) -> Option<Arc<crate::timens::TimeNs>> {
        match crate::sched::ns_of(pid, SLOT) {
            Some(crate::vfs::ns::Namespace::Time(ns)) => Some(ns),
            _ => None,
        }
    }
    Arc::new(ProcCtl {
        ino: next_ino(),
        read: Box::new(move || ns(pid).map(|ns| ns.offsets_text())),
        write: Box::new(move |text: &str| {
            let Some(ns) = ns(pid) else {
                return Err(VfsError::NoEntry);
            };
            if crate::sched::current_owner().0 != 0 {
                return Err(VfsError::Access);
            }
            // The two refusals the leaf can answer are both `EACCES` or
            // `EINVAL`, and `VfsError` carries exactly one of each: a frozen
            // namespace and a bad grammar are the file's two ways to say no.
            ns.write_offsets(text).map_err(|e| {
                if e == crate::err::EACCES {
                    VfsError::Access
                } else {
                    VfsError::Invalid
                }
            })
        }),
    })
}

/// The user namespace a `/proc/<pid>/uid_map`-style file belongs to.
///
/// Asked of the live task like [`timens_offsets_ctl`]'s, so a `unshare` a moment
/// ago is visible to the next read, and a task that has gone answers `None`.
#[cfg(target_os = "none")]
fn user_ns_of(pid: usize) -> Option<Arc<crate::userns::UserNs>> {
    /// The one slot the kind has: a user namespace moves its caller, so there is
    /// no `user_for_children` spelling for the file to be about.
    const SLOT: crate::vfs::ns::Entry = crate::vfs::ns::Entry {
        kind: crate::vfs::ns::Kind::User,
        slot: crate::vfs::ns::Slot::Own,
    };
    match crate::sched::ns_of(pid, SLOT) {
        Some(crate::vfs::ns::Namespace::User(ns)) => Some(ns),
        _ => None,
    }
}

/// The leaf's three refusals, as the two a `VfsError` can carry.
///
/// The map files answer `EPERM` for a second write or an id the writer does not
/// own, `EINVAL` for text the grammar refuses, and `EACCES` when the gid map is
/// written before `setgroups=deny`. All three are the leaf's; this only names
/// them.
#[cfg(target_os = "none")]
fn map_errno(result: Result<(), isize>) -> Result<(), VfsError> {
    result.map_err(|e| {
        if e == crate::err::EINVAL {
            VfsError::Invalid
        } else if e == crate::err::EACCES {
            VfsError::Access
        } else {
            VfsError::NotPermitted
        }
    })
}

/// `/proc/<pid>/uid_map` and `/proc/<pid>/gid_map`: the table one side's ids are
/// translated through.
///
/// One builder for both because they are the same file with two names and two
/// capabilities behind them - `CAP_SETUID` and `CAP_SETGID` in the *parent*
/// namespace, which is what decides whether the writer gets the one-line rule or
/// the free one. The writer's own stored id is passed to the leaf, because that
/// is the only id an unprivileged writer may name on the outside (§6.3), and it
/// is the stored id and not the shown one: the outside column is the parent
/// namespace's numbering, which is what the identity holds.
#[cfg(target_os = "none")]
fn uid_map_ctl(pid: usize) -> Arc<dyn Inode> {
    map_ctl(pid, true)
}

#[cfg(target_os = "none")]
fn gid_map_ctl(pid: usize) -> Arc<dyn Inode> {
    map_ctl(pid, false)
}

#[cfg(target_os = "none")]
fn map_ctl(pid: usize, uid: bool) -> Arc<dyn Inode> {
    Arc::new(ProcCtl {
        ino: next_ino(),
        read: Box::new(move || {
            user_ns_of(pid).map(|ns| {
                if uid {
                    ns.uid_map_text()
                } else {
                    ns.gid_map_text()
                }
            })
        }),
        write: Box::new(move |text: &str| {
            let Some(ns) = user_ns_of(pid) else {
                return Err(VfsError::NoEntry);
            };
            let creds = crate::sched::current_credentials();
            let privileged = if uid {
                creds.has_effective(crate::cred::CAP_SETUID)
            } else {
                creds.has_effective(crate::cred::CAP_SETGID)
            };
            let writer = crate::sched::current_identity().euid;
            let result = if uid {
                ns.write_uid_map(text, privileged, writer)
            } else {
                ns.write_gid_map(text, privileged, writer)
            };
            map_errno(result)
        }),
    })
}

/// `/proc/<pid>/setgroups`: whether the members may call `setgroups(2)`.
///
/// The file an unprivileged gid-map writer has to turn to `deny` first, which is
/// why the gate here is *not* "root only": a process may always write its own,
/// and that is exactly the dance `newgidmap` performs - the child denies its own
/// supplementary groups, then the privileged helper writes the map. Anyone else
/// needs to be root. A real kernel reaches the same two answers through the
/// ptrace access check on `/proc/<pid>`; this kernel has the process's own
/// number to compare, which is the same question asked of a smaller table.
#[cfg(target_os = "none")]
fn setgroups_ctl(pid: usize) -> Arc<dyn Inode> {
    Arc::new(ProcCtl {
        ino: next_ino(),
        read: Box::new(move || {
            user_ns_of(pid).map(|ns| alloc::string::String::from(ns.setgroups_text()))
        }),
        write: Box::new(move |text: &str| {
            let Some(ns) = user_ns_of(pid) else {
                return Err(VfsError::NoEntry);
            };
            let mine = crate::sched::current().0 == pid;
            if !mine && crate::sched::current_owner().0 != 0 {
                return Err(VfsError::Access);
            }
            map_errno(ns.write_setgroups(text))
        }),
    })
}

/// `/proc/<pid>`: the directory a task is described by - a process at its
/// leader's number, and a **thread** at its tid, which Linux serves the same
/// file set under.
///
/// Built when the name is looked up and thrown away when the next lookup
/// finds the process gone - a process's `/proc` entry is not a thing the
/// filesystem keeps, it is a view of the task table, and a view kept after the
/// process left would be a directory of files all answering nothing.
///
/// The tid spelling is `/proc/<tid>`, and it is what a debugger reads: measured
/// on a current host under `strace`, `gdb -p <tid>` opens `/proc/<tid>/status`
/// outright, while `ps -eLf` reaches threads the other way - 306 opens of the
/// *relative* name `task` beside not one absolute `/proc/<pid>/task/<tid>`
/// path. So both spellings have a program behind them and neither is a
/// convenience.
fn process_dir(slot: usize) -> Option<Arc<dyn Inode>> {
    // A tid names a **thread**, and a thread is a task this tree may describe -
    // which `process_ids` (the *listing*'s filter, which asks for a leader) is
    // not, and which the leader's old `pid_exists` gate could not answer at
    // all. So the gate is the table's own: is there a user task in this slot at
    // all? `leader` is the group's, and it is what
    // the names whose Linux owner is the *thread group* are asked of - `task`
    // (one listing for the group) and `limits`/`oom_score_adj` (Linux keeps
    // both in the group's `signal_struct`). Everything else is the *task's* and
    // is asked of `slot`, which is what Linux does even where the two answers
    // agree by construction: `maps`, `fd` and `ns` name the task's own
    // structures, and a thread that unshared one is the case that tells them
    // apart. Measured whole on a current host first
    // (`.workbuddy/probe_tid_dir.c`, Linux 7.2.11, a live thread of a live
    // group): `/proc/<tid>` carries the *process* file set - its names are the
    // leader's exactly - while `/proc/<pid>/task/<tid>` is the narrower
    // *thread* set, which is `children` against eight process-only names
    // (`autogroup`, `coredump_filter`, `map_files`, `mountstats`, `task`,
    // `timens_offsets`, `timers`, `timerslack_ns`). That is why the tid's
    // directory is built here and not by [`thread_dir`], which is the `task/`
    // spelling's set.
    let this = task(slot)?;
    if !tid_has_a_directory(this.state, this.tid, this.pid) {
        return None;
    }
    let leader = this.pid;
    let node = dir();

    node.add(
        "stat",
        NodeKind::File,
        maybe_file(move || stat_line(leader, slot)),
    );
    node.add(
        "statm",
        NodeKind::File,
        maybe_file(move || statm_line(slot)),
    );
    node.add("maps", NodeKind::File, maybe_file(move || maps_of(slot)));
    // `smaps` is `maps` with a field block under each line, and its records'
    // headers *are* the `maps` lines - which is what lets a reader compare the
    // two files, and what `t-smaps` pins.
    node.add("smaps", NodeKind::File, maybe_file(move || smaps_of(slot)));
    // The other two spellings of the same accounting, so that a reader asking
    // for one of them by name finds it: `numa_maps` (one line per mapping) and
    // `smaps_rollup` (one record for the whole space). Both read the same
    // records `smaps` does, in the same order.
    node.add(
        "numa_maps",
        NodeKind::File,
        maybe_file(move || numa_maps_of(slot)),
    );
    node.add(
        "smaps_rollup",
        NodeKind::File,
        maybe_file(move || smaps_rollup_of(slot)),
    );
    // The mount table, in both spellings, under *every* process rather than
    // once at the top of `/proc`.
    //
    // Linux keeps these two per mount namespace, so their home is the process;
    // this kernel has one namespace and every process gets the same answer -
    // but the answer has to be at this name, because `/proc/self/mountinfo` is
    // the path a program actually opens. `/proc/mounts` is what a shell reads
    // by hand; the same two files under `/proc/self` are what `libmount`, and
    // therefore `mount`, `findmnt`, `lsblk` and `df`, read.
    node.add("mounts", NodeKind::File, file(mounts_text));
    node.add("mountinfo", NodeKind::File, file(mountinfo_text));
    #[cfg(target_os = "none")]
    node.add(
        "limits",
        NodeKind::File,
        maybe_file(move || limits_of_pid(leader)),
    );
    node.add(
        "comm",
        NodeKind::File,
        maybe_file(move || Some(alloc::format!("{}\n", task(slot)?.comm))),
    );
    node.add(
        "cmdline",
        NodeKind::File,
        maybe_file(move || cmdline_of(slot)),
    );
    // The environment beside the command line, because they are one block on
    // one stack and a program that reads the second usually reads the first.
    //
    // Its own permission word is `0400` rather than the read-only default
    // (measured on a current host: `/proc/<pid>/environ` answers `0400`, and
    // `/proc/<pid>/cmdline` beside it `0444`): an environment can carry
    // secrets, so Linux gives this node the owner's read and nothing else.
    node.add(
        "environ",
        NodeKind::File,
        env_file(move || environ_of(slot)),
    );
    node.add(
        "status",
        NodeKind::File,
        maybe_file(move || status_of(slot)),
    );
    // The process's control-group membership - the same per-task answer the
    // thread directory carries, because a group move lands on every thread
    // at once and both spellings report it (`/proc/<pid>/cgroup` is what
    // `systemd-cgls` and a container runtime read first).
    node.add(
        "cgroup",
        NodeKind::File,
        maybe_file(move || crate::vfs::cgroupfs::cgroup_line(slot)),
    );
    // `/proc/<pid>/oom_score_adj` - the OOM-killer's score adjustment, which
    // `udevd`'s workers push down on every boot so the device database is not
    // the first thing killed. Linux keeps a *pair* here: the value, and the
    // most protective value ever written, which nothing may go below without
    // `CAP_SYS_RESOURCE` - so a process cannot be talked back into the line by
    // something it runs later. Both are stored on the process and read back
    // from it; that is the whole of what this kernel does with them, since
    // there is no killer to score. A missing name is what made those workers
    // hear `EOPNOTSUPP`: their `fopen(..., "w")` is an `open(O_CREAT)`, and
    // procfs invents no file to answer it.
    #[cfg(target_os = "none")]
    node.add("oom_score_adj", NodeKind::File, oom_score_adj_ctl(leader));
    // `/proc/<pid>/timens_offsets` - the clocks this process's children will
    // read, and the one door a `unshare(CLONE_NEWTIME)` has for saying by how
    // much. Named the way a real kernel names it, because the tools that write
    // it (`nsenter`, a container runtime setting up a restored snapshot) look
    // for exactly this path.
    #[cfg(target_os = "none")]
    node.add("timens_offsets", NodeKind::File, timens_offsets_ctl(slot));
    // The three files a user namespace is set up through. Linux keeps them
    // beside one another and in this order, and the tools that write them
    // (`newuidmap`/`newgidmap`, and every container runtime that does the work
    // itself) look for exactly these paths.
    #[cfg(target_os = "none")]
    node.add("uid_map", NodeKind::File, uid_map_ctl(slot));
    #[cfg(target_os = "none")]
    node.add("gid_map", NodeKind::File, gid_map_ctl(slot));
    #[cfg(target_os = "none")]
    node.add("setgroups", NodeKind::File, setgroups_ctl(slot));
    // The image and the working directory, as links: `readlink` is the only
    // way to ask for them, and their targets are facts about a process, not
    // bytes a file holds.
    node.add(
        "exe",
        NodeKind::Symlink,
        link(move || {
            let exe = task(slot)?.exe;
            if exe.is_empty() { None } else { Some(exe) }
        }),
    );
    node.add(
        "cwd",
        NodeKind::Symlink,
        link(move || Some(task(slot)?.cwd)),
    );
    node.add("root", NodeKind::Symlink, link(|| Some(String::from("/"))));

    // `/proc/<pid>/task`: one directory per thread, the leader among them -
    // and the *group's* list, which is why a tid's own directory lists what
    // its leader's does rather than one entry.
    let tasks = ProcDir::new(Some(Arc::new(Threads { pid: leader })));
    node.add("task", NodeKind::Dir, tasks);

    // `/proc/<pid>/fd`: one link per open descriptor, whose target is the
    // path it was opened by.
    node.add("fd", NodeKind::Dir, descriptor_dir(slot));

    // `/proc/<pid>/ns`: one link per namespace the process is in, naming the
    // namespace the way `readlink` reports it and handing `setns` the object
    // when it is opened.
    //
    // `0511` rather than the tree's `0555`, which is Linux's own word for this
    // one directory (measured on a current host): the names may be *reached*
    // - `setns` opens one - but the directory does not list what is in it.
    let namespaces = ProcDir::own(Some(Arc::new(Namespaces { pid: slot })), 0o511, false);
    node.add("ns", NodeKind::Dir, namespaces);

    Some(node)
}

/// The threads of one process, as `/proc/<pid>/task` names them.
struct Threads {
    pid: usize,
}

impl Dynamic for Threads {
    fn names(&self) -> Vec<(String, NodeKind)> {
        thread_ids(self.pid)
            .into_iter()
            .map(|tid| (alloc::format!("{}", reader_number(tid)), NodeKind::Dir))
            .collect()
    }

    fn node(&self, name: &str) -> Option<Arc<dyn Inode>> {
        // A tid is namespaced exactly as a pid is, so the number is read in the
        // reader's namespace and the directory it names is the thread's.
        let tid = reader_task(name.parse::<usize>().ok()?)?;
        if !thread_ids(self.pid).contains(&tid) {
            return None;
        }
        Some(thread_dir(self.pid, tid))
    }
}

/// The open descriptors of one process, as `/proc/<pid>/fd` names them.
struct Descriptors {
    pid: usize,
}

/// `/proc/<pid>/fd`, built with the metadata Linux gives that directory rather
/// than the kind's default.
///
/// It is the one proc directory whose own mode and size are about its contents:
/// measured on a current host (`.workbuddy/probe_proclink_mode.c`), `lstat`
/// answers `S_IFDIR | 0500` - not the `0555` every other proc directory carries
/// - and a size that is the number of descriptors it lists, so opening another
/// descriptor moves it by one and closing one moves it back. A listing tool
/// that filters or colours by the mode reads that `0500`, and a `du`-shaped
/// reader of `st_size` reads the count.
fn descriptor_dir(pid: usize) -> Arc<ProcDir> {
    ProcDir::own(Some(Arc::new(Descriptors { pid })), 0o500, true)
}

impl Dynamic for Descriptors {
    fn names(&self) -> Vec<(String, NodeKind)> {
        fd_numbers(self.pid)
            .into_iter()
            .map(|name| (name, NodeKind::Symlink))
            .collect()
    }

    fn node(&self, name: &str) -> Option<Arc<dyn Inode>> {
        let fd = name.parse::<usize>().ok()?;
        if !crate::sched::descriptor_numbers(self.pid).contains(&fd) {
            return None;
        }
        // The target is looked up when `readlink` asks for it, not here. This
        // runs for every entry of a `/proc/<pid>/fd` listing - which a tree
        // search performs on its way past - and naming the descriptor is a
        // tree search itself. Resolving it eagerly is what made the two call
        // each other until the kernel stack ran out.
        let pid = self.pid;
        Some(fd_link(pid, fd, move || fd_target(pid, fd)))
    }
}

/// One process's namespace, for the two readers that need the object rather
/// than its text. Two bodies for the reason [`task`] has two: the namespace
/// slots live in the scheduler's task table, which only exists on a machine
/// with tasks.
#[cfg(target_os = "none")]
fn namespace_of(pid: usize, entry: crate::vfs::ns::Entry) -> Option<crate::vfs::ns::Namespace> {
    crate::sched::ns_of(pid, entry)
}

#[cfg(not(target_os = "none"))]
fn namespace_of(_pid: usize, _entry: crate::vfs::ns::Entry) -> Option<crate::vfs::ns::Namespace> {
    None
}

/// A link that names a *namespace*: every entry under `/proc/<pid>/ns`.
///
/// The same shape [`fd_link`] has and for the same reason - the walk must hand
/// the link back rather than splice it - with a namespace where the descriptor
/// was. `readlink` answers the fact (`mnt:[<id>]`, measured: that is what a real
/// kernel's `readlink` prints), and `as_namespace` answers the object `setns`
/// joins. Both are asked of the *live* process, so a link opened after its
/// process unshared names the namespace it is in now.
fn ns_link(pid: usize, entry: crate::vfs::ns::Entry) -> Arc<dyn Inode> {
    Arc::new(ProcLink {
        ino: next_ino(),
        target: Box::new(move || namespace_of(pid, entry).map(|ns| ns.link_text())),
        pid,
        fd: None,
        ns: Some((pid, entry)),
        registered: 0,
    })
}

/// The namespaces of one process, as `/proc/<pid>/ns` lists them.
///
/// The kinds this kernel has, and *only* those: a machine built without a
/// namespace has no entry for it either (a real kernel without `CONFIG_TIME_NS`
/// answers `ENOENT` for `/proc/self/ns/time`), so a reader sees the names this
/// machine can actually isolate and is told nothing it cannot check. That list
/// is now the whole of `namespaces(7)`'s eight - `cgroup` landed last, with
/// the tree it views (`src/vfs/cgroupfs.rs`) - and a name that is not one of
/// them is `ENOENT` from the directory, `setns` never seeing a descriptor for
/// a namespace that does not exist.
///
/// The list is of [`crate::vfs::ns::Entry`]s rather than kinds because two kinds
/// have two names each: `pid`/`pid_for_children` and `time`/`time_for_children`
/// are the same kind's two slots, which is what lets a reader see where its
/// *children* would be born as well as where it is.
struct Namespaces {
    pid: usize,
}

impl Dynamic for Namespaces {
    fn names(&self) -> Vec<(String, NodeKind)> {
        crate::vfs::ns::Entry::ALL
            .iter()
            .map(|entry| (String::from(entry.name()), NodeKind::Symlink))
            .collect()
    }

    fn node(&self, name: &str) -> Option<Arc<dyn Inode>> {
        let entry = crate::vfs::ns::Entry::of_name(name)?;
        Some(ns_link(self.pid, entry))
    }
}

/// The processes themselves, as `/proc` names them - a *listing* that holds
/// leaders alone beside a *lookup* that answers a tid too.
///
/// A name is looked up as a number and answered only when a task answers to
/// that number: `/proc` holds a great many names that are not pids
/// (`mounts`, `uptime`, `self`), and a file that claimed every number was a
/// process would turn `mounts` into a directory and break the first tool that
/// asked for it.
///
/// The two halves are deliberately different questions, and Linux's are too
/// (measured: `readdir /proc` on a current host, 367 entries with the live
/// thread's tid **absent** and its leader listed). A listing that carried every
/// thread would make `ps -e` count a process once per thread; a lookup that
/// refused a tid would make `/proc/<tid>/status` - the path `gdb -p <tid>`
/// reads - `ENOENT`, which is the gap this directory closes.
struct Processes;

impl Dynamic for Processes {
    fn names(&self) -> Vec<(String, NodeKind)> {
        process_ids()
            .into_iter()
            .map(|pid| (alloc::format!("{pid}"), NodeKind::Dir))
            .collect()
    }

    fn node(&self, name: &str) -> Option<Arc<dyn Inode>> {
        // The number in the *reader's* namespace, turned back into the task it
        // names there: `/proc/1` inside a namespace is that namespace's first
        // child, and the same path outside names the machine's init. A number
        // the reader's namespace does not contain is not a directory at all -
        // and a number that names a *thread* of a group it does contain is one,
        // which is the whole of what the tid spelling adds.
        let number = name.parse::<usize>().ok()?;
        process_dir(reader_task(number)?)
    }
}

/// `/proc/bus/pci`: the PCI bus, as the kernel found it.
///
/// This is pciutils' second access method - `linux-proc`, tried when the sysfs
/// tree is not there - and it is the one `setpci` reaches for after it too. A
/// kernel without it is a kernel whose `lspci` fails outright the moment `/sys`
/// is not mounted, so the tree is built from the same snapshot `/sys` is:
/// [`crate::pci::devices`] answers both, and the two cannot disagree about
/// which devices exist.
///
/// Two halves, which are the two things the reader needs. `devices` is one
/// line per function - its identity, its resources and the driver bound to it
/// - and `<bus>/<dev>.<func>` is the function's own configuration space.
fn pci_dir() -> Arc<ProcDir> {
    let pci = ProcDir::new(Some(Arc::new(PciBuses)));
    pci.add("devices", NodeKind::File, file(pci_devices_text));
    pci
}

/// `/proc/bus/pci/devices`, the way Linux's `show_device` prints it.
fn pci_devices_text() -> String {
    let mut out = String::new();
    for device in crate::pci::devices() {
        out.push_str(&device.devices_line());
    }
    out
}

/// The bus directories under `/proc/bus/pci`, one per bus that has a device.
///
/// Dynamic for the same reason `/proc/<pid>` is: which buses exist is the
/// enumeration's answer, not a shape decided when the filesystem was mounted.
/// Linux creates them as it scans the bus, at boot; here they are made when
/// they are asked for, out of the same snapshot.
struct PciBuses;

impl Dynamic for PciBuses {
    fn names(&self) -> Vec<(String, NodeKind)> {
        let mut seen: Vec<String> = Vec::new();
        for device in crate::pci::devices() {
            let name = device.proc_dir();
            if !seen.contains(&name) {
                seen.push(name);
            }
        }
        seen.into_iter().map(|name| (name, NodeKind::Dir)).collect()
    }

    fn node(&self, name: &str) -> Option<Arc<dyn Inode>> {
        let bus = dir();
        let mut any = false;
        for device in crate::pci::devices() {
            if device.proc_dir() != name {
                continue;
            }
            any = true;
            bus.add(
                &device.proc_name(),
                NodeKind::File,
                Arc::new(ProcConfig {
                    ino: next_ino(),
                    bus: device.bus,
                    dev: device.dev,
                    func: device.func,
                    size: device.cfg_size,
                }),
            );
        }
        any.then_some(bus)
    }
}

/// One function's configuration space, as a file.
///
/// `pread` and `pwrite` reach the device itself rather than a snapshot, so
/// what a reader sees is the live registers and what `setpci` writes really
/// does change the device - which is the whole point of the file existing.
///
/// As long as the function's own `cfg_size`, the same as the `config`
/// attribute in `/sys`: 256 bytes for a conventional function, 4096 when the
/// device answered for its extended space.
struct ProcConfig {
    ino: Ino,
    bus: u8,
    dev: u8,
    func: u8,
    size: u64,
}

impl Inode for ProcConfig {
    fn metadata(&self) -> Metadata {
        Metadata {
            ino: self.ino,
            kind: NodeKind::File,
            size: self.size,
        }
    }

    fn mode(&self) -> u32 {
        // Measured on a current host: `/proc/bus/pci/00/00.0` answers `0644` -
        // the configuration space is read by anyone and written by whoever may
        // touch the device, which is the read-write file's own word.
        PROC_RW_FILE_MODE
    }

    #[cfg(all(target_os = "none", mpos_pci_config))]
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> VfsResult<usize> {
        Ok(crate::drivers::pci::config_read(
            self.bus, self.dev, self.func, self.size, offset, buf,
        ))
    }

    #[cfg(all(target_os = "none", mpos_pci_config))]
    fn write_at(&self, offset: u64, buf: &[u8]) -> VfsResult<usize> {
        Ok(crate::drivers::pci::config_write(
            self.bus, self.dev, self.func, self.size, offset, buf,
        ))
    }
}

/// Seconds since boot, split the way `/proc/uptime` spells them: whole seconds
/// and centiseconds. The kernel answers from its tick counter; the host (where
/// these generators are unit-tested) has no ticks, and reports zero.
#[cfg(target_os = "none")]
fn uptime() -> (u64, u64) {
    let ticks = crate::hal::ticks();
    let hz = crate::hal::TICK_HZ as u64;
    (ticks / hz, (ticks % hz) * 100 / hz)
}

#[cfg(not(target_os = "none"))]
fn uptime() -> (u64, u64) {
    (0, 0)
}

/// Free and total page frames. Host builds have no allocator; their
/// /proc/meminfo reads as zeros, which is what a machine with no memory
/// management would say.
#[cfg(target_os = "none")]
fn meminfo_kb() -> (u64, u64) {
    let (total, free) = critical_section::with(|cs| {
        let pmm = crate::mm::PMM.borrow(cs).borrow();
        (pmm.total_frames(), pmm.free_frames())
    });
    let kb = |frames: usize| (frames as u64) * 4096 / 1024;
    (kb(total), kb(free))
}

#[cfg(not(target_os = "none"))]
fn meminfo_kb() -> (u64, u64) {
    (0, 0)
}

/// One `/proc/partitions` row, in Linux's column widths.
///
/// `#blocks` counts 1024-byte blocks - half the 512-byte sectors the block
/// layer works in - and the columns are fixed-width because the programs that
/// read this file parse by column.
fn partition_row(name: &str, major: u32, minor: u32, sectors: u64) -> String {
    alloc::format!("{major:4} {minor:4} {:10} {name}\n", sectors / 2)
}

/// `/proc/partitions`: every disk this machine has and its partitions, one per
/// line.
///
/// The names, the numbers *and* the sizes come from
/// [`crate::uevent::Numbering`] - the same table `/dev` nodes are minted from
/// and `/sys/class/block/*/dev` prints - so the three cannot name the same
/// partition differently, and this file does not have to go back to the storage
/// to find out how big anything is.
///
/// Its existence is load-bearing in a way a missing file usually is not.
/// `fdisk -l` opens it before it does anything else, and an `ENOENT` there
/// leaves it holding a null `FILE *` that it passes straight to stdio: the
/// symptom is a page fault inside the libc's stream lock, on an address a few
/// hundred bytes into the program's own read-only image, which says nothing at
/// all about the missing file.
fn partitions() -> String {
    partitions_text(&crate::uevent::Numbering::of_this_machine())
}

/// [`partitions`] over a numbering, so the rendering can be checked without a
/// machine that has disks: the kernel hands it the table it built from the
/// disks it really brought up, and a host test hands it one it wrote down.
fn partitions_text(numbering: &crate::uevent::Numbering) -> String {
    let mut out = String::from("major minor  #blocks  name\n\n");
    for entry in numbering.devices.iter() {
        out.push_str(&partition_row(
            &entry.name,
            entry.major,
            entry.minor,
            entry.sectors,
        ));
    }
    out
}

// ---------------------------------------------------------------------------
// What is mounted where, in the shape each reader of it expects.
// ---------------------------------------------------------------------------

/// `/proc/mounts`: the mount table in `fstab`'s six columns - source, mount
/// point, type, options, dump, pass.
///
/// The fourth column is the one that matters: `ro` says a write here will be
/// refused, and userland reads it to decide whether to remount. The last two
/// are `dump`'s and `fsck`'s and are zero for everything here, as they are on
/// Linux for every filesystem no dump or fsck would ever be pointed at.
///
/// The table is **the caller's**, through `mount::mount_records` - the free
/// function that routes by `process_fs_context` - and not the machine's. This
/// is what makes a mount namespace observable at all: the file *is* the way
/// userland compares two namespaces (`findmnt`, OpenRC's scan), so a renderer
/// reading the machine's table would answer every task with the same list and
/// `unshare(CLONE_NEWNS)` would be invisible where it matters. Measured by
/// `t-ns-mnt`, whose child unshared, unmounted inside its copy, and read this
/// file back to see whether the peel had landed.
fn mounts_text() -> String {
    let mut out = String::new();
    for record in super::mount::mount_records() {
        out.push_str(&alloc::format!(
            "{} {} {} {} 0 0\n",
            if record.source.is_empty() {
                "none"
            } else {
                &record.source
            },
            record.path,
            record.fstype,
            record.options
        ));
    }
    out
}

/// `/proc/self/mountinfo`: the mount table with its identities attached.
///
/// A *different file* from `/proc/mounts`, not a longer spelling of it, and the
/// difference is what it is for. `/proc/mounts` is a list of what is mounted
/// where; mountinfo is that plus the three numbers a caller needs to *correlate*
/// the list with the rest of the machine - a unique id per mount, the id of the
/// mount it hangs under, and the `maj:min` of the device it lives on. The last
/// is the one that gets used: it is how a tool that has a block device in its
/// hand (`/sys/class/block/vda2`, say) finds out where that device is mounted,
/// and `lsblk`'s MOUNTPOINTS column, `findmnt`, `df` and systemd's mount units
/// are all that correlation.
///
/// The format is `proc(5)`'s, ten fields and a separator:
///
/// ```text
/// id parent maj:min root mountpoint options [optional...] - fstype source super-options
/// ```
///
/// `root` is always `/` here: this kernel mounts whole filesystems and has no
/// subtree mounts, so a mount's root *is* the filesystem's root. The optional
/// fields between the options and the `-` carry propagation and idmapping
/// information (`shared:`, `master:`, `uid_map`), none of which this kernel
/// has: a mount belongs to one namespace and shares with nothing, and an empty
/// list is what Linux prints for a mount that shares with nothing either.
fn mountinfo_text() -> String {
    // The caller's table, for the reason [`mounts_text`] gives: the two files
    // are two renderings of one fact, and a reader that found them disagreeing
    // about which mounts exist would have no way to tell which was right.
    mountinfo_lines(&super::mount::mount_records())
}

/// [`mountinfo_text`] over a mount table the caller supplies, so the rendering
/// can be checked without a machine to have mounted anything: the kernel hands
/// it the table it built from the mounts it really made, and a host test hands
/// it one it wrote down.
fn mountinfo_lines(records: &[crate::vfs::mount::MountRecord]) -> String {
    let mut out = String::new();
    for record in records {
        // The device column, in the `maj:min` spelling every reader decodes -
        // the same two numbers `/sys/class/block/<name>/dev` publishes and
        // `stat`'s `st_dev` packs, which is the point: a reader that has one of
        // the three can compare it against this column directly.
        let dev = alloc::format!(
            "{}:{}",
            crate::uevent::major_of(record.dev),
            crate::uevent::minor_of(record.dev)
        );
        // The superblock options are the write promise, and they say the same
        // thing the per-mount column does only because a mount here has no
        // per-mount flag the filesystem does not also have.
        let super_options = if record.options.starts_with("ro") {
            "ro"
        } else {
            "rw"
        };
        out.push_str(&alloc::format!(
            "{} {} {} / {} {} - {} {} {}\n",
            record.mount_id,
            record.parent_id,
            dev,
            record.path,
            record.options,
            record.fstype,
            if record.source.is_empty() {
                "none"
            } else {
                &record.source
            },
            super_options
        ));
    }
    out
}

// ---------------------------------------------------------------------------
// The name of this machine, as the caller's namespace spells it.
// ---------------------------------------------------------------------------
/// The name the *caller's* namespace calls itself - `uname(2)`'s `nodename`,
/// `hostname(1)`'s answer, and the contents of `/proc/sys/kernel/hostname`.
///
/// The pair of names used to be kept here as one process-wide value, which was
/// "one fact, one home" only for as long as there was one UTS namespace - and
/// there is more than one the moment a program calls
/// `unshare(CLONE_NEWUTS)`. They live in `crate::uts` now, one object per
/// namespace, and these four functions ask the *running task* which one it is
/// in. That is what makes the door that writes the file and the door that reads
/// it talk about the same namespace, which is the whole of what
/// `/proc/sys/kernel/hostname` means.
pub fn hostname() -> String {
    crate::sched::current_uts().nodename()
}

/// Give the caller's namespace a name. `sethostname(2)` spells it through here,
/// and so does a write to `/proc/sys/kernel/hostname` - one fact, two doors, one
/// namespace behind both.
pub fn set_hostname(name: &str) {
    crate::sched::current_uts().set_nodename(name);
}

/// The domain the caller's namespace belongs to: `(none)` until somebody names
/// it, which is what Linux reports for a machine nobody named. Read by
/// `uname(2)` and by `/proc/sys/kernel/domainname`.
pub fn domainname() -> String {
    crate::sched::current_uts().domainname()
}

/// The header `/proc/sysvipc/shm` starts with, measured character for
/// character on a current host: the column widths are the kernel's, and a reader
/// that splits a line counts them.
const SYSVIPC_SHM_HEADER: &str = "       key      shmid perms                  size  cpid  lpid nattch   uid   gid  cuid  cgid      atime      dtime      ctime                   rss                  swap\n";

/// `/proc/sysvipc/shm`: one line per segment, in the kernel's `ipcs` layout.
///
/// `perms` is the mode word in octal (so a locked segment reads `2600`), `size`
/// is the size asked for rather than the page-rounded mapping, `nattch` is the
/// attachment count folded out of every address space, and `rss` is the pages
/// that have a frame. The last two columns - the rss and the swap count - are 22
/// wide because they are counters, not sizes.
pub fn sysvipc_shm_text() -> String {
    let mut out = String::from(SYSVIPC_SHM_HEADER);
    for segment in crate::mm::sysv_shm::all(crate::sched::current_ipc_ns()) {
        let perm = segment.perm();
        let times = segment.times();
        let attachments = crate::sched::shm_attachment_count(segment.id());
        out.push_str(&alloc::format!(
            "{:10}{:11}{:6o}{:22}{:6}{:6}{:7}{:6}{:6}{:6}{:6}{:11}{:11}{:11}{:22}{:22}\n",
            perm.key,
            perm.id,
            perm.mode,
            perm.size,
            perm.cpid,
            times.lpid,
            attachments,
            perm.uid,
            perm.gid,
            perm.cuid,
            perm.cgid,
            times.atime,
            times.dtime,
            times.ctime,
            segment.resident_pages(),
            0,
        ));
    }
    out
}

/// The header `/proc/sysvipc/sem` starts with, measured off a current host in
/// the same way the segment table's was - the widths are the kernel's, and the
/// `perms` column carries its own leading space (six wide, not five plus one).
const SYSVIPC_SEM_HEADER: &str =
    "       key      semid perms      nsems   uid   gid  cuid  cgid      otime      ctime\n";

/// `/proc/sysvipc/sem`: one line per set, in the kernel's `ipcs` layout.
///
/// The key is decimal and 10 wide, `perms` is the mode word in octal, `nsems` is
/// 11 wide, the four ids are 6 each, and the two clocks are 11 each -
/// `sem_otime` then `sem_ctime`, zero until something has operated on the set.
pub fn sysvipc_sem_text() -> String {
    let mut out = String::from(SYSVIPC_SEM_HEADER);
    for set in crate::sched::sysv_sem::all(crate::sched::current_ipc_ns()) {
        let perm = set.perm();
        let (otime, ctime) = set.times();
        out.push_str(&alloc::format!(
            "{:10}{:11}{:6o}{:11}{:6}{:6}{:6}{:6}{:11}{:11}\n",
            perm.key,
            perm.id,
            perm.mode,
            perm.nsems,
            perm.uid,
            perm.gid,
            perm.cuid,
            perm.cgid,
            otime,
            ctime,
        ));
    }
    out
}

/// The header `/proc/sysvipc/msg` starts with, measured character for
/// character on a current host - the widths are the kernel's, and `ipcs`
/// splits a line on them.
const SYSVIPC_MSG_HEADER: &str = "       key      msqid perms      cbytes       qnum lspid lrpid   uid   gid  cuid  cgid      stime      rtime      ctime\n";

/// `/proc/sysvipc/msg`: one line per queue, in the kernel's `ipcs` layout.
///
/// The columns are the `msqid64_ds` counters - bytes queued, messages queued -
/// the two pids, and the three clocks, `stime` (last send) through `ctime`.
pub fn sysvipc_msg_text() -> String {
    let mut out = String::from(SYSVIPC_MSG_HEADER);
    for queue in crate::sched::sysv_msg::all(crate::sched::current_ipc_ns()) {
        let perm = queue.perm();
        let (cbytes, qnum, _qbytes, lspid, lrpid, stime, rtime, ctime) = queue.stats();
        out.push_str(&alloc::format!(
            "{:10}{:11}{:6o}{:12}{:11}{:6}{:6}{:6}{:6}{:6}{:6}{:11}{:11}{:11}\n",
            perm.key,
            perm.id,
            perm.mode,
            cbytes,
            qnum,
            lspid,
            lrpid,
            perm.uid,
            perm.gid,
            perm.cuid,
            perm.cgid,
            stime,
            rtime,
            ctime,
        ));
    }
    out
}

/// Name the caller's namespace's domain. `setdomainname(2)` lands here, and so
/// does a write to `/proc/sys/kernel/domainname`.
pub fn set_domainname(name: &str) {
    crate::sched::current_uts().set_domainname(name);
}

/// The filesystem itself.
pub struct ProcFs {
    root: Arc<ProcDir>,
}

impl ProcFs {
    pub fn new() -> Arc<ProcFs> {
        let fs = Arc::new(ProcFs {
            root: ProcDir::new(Some(Arc::new(Processes))),
        });
        fs.build();
        fs
    }

    /// The tree. Each file's generator is written where its content is
    /// decided, so the file and the fact it reports stay next to each other.
    fn build(&self) {
        // What is mounted where: the only record of it that userland can
        // read, and the thing `mountinfo` and `mount -a` stand on. Linux makes
        // this name a **link** to the per-process file (`self/mounts`) and this
        // is a link to the same place, so a reader arrives at one file by either
        // spelling - and the file it arrives at is the one `libmount` opens.
        self.root
            .add("mounts", NodeKind::Symlink, fixed_link("self/mounts"));
        // Which filesystem types a mount(2) may name. The `nodev` mark is
        // not decoration: a caller mounting a *device* - busybox's mount,
        // probing for the type it was not told - tries the unmarked entries
        // and only those, and a pseudo-filesystem without the mark would be
        // tried first and mounted over the device's data.
        self.root.add(
            "filesystems",
            NodeKind::File,
            file(|| {
                let mut out = String::new();
                for (name, nodev) in [
                    ("sysfs", true),
                    ("rootfs", true),
                    ("proc", true),
                    ("devtmpfs", true),
                    ("devpts", true),
                    ("tmpfs", true),
                    ("ramfs", true),
                    ("ext2", false),
                    ("ext3", false),
                    ("ext4", false),
                ] {
                    if nodev {
                        out.push_str("nodev\t");
                    }
                    out.push_str(name);
                    out.push('\n');
                }
                out
            }),
        );
        self.root.add(
            "uptime",
            NodeKind::File,
            file(|| {
                let (secs, centis) = uptime();
                alloc::format!("{}.{} 0.0\n", secs, centis)
            }),
        );
        self.root.add(
            "meminfo",
            NodeKind::File,
            file(|| {
                let (total, free) = meminfo_kb();
                alloc::format!(
                    "MemTotal:       {} kB\nMemFree:        {} kB\nMemAvailable:   {} kB\n",
                    total,
                    free,
                    free
                )
            }),
        );
        self.root.add("cpuinfo", NodeKind::File, file(cpuinfo_text));
        self.root.add(
            "version",
            NodeKind::File,
            file(|| {
                alloc::format!(
                    "{} version {} \n",
                    env!("CARGO_PKG_NAME"),
                    env!("CARGO_PKG_VERSION")
                )
            }),
        );
        // The command line the machine was booted with. An initramfs's init
        // reads it to learn `root=`, which is how it knows what to mount.
        //
        // Its own `st_size` is that command line's length, which is Linux's
        // answer for this one file (`/proc/<pid>/cmdline` beside it reports
        // zero): the text cannot change after the boot, so the length is known
        // when the tree is built rather than when the file is read.
        let cmdline = alloc::format!("{}\n", crate::boot::cmdline());
        let cmdline_len = cmdline.len() as u64;
        self.root.add(
            "cmdline",
            NodeKind::File,
            file_of_size(cmdline_len, move || cmdline.clone()),
        );
        self.root.add(
            "loadavg",
            NodeKind::File,
            file(|| {
                // The last field is the most recent pid, which a shell uses
                // to pick a number for a job. Ours comes from the same place
                // a pid comes from, so it agrees with what `ps` lists.
                let newest = process_ids().into_iter().max().unwrap_or(1);
                alloc::format!("0.00 0.00 0.00 1/{} {}\n", process_count(), newest)
            }),
        );
        self.root
            .add("partitions", NodeKind::File, file(partitions));
        // `/proc/modules`: what is loaded here, which is nothing.
        //
        // Not a stub. This kernel has no loadable modules - every driver is
        // compiled into the image - and an empty file is how that fact is said
        // in the shape the reader expects. `lsmod` prints its header and no rows,
        // exactly as it does on a Linux built with no modules; an *absent* file
        // is an error the caller has to special-case instead, which is what it
        // was. Nor is a file that says "cannot tell you" better: the difference
        // between "none" and "unknown" is the whole value of the file.
        self.root.add("modules", NodeKind::File, file(String::new));
        self.root.add(
            "stat",
            NodeKind::File,
            file(|| {
                let (secs, _) = uptime();
                alloc::format!("cpu  {} 0 0 0 0 0 0 0 0 0\nintr 0\nctxt 0\n", secs)
            }),
        );
        // Every device the kernel has registered, split the way a reader of
        // `/proc/devices` expects: character devices first, then block. It is
        // what a `ls -l /dev` cross-references a device number against, and
        // what tells a tool whether a major number is a char device or a disk.
        self.root.add(
            "devices",
            NodeKind::File,
            file(|| {
                let mut out = String::from("Character devices:\n");
                for (name, major) in [("mem", 1u32), ("tty", 4), ("ttyS", 4), ("misc", 10)] {
                    out.push_str(&alloc::format!("{major} {name}\n"));
                }
                out.push_str("\nBlock devices:\n");
                out.push_str("254 virtblk\n");
                out
            }),
        );

        // `/proc/sys`: the knobs. `sysctl -a` walks this tree, and OpenRC's
        // `sysctl` service writes into it at boot - a tree that is absent is
        // a service that fails on every entry in /etc/sysctl.conf, not one
        // that quietly does nothing.
        let sys = dir();
        let kernel = dir();
        // The three `uname` answers that live here rather than in /proc: a
        // program asking what it is running on reads these, and they have to
        // agree with what `uname(2)` said or the two disagree about the
        // machine.
        kernel.add("ostype", NodeKind::File, file(|| String::from("Linux\n")));
        kernel.add(
            "osrelease",
            NodeKind::File,
            file(|| alloc::format!("{}\n", env!("CARGO_PKG_VERSION"))),
        );
        kernel.add(
            "version",
            NodeKind::File,
            file(|| String::from("#1 SMP PREEMPT mpos\n")),
        );
        kernel.add(
            "hostname",
            NodeKind::File,
            // Read back with the newline every sibling sysctl ends in, and
            // written under the same 64-byte cap `sethostname(2)` enforces:
            // one fact, two doors, one rule - a name the file accepted and
            // `uname` then truncated would be two facts wearing one label.
            ctl(
                || alloc::format!("{}\n", hostname()),
                |name| {
                    // On Linux this write is gated by the file's own mode
                    // (`/proc/sys/kernel/*` is 0644), and procfs here keeps no
                    // modes to *enforce* - it reports each node's own word
                    // (this file's is 0644, matching Linux's) but polices
                    // nothing, so the capability the adjacent syscall checks is
                    // checked here too, and a non-root writer is refused by the
                    // door as well as by `sethostname(2)`.
                    // `EACCES` is Linux's answer for this door, and it is what
                    // `VfsError::Access` carries. `current_owner` is the
                    // effective id, which is the one a capability is about.
                    if crate::sched::current_owner().0 != 0 {
                        return Err(VfsError::Access);
                    }
                    if name.len() > 64 {
                        return Err(VfsError::Invalid);
                    }
                    set_hostname(name);
                    Ok(())
                },
            ),
        );
        kernel.add(
            "domainname",
            NodeKind::File,
            // The hostname's twin, and under the same rule: the file reports
            // what `uname` reports, and a write is the same 64-byte-capped set
            // `setdomainname(2)` performs. It used to hardcode `(none)` on the
            // read *and* drop the write, which left `setdomainname(2)` with no
            // observable effect through any door at all - the call stored a name
            // nothing could see.
            ctl(
                || alloc::format!("{}\n", domainname()),
                |name| {
                    // The hostname's rule, for the hostname's reason.
                    if crate::sched::current_owner().0 != 0 {
                        return Err(VfsError::Access);
                    }
                    if name.len() > 64 {
                        return Err(VfsError::Invalid);
                    }
                    set_domainname(name);
                    Ok(())
                },
            ),
        );
        kernel.add(
            "pid_max",
            NodeKind::File,
            file(|| alloc::format!("{}\n", max_pids())),
        );
        kernel.add(
            "threads-max",
            NodeKind::File,
            file(|| alloc::format!("{}\n", max_pids())),
        );
        kernel.add(
            "ngroups_max",
            NodeKind::File,
            file(|| String::from("65536\n")),
        );
        // The knobs OpenRC's sysctl service writes on every boot. Neither
        // policy has machinery behind it here - no panic-timeout and no bpf
        // syscall at all - so each answers its Linux default and accepts the
        // write; see `stub`.
        kernel.add("panic", NodeKind::File, stub("0"));
        kernel.add("unprivileged_bpf_disabled", NodeKind::File, stub("2"));
        kernel.add("random", NodeKind::Dir, random_dir());
        kernel.add("pty", NodeKind::Dir, pty_dir());
        sys.add("kernel", NodeKind::Dir, kernel);
        // `/proc/sys/fs`: the two hardening link guards. mpos's path
        // resolution does not follow symlinks in the sticky-world-writable
        // sense these knobs toggle, so the defaults are reported and writes
        // accepted, but nothing re-checks on the kernel's side.
        let fs_knobs = dir();
        fs_knobs.add("protected_hardlinks", NodeKind::File, stub("1"));
        fs_knobs.add("protected_symlinks", NodeKind::File, stub("1"));
        sys.add("fs", NodeKind::Dir, fs_knobs);
        // `/proc/sys/net`: the per-protocol and per-interface knobs. The
        // network stack here is smoltcp with no sysctl surface, so every one
        // of these is a `stub` - the Linux default as the answer, the write
        // accepted, and no packet ever consults it.
        sys.add("net", NodeKind::Dir, sysnet_dir());
        self.root.add("sys", NodeKind::Dir, sys);

        // `/proc/net`: the socket table `ss`/`netstat` read. `tcp` lists the
        // TCP endpoints (with their state byte), `udp` the datagram ones; both
        // resolve through the registry into the `NetRow` shape the formatter
        // turns into the kernel's `local_address:port rem_address:port` text.
        let net = dir();
        net.add("tcp", NodeKind::File, file(|| format_net(&net_rows(true))));
        net.add("udp", NodeKind::File, file(|| format_net(&net_rows(false))));
        net.add("route", NodeKind::File, file(route_table_text));
        // The per-interface counters. §10.4's instrument reads through this
        // door - Linux's own one, so no private protocol was needed for it.
        net.add("dev", NodeKind::File, file(net_dev_text));
        self.root.add("net", NodeKind::Dir, net);

        // `/proc/sysvipc/shm`: the segment table `ipcs` renders, in the kernel's
        // own column layout. Measured off a current host - the header line and
        // every column's width, including the two 22-wide right-hand counters -
        // because a reader splits the line rather than parsing names
        // (`.workbuddy/probe_shm_perm.c`; `docs/notes/` §6.13).
        let sysvipc = dir();
        sysvipc.add("shm", NodeKind::File, file(sysvipc_shm_text));
        sysvipc.add("sem", NodeKind::File, file(sysvipc_sem_text));
        sysvipc.add("msg", NodeKind::File, file(sysvipc_msg_text));
        self.root.add("sysvipc", NodeKind::Dir, sysvipc);

        // `/proc/bus/pci`: pciutils' fallback access method, and the one
        // `setpci` writes through. It reads the same device snapshot the
        // sysfs tree is built from, so the two describe one machine.
        self.root.add("bus", NodeKind::Dir, {
            let bus = dir();
            bus.add("pci", NodeKind::Dir, pci_dir());
            bus
        });

        // `/proc/self`: whichever process is reading, which is not knowable
        // when the tree is built - and the shape Linux answers it with is a
        // **link** to that process's number, not a directory that mirrors it.
        // The number is the one this tree *names* the reader by
        // ([`reader_number`], the rule the pid namespace made "the reader's"),
        // so the link and the directory it leads to cannot disagree; a process
        // that has gone answers `NoEntry` from the follow, which is the same
        // thing `ls -l /proc/self` says on Linux for a pid that no longer
        // exists.
        self.root.add(
            "self",
            NodeKind::Symlink,
            link(|| Some(alloc::format!("{}", reader_number(current_pid())))),
        );
        // `/proc/thread-self` is the same idea for the calling *thread*, and
        // exists because a threaded program asking about itself gets a tid,
        // not a pid - `pthread_self` and `gettid` agree with each other and
        // with this, and not with `getpid`.
        self.root.add(
            "thread-self",
            NodeKind::Symlink,
            link(|| {
                Some(alloc::format!(
                    "{}/task/{}",
                    reader_number(current_pid()),
                    reader_number(current_tid())
                ))
            }),
        );
    }
}

/// `/proc/sys/kernel/pty`: how many pseudo-terminals there can be and how
/// many are open.
///
/// `max` is what a library reads before deciding whether it can open another
/// pty; `nr` is how many it thinks are in use. This kernel's devpts has no
/// limit of its own, so `max` reports the number of task slots - the real
/// ceiling, since a pty is driven by whoever opened it - and `nr` counts the
/// descriptors devpts has handed out.
fn pty_dir() -> Arc<ProcDir> {
    let pty = dir();
    pty.add(
        "max",
        NodeKind::File,
        file(|| alloc::format!("{}\n", max_pids())),
    );
    pty.add(
        "nr",
        NodeKind::File,
        file(|| {
            let open = crate::vfs::devpts::open_count();
            alloc::format!("{open}\n")
        }),
    );
    pty
}

/// `/proc/sys/kernel/random`: what entropy tools ask the pool about.
///
/// This kernel has no pool - one ChaCha20 stream, always ready - so these
/// answer in the units the readers expect: `poolsize` in bits (the file
/// Linux 5.18 removed, kept because the seedrng in this image reads it if
/// present and nothing if not), `urandom_min_res_cov` as the creditable
/// fraction, and `boot_id` as one UUID per boot.
fn random_dir() -> Arc<ProcDir> {
    let random = dir();
    random.add("poolsize", NodeKind::File, file(|| String::from("4096\n")));
    random.add(
        "urandom_min_res_cov",
        NodeKind::File,
        file(|| String::from("64\n")),
    );
    random.add("boot_id", NodeKind::File, file(boot_id));
    random
}

/// The one UUID this boot gets, made on first read and never re-rolled:
/// tools correlate events across a boot by it, so two reads of one boot that
/// disagree would be worse than a fixed string. Version-4 layout, random
/// body from the kernel's own stream.
fn boot_id() -> String {
    critical_section::with(|cs| {
        let mut slot = BOOT_ID.borrow(cs).borrow_mut();
        if slot.is_none() {
            let mut b = [0u8; 16];
            crate::rand::fill(&mut b);
            b[6] = (b[6] & 0x0f) | 0x40;
            b[8] = (b[8] & 0x3f) | 0x80;
            let hex = |bytes: &[u8]| -> String {
                let mut s = String::new();
                for byte in bytes {
                    s.push_str(&alloc::format!("{:02x}", byte));
                }
                s
            };
            *slot = Some(alloc::format!(
                "{}-{}-{}-{}-{}\n",
                hex(&b[0..4]),
                hex(&b[4..6]),
                hex(&b[6..8]),
                hex(&b[8..10]),
                hex(&b[10..16])
            ));
        }
        slot.clone().unwrap_or_default()
    })
}

static BOOT_ID: Mutex<RefCell<Option<String>>> = Mutex::new(RefCell::new(None));

/// `/proc/sys/net`: the protocol knobs OpenRC's `/etc/sysctl.conf` writes.
///
/// Two trees per family (`ipv4`, `ipv6`), each with its per-interface `conf`
/// directories - `all` and `default` are the two every distro's sysctl.conf
/// addresses. The values are Linux's shipped defaults and the writes are
/// accepted and ignored (`stub`); smoltcp consults none of them, and neither
/// does anything else here.
fn sysnet_dir() -> Arc<ProcDir> {
    fn conf_dir(keys: &[(&str, &'static str)]) -> Arc<ProcDir> {
        let conf = dir();
        for iface in ["all", "default"] {
            let entry = dir();
            for (name, value) in keys {
                entry.add(name, NodeKind::File, stub(value));
            }
            conf.add(iface, NodeKind::Dir, entry);
        }
        conf
    }

    let net = dir();
    let ipv4 = dir();
    ipv4.add("tcp_syncookies", NodeKind::File, stub("1"));
    ipv4.add("tcp_rfc1337", NodeKind::File, stub("0"));
    // Linux's default is the empty range `1 0` - no group may ping.
    ipv4.add("ping_group_range", NodeKind::File, stub("1 0"));
    ipv4.add(
        "conf",
        NodeKind::Dir,
        conf_dir(&[
            ("rp_filter", "0"),
            ("accept_redirects", "1"),
            ("secure_redirects", "1"),
            ("accept_source_route", "0"),
        ]),
    );
    let ipv6 = dir();
    ipv6.add(
        "conf",
        NodeKind::Dir,
        conf_dir(&[
            ("accept_redirects", "1"),
            ("accept_source_route", "0"),
            ("use_tempaddr", "0"),
        ]),
    );
    net.add("ipv4", NodeKind::Dir, ipv4);
    net.add("ipv6", NodeKind::Dir, ipv6);
    net
}

/// The largest pid there can be: the task table's size, which is where pids
/// come from and therefore the number a tool checking "can there be another
/// one" is actually asking about.
#[cfg(target_os = "none")]
fn max_pids() -> usize {
    crate::sched::MAX_TASKS
}

#[cfg(not(target_os = "none"))]
fn max_pids() -> usize {
    64
}

/// How many processes are running, for `/proc/loadavg`'s "1/n" field.
#[cfg(target_os = "none")]
fn process_count() -> usize {
    process_ids().len()
}

#[cfg(not(target_os = "none"))]
fn process_count() -> usize {
    1
}

/// The calling thread's id, for `/proc/thread-self`.
#[cfg(target_os = "none")]
fn current_tid() -> usize {
    crate::sched::current().0
}

#[cfg(not(target_os = "none"))]
fn current_tid() -> usize {
    1
}

impl FileSystem for ProcFs {
    fn root(&self) -> Arc<dyn Inode> {
        self.root.clone()
    }

    fn name(&self) -> &'static str {
        "proc"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `/proc/<pid>/fd/<N>`'s own permission bits come from the direction its
    /// descriptor was opened in, and the three directions are the three words
    /// measured on a current host (`.workbuddy/probe_proclink_mode.c`).
    #[test]
    fn an_fd_links_own_mode_is_the_direction_it_was_opened_in() {
        use crate::vfs::file::{ACC_RDONLY, ACC_RDWR, ACC_WRONLY};
        assert_eq!(fd_link_mode(ACC_RDONLY), 0o500);
        assert_eq!(fd_link_mode(ACC_WRONLY), 0o300);
        assert_eq!(fd_link_mode(ACC_RDWR), 0o700);
        // The two halves are bits, so the both-ways word is their union rather
        // than a third value - the shape a socketpair's end reports.
        assert_eq!(fd_link_mode(ACC_RDONLY) | fd_link_mode(ACC_WRONLY), 0o700);
    }

    /// `/proc/<pid>/stat`'s field 38 is the signal this task's death reports,
    /// and a **thread**'s is `-1` - a number no signal byte can carry, and the
    /// reason the field is signed. Measured whole on a current host
    /// (`.workbuddy/probe_exit_signal_field.c`, Linux 7.2.11): `17` for a
    /// `fork`'s child, `0` for a raw `clone(0)` child, `15` for a
    /// `clone(SIGTERM)` child, `-1` for a thread, and the leader of a threaded
    /// process keeps its own byte.
    ///
    /// The `-1` is the *rendering*'s and not the storage's here, because this
    /// kernel's threads store `SIGCHLD` on purpose (its group's `Retired` must
    /// not be readable as a "clone" child's) - which is what makes the rule
    /// worth a test that needs no machine under it.
    #[test]
    fn a_tasks_exit_signal_field_is_its_byte_or_minus_one_for_a_thread() {
        assert_eq!(exit_signal_field(17, false), 17, "a fork's child");
        assert_eq!(exit_signal_field(0, false), 0, "a clone(0) child");
        assert_eq!(exit_signal_field(15, false), 15, "a clone(SIGTERM) child");
        assert_eq!(
            exit_signal_field(17, true),
            -1,
            "a thread, whatever byte its creation passed"
        );
        assert_eq!(exit_signal_field(0, true), -1);
    }

    /// A dead task keeps its directory only when it leads the group - Linux's
    /// `__unhash_process`, and the clause that lets a tid's directory be served
    /// at all without serving one for a thread that has left.
    ///
    /// Measured whole on a current host (`.workbuddy/probe_tid_dir.c`, Linux
    /// 7.2.11): a thread that returned from its start routine - joined or not -
    /// is out of `/proc/<pid>/task` and `/proc/<tid>` is `ENOENT`, while the
    /// zombie *process* of item 63's pin still answers its own `stat`.
    ///
    /// Like its neighbour above, this is a rule about what the *tree* says and
    /// not about what the table holds, which is why it needs no machine under
    /// it: this kernel's dead thread does stay in the table until its slot is
    /// reused.
    #[test]
    fn a_dead_threads_tid_names_nothing_and_a_zombie_leaders_does() {
        assert!(tid_has_a_directory('R', 42, 42), "a running leader");
        assert!(tid_has_a_directory('S', 42, 42), "a sleeping leader");
        assert!(tid_has_a_directory('Z', 42, 42), "a zombie *process*");
        assert!(tid_has_a_directory('S', 42, 41), "a live thread");
        assert!(
            tid_has_a_directory('T', 42, 41),
            "a stopped thread is a task"
        );
        assert!(
            !tid_has_a_directory('Z', 42, 41),
            "a thread that has exited has no tid of its own any more"
        );
    }

    /// The three names Linux answers as **links** are links here, with the two
    /// sizes it reports for them - measured on a current host
    /// (`.workbuddy/probe_proc_self_links.c`): a live target is registered with
    /// an empty text and so reports `st_size` **0** even though `readlink`
    /// answers several bytes, where a fixed target reports **11**, the length of
    /// `self/mounts`.
    #[test]
    fn the_three_names_linux_makes_links_are_links() {
        let live = link(|| Some(String::from("1234")));
        assert_eq!(live.metadata().kind, NodeKind::Symlink);
        assert_eq!(live.mode(), 0o777);
        assert_eq!(live.metadata().size, 0, "a live target is registered empty");
        assert_eq!(live.readlink().as_deref(), Ok("1234"));

        let fixed = fixed_link("self/mounts");
        assert_eq!(fixed.metadata().kind, NodeKind::Symlink);
        assert_eq!(fixed.mode(), 0o777);
        assert_eq!(
            fixed.metadata().size,
            "self/mounts".len() as u64,
            "a fixed target's size is the text it was registered with"
        );
        assert_eq!(fixed.readlink().as_deref(), Ok("self/mounts"));

        // A link to a process that is gone is `ENOENT`, which is what
        // `/proc/self` says on Linux after the process it named is reaped.
        assert_eq!(link(|| None).readlink().err(), Some(VfsError::NoEntry));
    }

    /// `/proc/self` and `/proc/thread-self` answer the reader's *number*, and
    /// the number is the one this tree names that reader by - the rule the pid
    /// namespace made "the reader's" ([`reader_number`]) - so the link and the
    /// directory it leads to cannot disagree. The second name is the first
    /// name's, plus its own thread's, which is the shape Linux's
    /// `<pid>/task/<tid>` has.
    #[test]
    fn the_self_names_name_the_reader() {
        let fs = ProcFs::new();
        let root = fs.root();

        let self_link = root.lookup("self").expect("a self name");
        assert_eq!(self_link.metadata().kind, NodeKind::Symlink);
        let number = self_link.readlink().expect("a live target");
        assert!(
            number.chars().all(|c| c.is_ascii_digit()),
            "the target is the reader's number: {number}"
        );
        assert_eq!(
            number,
            alloc::format!("{}", reader_number(current_pid())),
            "and it is the number this tree names the reader by"
        );

        let thread = root
            .lookup("thread-self")
            .expect("a thread-self name")
            .readlink()
            .expect("a live target");
        let want = alloc::format!(
            "{}/task/{}",
            reader_number(current_pid()),
            reader_number(current_tid())
        );
        assert_eq!(thread, want);
        assert!(
            thread.starts_with(&alloc::format!("{number}/task/")),
            "the thread's name begins with the process's: {thread}"
        );

        let mounts = root.lookup("mounts").expect("a mounts name");
        assert_eq!(mounts.metadata().kind, NodeKind::Symlink);
        assert_eq!(mounts.metadata().size, "self/mounts".len() as u64);
        assert_eq!(mounts.readlink().as_deref(), Ok("self/mounts"));
    }

    /// The word a procfs node reports about itself is its own shape's, not the
    /// kind's default, and the three nodes whose word is narrower than their
    /// shape say so - each measured on a current host
    /// (`.workbuddy/probe_proc_node_modes.c`).
    #[test]
    fn a_proc_nodes_own_mode_follows_its_shape() {
        // A directory in the tree is `0555`, not the kind's `0755`; the two
        // whose own word is narrower are `/proc/<pid>/fd` and
        // `/proc/<pid>/ns`.
        assert_eq!(ProcDir::new(None).mode(), PROC_DIR_MODE);
        assert_eq!(ProcDir::own(None, 0o500, true).mode(), 0o500);
        assert_eq!(ProcDir::own(None, 0o511, false).mode(), 0o511);
        // A generated file nothing can write is `0444`; a `ProcCtl` and the
        // configuration-space window are `0644`; and the environment is
        // `0400`, which is the one node whose word is its own.
        assert_eq!(file(|| String::new()).mode(), PROC_RO_FILE_MODE);
        assert_eq!(maybe_file(|| None).mode(), PROC_RO_FILE_MODE);
        assert_eq!(env_file(|| None).mode(), 0o400);
        assert_eq!(
            ctl(|| String::new(), |_| Ok(())).mode(),
            PROC_RW_FILE_MODE,
            "a sysctl is read-write"
        );
        // The size is zero for a file whose contents are generated, and the
        // node's own where it is known before the read.
        assert_eq!(file(|| String::new()).metadata().size, 0);
        assert_eq!(
            file_of_size(9, || String::from("nine byt\n"))
                .metadata()
                .size,
            9
        );
    }

    /// `/proc/net/tcp` is parsed by fixed column positions, so its address byte
    /// order (`127.0.0.1` -> `0100007F`) and the state byte must be exact.
    #[test]
    fn the_net_table_prints_addresses_and_state_as_linux_does() {
        let rows = [NetRow {
            local: ([127, 0, 0, 1], 8080),
            remote: ([10, 0, 2, 15], 443),
            state: 1,
            uid: 0,
            inode: 42,
        }];
        let text = format_net(&rows);
        let head = text.lines().next().unwrap();
        assert!(
            head.starts_with("  sl  local_address rem_address"),
            "{head}"
        );
        let row = text.lines().nth(1).unwrap();
        // local 127.0.0.1:8080 = 0100007F:1F90, remote 10.0.2.15:443 = 0F02000A:01BB.
        assert!(row.contains("0100007F:1F90"), "{row}");
        assert!(row.contains("0F02000A:01BB"), "{row}");
        // state ESTABLISHED (1) and the inode appear.
        assert!(row.contains(" 01 ") && row.contains(" 42 "), "{row}");
        // No rows -> just the header line.
        assert_eq!(format_net(&[]).lines().count(), 1);
    }

    /// `/proc/net/route` is what `fastfetch`'s LocalIp (and `ip route`) parse to
    /// name the default interface; the first tab-field is the iface and the
    /// destination `00000000` line is the default route.
    #[test]
    fn the_route_table_names_the_default_interface_and_gateway() {
        let text = format_route_table(&[10, 0, 2, 15], &[10, 0, 2, 2]);
        let mut lines = text.lines();
        assert!(
            lines
                .next()
                .unwrap()
                .starts_with("Iface\tDestination\tGateway")
        );
        // Default route: destination 00000000, gateway 10.0.2.2 -> 0202000A.
        let def = lines.next().unwrap();
        assert!(def.starts_with("eth0\t00000000\t0202000A\t0003"), "{def}");
        // Link route: the /24 network 10.0.2.0 -> 0002000A, mask 00FFFFFF.
        let link = lines.next().unwrap();
        assert!(link.starts_with("eth0\t0002000A\t00000000\t0001"), "{link}");
        assert!(link.ends_with("00FFFFFF\t0\t0\t0"), "{link}");
    }

    /// The shapes below are copied from what Linux prints, because that is
    /// the only specification these files have: they are parsed by column
    /// width, and a program that reads them is not asked to be forgiving.
    #[test]
    fn a_partition_row_has_linuxs_column_widths() {
        // ` 254        0   20971520 vda` - a 20 GiB disk, major 254 minor 0.
        assert_eq!(
            partition_row("vda", 254, 0, 20_971_520 * 2),
            " 254    0   20971520 vda\n"
        );
        // ` 254        3     131072 vda3` - a 64 MiB partition.
        assert_eq!(
            partition_row("vda3", 254, 3, 131_072 * 2),
            " 254    3     131072 vda3\n"
        );
    }

    /// `/proc/net/dev`'s row shape, byte for byte against what real Linux
    /// prints (host, 2026-10-11): the six-wide right-aligned name and its
    /// colon, then sixteen right-justified counters at their measured widths.
    /// The two strings below are copied from the host's own file - a short
    /// counter row and one whose ten-digit byte count overflows its column and
    /// pushes only itself - so this test is the measurement, not a restatement
    /// of the format string.
    ///
    /// The counterfactual is §10.4's instrument read wrong: a renderer that
    /// shifted any column, or left the counters unaligned, would still let
    /// `t-netstats` find *a* number on the line while breaking every reader
    /// that parses by position - which is the whole reason the file exists.
    #[test]
    fn a_net_dev_row_has_linuxs_column_widths() {
        let row = |name, rx_bytes, rx_packets, tx_bytes, tx_packets| NetDevRow {
            name,
            rx_bytes,
            rx_packets,
            tx_bytes,
            tx_packets,
            mtu: 1500,
            pool_hits: 0,
            pool_misses: 0,
        };
        // The host's `lo:` row verbatim (its counters replaced with the same
        // numbers on both sides, as this kernel reports no drops).
        let expected = "    lo:  102781    1346    0    0    0     0          0         0   102781    1346    0    0    0     0       0          0";
        let mut got = net_dev_row(&row("lo", 102_781, 1_346, 102_781, 1_346));
        // Strip the appended words before comparing to Linux's line; they are
        // claimed separately below.
        got.truncate(expected.len() + 1);
        assert_eq!(got.trim_end(), expected, "the standard columns drifted");
        // A ten-digit byte count overflows its column and pushes only itself,
        // exactly as the host's wlan0 row does.
        let big = net_dev_row(&row(
            "wlan0",
            558_747_477,
            1_615_726,
            3_399_828_847,
            2_768_186,
        ));
        assert!(
            big.starts_with(
                " wlan0: 558747477 1615726    0    0    0     0          0         0 3399828847 2768186    0    0    0     0       0          0"
            ),
            "{big}"
        );
        // The appended words, named once each: the settable MTU and the pool's
        // pair, after the last standard column.
        let eth = net_dev_row(&NetDevRow {
            name: "eth0",
            rx_bytes: 7,
            rx_packets: 1,
            tx_bytes: 9,
            tx_packets: 2,
            mtu: 1500,
            pool_hits: 41,
            pool_misses: 3,
        });
        assert!(eth.ends_with("mtu 1500 pool=41:3\n"), "{eth}");
    }

    /// Every disk this machine has, each under its own major and with its own
    /// size - and the size comes out of the table rather than being read back
    /// from the storage, which is what makes `/proc/partitions`, `/dev` and
    /// `/sys/class/block` three renderings of one table instead of three
    /// readers of a disk.
    ///
    /// The counterfactual is the second disk: while the numbering was "the
    /// boot disk and its partitions" there was one size in this file, and a
    /// renderer that used it for everything would show the SATA disk as the
    /// size of the virtio one.
    /// `/proc/modules` exists, and reads as nothing at all.
    ///
    /// The counterfactual is the report that started this: `lsmod` printed
    /// `/proc/modules: No such file or directory` and exited 1. With the file
    /// there and empty it prints its header and no rows and exits 0, which is
    /// what it does on a Linux with no modules - and a file that existed but
    /// said "unknown" would be the wrong answer in a way `lsmod` could not
    /// report, since it has no vocabulary for one.
    #[test]
    fn modules_is_present_and_empty() {
        let fs = ProcFs::new();
        let modules = fs.root().lookup("modules").expect("/proc/modules");
        assert_eq!(modules.metadata().kind, NodeKind::File);
        let mut buf = [0u8; 16];
        assert_eq!(modules.read_at(0, &mut buf).expect("a read"), 0);
    }

    /// `mountinfo` is parsed by field position and by nothing else, so the
    /// position of every field is the whole of its contract.
    ///
    /// The measurement behind the shape: busybox 1.38's `lsblk` - which is what
    /// this image's `/bin/lsblk` is - was run on the host against a mockup of
    /// this tree and a file rendered by this function, and it printed the device
    /// table *with* its `MOUNTPOINTS` column, matching the mount to the disk by
    /// the third field alone. Take that field away, or put the attachment
    /// counter there as the kernel used to, and the column comes out blank.
    #[test]
    fn mountinfo_prints_the_fields_proc_5_lists_in_the_order_it_lists_them() {
        let records = [
            crate::vfs::mount::MountRecord {
                mount_id: 1,
                parent_id: 1,
                dev: crate::uevent::makedev(0, 1),
                source: String::from("rootfs"),
                path: String::from("/"),
                fstype: String::from("ramfs"),
                options: String::from("rw"),
            },
            crate::vfs::mount::MountRecord {
                mount_id: 2,
                parent_id: 1,
                dev: crate::uevent::makedev(254, 2),
                source: String::from("/dev/vda2"),
                path: String::from("/sysroot"),
                fstype: String::from("ext4"),
                options: String::from("rw"),
            },
            crate::vfs::mount::MountRecord {
                mount_id: 3,
                parent_id: 2,
                dev: crate::uevent::makedev(0, 2),
                source: String::from("proc"),
                path: String::from("/sysroot/proc"),
                fstype: String::from("proc"),
                options: String::from("ro,bind"),
            },
        ];
        let rendered = mountinfo_lines(&records);
        let lines: Vec<&str> = rendered.lines().collect();
        assert_eq!(lines.len(), 3, "one line per mount");

        let fields: Vec<&str> = lines[1].split(' ').map(|f| f).collect();
        assert_eq!(fields[0], "2", "field 1: the mount's own id");
        assert_eq!(fields[1], "1", "field 2: the mount it hangs under");
        assert_eq!(fields[2], "254:2", "field 3: the device, as maj:min");
        assert_eq!(fields[3], "/", "field 4: the root of the mount in the fs");
        assert_eq!(fields[4], "/sysroot", "field 5: where it is mounted");
        assert_eq!(fields[5], "rw", "field 6: the per-mount options");
        assert_eq!(fields[6], "-", "field 7: the separator, nothing optional");
        assert_eq!(fields[7], "ext4", "field 8: the type");
        assert_eq!(fields[8], "/dev/vda2", "field 9: the source");
        assert_eq!(fields[9], "rw", "field 10: the superblock options");
        assert_eq!(fields.len(), 10, "and nothing else - ten fields, no more");

        // The root hangs under itself, which is how Linux reports the root of a
        // namespace, and a pseudo-filesystem's device is anonymous rather than
        // absent: 0:1, not the `0:0` that means "no device".
        assert_eq!(lines[0].split(' ').nth(1), Some("1"), "the root's parent");
        assert_eq!(lines[0].split(' ').nth(2), Some("0:1"));
        assert_eq!(lines[2].split(' ').nth(2), Some("0:2"));
        // A read-only mount says so, and says it in both columns - the third
        // mount is a bind, so the per-mount options carry both facts.
        assert_eq!(lines[2].split(' ').nth(5), Some("ro,bind"));
        assert_eq!(lines[2].split(' ').last(), Some("ro"));
    }

    #[test]
    fn partitions_lists_every_disk_the_machine_has() {
        use crate::uevent::BlockEntry;
        let numbering = crate::uevent::Numbering {
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
                    name: String::from("vda1"),
                    major: 254,
                    minor: 1,
                    whole: false,
                    devpath: "/devices/virtio/block",
                    sectors: 262_144,
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
        };
        let text = partitions_text(&numbering);
        let mut lines = text.lines();
        assert_eq!(lines.next().unwrap(), "major minor  #blocks  name");
        assert_eq!(lines.next().unwrap(), "");
        assert!(text.contains(" 10485760 vda\n"), "{text}");
        assert!(text.contains(" 131072 vda1\n"), "{text}");
        assert!(text.contains(" 2097152 sda\n"), "{text}");
        // Each name under the major its own driver gave it, which is how a
        // program tells the two disks apart without asking either driver.
        assert!(text.contains("   8    0 "), "{text}");
    }

    /// A machine with no block device still gets the header and its blank
    /// line: the tools that read this file read the columns after it, not the
    /// file's emptiness.
    #[test]
    fn a_machine_with_no_disk_still_has_the_header() {
        let text = partitions();
        assert!(
            text.starts_with("major minor  #blocks  name\n\n"),
            "{text:?}"
        );
    }

    /// The named-line shape, pinned against a line Linux actually printed: the
    /// two are byte for byte the same, which is the only specification
    /// `/proc/<pid>/maps` has.
    #[test]
    fn a_named_mapping_lands_in_the_column_linux_prints_it_in() {
        let text = format_maps(&[MapLine {
            start: 0x7f7986_e00000,
            end: 0x7f7987_3f4000,
            prot: MapFlags::READ | MapFlags::USER,
            shared: false,
            offset: 0,
            dev: (0x103, 0x03),
            inode: 15_656_790,
            name: Some(String::from("/usr/lib/locale/locale-archive")),
        }]);
        assert!(
            text.starts_with("7f7986e00000-7f79873f4000 r--p 00000000 103:03 15656790"),
            "{text:?}"
        );
        assert_eq!(
            text.find('/'),
            Some(MAPS_NAME_COLUMN),
            "the name starts where Linux starts it"
        );
        assert!(
            text.ends_with("/usr/lib/locale/locale-archive\n"),
            "{text:?}"
        );
        // Everything between the inode and the name is the padding, and the
        // padding is spaces - a tab there would break a reader that counts.
        let prefix = "7f7986e00000-7f79873f4000 r--p 00000000 103:03 15656790".len();
        assert!(
            text[prefix..MAPS_NAME_COLUMN].bytes().all(|b| b == b' '),
            "the name column is reached with spaces: {:?}",
            &text[prefix..MAPS_NAME_COLUMN]
        );
    }

    /// The permission column is the *grant*, not the bit this kernel records
    /// "the range is present" in: a mapping nobody may touch prints `---p`,
    /// which is the line Linux prints for the same reservation (measured,
    /// `.workbuddy/probe_maps_prot.c`: `7f9132fd1000-7f9132fd3000 ---p
    /// 00000000 00:00 0`).
    ///
    /// The counterfactual is the shape this replaced: printed from the internal
    /// `READ` bit, a `PROT_NONE` reservation read `r--p`, and a tool that
    /// branches on that column would try to read a range whose every access
    /// takes a fault - the divergence `t-maps` pins.
    #[test]
    fn a_permission_column_is_the_grant_and_not_the_present_bit() {
        let user = MapFlags::USER;
        assert_eq!(granted(user | MapFlags::READ), MapFlags::READ);
        assert_eq!(
            granted(user | MapFlags::READ | MapFlags::WRITE),
            MapFlags::READ | MapFlags::WRITE
        );
        assert_eq!(
            granted(user | MapFlags::READ | MapFlags::EXEC),
            MapFlags::READ | MapFlags::EXEC
        );
        assert_eq!(
            granted(MapFlags::READ),
            MapFlags::empty(),
            "PROT_NONE is present and grants nothing"
        );
        assert_eq!(
            granted(MapFlags::READ | MapFlags::WRITE | MapFlags::COW),
            MapFlags::empty(),
            "the copy-on-write bit is not a permission either"
        );
        let text = format_maps(&[MapLine {
            start: 0x7f91_32fd_1000,
            end: 0x7f91_32fd_3000,
            prot: granted(MapFlags::READ),
            shared: false,
            offset: 0,
            dev: (0, 0),
            inode: 0,
            name: None,
        }]);
        assert_eq!(text, "7f9132fd1000-7f9132fd3000 ---p 00000000 00:00 0 \n");
    }

    /// One whole `smaps` record, byte for byte against the record real Linux
    /// printed for the same shape (host, 2026-10-18: a private anonymous
    /// mapping of two pages with one of them resident, `.workbuddy/probe_smaps.c`).
    ///
    /// Every line, the order of them and the column each number ends in are the
    /// file's whole contract: its readers are parsers (`smem`, `ps_mem`, and
    /// everything that asks how much of a compositor's buffer is resident), so a
    /// missing or renamed line is the difference between this kernel's memory
    /// being visible and it not being there.
    ///
    /// The one line that is *not* Linux's own is `VmFlags`, and what it leaves
    /// out are the two words this kernel cannot stand behind: `ac` (this mapping
    /// is overcommit-accounted) and `sd` (soft-dirty tracking, a state nothing
    /// here records). Both are named in `docs/uapi/` §5.2 with Linux's own line
    /// beside them; the words that remain are `vma_flags`' documented rule.
    #[test]
    fn a_smaps_record_has_linuxs_lines_in_linuxs_order() {
        let text = format_smaps(&[SmapsLine {
            map: MapLine {
                start: 0x7f_3ab9_6ca000,
                end: 0x7f_3ab9_6cc000,
                prot: MapFlags::READ | MapFlags::WRITE,
                shared: false,
                offset: 0,
                dev: (0, 0),
                inode: 0,
                name: None,
            },
            rss_pages: 1,
            anonymous: true,
        }]);
        assert_eq!(
            text,
            concat!(
                "7f3ab96ca000-7f3ab96cc000 rw-p 00000000 00:00 0 \n",
                "Size:                  8 kB\n",
                "KernelPageSize:        4 kB\n",
                "MMUPageSize:           4 kB\n",
                "Rss:                   4 kB\n",
                "Pss:                   4 kB\n",
                "Pss_Dirty:             4 kB\n",
                "Shared_Clean:          0 kB\n",
                "Shared_Dirty:          0 kB\n",
                "Private_Clean:         0 kB\n",
                "Private_Dirty:         4 kB\n",
                "Referenced:            4 kB\n",
                "Anonymous:             4 kB\n",
                "KSM:                   0 kB\n",
                "LazyFree:              0 kB\n",
                "AnonHugePages:         0 kB\n",
                "ShmemPmdMapped:        0 kB\n",
                "FilePmdMapped:         0 kB\n",
                "Shared_Hugetlb:        0 kB\n",
                "Private_Hugetlb:       0 kB\n",
                "Swap:                  0 kB\n",
                "SwapPss:               0 kB\n",
                "Locked:                0 kB\n",
                "THPeligible:           0\n",
                "ProtectionKey:         0\n",
                "VmFlags: rd wr mr mw me \n",
            ),
            "the record's lines are Linux's, in Linux's order"
        );
    }

    /// A value wider than one digit ends in the *same* column, which is the
    /// half of the layout a test with one-digit numbers cannot see: Linux
    /// prints `8` and `1516` with both ending in column 24, and a reader that
    /// splits the file on whitespace still finds the number in the same field.
    #[test]
    fn a_wider_value_keeps_the_column_linux_puts_it_in() {
        // The two lines are copied from the host's own file (`probe_smaps.out`,
        // the `[heap]` record and a library record).
        let one = "Size:                136 kB";
        let four = "Size:               1516 kB";
        assert_eq!(one.len(), 27);
        assert_eq!(four.len(), 27);
        let line = |start, end| SmapsLine {
            map: MapLine {
                start,
                end,
                prot: MapFlags::READ | MapFlags::WRITE,
                shared: false,
                offset: 0,
                dev: (0, 0),
                inode: 0,
                name: None,
            },
            rss_pages: 0,
            anonymous: true,
        };
        let text = format_smaps(&[line(0x55f4_db03_6000, 0x55f4_db05_8000)]);
        assert!(text.contains(one), "{text:?}");
        let text = format_smaps(&[line(0x7f42_7140_0000, 0x7f42_7157_b000)]);
        assert!(text.contains(four), "{text:?}");
    }

    /// Nothing resident is every number zero - including `Anonymous`, whose
    /// subject is the *kind* of page rather than how many there are: a mapping
    /// whose pages have never arrived is mapped, and unmaterialised.
    #[test]
    fn a_record_with_nothing_resident_is_all_zeroes() {
        let text = format_smaps(&[SmapsLine {
            map: MapLine {
                start: 0x7f_0000_0000,
                end: 0x7f_0002_0000,
                prot: MapFlags::READ,
                shared: false,
                offset: 0,
                dev: (0, 0),
                inode: 0,
                name: None,
            },
            rss_pages: 0,
            anonymous: true,
        }]);
        // Size is the mapping's own length; every accounting line is zero.
        assert!(text.contains("Size:                128 kB\n"), "{text:?}");
        for zero in [
            "Rss:",
            "Pss:",
            "Pss_Dirty:",
            "Shared_Clean:",
            "Shared_Dirty:",
            "Private_Clean:",
            "Private_Dirty:",
            "Referenced:",
            "Anonymous:",
            "Swap:",
            "Locked:",
        ] {
            let needle = alloc::format!("{zero:<16}{:>8} kB\n", 0);
            assert!(text.contains(&needle), "{zero} is zero in {text:?}");
        }
    }

    /// The `VmFlags` words this kernel prints, per shape, against Linux's own
    /// (measured, `probe_smaps.out`): a guard page prints `mr mw me` because
    /// `mprotect` may grant any protection, a shared mapping adds `sh` and `ms`,
    /// and an executable one adds `ex` - and the trailing space before the
    /// newline is Linux's, because each word is written followed by one.
    #[test]
    fn the_flags_are_the_words_this_kernel_can_stand_behind() {
        let flags = |prot, shared| {
            vma_flags(&MapLine {
                start: 0,
                end: 4096,
                prot,
                shared,
                offset: 0,
                dev: (0, 0),
                inode: 0,
                name: None,
            })
            .join(" ")
        };
        assert_eq!(
            flags(MapFlags::READ | MapFlags::WRITE, false),
            "rd wr mr mw me"
        );
        assert_eq!(
            flags(MapFlags::READ | MapFlags::EXEC, false),
            "rd ex mr mw me"
        );
        assert_eq!(flags(MapFlags::USER, true), "sh mr mw me ms");
        assert_eq!(
            flags(MapFlags::READ | MapFlags::WRITE | MapFlags::USER, true),
            "rd wr sh mr mw me ms"
        );
    }

    /// An unnamed mapping is the prefix and the space a name would have
    /// followed - *not* a line padded out to the name column with nothing in
    /// it, which is what Linux prints and what a reader splitting on
    /// whitespace can tell apart from a name of spaces.
    #[test]
    fn an_unnamed_mapping_ends_after_the_space_no_name_followed() {
        let text = format_maps(&[MapLine {
            start: 0x7f7987_61b000,
            end: 0x7f7987_623000,
            prot: MapFlags::READ | MapFlags::WRITE | MapFlags::USER,
            shared: false,
            offset: 0,
            dev: (0, 0),
            inode: 0,
            name: None,
        }]);
        assert_eq!(text, "7f798761b000-7f7987623000 rw-p 00000000 00:00 0 \n");
        assert!(text.len() < MAPS_NAME_COLUMN);
    }

    /// The fourth permission character is the *shared* bit, and the other
    /// three are the mapping's own bits - a `PROT_NONE` range prints `---p`
    /// rather than a plausible-looking `r`, because it is not readable.
    #[test]
    fn the_permission_word_spells_the_mapping_out() {
        let word = |prot: MapFlags, shared: bool| {
            let text = format_maps(&[MapLine {
                start: 0x1000,
                end: 0x2000,
                prot,
                shared,
                offset: 0,
                dev: (0, 0),
                inode: 0,
                name: None,
            }]);
            // `00001000-00002000 r--p ...`: 8 + 1 + 8 + 1 bytes before the
            // permission word.
            String::from(&text[18..22])
        };
        assert_eq!(word(MapFlags::READ | MapFlags::USER, false), "r--p");
        assert_eq!(
            word(MapFlags::READ | MapFlags::WRITE | MapFlags::USER, true),
            "rw-s"
        );
        assert_eq!(
            word(MapFlags::READ | MapFlags::EXEC | MapFlags::USER, false),
            "r-xp"
        );
        assert_eq!(word(MapFlags::empty(), false), "---p");
    }

    /// Read a whole generated file, the way `cat` does.
    fn read(node: &Arc<dyn Inode>) -> String {
        let mut out = Vec::new();
        let mut buf = [0u8; 256];
        loop {
            let n = node.read_at(out.len() as u64, &mut buf).expect("a read");
            if n == 0 {
                break;
            }
            out.extend_from_slice(&buf[..n]);
        }
        String::from_utf8(out).expect("utf-8")
    }

    #[test]
    fn the_bus_tree_lists_the_functions_the_kernel_found() {
        // The device list is global, so this test takes its turn with the
        // others that publish one.
        let _lock = crate::pci::test_lock::take();
        let mut disk = crate::pci::Device::fixture(0, 3, 0);
        disk.driver = Some("virtio-pci");
        let mut bridge = crate::pci::Device::fixture(0, 1, 2);
        bridge.header_type = 0x01;
        bridge.bridge = Some(0x01);
        let behind = crate::pci::Device::fixture(1, 0, 0);
        crate::pci::publish(alloc::vec![disk, bridge, behind]);

        let fs = ProcFs::new();
        let pci = fs
            .root()
            .lookup("bus")
            .unwrap()
            .lookup("pci")
            .expect("the fallback pciutils' second access method needs");

        // `devices`: one line per function, and every line the seventeen
        // numbers pciutils' parser insists on.
        let devices = read(&pci.lookup("devices").unwrap());
        let lines: Vec<&str> = devices.lines().collect();
        assert_eq!(lines.len(), 3);
        assert!(
            lines[0].starts_with("0018\t1af41001\tb\t"),
            "{:?}",
            lines[0]
        );
        assert!(lines[0].ends_with("\tvirtio-pci"), "{:?}", lines[0]);
        assert!(
            lines[2].starts_with("0100\t1af41001\tb\t"),
            "{:?}",
            lines[2]
        );
        assert!(lines[2].ends_with('\t'), "an unclaimed device names nobody");

        // The bus directories, one per bus that has a device, each holding
        // the functions on it under Linux's `<dev>.<func>` names. They sit
        // beside `devices`, which is the other thing the directory holds.
        let mut buses: Vec<String> = Vec::new();
        for (name, kind) in pci.read_dir().unwrap() {
            match kind {
                NodeKind::File => assert_eq!(name, "devices"),
                NodeKind::Dir => buses.push(name),
                other => panic!("{name} is a {other:?}"),
            }
        }
        buses.sort();
        assert_eq!(buses, alloc::vec![String::from("00"), String::from("01")]);

        let bus0 = pci.lookup("00").unwrap();
        let mut names: Vec<String> = bus0
            .read_dir()
            .unwrap()
            .into_iter()
            .map(|(name, kind)| {
                assert_eq!(kind, NodeKind::File);
                name
            })
            .collect();
        names.sort();
        assert_eq!(
            names,
            alloc::vec![String::from("01.2"), String::from("03.0")]
        );
        assert!(pci.lookup("01").unwrap().lookup("00.0").is_ok());
        // A bus that is not there names nothing rather than an empty
        // directory: a listing must not invent one.
        assert!(pci.lookup("ff").is_err());

        // Each function's file is its configuration space - 256 bytes of it,
        // the most mechanism 1 can address.
        let config = bus0.lookup("03.0").unwrap();
        assert_eq!(config.metadata().kind, NodeKind::File);
        assert_eq!(config.metadata().size, 256);

        crate::pci::publish(Vec::new());
    }

    /// A `numa_maps` line is the start alone - no `-end`, unlike `maps` - and
    /// the counts are the resident pages and nothing else: `anon` only for a
    /// range whose pages are the mapping's own, `dirty` always (the rule
    /// `smaps` applies), `active=0` because there is no LRU to be active on,
    /// and `N0` the resident count because there is one node. All measured on
    /// a current host first (`.workbuddy/probe_numamaps2.c`, Linux 7.2.11):
    /// `7f9092bdf000 default anon=1 dirty=1 active=0 N0=1 kernelpagesize_kB=4`.
    #[test]
    fn a_numa_maps_line_is_the_start_the_policy_and_the_resident_count() {
        let line = |start, pages, anonymous, name| SmapsLine {
            map: MapLine {
                start,
                end: start + 0x4000,
                prot: MapFlags::READ | MapFlags::WRITE | MapFlags::USER,
                shared: false,
                offset: 0,
                dev: (0, 0),
                inode: 0,
                name,
            },
            rss_pages: pages,
            anonymous,
        };
        let text = format_numa_maps(&[
            line(0x7f90_92bd_f000, 1, true, None),
            line(0x7f90_92bb_5000, 1, false, Some(String::from("/bin/sh"))),
            line(0x7f90_92ba_9000, 0, false, Some(String::from("/bin/sh"))),
            line(0x55_5555_4000, 3, true, Some(String::from("[heap]"))),
            line(0x7f90_92b0_0000, 2, true, Some(String::from("[vdso]"))),
        ]);
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(
            lines[0],
            "7f9092bdf000 default anon=1 dirty=1 active=0 N0=1 kernelpagesize_kB=4"
        );
        assert_eq!(
            lines[1],
            "7f9092bb5000 default file=/bin/sh dirty=1 active=0 N0=1 kernelpagesize_kB=4"
        );
        // A range with nothing resident says the name and the policy, and no
        // number at all - which is what the host printed for an untouched
        // mapping of a file it could still name.
        assert_eq!(lines[2], "7f9092ba9000 default file=/bin/sh");
        assert_eq!(
            lines[3],
            "5555554000 default heap anon=3 dirty=3 active=0 N0=3 kernelpagesize_kB=4"
        );
        // A name that is not a path is not printed: `[vdso]` carries no
        // `file=` on Linux either.
        assert_eq!(
            lines[4],
            "7f9092b00000 default anon=2 dirty=2 active=0 N0=2 kernelpagesize_kB=4"
        );
    }

    /// A path's space and tab are written `\040` and `\011` - measured, with a
    /// path carrying both - because a `numa_maps` line is whitespace-separated
    /// and a name that broke it would break the count a reader is after.
    #[test]
    fn a_numa_maps_path_escapes_what_would_break_the_line() {
        let text = format_numa_maps(&[SmapsLine {
            map: MapLine {
                start: 0x1000,
                end: 0x2000,
                prot: MapFlags::READ | MapFlags::USER,
                shared: true,
                offset: 0,
                dev: (0, 0),
                inode: 0,
                name: Some(String::from("/tmp/a b\tc.bin")),
            },
            rss_pages: 1,
            anonymous: false,
        }]);
        assert!(
            text.starts_with("00001000 default file=/tmp/a\\040b\\011c.bin"),
            "{text:?}"
        );
    }

    /// `smaps_rollup` is the *bounds* of the address space under a `[rollup]`
    /// name, then the twenty-two field lines that survive the summing: the
    /// three per-mapping sizes and the three per-mapping flags drop out, and
    /// the three `Pss_*` that only a total can carry come in. Measured on a
    /// current host (`.workbuddy/probe_rollup.c`).
    #[test]
    fn the_rollup_is_one_record_for_the_whole_space() {
        let line = |start, end, pages, anonymous| SmapsLine {
            map: MapLine {
                start,
                end,
                prot: MapFlags::READ | MapFlags::WRITE | MapFlags::USER,
                shared: false,
                offset: 0,
                dev: (0, 0),
                inode: 0,
                name: None,
            },
            rss_pages: pages,
            anonymous,
        };
        let text = format_smaps_rollup(&[
            line(0x0000_1000, 0x0000_2000, 1, true),
            line(0x0000_2000, 0x0000_3000, 2, false),
        ]);
        let mut lines = text.lines();
        let header = lines.next().expect("a header");
        assert_eq!(
            header,
            "00001000-00003000 ---p 00000000 00:00 0                                  [rollup]"
        );
        // The name is in the same column a `maps` line names its mapping,
        // which is what lets one parser read both.
        assert_eq!(header.find("[rollup]"), Some(MAPS_NAME_COLUMN));

        let keys: Vec<&str> = lines
            .filter_map(|l| l.split_once(':').map(|(k, _)| k))
            .collect();
        assert_eq!(
            keys,
            alloc::vec![
                "Rss",
                "Pss",
                "Pss_Dirty",
                "Pss_Anon",
                "Pss_File",
                "Pss_Shmem",
                "Shared_Clean",
                "Shared_Dirty",
                "Private_Clean",
                "Private_Dirty",
                "Referenced",
                "Anonymous",
                "KSM",
                "LazyFree",
                "AnonHugePages",
                "ShmemPmdMapped",
                "FilePmdMapped",
                "Shared_Hugetlb",
                "Private_Hugetlb",
                "Swap",
                "SwapPss",
                "Locked",
            ]
        );
        // The numbers are the sums of the same numbers `smaps` prints: 12 kB
        // resident in all, 4 of it anonymous and 8 of it an object's.
        assert!(text.contains("Rss:                  12 kB\n"), "{text:?}");
        assert!(text.contains("Pss_Anon:              4 kB\n"), "{text:?}");
        assert!(text.contains("Pss_File:              8 kB\n"), "{text:?}");
        assert!(text.contains("Anonymous:             4 kB\n"), "{text:?}");
        assert!(text.contains("Private_Dirty:        12 kB\n"), "{text:?}");
    }

    /// One process record for the test below: the fields the rule reads, and
    /// the defaults the rest of the file never asks a fixture for.
    ///
    /// `group` is what the record's `pid` carries (the thread group leader's
    /// id) while `ppid` is the **group of the task that forked it** - the two
    /// numbers this item keeps apart, so a fixture that set one from the other
    /// would hide the rule it is a fixture for. The host's `Process` is the
    /// same shape the machine's snapshot is, which is the point of the mirror.
    fn process(
        group: usize,
        tid: usize,
        parent: Option<usize>,
        ppid: usize,
        state: char,
    ) -> Process {
        Process {
            comm: alloc::string::String::from("t"),
            exe: alloc::string::String::new(),
            argv: Default::default(),
            env: Default::default(),
            cwd: alloc::string::String::from("/"),
            state,
            pid: group,
            tid,
            parent,
            ppid,
            pgid: group,
            session: 0,
            threads: 1,
            rss_pages: 0,
            exit_signal: 17,
        }
    }

    /// A task's `children` file lists the children **it** forked - by the link
    /// and not by the group - which is item 64's residual and the rule one
    /// probe settled. Measured on a current host
    /// (`.workbuddy/probe_children_owner.c`, Linux 7.2.11): a child forked by a
    /// non-leader thread was listed under that thread's `task/<tid>/children`
    /// alone, while the leader's file carried the leader's own child.
    ///
    /// The two other clauses are the file's: a **thread** goes on no list (the
    /// measurement's `pthread_create`d thread was on neither file), and a
    /// **corpse stays** until it is reaped - `Z` is what the exit writes, so the
    /// state letter is the only thing telling the two apart from a live child.
    #[test]
    fn a_children_file_lists_the_children_the_task_itself_forked() {
        // The leader 40 (nobody forked it); a thread 41 of that group, whose
        // own parent is the leader; a child the leader forked 42; a child the
        // *thread* forked 43 - whose `parent` is the thread 41 while its `ppid`
        // is the group 40, which is the whole point; and 42's own child 44, a
        // corpse.
        let table = alloc::vec![
            process(40, 40, None, 0, 'S'),
            process(40, 41, Some(40), 40, 'S'),
            process(42, 42, Some(40), 40, 'S'),
            process(43, 43, Some(41), 40, 'S'),
            process(44, 44, Some(42), 42, 'Z'),
        ];
        assert_eq!(
            child_slots(&table, 40),
            alloc::vec![42],
            "the leader's own child, and not the thread's nor the thread itself"
        );
        assert_eq!(
            child_slots(&table, 41),
            alloc::vec![43],
            "the forking thread's own child"
        );
        assert_eq!(child_slots(&table, 42), alloc::vec![44], "a corpse stays");
        assert_eq!(
            child_slots(&table, 43),
            alloc::vec![],
            "and a task that forked nobody has an empty list"
        );
        // The filter is the *link*: a task whose parent is nobody is on no
        // file, and asking for a slot nobody holds answers nothing.
        assert_eq!(child_slots(&table, 44), alloc::vec![]);
        assert_eq!(child_slots(&table, 99), alloc::vec![]);
    }
}
