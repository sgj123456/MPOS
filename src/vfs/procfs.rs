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
}

impl Inode for ProcFile {
    fn metadata(&self) -> Metadata {
        // Size zero, as Linux reports for the files it generates: the answer
        // is not known until it is asked for, and asking is the work. A
        // generator that ran here would run during every directory listing -
        // and /proc/mounts' generator walks the tree, which walks /proc.
        Metadata {
            ino: self.ino,
            kind: NodeKind::File,
            size: 0,
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
    ns: Option<(usize, crate::vfs::ns::Kind)>,
}

impl Inode for ProcLink {
    fn metadata(&self) -> Metadata {
        Metadata {
            ino: self.ino,
            kind: NodeKind::Symlink,
            size: 0,
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
        // Asked at open time, not recorded at build time: a process that
        // unshared a moment ago is in a different namespace now, and a link
        // that answered with what it was built from would be naming the one
        // the process left. `None` when the process is gone - the same answer
        // `readlink` gives, one instant later.
        let (pid, kind) = self.ns?;
        namespace_of(pid, kind)
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
}

impl ProcDir {
    fn new(dynamic: Option<Arc<dyn Dynamic>>) -> Arc<ProcDir> {
        Arc::new(ProcDir {
            ino: next_ino(),
            entries: Mutex::new(RefCell::new(BTreeMap::new())),
            dynamic,
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
            size: 0,
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

/// A directory that is whichever directory the closure answers, decided at
/// every lookup.
///
/// `/proc/self` is why this exists: it names a different process's directory
/// depending on who reads it, so it cannot be built when the tree is built.
/// Every operation forwards to whatever is true now, and a process that has
/// gone answers `NoEntry` - the same thing `ls /proc/self` says on Linux for
/// a pid that no longer exists.
struct MirrorDir {
    ino: Ino,
    resolve: Box<dyn Fn() -> Option<Arc<dyn Inode>> + Send + Sync>,
}

impl Inode for MirrorDir {
    fn metadata(&self) -> Metadata {
        Metadata {
            ino: self.ino,
            kind: NodeKind::Dir,
            size: 0,
        }
    }

    fn lookup(&self, name: &str) -> VfsResult<Arc<dyn Inode>> {
        (self.resolve)().ok_or(VfsError::NoEntry)?.lookup(name)
    }

    fn read_dir(&self) -> VfsResult<Vec<(String, NodeKind)>> {
        (self.resolve)().ok_or(VfsError::NoEntry)?.read_dir()
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
    })
}

/// A file that reports something which can go away.
fn maybe_file(generate: impl Fn() -> Option<String> + Send + Sync + 'static) -> Arc<dyn Inode> {
    Arc::new(ProcFile {
        ino: next_ino(),
        generate: Box::new(generate),
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

#[cfg(target_os = "none")]
fn pid_exists(pid: usize) -> bool {
    crate::sched::snapshot(pid)
        .map(|task| task.pid == pid)
        .unwrap_or(false)
}

#[cfg(not(target_os = "none"))]
fn pid_exists(_pid: usize) -> bool {
    false
}

/// The ids of every thread in the process `pid`, leader included.
#[cfg(target_os = "none")]
fn thread_ids(pid: usize) -> Vec<usize> {
    crate::sched::thread_snapshots(pid)
        .into_iter()
        .map(|task| task.tid)
        .collect()
}

#[cfg(not(target_os = "none"))]
fn thread_ids(_pid: usize) -> Vec<usize> {
    Vec::new()
}

/// The direct children of `pid`, as `children` reports them.
#[cfg(target_os = "none")]
fn child_ids(pid: usize) -> Vec<usize> {
    let mut ids: Vec<usize> = crate::sched::snapshots()
        .into_iter()
        .filter(|task| task.pid == task.tid && task.ppid == pid)
        .map(|task| task.pid)
        .collect();
    ids.sort_unstable();
    ids
}

#[cfg(not(target_os = "none"))]
fn child_ids(_pid: usize) -> Vec<usize> {
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
    pub ppid: usize,
    pub pgid: usize,
    pub threads: usize,
    pub rss_pages: usize,
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
        Cow::Borrowed("0"),                          // wchan
        Cow::Borrowed("0"),                          // nswap
        Cow::Borrowed("0"),                          // cnswap
        Cow::Borrowed("17"),                         // exit_signal: SIGCHLD
        Cow::Borrowed("0"),                          // processor
        Cow::Borrowed("0"),                          // rt_priority
        Cow::Borrowed("0"),                          // policy
        Cow::Borrowed("0"),                          // delayacct_blkio_ticks
        Cow::Borrowed("0"),                          // guest_time
        Cow::Borrowed("0"),                          // cguest_time
        Cow::Borrowed("0"),                          // start_data
        Cow::Borrowed("0"),                          // end_data
        Cow::Borrowed("0"),                          // start_brk
        Cow::Borrowed("0"),                          // arg_start
        Cow::Borrowed("0"),                          // arg_end
        Cow::Borrowed("0"),                          // env_start
        Cow::Borrowed("0"),                          // env_end
        Cow::Borrowed("0"),                          // exit_code
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
    /// The permissions bits, as `mmap` left them.
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
pub fn format_maps(lines: &[MapLine]) -> String {
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

/// `/proc/<pid>/maps`: every range in the process's address space.
///
/// Three sources, because the address space is recorded in three places: the
/// VMA table holds what `mmap`, `mprotect` and `brk` put there, the loader's
/// recorded ranges hold what `execve` and a private file mapping filled in
/// eagerly (neither leaves a VMA - see [`crate::mm::vma::MappedRange`]), and the
/// program break is what names the heap.
///
/// What this cannot report, and does not pretend to: a mapping's `offset` and
/// `inode` when the pages are a *copy* of a file rather than the file's own
/// pages. This kernel reads such a file in rather than paging it, so there is
/// no object left to name - the mapping gets the file's name and zeroes for the
/// two columns, and `NOTES.md` §7.4 records the gap.
#[cfg(target_os = "none")]
fn maps_of(pid: usize) -> Option<String> {
    use crate::mm::vma::{Vma, VmaKind};

    let facts = crate::sched::map_facts(pid)?;

    // `[heap]` is a reporting convention rather than a mapping's own name, so
    // it is applied here, by range: Linux prints it for the break's VMA, and
    // the break's VMA here is an ordinary anonymous one (see `sched::brk`).
    let heap_name = |vma: &Vma| -> Option<String> {
        let (start, end) = facts.heap?;
        (vma.start >= start && vma.end <= end && vma.name.is_none()).then(|| String::from("[heap]"))
    };

    let mut lines: Vec<MapLine> = Vec::new();
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
        lines.push(MapLine {
            start: vma.start,
            end: vma.end,
            prot: vma.prot,
            shared,
            offset,
            dev,
            inode,
            name,
        });
    }
    for range in &facts.eager {
        lines.push(MapLine {
            start: range.start,
            end: range.end,
            prot: range.prot,
            // Eagerly-filled ranges are a private copy or an image: `private`,
            // which is what Linux prints for both.
            shared: false,
            offset: 0,
            dev: (0, 0),
            inode: 0,
            name: range.name.as_deref().map(String::from),
        });
    }

    lines.sort_by_key(|line| (line.start, line.end));
    Some(format_maps(&lines))
}

#[cfg(not(target_os = "none"))]
fn maps_of(_pid: usize) -> Option<String> {
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
fn status_of(pid: usize) -> Option<String> {
    let task = task(pid)?;
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
         NStgid:\t{tgid}\n\
         NSpid:\t{pid}\n\
         NSpgid:\t{pgid}\n\
         NSsid:\t0\n\
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
        tgid = reader_number(task.pid),
        pid = reader_number(task.pid),
        ppid = reader_number(task.ppid),
        pgid = reader_number(task.pgid),
        threads = task.threads,
    ))
}

/// `/proc/<pid>/task/<tid>/children`: the process's direct children, space
/// separated, as `pstree` reads it to walk the tree without scanning /proc.
fn children_of(pid: usize, tid: usize) -> Option<String> {
    if !pid_exists(pid) {
        return None;
    }
    let _ = tid;
    let mut out = String::new();
    for id in child_ids(pid) {
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(&alloc::format!("{id}"));
    }
    out.push('\n');
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

/// One thread's directory: the same files a process has, named for the
/// thread, plus `children` - which Linux puts under `task` because children
/// belong to the process, not to whichever thread is listed.
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
    // shares its process's environment, and Linux answers it here too.
    node.add(
        "environ",
        NodeKind::File,
        maybe_file(move || environ_of(tid)),
    );
    node.add("status", NodeKind::File, maybe_file(move || status_of(tid)));
    node.add(
        "children",
        NodeKind::File,
        maybe_file(move || children_of(pid, tid)),
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

/// `/proc/<pid>`: the directory a process is described by.
///
/// Built when the name is looked up and thrown away when the next lookup
/// finds the process gone - a process's `/proc` entry is not a thing the
/// filesystem keeps, it is a view of the task table, and a view kept after the
/// process left would be a directory of files all answering nothing.
fn process_dir(pid: usize) -> Option<Arc<dyn Inode>> {
    if !pid_exists(pid) {
        return None;
    }
    let node = dir();

    node.add(
        "stat",
        NodeKind::File,
        maybe_file(move || stat_line(pid, pid)),
    );
    node.add("statm", NodeKind::File, maybe_file(move || statm_line(pid)));
    node.add("maps", NodeKind::File, maybe_file(move || maps_of(pid)));
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
        maybe_file(move || limits_of_pid(pid)),
    );
    node.add(
        "comm",
        NodeKind::File,
        maybe_file(move || Some(alloc::format!("{}\n", task(pid)?.comm))),
    );
    node.add(
        "cmdline",
        NodeKind::File,
        maybe_file(move || cmdline_of(pid)),
    );
    // The environment beside the command line, because they are one block on
    // one stack and a program that reads the second usually reads the first.
    node.add(
        "environ",
        NodeKind::File,
        maybe_file(move || environ_of(pid)),
    );
    node.add("status", NodeKind::File, maybe_file(move || status_of(pid)));
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
    node.add("oom_score_adj", NodeKind::File, oom_score_adj_ctl(pid));
    // The image and the working directory, as links: `readlink` is the only
    // way to ask for them, and their targets are facts about a process, not
    // bytes a file holds.
    node.add(
        "exe",
        NodeKind::Symlink,
        link(move || {
            let exe = task(pid)?.exe;
            if exe.is_empty() { None } else { Some(exe) }
        }),
    );
    node.add("cwd", NodeKind::Symlink, link(move || Some(task(pid)?.cwd)));
    node.add("root", NodeKind::Symlink, link(|| Some(String::from("/"))));

    // `/proc/<pid>/task`: one directory per thread, the leader among them.
    let tasks = ProcDir::new(Some(Arc::new(Threads { pid })));
    node.add("task", NodeKind::Dir, tasks);

    // `/proc/<pid>/fd`: one link per open descriptor, whose target is the
    // path it was opened by.
    let fds = ProcDir::new(Some(Arc::new(Descriptors { pid })));
    node.add("fd", NodeKind::Dir, fds);

    // `/proc/<pid>/ns`: one link per namespace the process is in, naming the
    // namespace the way `readlink` reports it and handing `setns` the object
    // when it is opened.
    let namespaces = ProcDir::new(Some(Arc::new(Namespaces { pid })));
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
fn namespace_of(pid: usize, kind: crate::vfs::ns::Kind) -> Option<crate::vfs::ns::Namespace> {
    crate::sched::ns_of(pid, kind)
}

#[cfg(not(target_os = "none"))]
fn namespace_of(_pid: usize, _kind: crate::vfs::ns::Kind) -> Option<crate::vfs::ns::Namespace> {
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
fn ns_link(pid: usize, kind: crate::vfs::ns::Kind) -> Arc<dyn Inode> {
    Arc::new(ProcLink {
        ino: next_ino(),
        target: Box::new(move || namespace_of(pid, kind).map(|ns| ns.link_text())),
        pid,
        fd: None,
        ns: Some((pid, kind)),
    })
}

/// The namespaces of one process, as `/proc/<pid>/ns` lists them.
///
/// The kinds this kernel has, and *only* those: a machine built without a
/// namespace has no entry for it either (a real kernel without `CONFIG_TIME_NS`
/// answers `ENOENT` for `/proc/self/ns/time`), so a reader sees two names and is
/// told nothing it cannot check. A name that is not one of them - `pid`, `net`,
/// `user`, `cgroup`, `time` - is `ENOENT` from the directory, and `setns` never
/// sees a descriptor for a namespace that does not exist.
struct Namespaces {
    pid: usize,
}

impl Dynamic for Namespaces {
    fn names(&self) -> Vec<(String, NodeKind)> {
        crate::vfs::ns::Kind::ALL
            .iter()
            .map(|kind| (String::from(kind.name()), NodeKind::Symlink))
            .collect()
    }

    fn node(&self, name: &str) -> Option<Arc<dyn Inode>> {
        let kind = crate::vfs::ns::Kind::of_name(name)?;
        Some(ns_link(self.pid, kind))
    }
}

/// The processes themselves, as `/proc` names them.
///
/// A name is looked up as a number and answered only when a process answers
/// to that number: `/proc` holds a great many names that are not pids
/// (`mounts`, `uptime`, `self`), and a file that claimed every number was a
/// process would turn `mounts` into a directory and break the first tool that
/// asked for it.
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
        // the reader's namespace does not contain is not a directory at all.
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
/// 256 bytes, the same as the `config` attribute in `/sys` and for the same
/// reason: mechanism 1's address register has eight bits of offset, and the
/// 4096-byte extended space is not reachable through it.
struct ProcConfig {
    ino: Ino,
    bus: u8,
    dev: u8,
    func: u8,
}

impl Inode for ProcConfig {
    fn metadata(&self) -> Metadata {
        Metadata {
            ino: self.ino,
            kind: NodeKind::File,
            size: crate::pci::CFG_SIZE,
        }
    }

    #[cfg(all(target_os = "none", mpos_pci_config))]
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> VfsResult<usize> {
        Ok(crate::drivers::pci::config_read(
            self.bus, self.dev, self.func, offset, buf,
        ))
    }

    #[cfg(all(target_os = "none", mpos_pci_config))]
    fn write_at(&self, offset: u64, buf: &[u8]) -> VfsResult<usize> {
        Ok(crate::drivers::pci::config_write(
            self.bus, self.dev, self.func, offset, buf,
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
        // read, and the thing `mountinfo` and `mount -a` stand on.
        self.root.add("mounts", NodeKind::File, file(mounts_text));
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
        self.root.add(
            "cpuinfo",
            NodeKind::File,
            file(|| {
                alloc::format!(
                    "processor\t: 0\ncpu family\t: x86_64\nmodel name\t: {} x86_64 processor\n\n",
                    env!("CARGO_PKG_NAME")
                )
            }),
        );
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
        self.root.add(
            "cmdline",
            NodeKind::File,
            file(|| alloc::format!("{}\n", crate::boot::cmdline())),
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
                    // per-node modes to enforce - so the capability the adjacent
                    // syscall checks is checked here too, and a non-root writer
                    // is refused by the door as well as by `sethostname(2)`.
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

        // `/proc/bus/pci`: pciutils' fallback access method, and the one
        // `setpci` writes through. It reads the same device snapshot the
        // sysfs tree is built from, so the two describe one machine.
        self.root.add("bus", NodeKind::Dir, {
            let bus = dir();
            bus.add("pci", NodeKind::Dir, pci_dir());
            bus
        });

        // `/proc/self`: whichever process is reading, which is not knowable
        // when the tree is built. The link's *reading* is short-circuited in
        // the readlink handler for the two names that need a shortcut -
        // `exe` and `fd/<n>` - but every other name has to resolve through
        // the process's own directory to be answered at all.
        let self_dir: Arc<dyn Inode> = Arc::new(MirrorDir {
            ino: next_ino(),
            resolve: Box::new(|| process_dir(current_pid())),
        });
        self.root.add("self", NodeKind::Dir, self_dir);
        // `/proc/thread-self` is the same idea for the calling *thread*, and
        // exists because a threaded program asking about itself gets a tid,
        // not a pid - `pthread_self` and `gettid` agree with each other and
        // with this, and not with `getpid`.
        let thread_self: Arc<dyn Inode> = Arc::new(MirrorDir {
            ino: next_ino(),
            resolve: Box::new(|| {
                let tid = current_tid();
                let pid = current_pid();
                Some(thread_dir(pid, tid))
            }),
        });
        self.root.add("thread-self", NodeKind::Dir, thread_self);
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
}
