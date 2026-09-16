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

use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::cell::RefCell;

use critical_section::Mutex;

use super::{FileSystem, Ino, Inode, Metadata, NodeKind, VfsError, VfsResult};

/// What a generated file reads back as: a function of the moment.
type Generator = Box<dyn Fn() -> String + Send + Sync>;

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
        let content = (self.generate)();
        Metadata {
            ino: self.ino,
            kind: NodeKind::File,
            size: content.len() as u64,
        }
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> VfsResult<usize> {
        let content = (self.generate)();
        let bytes = content.as_bytes();
        let start = offset.min(bytes.len() as u64) as usize;
        let end = (start + buf.len()).min(bytes.len());
        buf[..end - start].copy_from_slice(&bytes[start..end]);
        Ok(end - start)
    }

    // No `write_at`: the default answers `Unsupported`, and that is the
    // correct answer. These files report; they do not accept.
}

/// A directory of generated files and other directories, by name.
struct ProcDir {
    ino: Ino,
    entries: Mutex<RefCell<BTreeMap<String, Arc<dyn Inode>>>>,
}

impl ProcDir {
    fn add(&self, name: &str, node: Arc<dyn Inode>) {
        critical_section::with(|cs| {
            self.entries
                .borrow(cs)
                .borrow_mut()
                .insert(String::from(name), node);
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

/// The calling process's own executable path, and its pid. Both are questions
/// the scheduler answers on the kernel and that the host test build answers
/// with placeholders, because the tests exercise the *file shapes*.
#[cfg(target_os = "none")]
fn self_exe() -> String {
    crate::sched::current_exe()
}

#[cfg(not(target_os = "none"))]
fn self_exe() -> String {
    String::from("/sbin/init")
}

#[cfg(target_os = "none")]
fn self_pid() -> usize {
    crate::sched::current().0
}

#[cfg(not(target_os = "none"))]
fn self_pid() -> usize {
    1
}

/// The filesystem itself.
pub struct ProcFs {
    root: Arc<ProcDir>,
    next_ino: Mutex<RefCell<Ino>>,
}

impl ProcFs {
    pub fn new() -> Arc<ProcFs> {
        let fs = Arc::new(ProcFs {
            root: Arc::new(ProcDir {
                ino: 0,
                entries: Mutex::new(RefCell::new(BTreeMap::new())),
            }),
            next_ino: Mutex::new(RefCell::new(1)),
        });
        fs.build();
        fs
    }

    fn ino(&self) -> Ino {
        critical_section::with(|cs| {
            let next = self.next_ino.borrow(cs);
            let mut next = next.borrow_mut();
            let ino = *next;
            *next += 1;
            ino
        })
    }

    fn file<F: Fn() -> String + Send + Sync + 'static>(&self, generate: F) -> Arc<dyn Inode> {
        Arc::new(ProcFile {
            ino: self.ino(),
            generate: Box::new(generate),
        })
    }

    /// The tree. Each file's generator is written where its content is
    /// decided, so the file and the fact it reports stay next to each other.
    fn build(&self) {
        // What is mounted where: the only record of it that userland can
        // read, and the thing `mountinfo` and `mount -a` stand on.
        self.root.add(
            "mounts",
            self.file(|| {
                let mut out = String::new();
                for (source, path, fstype) in super::mount::global().mount_table() {
                    out.push_str(&alloc::format!(
                        "{} {} {} rw 0 0\n",
                        if source.is_empty() { "none" } else { &source },
                        path,
                        fstype
                    ));
                }
                out
            }),
        );
        self.root.add(
            "uptime",
            self.file(|| {
                let (secs, centis) = uptime();
                alloc::format!("{}.{} 0.0\n", secs, centis)
            }),
        );
        self.root.add(
            "meminfo",
            self.file(|| {
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
            self.file(|| {
                alloc::format!(
                    "processor\t: 0\ncpu family\t: x86_64\nmodel name\t: {} x86_64 processor\n\n",
                    env!("CARGO_PKG_NAME")
                )
            }),
        );
        self.root.add(
            "version",
            self.file(|| {
                alloc::format!(
                    "{} version {} \n",
                    env!("CARGO_PKG_NAME"),
                    env!("CARGO_PKG_VERSION")
                )
            }),
        );
        self.root.add("cmdline", self.file(|| String::from("\n")));
        self.root.add(
            "loadavg",
            self.file(|| String::from("0.00 0.00 0.00 1/1 1\n")),
        );
        self.root.add(
            "stat",
            self.file(|| {
                let (secs, _) = uptime();
                alloc::format!("cpu  {} 0 0 0 0 0 0 0 0 0\nintr 0\nctxt 0\n", secs)
            }),
        );

        // `/proc/self`: the calling process, resolved at read time. The `exe`
        // link's *reading* is short-circuited in the readlink handler, but the
        // name still has to resolve for a caller that stats it first.
        let self_dir = Arc::new(ProcDir {
            ino: self.ino(),
            entries: Mutex::new(RefCell::new(BTreeMap::new())),
        });
        self_dir.add(
            "cmdline",
            self.file(|| {
                let exe = self_exe();
                if exe.is_empty() {
                    String::from("\n")
                } else {
                    alloc::format!("{}\n", exe)
                }
            }),
        );
        self_dir.add("exe", self.file(self_exe));
        self_dir.add(
            "stat",
            self.file(|| {
                let exe = self_exe();
                let name = exe.rsplit('/').next().unwrap_or("unknown");
                alloc::format!(
                    "{} ({}) R 1 1 1 0 -1 0 0 0 0 0 0 0 0 0 0 0 1 0 0\n",
                    self_pid(),
                    name
                )
            }),
        );
        self.root.add("self", self_dir);
    }
}

impl FileSystem for ProcFs {
    fn root(&self) -> Arc<dyn Inode> {
        self.root.clone()
    }

    fn name(&self) -> &'static str {
        "proc"
    }
}
