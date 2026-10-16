//! Open file descriptions and the fd tables that name them.
//!
//! A [`File`] pairs an inode with a cursor. It is the thing fds name; several
//! fds can share one `File` (fork semantics, at M9), and several `File`s can
//! share one inode (independent cursors over the same bytes).
//!
//! An fd number only means something next to the table it indexes, and that
//! table belongs to a task: [`FdTable`] is one process's descriptors. A user
//! process is born with a table of its own holding the standard three, so what
//! it closes is its business alone; kernel tasks share one table, because they
//! are all the same caller and hand fds to each other freely.

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::cell::RefCell;
use core::sync::atomic::{AtomicU8, AtomicU64, Ordering};

use critical_section::Mutex;

use super::{Inode, Metadata, NodeKind, VfsError, VfsResult, resolve};

/// What an fd number is.
pub type RawFd = i32;

/// The `FD_CLOEXEC` flag bit: close this descriptor on `execve`.
pub const FD_CLOEXEC: u32 = 1;

/// The `O_NONBLOCK` open flag, as `fcntl(F_SETFL)` stores it: an operation
/// that would block answers `EAGAIN` instead. The only flag with teeth here -
/// a datagram socket that refuses to wait is exactly what a DNS resolver's
/// timeout loop is built on.
pub const O_NONBLOCK: u32 = 0o4000;

/// The `O_APPEND` open flag: every write lands at the end of the file,
/// whatever the description's cursor says.
///
/// It is the flag a log file is written with, and the reason it has to live on
/// the *description* rather than be folded into the open: the point is that
/// the position is re-read on every write, so a writer that has been running
/// since before somebody else appended to the file does not clobber their
/// bytes. `busybox syslogd` opens `/var/log/messages` with it, and a kernel
/// that ignores the flag writes the new line over the start of the old one -
/// which is what made the guest's log file a single line long for ever.
pub const O_APPEND: u32 = 0o2000;

/// The bits `fcntl(F_SETFL)` may replace - Linux's own `SETFL_MASK`, minus the
/// members this kernel does not implement (`O_DIRECT`, `O_NOATIME`): the two
/// flags the read and write paths consult, and the whole of what an open's flag
/// word keeps here.
///
/// The access mode is deliberately not a member. It is [`File::access`], the
/// one owner of that fact, and [`File::open_flags`] reports it *beside* this
/// word rather than inside it - which is what makes a caller's `F_SETFL` unable
/// to move it, as Linux's mask makes it unable to move `filp->f_flags`' mode.
/// The creation flags (`O_CREAT`, `O_EXCL`, `O_TRUNC`, `O_NOCTTY`) are not
/// members either: they are spent by the open itself, and Linux's `F_GETFL`
/// does not report them. Neither is `O_LARGEFILE`, which that kernel adds to
/// every path open and this one does not carry at all (`docs/uapi/` §5.2).
pub const SETFL_MASK: u32 = O_APPEND | O_NONBLOCK;

/// The access mode a description was made under, as the low two bits of
/// `O_ACCMODE`. `File::read`/`File::write` refuse a mismatch with `EBADF`, which
/// is Linux's answer for `write()` on an `O_RDONLY` descriptor and the reverse,
/// and it is half of the word `fcntl(F_GETFL)` reports - so this is the *one*
/// owner of the fact, and nothing keeps a second copy of it in the status word.
///
/// The default is [`ACC_RDWR`] - permissive - because only the doors that know
/// a direction record one: a path `open`, `pipe2`'s two ends, and a door that
/// mints a readable-only description (`inotify_init1`). Everything else Linux
/// itself mints read-write (a socket, an `eventfd`, a `timerfd`, a `signalfd`,
/// an `epoll`, a `memfd`) inherits the default, which is the answer the host
/// gives those descriptors too. A forgotten `set_access` therefore leaves the
/// description permissive rather than wrongly refusing.
pub const ACC_RDONLY: u32 = 0;
pub const ACC_WRONLY: u32 = 1;
pub const ACC_RDWR: u32 = 2;

/// An open file description: an inode plus where the next read/write lands.
pub struct File {
    inode: Arc<dyn Inode>,
    pos: AtomicU64,
    /// The open flags `fcntl(F_GETFL/F_SETFL)` report. Only [`O_NONBLOCK`]
    /// changes behavior; the rest are stored so a caller reads back what it
    /// wrote.
    status: core::sync::atomic::AtomicU32,
    /// Whether this description was opened for writing - the question
    /// `close(2)` answers with `IN_CLOSE_WRITE` rather than
    /// `IN_CLOSE_NOWRITE`, recorded once at open because the flags word a
    /// description keeps is the changeable `F_SETFL` half, not the access
    /// mode it was opened under.
    opened_for_write: core::sync::atomic::AtomicBool,
    /// The `O_ACCMODE` this description was opened under, for the read/write
    /// `EBADF` refusal and for the half of `fcntl(F_GETFL)` it answers.
    /// [`ACC_RDWR`] unless a path `open`, a pipe end or a readable-only door
    /// recorded a narrower mode; see the `ACC_*` constants.
    access: AtomicU8,
    /// The path this description was opened by, when the caller that made it
    /// knew one.
    ///
    /// A directory needs this: `fchdir` has to hand the new cwd a name that
    /// `getcwd` can read back, and a name is not something an inode carries -
    /// one directory answers to as many names as there are edges to it.
    /// Recording the name at open time is exact and costs one string;
    /// recovering it later means searching the whole tree for the inode, which
    /// is both slow and, on a path that cannot be reached from the root, has
    /// no answer at all.
    label: Mutex<RefCell<Option<alloc::string::String>>>,
    /// The device number this description lives on, recorded at open from the
    /// same path the label keeps. `DEV_UNSET` until then.
    ///
    /// `st_dev` is a property of the *file*, and the two doors that report it
    /// have to agree. `stat(path)` looks the answer up by path; a descriptor
    /// cannot, and an inode does not name its own filesystem, so the fallback
    /// ([`crate::vfs::mount::dev_of_inode`]) can only match a mount by its
    /// *root* inode - a plain file inside a mount therefore answered the root
    /// filesystem's number while `stat` on the same path answered the mount's.
    /// A caller that locks a file and then verifies the name still names the
    /// inode it locked (the standard way to close the lock-file race, and what
    /// `wayland-backend` does on `wayland-N.lock`) compares exactly these two
    /// answers, so a disagreement is not a wrong number but a retry that never
    /// converges. Recording the number where the path *is* known fixes it at
    /// the source. See `NOTES.md` §7.4.
    dev: core::sync::atomic::AtomicU64,
}

