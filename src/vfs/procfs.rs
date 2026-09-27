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

/// What a generated file reads back as: a function of the moment.
///
/// `None` is not "empty", it is "the thing this file reports no longer
/// exists" - a process that exited between the listing that named it and the
/// read that opened it. That is `ENOENT` and not a zero-byte file, because a
/// `ps` that read an empty `/proc/57/stat` would print a process with no name
/// and no parent rather than skipping one that has already gone.
type Generator = Box<dyn Fn() -> Option<String> + Send + Sync>;

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
    write: Box<dyn Fn(&str) -> VfsResult<()> + Send + Sync>,
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
    entries: Mutex<RefCell<BTreeMap<String, (Arc<dyn Inode>, NodeKind)>>>,
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

/// Every process id, in the order the task table holds them. Threads are left
/// out: a process *is* its leader's id, and listing a thread's tid beside it
/// would be two names for one process.
#[cfg(target_os = "none")]
fn process_ids() -> Vec<usize> {
    crate::sched::snapshots()
        .into_iter()
        .filter(|task| task.pid == task.tid)
        .map(|task| task.pid)
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
    let kb = task.rss_pages * 4096 / 1024;
    let fields: [alloc::borrow::Cow<'_, str>; 52] = [
        Cow::Owned(alloc::format!("{}", shown)),
        Cow::Owned(alloc::format!("({})", task.comm)),
        Cow::Owned(alloc::format!("{}", task.state)),
        Cow::Owned(alloc::format!("{}", task.ppid)),
        Cow::Owned(alloc::format!("{}", task.pgid)),
        // session: no `setsid` has ever been answered with a group of its
        // own, so the process group is the closest thing there is.
        Cow::Owned(alloc::format!("{}", task.pgid)),
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
        tgid = task.pid,
        pid = task.pid,
        ppid = task.ppid,
        pgid = task.pgid,
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
        ("Max open files", "files"),
        ("Max locked memory", "bytes"),
        ("Max processes", "processes"),
        ("Max pending signals", "signals"),
        ("Max locks", "locks"),
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
    node.add("status", NodeKind::File, maybe_file(move || status_of(tid)));
    node.add(
        "children",
        NodeKind::File,
        maybe_file(move || children_of(pid, tid)),
    );
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
            .map(|tid| (alloc::format!("{tid}"), NodeKind::Dir))
            .collect()
    }

    fn node(&self, name: &str) -> Option<Arc<dyn Inode>> {
        let tid = name.parse::<usize>().ok()?;
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
        let pid = name.parse::<usize>().ok()?;
        process_dir(pid)
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

/// Seconds since boot, split the way `/proc/uptime` spells them: whole seconds
/// and centiseconds. The kernel answers from its tick counter; the host (where
/// these generators are unit-tested) has no ticks, and reports zero.
#[cfg(target_os = "none")]
fn uptime() -> (u64, u64) {
    let ticks = crate::arch::timer::ticks();
    let hz = crate::arch::timer::TICK_HZ as u64;
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

/// `/proc/partitions`: the boot disk and its partitions, one per line.
///
/// The names and numbers come from [`crate::uevent::Numbering`] - the same
/// numbering `/dev` nodes are minted from and `/sys/class/block/*/dev` prints -
/// so the three cannot name the same partition differently.
///
/// Its existence is load-bearing in a way a missing file usually is not.
/// `fdisk -l` opens it before it does anything else, and an `ENOENT` there
/// leaves it holding a null `FILE *` that it passes straight to stdio: the
/// symptom is a page fault inside the libc's stream lock, on an address a few
/// hundred bytes into the program's own read-only image, which says nothing at
/// all about the missing file.
fn partitions() -> String {
    let mut out = String::from("major minor  #blocks  name\n\n");
    let Some(raw) = crate::blk::raw_disk() else {
        return out;
    };
    let table = crate::blk::part::partitions(raw.as_ref());
    for (name, major, minor, whole) in crate::uevent::Numbering::of_boot_disk().devices {
        let sectors = if whole {
            raw.capacity_sectors()
        } else {
            // Partitions take minors 1.. in table order, so the minor is the
            // index into the table plus one.
            table
                .get((minor as usize).saturating_sub(1))
                .map_or(0, |(_, sectors)| *sectors)
        };
        out.push_str(&partition_row(&name, major, minor, sectors));
    }
    out
}

// ---------------------------------------------------------------------------
// The name of this machine.
// ---------------------------------------------------------------------------

/// The system's hostname, as `sethostname` last set it and as
/// `/proc/sys/kernel/hostname` reports it.
///
/// It starts as the kernel's own name rather than empty, because the first
/// thing an init prints is usually a banner with the hostname in it and an
/// empty name there reads as a failure to look one up.
static HOSTNAME: Mutex<RefCell<String>> = Mutex::new(RefCell::new(String::new()));

/// The name this machine calls itself.
pub fn hostname() -> String {
    critical_section::with(|cs| {
        let name = HOSTNAME.borrow(cs).borrow();
        if name.is_empty() {
            String::from(DEFAULT_HOSTNAME)
        } else {
            name.clone()
        }
    })
}

/// Give the machine a name. `sethostname(2)` spells it through here, and so
/// does a write to `/proc/sys/kernel/hostname` - one fact, two doors, which
/// is why the name is kept here rather than in either caller.
pub fn set_hostname(name: &str) {
    critical_section::with(|cs| {
        *HOSTNAME.borrow(cs).borrow_mut() = String::from(name);
    });
}

/// The name before anyone sets one.
const DEFAULT_HOSTNAME: &str = "mpos";

/// The name this machine's network domain is, set through `setdomainname(2)`.
/// As with the hostname: one fact, one home, so a call that sets it and a
/// reader that reports it cannot disagree. The default is `(none)`, which is
/// what Linux's own `uname` reports for a machine nobody named.
static DOMAINNAME: Mutex<RefCell<String>> = Mutex::new(RefCell::new(String::new()));

/// The domain this machine belongs to, `(none)` until somebody names it.
pub fn domainname() -> String {
    critical_section::with(|cs| {
        let name = DOMAINNAME.borrow(cs).borrow();
        if name.is_empty() {
            String::from(DEFAULT_DOMAINNAME)
        } else {
            name.clone()
        }
    })
}

/// Name the domain. `setdomainname(2)` lands here.
pub fn set_domainname(name: &str) {
    critical_section::with(|cs| {
        *DOMAINNAME.borrow(cs).borrow_mut() = String::from(name);
    });
}

/// The domain before anyone sets one.
const DEFAULT_DOMAINNAME: &str = "(none)";

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
        self.root.add(
            "mounts",
            NodeKind::File,
            file(|| {
                let mut out = String::new();
                for (source, path, fstype, options) in super::mount::global().mount_table() {
                    // The fourth column is the mount options, and it is the one
                    // that matters: `ro` says a write here will be refused, and
                    // userland reads it to decide whether to remount.
                    out.push_str(&alloc::format!(
                        "{} {} {} {} 0 0\n",
                        if source.is_empty() { "none" } else { &source },
                        path,
                        fstype,
                        options
                    ));
                }
                out
            }),
        );
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
            ctl(|| String::from("(none)\n"), |_| Ok(())),
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