/// `File::dev` before an open has recorded one. `u64::MAX` is not a device
/// number this mount table ever issues, so it is a safe "nobody said".
const DEV_UNSET: u64 = u64::MAX;

impl File {
    /// A fresh description positioned at the start.
    pub fn new(inode: Arc<dyn Inode>) -> File {
        File {
            inode,
            pos: AtomicU64::new(0),
            opened_for_write: core::sync::atomic::AtomicBool::new(false),
            access: AtomicU8::new(ACC_RDWR as u8),
            status: core::sync::atomic::AtomicU32::new(0),
            label: Mutex::new(RefCell::new(None)),
            dev: core::sync::atomic::AtomicU64::new(DEV_UNSET),
        }
    }

    /// Record the filesystem this description was opened on.
    pub fn set_dev(&self, dev: u64) {
        self.dev.store(dev, core::sync::atomic::Ordering::Relaxed);
    }

    /// The filesystem id this description was opened on, if an open recorded
    /// one. `None` for a descriptor that no path made.
    pub fn dev(&self) -> Option<u64> {
        let dev = self.dev.load(core::sync::atomic::Ordering::Relaxed);
        (dev != DEV_UNSET).then_some(dev)
    }

    /// Record the path this description was opened by.
    pub fn set_label(&self, path: alloc::string::String) {
        critical_section::with(|cs| *self.label.borrow(cs).borrow_mut() = Some(path));
    }

    /// The path this description was opened by, if it was ever told.
    pub fn label(&self) -> Option<alloc::string::String> {
        critical_section::with(|cs| self.label.borrow(cs).borrow().clone())
    }

    /// The name `/proc/<pid>/fd` reports this descriptor by, which is also
    /// what `readlink` on one of those entries answers.
    ///
    /// The recorded label comes first: it is the path this very description
    /// was opened by, and the exact answer. Without one the tree is searched
    /// for the inode - a descriptor opened by another name, or made by `dup`
    /// over one that was - and failing that, Linux's spelling of the kind is
    /// the honest remainder: `pipe:[ino]` for a pipe, `socket:[ino]` for a
    /// socket, `anon:[ino]` for whatever else has no name anywhere.
    pub fn display_name(&self) -> alloc::string::String {
        if let Some(path) = self.label() {
            return path;
        }
        if let Some(path) = super::global().path_of(&self.inode) {
            return path;
        }
        let meta = self.inode.metadata();
        if self.inode.as_duplex().is_some() {
            alloc::format!("pipe:[{}]", meta.ino)
        } else if self.inode.socket_handle() != 0 {
            alloc::format!("socket:[{}]", meta.ino)
        } else {
            alloc::format!("anon:[{}]", meta.ino)
        }
    }

    pub fn metadata(&self) -> Metadata {
        self.inode.metadata()
    }

    pub fn inode(&self) -> Arc<dyn Inode> {
        self.inode.clone()
    }

    /// The identity of this open file description, as everything that
    /// arbitrates *per description* spells it.
    ///
    /// `flock` owns a whole-file lock by it, `EVIOCGRAB` claims a seat by it,
    /// and an evdev read has to know its own client by it, because those three
    /// questions have the same answer: one description is one reader, however
    /// many descriptors name it (`dup`, or an inherited fd - one address, one
    /// identity) and however few processes are involved (two `open`s are two
    /// addresses, two identities).
    ///
    /// It is the address of the `File` itself. Nothing dereferences it: it is
    /// a key, meaningful for exactly as long as the description is alive,
    /// which is what makes the address the right value to use.
    pub fn ofd(&self) -> u64 {
        self as *const File as u64
    }

    /// The open flag word, as `fcntl(F_GETFL)` reports it: the access mode the
    /// description was made under, OR'd with the changeable flags it carries.
    ///
    /// Linux reports `filp->f_flags`, and the access mode is the half a caller
    /// most often reads the word for - glibc's streams ask
    /// `fcntl(F_GETFL) & O_ACCMODE` to learn which way a stream is open. Its
    /// word also carries `O_LARGEFILE` and whatever else the open recorded
    /// (`O_DIRECTORY`, `O_NOFOLLOW`, `O_PATH`, `O_SYNC`...); those are named in
    /// `docs/uapi/` §5.2 rather than invented here, since this kernel neither
    /// implements them nor stores them.
    pub fn open_flags(&self) -> u32 {
        self.status() | self.access()
    }

    /// The changeable open flags the description carries - what
    /// `fcntl(F_SETFL)` and `ioctl(FIONBIO)` write, and the half of
    /// [`Self::open_flags`] that is theirs to change.
    pub fn status(&self) -> u32 {
        self.status.load(Ordering::Relaxed)
    }

    /// Replace the flags `fcntl(F_SETFL)` may change.
    ///
    /// Only [`SETFL_MASK`] is replaced, which is Linux's rule rather than a
    /// simplification of it: `filp->f_flags` is rewritten as
    /// `(arg & SETFL_MASK) | (old & ~SETFL_MASK)`, so a caller can neither
    /// change the access mode (the mask has no member for it) nor clear it.
    /// Measured on a current host: an `O_WRONLY` descriptor asked for
    /// `O_NONBLOCK` alone still reports `O_WRONLY|O_NONBLOCK` from `F_GETFL`,
    /// and one asked for `O_RDWR` or `O_RDONLY` reports `O_WRONLY` unchanged.
    /// A word-wide store instead reopens the description's direction to
    /// `O_RDONLY` for the reading, and `F_SETFL(O_RDWR)` reports a write the
    /// description is not allowed to do.
    pub fn set_status(&self, flags: u32) {
        self.status.store(flags & SETFL_MASK, Ordering::Relaxed);
    }

    pub fn pos(&self) -> u64 {
        self.pos.load(Ordering::Relaxed)
    }

    pub fn set_pos(&self, new_pos: u64) {
        self.pos.store(new_pos, Ordering::Relaxed);
    }

    pub fn read(&self, buf: &mut [u8]) -> VfsResult<usize> {
        // Wrong-mode first, before the empty-buffer short-circuit: Linux answers a
        // `read(2)` on an `O_WRONLY` descriptor with `EBADF` even for length 0.
        self.check_readable()?;
        // A zero-length read is `vfs_read`'s own short-circuit: the file is not
        // touched at all, and here that is a rule with a consequence rather
        // than a courtesy. A *message* - what a datagram socket holds - is
        // consumed by a receive, so passing an empty buffer down to the node
        // would eat a datagram and answer 0 for it. The `recv(2)` family is the
        // other half of that pair and *does* consume a message with no data
        // room (a zero-byte message is a message: eudev's worker channel is
        // nothing but zero-byte messages), which is why the short-circuit
        // belongs to this door and not to the socket.
        if buf.is_empty() {
            return Ok(0);
        }
        let pos = self.pos.load(Ordering::Relaxed);
        // The description is named, not just its position: a node whose answer
        // depends on *which* client is asking - an evdev node's is, because its
        // events belong to whoever holds `EVIOCGRAB` - cannot tell who that is
        // from an offset alone.
        let n = self.inode.read_at_client(pos, buf, self.ofd())?;
        self.pos.fetch_add(n as u64, Ordering::Relaxed);
        Ok(n)
    }

    /// Record the access mode the description was opened under, for the
    /// close-time `IN_CLOSE_WRITE`/`IN_CLOSE_NOWRITE` question.
    pub fn mark_opened_for(&self, write: bool) {
        self.opened_for_write
            .store(write, core::sync::atomic::Ordering::Relaxed);
    }

    /// Record the `O_ACCMODE` a path `open` - or a door that knows a direction
    /// itself, `pipe2`'s two ends and `inotify_init1` - made this description
    /// under, so the read/write entry points can refuse a mismatch with `EBADF`
    /// as Linux does and so `fcntl(F_GETFL)` reports the mode it was made with.
    /// The argument is the low two bits of the open flags (`0` read, `1` write,
    /// `2` read-write).
    pub fn set_access(&self, accmode: u32) {
        self.access.store((accmode & 0o3) as u8, Ordering::Relaxed);
    }

    /// The `O_ACCMODE` this description was made under - the write half of
    /// [`Self::open_flags`], and the question [`Self::may_write`] answers.
    pub fn access(&self) -> u32 {
        self.access.load(Ordering::Relaxed) as u32
    }

    /// The `read(2)` door refuses an `O_WRONLY` descriptor with `EBADF` - Linux's
    /// answer, and the check is here rather than in each handler so every read
    /// path (`read`/`readv`/`pread`/`preadv2`) shares it. A zero-length read still
    /// short-circuits first only when the mode permits reading at all; Linux
    /// refuses the wrong-mode `read(2, buf, 0)` with `EBADF` too.
    fn check_readable(&self) -> VfsResult<()> {
        if self.access() == ACC_WRONLY {
            return Err(VfsError::BadFd);
        }
        Ok(())
    }

    /// The `write(2)` door refuses an `O_RDONLY` descriptor with `EBADF`, the same
    /// per-description rule as [`Self::check_readable`], covering
    /// `write`/`writev`/`pwrite`/`pwritev2`/`splice`/`copy_file_range`.
    fn check_writable(&self) -> VfsResult<()> {
        if self.may_write() {
            Ok(())
        } else {
            Err(VfsError::BadFd)
        }
    }

    /// Whether this description may change the file's bytes at all - the fact
    /// behind [`Self::check_writable`], asked directly by the operations that
    /// write without being a `write(2)`: `ftruncate` and `fallocate`, which owe
    /// their own errno (`EINVAL` and `EBADF`) rather than the door's.
    ///
    /// Every "does this descriptor allow a write" question goes through here.
    /// Two spellings of the same rule is how a third mode - or a fourth
    /// write-shaped operation - comes to be enforced in one place and missed in
    /// the other.
    pub(crate) fn may_write(&self) -> bool {
        self.access() != ACC_RDONLY
    }

    /// Whether this description was opened for writing.
    pub fn opened_for_write(&self) -> bool {
        self.opened_for_write
            .load(core::sync::atomic::Ordering::Relaxed)
    }

    pub fn write(&self, buf: &[u8]) -> VfsResult<usize> {
        // Wrong-mode first, as for `read`: Linux answers a `write(2)` on an
        // `O_RDONLY` descriptor with `EBADF`, including for a zero-length write.
        self.check_writable()?;
        // Then the inode's own flags, which is Linux's `rw_verify_area` and the
        // reason the check is here rather than only at the open: an inode can
        // become immutable between the two, and the flag is a fact about the
        // *file*, not about the description. The two flags differ, and the
        // difference is the whole of what they mean: an immutable file refuses
        // the write, an append-only one refuses a write that is not going to its
        // end.
        crate::vfs::may_modify(&self.inode)?;
        let status = self.status.load(Ordering::Relaxed);
        if crate::vfs::is_append_only(&self.inode) && status & O_APPEND == 0 {
            return Err(VfsError::NotPermitted);
        }
        // `O_APPEND` decides the position here rather than at the open,
        // because that is what the flag promises: the end of the file *now*,
        // re-read on every write, so a description held open while somebody
        // else appends does not overwrite what they wrote. A regular file is
        // the only thing the flag means anything for - a pipe or a terminal
        // has no end to append at, and its offset is ignored anyway - so the
        // size is asked of the inode only for one.
        let pos = if status & O_APPEND != 0 && self.inode.metadata().kind == NodeKind::File {
            self.inode.metadata().size
        } else {
            self.pos.load(Ordering::Relaxed)
        };
        let n = self.inode.write_at_client(pos, buf, self.ofd())?;
        self.pos.fetch_add(n as u64, Ordering::Relaxed);
        // The one IN_MODIFY chokepoint of the userland write paths: every
        // write/writev/pwrite/splice/copy handler reaches the inode through
        // here, so the event is said once. Kernel-internal writers that must
        // not report (the rename copy) call `Inode::write_at` directly.
        if n > 0 {
            crate::vfs::inotify::emit_write(&self.inode);
        }
        Ok(n)
    }
}

/// One process's descriptors: one slot per fd, `None` is free, and slots are
/// reused lowest-first, which is what the fd contract promises callers may
/// assume.
pub struct FdTable {
    slots: Mutex<RefCell<Vec<Option<Arc<File>>>>>,
    /// Per-descriptor flags - the `FD_CLOEXEC` word `fcntl(F_SETFD)` stores.
    /// Indexed in lockstep with `slots`; a fresh description always starts
    /// with no flags, including one that lands on a reused or duplicated fd.
    fd_flags: Mutex<RefCell<Vec<u32>>>,
    /// The `RLIMIT_NOFILE` soft ceiling: no descriptor may be allocated at a
    /// number at or above it. Seeded from the default and lifted by
    /// `setrlimit`; inherited whole across `fork`.
    nofile: Mutex<RefCell<usize>>,
}

/// The descriptor allowance a process starts with, matching the default
/// `RLIMIT_NOFILE` the scheduler seeds each task with.
const DEFAULT_NOFILE: usize = 1024;

impl FdTable {
    pub fn new() -> FdTable {
        FdTable {
            slots: Mutex::new(RefCell::new(Vec::new())),
            fd_flags: Mutex::new(RefCell::new(Vec::new())),
            nofile: Mutex::new(RefCell::new(DEFAULT_NOFILE)),
        }
    }

    /// A table holding only the three descriptors every POSIX program assumes
    /// it was born with: one description of the console each for input,
    /// output and error.
    pub fn with_standard(tty: Arc<dyn Inode>) -> FdTable {
        let table = FdTable::new();
        table.push_standard(tty);
        table
    }

    /// Hand over every description the table holds, leaving it empty.
    ///
    /// This is what a process exit does with its descriptors. The table itself
    /// has to stay - a zombie still has a slot, and its pid still has to answer
    /// `wait` - but nothing the process had open may stay open: a pipe whose
    /// write end outlived its writer never reports end-of-file, and the reader
    /// at the other end waits forever.
    ///
    /// The descriptions are *returned*, not dropped. Dropping one can wake
    /// whoever was blocked on it, and the caller may be holding locks that wake
    /// would need; the returned vector is dropped by whoever is safe to do it.
    pub fn take_all(&self) -> alloc::vec::Vec<Arc<File>> {
        critical_section::with(|cs| {
            let files: alloc::vec::Vec<Arc<File>> = self
                .slots
                .borrow(cs)
                .borrow_mut()
                .drain(..)
                .flatten()
                .collect();
            self.fd_flags.borrow(cs).borrow_mut().clear();
            files
        })
    }

    /// Every open descriptor with its number, in number order: what
    /// `/proc/<pid>/fd` is a listing of.
    ///
    /// Only the open ones are listed - Linux's own rule. A closed slot keeps
    /// no name, and a listing that carried its number would point tools at an
    /// entry that answers `ENOENT` on the very next lookup. Each entry carries
    /// the number it actually occupies, so the numbers stay the process's own
    /// whether or not anything below has been closed.
    pub fn open_files(&self) -> alloc::vec::Vec<(RawFd, Arc<File>)> {
        critical_section::with(|cs| {
            self.slots
                .borrow(cs)
                .borrow()
                .iter()
                .enumerate()
                .filter_map(|(fd, slot)| slot.as_ref().map(|file| (fd as RawFd, file.clone())))
                .collect()
        })
    }

    /// Add fds 0, 1 and 2 over one inode. Without them the first `pipe()`
    /// would be handed fd 0.
    fn push_standard(&self, tty: Arc<dyn Inode>) {
        for _ in 0..3 {
            // The three standard descriptors are far below the ceiling the
            // table starts with, so allocating one cannot fail; say so.
            self.install(Arc::new(File::new(tty.clone())))
                .expect("standard descriptors fit under the default ceiling");
        }
    }

    /// A copy of this table: the same numbers naming the same open file
    /// descriptions, with the same per-descriptor flags.
    ///
    /// This is what `fork` gives a child, and the distinction it draws is the
    /// whole of POSIX's inheritance rule. The *descriptions* are shared - both
    /// processes read and write through one open file, sharing one file
    /// position - but the *numbering* is not: closing fd 3 in the parent must
    /// not close it in the child, and a `dup2` on one side must not move the
    /// other. Handing the child the same `Arc<FdTable>` would make all of that
    /// leak across, and a shell's pipeline - which is nothing but `dup2` and
    /// `close` on both sides of a fork - would close the parent's own pipe ends
    /// out from under it.
    pub fn duplicate(&self) -> FdTable {
        let copy = FdTable::new();
        critical_section::with(|cs| {
            let slots = self.slots.borrow(cs).borrow();
            let flags = self.fd_flags.borrow(cs).borrow();
            let nofile = *self.nofile.borrow(cs).borrow();
            *copy.slots.borrow(cs).borrow_mut() = slots.clone();
            *copy.fd_flags.borrow(cs).borrow_mut() = flags.clone();
            *copy.nofile.borrow(cs).borrow_mut() = nofile;
        });
        copy
    }

    /// Raise or lower the descriptor ceiling - what `setrlimit(RLIMIT_NOFILE)`
    /// does to the process's table. A value past `usize`'s range (an
    /// `RLIM_INFINITY` of `u64::MAX`) is taken as "no practical cap".
    pub fn set_nofile(&self, n: u64) {
        critical_section::with(|cs| {
            *self.nofile.borrow(cs).borrow_mut() = n as usize;
        });
    }

    /// The current ceiling, for tests and `/proc`.
    pub fn nofile(&self) -> usize {
        critical_section::with(|cs| *self.nofile.borrow(cs).borrow())
    }

    pub fn install(&self, file: Arc<File>) -> VfsResult<RawFd> {
        critical_section::with(|cs| {
            let mut slots = self.slots.borrow(cs).borrow_mut();
            let mut flags = self.fd_flags.borrow(cs).borrow_mut();
            let cap = *self.nofile.borrow(cs).borrow();
            let slot = match slots.iter().position(Option::is_none) {
                Some(slot) => slot,
                None => slots.len(),
            };
            // A new descriptor at or past the ceiling is EMFILE, not growth -
            // this is the line `RLIMIT_NOFILE` draws, and the reason the table
            // no longer grows without bound.
            if slot >= cap {
                return Err(VfsError::TooManyDescriptors);
            }
            if slot == slots.len() {
                slots.push(Some(file));
                flags.push(0);
            } else {
                slots[slot] = Some(file);
                flags[slot] = 0;
            }
            Ok(slot as RawFd)
        })
    }

    /// [`install`](Self::install) with a floor: the descriptor handed back is
    /// the lowest free one at or above `at`, which is what `fcntl(F_DUPFD)`
    /// promises. A free slot below the floor is left alone - the caller asked
    /// for a number it can keep out of the way of something else.
    pub fn install_at_least(&self, file: Arc<File>, at: RawFd) -> VfsResult<RawFd> {
        critical_section::with(|cs| {
            let mut slots = self.slots.borrow(cs).borrow_mut();
            let mut flags = self.fd_flags.borrow(cs).borrow_mut();
            let cap = *self.nofile.borrow(cs).borrow();
            let at = (at.max(0)) as usize;
            let slot = (at..slots.len())
                .find(|&slot| slots[slot].is_none())
                .unwrap_or(slots.len());
            if slot >= cap {
                return Err(VfsError::TooManyDescriptors);
            }
            if slot == slots.len() {
                slots.push(Some(file));
                flags.push(0);
            } else {
                slots[slot] = Some(file);
                flags[slot] = 0;
            }
            Ok(slot as RawFd)
        })
    }

    /// The description `fd` names. Cloned out rather than borrowed: a read can
    /// block for a line, and no lock may be held while it does.
    fn get(&self, fd: RawFd) -> VfsResult<Arc<File>> {
        critical_section::with(|cs| {
            self.slots
                .borrow(cs)
                .borrow()
                .get(usize::try_from(fd).unwrap_or(usize::MAX))
                .cloned()
                .flatten()
                .ok_or(VfsError::BadFd)
        })
    }

    /// Release an fd. The description dies when the last fd naming it does.
    pub fn close(&self, fd: RawFd) -> VfsResult<()> {
        critical_section::with(|cs| {
            let mut slots = self.slots.borrow(cs).borrow_mut();
            let slot = usize::try_from(fd).map_err(|_| VfsError::BadFd)?;
            slots
                .get_mut(slot)
                .filter(|entry| entry.is_some())
                .ok_or(VfsError::BadFd)?
                .take();
            Ok(())
        })
    }

    /// Make `new` name what `old` names, closing whatever `new` named first.
    /// Equal numbers are a successful no-op, which is how POSIX spells a
    /// duplicate that is already where it was asked to go.
    pub fn dup2(&self, old: RawFd, new: RawFd) -> VfsResult<RawFd> {
        let file = self.get(old)?;
        self.install_at(file, new)?;
        Ok(new)
    }

    /// Put `file` at `new` in this table, dropping whatever was there.
    ///
    /// The cross-table half of [`dup2`](Self::dup2), and the whole of what
    /// starting a process with a redirected descriptor needs: a child's table
    /// is built fresh, and the descriptions it should hold live in whoever is
    /// starting it.
    pub fn install_at(&self, file: Arc<File>, new: RawFd) -> VfsResult<()> {
        let slot = usize::try_from(new).map_err(|_| VfsError::BadFd)?;
        critical_section::with(|cs| {
            let mut slots = self.slots.borrow(cs).borrow_mut();
            let mut flags = self.fd_flags.borrow(cs).borrow_mut();
            while slots.len() <= slot {
                slots.push(None);
                flags.push(0);
            }
            slots[slot] = Some(file);
            flags[slot] = 0;
        });
        Ok(())
    }

    /// The description `fd` names, for handing to another table.
    pub fn clone_at(&self, fd: RawFd) -> VfsResult<Arc<File>> {
        self.get(fd)
    }

    /// The flag word `fd` carries - what `fcntl(F_GETFD)` reads back.
    pub fn fd_flags_get(&self, fd: RawFd) -> VfsResult<u32> {
        self.get(fd)?;
        Ok(critical_section::with(|cs| {
            self.fd_flags
                .borrow(cs)
                .borrow()
                .get(usize::try_from(fd).unwrap_or(usize::MAX))
                .copied()
                .unwrap_or(0)
        }))
    }

    /// Store `fd`'s flag word - what `fcntl(F_SETFD)` writes.
    pub fn fd_flags_set(&self, fd: RawFd, flags: u32) -> VfsResult<()> {
        self.get(fd)?;
        critical_section::with(|cs| {
            let slot = usize::try_from(fd).map_err(|_| VfsError::BadFd)?;
            let mut flag_words = self.fd_flags.borrow(cs).borrow_mut();
            while flag_words.len() <= slot {
                flag_words.push(0);
            }
            flag_words[slot] = flags;
            Ok(())
        })
    }
}

impl Default for FdTable {
    fn default() -> FdTable {
        FdTable::new()
    }
}

/// The table every kernel task shares. Kernel code has no process identity,
/// and one task handing an fd to another - a pipe end, in m11 - only works if
/// the numbers mean the same thing on both sides.
static KERNEL_FDS: Mutex<RefCell<Option<Arc<FdTable>>>> = Mutex::new(RefCell::new(None));

pub fn kernel_fds() -> Arc<FdTable> {
    critical_section::with(|cs| {
        KERNEL_FDS
            .borrow(cs)
            .borrow_mut()
            .get_or_insert_with(|| Arc::new(FdTable::new()))
            .clone()
    })
}

/// The fd table of whoever is calling. On the machine that is the running
/// task's; on the host, where there are no tasks, it is the kernel's.
#[cfg(target_os = "none")]
fn current_table() -> Arc<FdTable> {
    crate::sched::current_fds()
}

#[cfg(not(target_os = "none"))]
fn current_table() -> Arc<FdTable> {
    kernel_fds()
}

/// Give the kernel's own table the standard descriptors: three open
/// descriptions of the console tty, one cursor each. Call once, at start-up,
/// before anything opens a file.
pub fn init_standard(tty: Arc<dyn Inode>) {
    kernel_fds().push_standard(tty);
}

/// Resolve `path` and open a fresh description on it.
pub fn open(path: &str) -> VfsResult<RawFd> {
    let inode = resolve(path)?;
    current_table().install(Arc::new(File::new(inode)))
}

/// [`open`](fn.open.html) with a caller-supplied start for relative paths.
/// `None` is the root, which is what every kernel caller means.
pub fn open_opt(path: &str, start: Option<Arc<dyn Inode>>) -> VfsResult<RawFd> {
    let inode = match start {
        Some(dir) => super::resolve_at(path, dir)?,
        None => resolve(path)?,
    };
    // A name that is a descriptor's second name opens the descriptor, not a
    // file: `/dev/stdout` is fd 1 with its position and its flags, and a
    // fresh description of a link node would be neither - it would be an
    // inode with no read and write, and the first write to it would fail.
    if let Some(fd) = inode.as_fd() {
        let file = current_table().clone_at(fd as RawFd)?;
        return current_table().install(file);
    }
    // A name can also be a door rather than a thing: opening `/dev/ptmx`
    // mints a new pty pair and hands back that pair's master, and opening a
    // locked pty's slave refuses. The node itself says which it is.
    let inode = match inode.open_replacement()? {
        Some(replacement) => replacement,
        None => inode,
    };
    current_table().install(Arc::new(File::new(inode)))
}

/// Call `tell` once for each description in `files` that the batch holds every
/// reference to, naming it by its [`File::ofd`].
///
/// This is the exit path's question - "which of these descriptions is really
/// gone?" - and it is a question about *references*, not descriptors: a `dup`
/// that outlives the batch is the same reader, and a description several of the
/// batch's slots name is one description. `inotify`'s close events, `flock`'s
/// owners and the input layer's per-client state all have to answer it the same
/// way, so the rule is written once, here, next to the type it is about.
pub fn for_each_last(files: &[Arc<File>], mut tell: impl FnMut(&Arc<File>, u64)) {
    let mut seen: Vec<u64> = Vec::new();
    for file in files {
        let ofd = file.ofd();
        if seen.contains(&ofd) {
            continue;
        }
        let total = Arc::strong_count(file);
        let in_batch = files.iter().filter(|other| other.ofd() == ofd).count();
        if total != in_batch {
            continue;
        }
        seen.push(ofd);
        tell(file, ofd);
    }
}

/// Open a fresh description on an inode already in hand - a pipe end, whose
/// channel is anonymous and has no name to resolve.
pub fn open_inode(inode: Arc<dyn Inode>) -> VfsResult<RawFd> {
    current_table().install(Arc::new(File::new(inode)))
}

/// Read at the description's cursor, advancing it.
pub fn read(fd: RawFd, buf: &mut [u8]) -> VfsResult<usize> {
    current_table().get(fd)?.read(buf)
}

/// Write at the description's cursor, advancing it.
pub fn write(fd: RawFd, buf: &[u8]) -> VfsResult<usize> {
    current_table().get(fd)?.write(buf)
}

/// Release an fd, in the caller's own table.
pub fn close(fd: RawFd) -> VfsResult<()> {
    current_table().close(fd)
}

/// Duplicate one descriptor onto another, in the caller's own table. Returns
/// `new`, so the caller can use the result without a second look.
pub fn dup2(old: RawFd, new: RawFd) -> VfsResult<RawFd> {
    current_table().dup2(old, new)
}

#[cfg(all(test, not(target_os = "none")))]
mod tests {
    use alloc::sync::Arc;

    use super::{FdTable, File, close, open, read, write};
    use crate::vfs::ramfs::RamFs;
    use crate::vfs::{FileSystem, NodeKind, VfsError};

    #[test]
    fn a_file_cursor_walks_what_it_writes_then_hits_eof() {
        let inode = RamFs::new().root().create("f", NodeKind::File).unwrap();
        let file = File::new(inode.clone());

        assert_eq!(file.write(b"hello").unwrap(), 5);
        assert_eq!(file.write(b" world").unwrap(), 6);
        assert_eq!(file.metadata().size, 11);

        // One cursor per description, shared by reads and writes: the writer's
        // own cursor sits at the end now, so its next read is EOF, while a
        // fresh description starts at the beginning.
        let mut buf = [0u8; 16];
        let back = File::new(inode);
        assert_eq!(back.read(&mut buf).unwrap(), 11);
        assert_eq!(&buf[..11], b"hello world");
        assert_eq!(file.read(&mut buf).unwrap(), 0);
    }

    /// `O_APPEND` lands every write at the end of the file, and it is the
    /// *description's* flag, not a cursor trick: a description that has never
    /// written - whose cursor still reads 0 - appends where the file ends.
    /// That is the case `busybox syslogd` is: it reopens `/var/log/messages`
    /// on every write, so a kernel that wrote at the cursor put each new line
    /// over the start of the previous one.
    #[test]
    fn a_description_opened_for_append_writes_at_the_end() {
        let inode = RamFs::new().root().create("f", NodeKind::File).unwrap();
        let first = File::new(inode.clone());
        assert_eq!(first.write(b"one\n").unwrap(), 4);

        // A fresh description, as the second `open` of a log file makes: its
        // cursor is 0, and with `O_APPEND` set the write must not use it.
        let appending = File::new(inode.clone());
        appending.set_status(super::O_APPEND);
        assert_eq!(appending.write(b"two\n").unwrap(), 4);

        let mut buf = [0u8; 16];
        assert_eq!(File::new(inode).read(&mut buf).unwrap(), 8);
        assert_eq!(&buf[..8], b"one\ntwo\n");

        // Without the flag the same fresh description writes at its cursor,
        // which is the behaviour the flag exists to change.
        let overwriting = File::new(RamFs::new().root().create("g", NodeKind::File).unwrap());
        overwriting.write(b"abcdef").unwrap();
        let plain = File::new(overwriting.inode());
        plain.write(b"XY").unwrap();
        let mut buf = [0u8; 8];
        let n = File::new(plain.inode()).read(&mut buf).unwrap();
        assert_eq!(&buf[..n], b"XYcdef");
    }

    /// The flag bits the description keeps are the ones the headers spell:
    /// `O_APPEND` is `0o2000`, `O_NONBLOCK` is `0o4000`, and the access mode is
    /// *not* one of them - it lives in [`File::access`], and the reported word
    /// is composed from both.
    #[test]
    fn the_status_flag_bits_match_the_headers() {
        assert_eq!(super::O_APPEND, 0o2000);
        assert_eq!(super::O_NONBLOCK, 0o4000);
        assert_eq!(super::SETFL_MASK, 0o2000 | 0o4000);
        assert_eq!(super::ACC_RDONLY, 0);
        assert_eq!(super::ACC_WRONLY, 1);
        assert_eq!(super::ACC_RDWR, 2);
    }
    #[test]
    fn descriptions_over_one_inode_read_independently() {
        let inode = RamFs::new().root().create("f", NodeKind::File).unwrap();
        let writer = File::new(inode.clone());
        let reader = File::new(inode);
        assert_eq!(writer.write(b"abcd").unwrap(), 4);

        let mut buf = [0u8; 2];
        assert_eq!(reader.read(&mut buf).unwrap(), 2);
        assert_eq!(&buf, b"ab");
        // The writer's cursor moved with its writes and sits at EOF; the
        // reader's moved only with its own reads.
        assert_eq!(writer.read(&mut buf).unwrap(), 0);
        assert_eq!(reader.read(&mut buf).unwrap(), 2);
        assert_eq!(&buf, b"cd");
    }

    #[test]
    fn the_fd_table_reuses_the_lowest_free_slot_and_reports_bad_fds() {
        // The one test here that uses the global tree and table; the mount is
        // first-come and nothing else in this module competes for either.
        crate::vfs::mount_root(RamFs::new());
        crate::vfs::resolve("/")
            .unwrap()
            .create("f", NodeKind::File)
            .unwrap();

        let fd = open("/f").unwrap();
        assert_eq!(fd, 0);
        assert_eq!(write(fd, b"hi").unwrap(), 2);
        close(fd).unwrap();

        // The freed slot is the lowest one, so the next open takes it.
        assert_eq!(open("/f").unwrap(), fd);
        let mut buf = [0u8; 2];
        assert_eq!(read(fd, &mut buf).unwrap(), 2);
        assert_eq!(&buf, b"hi");

        close(fd).unwrap();
        assert_eq!(close(fd).unwrap_err(), VfsError::BadFd);
        assert_eq!(read(fd, &mut buf).unwrap_err(), VfsError::BadFd);
        assert_eq!(write(-1, b"x").unwrap_err(), VfsError::BadFd);
        assert_eq!(open("/nope").unwrap_err(), VfsError::NoEntry);
    }

    fn file_on(name: &str) -> Arc<File> {
        let inode = RamFs::new().root().create(name, NodeKind::File).unwrap();
        Arc::new(File::new(inode))
    }

    #[test]
    fn numbers_mean_nothing_without_the_table_they_index() {
        let a = FdTable::new();
        let b = FdTable::new();
        let fd = a.install(file_on("f")).unwrap();

        // The same number in another process's table names nothing at all.
        assert_eq!(b.get(fd).err(), Some(VfsError::BadFd));
        b.close(fd).unwrap_err();
    }

    #[test]
    fn the_descriptor_table_honours_its_ceiling() {
        let table = FdTable::new();
        table.set_nofile(4);
        // Slots 0..3 all sit below a ceiling of 4.
        for expected in 0..4i32 {
            assert_eq!(table.install(file_on("f")).unwrap(), expected);
        }
        // The fifth would land on number 4, at the ceiling: EMFILE, and no
        // growth - the table no longer extends past RLIMIT_NOFILE.
        assert_eq!(
            table.install(file_on("f")).err(),
            Some(VfsError::TooManyDescriptors)
        );
        // Closing one frees a slot below the ceiling, and reuse fits again.
        table.close(1).unwrap();
        assert_eq!(table.install(file_on("f")).unwrap(), 1);
    }

    #[test]
    fn a_fresh_process_starts_with_the_standard_three() {
        let table = FdTable::with_standard(RamFs::new().root());
        for fd in 0..3 {
            table.get(fd).expect("every process is born with fd {fd}");
        }
        assert_eq!(table.get(3).err(), Some(VfsError::BadFd));
        // Whatever it opens next lands past them: without the standard three
        // the first pipe would be handed fd 0.
        assert_eq!(table.install(file_on("f")).unwrap(), 3);
    }

    #[test]
    fn dup2_points_a_second_number_at_the_same_description() {
        let table = FdTable::new();
        let fd = table.install(file_on("f")).unwrap();

        assert_eq!(table.dup2(fd, 2).unwrap(), 2);
        assert!(Arc::ptr_eq(&table.get(fd).unwrap(), &table.get(2).unwrap()));

        // The two numbers are independent: closing one leaves the other.
        table.close(fd).unwrap();
        assert_eq!(table.get(fd).err(), Some(VfsError::BadFd));
        assert!(table.get(2).is_ok());

        // Duplicating onto itself is the no-op POSIX says it is, and a number
        // that names nothing cannot be duplicated from.
        assert_eq!(table.dup2(2, 2).unwrap(), 2);
        assert!(table.get(2).is_ok());
        assert_eq!(table.dup2(7, 1).err(), Some(VfsError::BadFd));
        assert_eq!(table.get(1).err(), Some(VfsError::BadFd));
    }

    /// The access mode is enforced after the open, as Linux does: `write()` on an
    /// `O_RDONLY` description and `read()` on an `O_WRONLY` one answer `EBADF`.
    /// Before this, `File::read`/`write` never consulted the mode, so an `O_RDONLY`
    /// description happily took a write - the silent-success this pins against.
    /// The default (no `set_access`) is read-write, which is what keeps a
    /// description whose direction Linux itself mints as read-write - a socket,
    /// an `eventfd`, a `timerfd` - permissive, and is the answer the host gives
    /// those descriptors too.
    #[test]
    fn access_mode_is_enforced_after_open() {
        let inode = RamFs::new().root().create("f", NodeKind::File).unwrap();

        // A fresh description is permissive: this is the anonymous-object case.
        let both = File::new(inode.clone());
        assert_eq!(both.write(b"hi").unwrap(), 2);

        // Opened read-only: the write is refused before anything is touched, and
        // even a zero-length write cannot sneak past the mode check.
        let rd = File::new(inode.clone());
        rd.set_access(super::ACC_RDONLY);
        assert_eq!(rd.write(b"x").err(), Some(VfsError::BadFd));
        assert_eq!(rd.write(b"").err(), Some(VfsError::BadFd));
        assert!(!rd.may_write(), "and the fact is one question, not two");
        let mut buf = [0u8; 8];
        assert_eq!(rd.read(&mut buf).unwrap(), 2, "the read still works");

        // Opened write-only: the read is refused, the write is not.
        let wr = File::new(inode);
        wr.set_access(super::ACC_WRONLY);
        assert_eq!(wr.read(&mut buf).err(), Some(VfsError::BadFd));
        assert_eq!(wr.write(b"y").unwrap(), 1);
        assert!(wr.may_write(), "write-only may still write");
    }

    /// `F_GETFL` reports the access mode the description was made under, and
    /// `F_SETFL` cannot move it: Linux rewrites `filp->f_flags` as
    /// `(arg & SETFL_MASK) | (old & ~SETFL_MASK)`, and `SETFL_MASK` has no
    /// member for the access mode, so a caller that asks for `O_NONBLOCK` alone
    /// keeps the direction it opened with. Measured on a current host: an
    /// `O_WRONLY` descriptor asked for `O_NONBLOCK` reports `1|0o4000`, and one
    /// asked for `O_RDWR` still reports `1`.
    #[test]
    fn the_reported_word_keeps_the_access_mode_setfl_cannot_change() {
        let inode = RamFs::new().root().create("f", NodeKind::File).unwrap();
        let file = File::new(inode);
        assert_eq!(
            file.open_flags(),
            super::ACC_RDWR,
            "a description no door gave a direction to is read-write"
        );

        file.set_access(super::ACC_WRONLY);
        assert_eq!(file.open_flags(), super::ACC_WRONLY);

        file.set_status(super::O_NONBLOCK);
        assert_eq!(file.open_flags(), super::ACC_WRONLY | super::O_NONBLOCK);

        // A word that names another access mode cannot move the one the
        // description has, and cannot clear it either.
        file.set_status(super::ACC_RDWR);
        assert_eq!(file.open_flags(), super::ACC_WRONLY);
        file.set_status(super::O_APPEND);
        assert_eq!(
            file.open_flags(),
            super::ACC_WRONLY | super::O_APPEND,
            "the one bit replaced was O_NONBLOCK, and O_APPEND arrived"
        );

        // The other direction, and it is a replacement rather than a merge:
        // the open-only flags (`O_CREAT|O_EXCL|O_NOCTTY|O_TRUNC`, `0o1700`)
        // were spent by the open and are not file status, so a word that
        // carries them and nothing else clears `O_APPEND` - measured, a host
        // `F_SETFL(0o1700)` on an `O_WRONLY|O_APPEND` descriptor answers
        // `O_WRONLY` - while the access mode, which the caller cannot name,
        // is untouched.
        file.set_status(0o1700);
        assert_eq!(file.open_flags(), super::ACC_WRONLY);
    }
}
