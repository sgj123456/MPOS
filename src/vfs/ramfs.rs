//! An in-memory filesystem: the reference implementation of [`Inode`], the
//! substrate for the M8 initramfs, and the shape ext2 will be compared
//! against.
//!
//! Each node owns its interior mutability as `critical_section::Mutex` over a
//! `RefCell`, which is all the exclusion a preemptible single core needs and
//! the same arrangement the frame allocator uses. Inode numbers come from a
//! global counter: nothing consumes them across filesystems yet, and ext2 will
//! report its on-disk numbers regardless.

use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use core::cell::{Cell, RefCell};
use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

/// The twelve bits a mode word may hold. A filesystem that stores modes
/// stores *only* these - the file-type half belongs to `Metadata::kind`,
/// and `abi::mode_of` re-attaches it for `stat` - which is why "this node
/// keeps no mode" (`None`) and "this node is `chmod 000`" (`Some(0)`) are
/// different answers a permission check cannot tell apart by number alone.
const MODE_BITS: u32 = 0o7777;

/// The mode a node is born with, from the kind's default minus the creator's
/// mask - Linux's `init_user_ns` umask for a kernel creation, the caller's
/// `umask(2)` for a syscall one. `None` is the answer for a node that carries
/// no metadata at all (a device placeholder), which is not the same thing as
/// a stored zero.
///
/// A directory starts from *all* the permission bits (`S_IRWXUGO`, i.e.
/// `0777`) rather than the file's `0644`: that is what `mkdir(2)` and
/// `do_mkdir` hand to `vfs_mkmode` before the mask, and it is why `/tmp` -
/// made by an initramfs `mkdir -p`, or by this kernel's own mount plumbing -
/// is `0755` on a real system and traversable by everyone. Spelling the
/// directory half as `0755 & ~umask` instead would have made every directory
/// this filesystem creates unreadable below its owner's group, which is the
/// shape `t-unix`'s bind under `/tmp/…` caught: the walk refused to search
/// `/tmp` before it ever reached the plain file it was meant to report.
fn birth_mode(kind: NodeKind) -> Option<u32> {
    let base = match kind {
        NodeKind::CharDev | NodeKind::BlockDev => return None,
        NodeKind::Dir | NodeKind::Symlink | NodeKind::Socket => 0o777,
        _ => 0o644,
    };
    Some(base & !crate::sched::current_umask() & MODE_BITS)
}

use critical_section::Mutex;

use super::{FileSystem, Ino, Inode, Metadata, NodeKind, VfsError, VfsResult};

static NEXT_INO: AtomicU64 = AtomicU64::new(1);

fn fresh_ino() -> Ino {
    NEXT_INO.fetch_add(1, Ordering::Relaxed)
}

/// The wall clock in unix seconds, the same sum `gettimeofday` reports. A
/// memory filesystem that stamps nothing makes every file it holds look
/// older than 1970-adjacent files on disk: OpenRC read its freshly-written
/// `/run/openrc/deptree` as epoch, declared clock skew against the real
/// mtimes of `/etc/init.d`, and warned on every boot. On the host there is
/// no clock, and the tests want one that moves - see the ext2 driver's
/// equivalent.
fn now() -> u64 {
    crate::vfs::now_seconds()
}

/// Whether the kernel can take `need` more bytes for a file and still keep the
/// machine running. On the host there is no frame allocator and nothing is ever
/// at risk, so the answer is always yes.
#[cfg(target_os = "none")]
fn machine_has_room(need: u64) -> bool {
    // What the caller could actually be given, which is more than the frames
    // still sitting untouched: the heap keeps what it has already taken, so the
    // bytes a shrink just gave back are in its free lists and perfectly
    // reusable, while `free` never rises again. Counting only frames made this
    // refuse a write that should have fit, after a file had just handed the
    // space back.
    let free = crate::mm::frame_stats().free as u64 * 4096;
    let cached = crate::mm::heap_stats().cached as u64;
    // Twice what is asked for: once to hold it, once to leave behind. A write
    // that would take the last of the memory is refused a moment early, which
    // is the correct direction for an error a program can print.
    free + cached >= need.saturating_mul(2)
}

#[cfg(not(target_os = "none"))]
fn machine_has_room(_need: u64) -> bool {
    true
}

/// One mount's room, and what it is holding.
///
/// The reason this exists at all is that ramfs bytes come out of the *kernel's*
/// memory, so a process that fills `/tmp` was not merely using its own
/// allocation - it was asking the kernel heap until the heap said no, and the
/// answer to that was a panic and a dead machine (`dd` of 700 MB was enough).
/// Charging the mount *before* allocating is what turns that into a number a
/// shell prints.
///
/// Linux's tmpfs defaults to half of physical memory; this takes a quarter,
/// because unlike Linux there is no swap and no separate page cache budget - the
/// bytes charged here are kernel heap that the rest of the machine still needs
/// to run in.
pub struct Usage {
    used: AtomicU64,
    limit: u64,
}

impl Usage {
    /// A mount with room for a quarter of the machine's physical memory.
    #[cfg(target_os = "none")]
    pub fn new() -> Arc<Usage> {
        let frames = critical_section::with(|cs| crate::mm::PMM.borrow(cs).borrow().total_frames());
        Usage::with_limit((frames as u64) * 4096 / 4)
    }

    #[cfg(not(target_os = "none"))]
    pub fn new() -> Arc<Usage> {
        // No frame allocator on the host; tests that care name their own limit.
        Usage::with_limit(64 * 1024 * 1024)
    }

    pub fn with_limit(limit: u64) -> Arc<Usage> {
        Arc::new(Usage {
            used: AtomicU64::new(0),
            limit,
        })
    }

    /// Take `need` more bytes of room. `false` is the mount full - the answer
    /// that becomes `ENOSPC`, and the one that keeps the caller's write from
    /// reaching an allocator that has nothing left to give it.
    ///
    /// The mount's own budget is only half of that question. These bytes come
    /// from the kernel heap, which grows out of the *frame* allocator as it is
    /// asked, so a mount could be inside its limit and still be the thing that
    /// empties the machine: sizing the limit against RAM is a promise about one
    /// filesystem, and the panic was about the whole kernel. So a charge that
    /// passes the budget still has to leave the machine somewhere to allocate -
    /// here, half again as much free memory as it is about to take, which covers
    /// both the growth itself and the copy a `Vec` reallocation needs while it
    /// holds the old one.
    fn charge(&self, need: u64) -> bool {
        let mut cur = self.used.load(Ordering::Relaxed);
        loop {
            let Some(next) = cur.checked_add(need) else {
                return false;
            };
            if next > self.limit || !machine_has_room(need) {
                return false;
            }
            match self
                .used
                .compare_exchange(cur, next, Ordering::Relaxed, Ordering::Relaxed)
            {
                Ok(_) => return true,
                Err(actual) => cur = actual,
            }
        }
    }

    /// Give bytes back: a shrink, or a file whose last name went away.
    fn release(&self, bytes: u64) {
        // Never below zero, whatever the order the drops arrive in.
        let mut cur = self.used.load(Ordering::Relaxed);
        loop {
            let next = cur.saturating_sub(bytes);
            match self
                .used
                .compare_exchange(cur, next, Ordering::Relaxed, Ordering::Relaxed)
            {
                Ok(_) => return,
                Err(actual) => cur = actual,
            }
        }
    }

    /// Bytes held, for the tests and for anything that wants to report a full
    /// tmpfs the way `df` does.
    pub fn used(&self) -> u64 {
        self.used.load(Ordering::Relaxed)
    }
}

/// One file's bytes, as a list of fixed-size chunks.
///
/// The shape is the whole point. The first version was a single `Vec<u8>`, and
/// growing it means reallocating - so writing a file past a hundred-odd
/// megabytes ends in a request for one *contiguous* run that large. That is not
/// a memory shortage, it is an order the buddy allocator never serves, and the
/// measured result was a kernel panic, `memory allocation of 134217728 bytes
/// failed`, with a `write(2)` in front of it: a machine taken down by a
/// filesystem that was still inside its own budget. Chunked growth asks for
/// 64 KiB at a time however big the file becomes, which is why Linux keeps a
/// file in pages rather than in a buffer.
pub(crate) const CHUNK: usize = 65_536;

struct Chunked {
    chunks: Vec<Box<[u8]>>,
}

impl Chunked {
    fn new() -> Chunked {
        Chunked { chunks: Vec::new() }
    }

    fn len(&self) -> usize {
        match self.chunks.last() {
            Some(last) => (self.chunks.len() - 1) * CHUNK + last.len(),
            None => 0,
        }
    }

    /// Take the chunk holding `at` off the end, or push a fresh one. Only ever
    /// one allocation, and always of `CHUNK`.
    fn ensure(&mut self, at: usize) {
        let want = at / CHUNK + 1;
        while self.chunks.len() < want {
            self.chunks.push(vec![0u8; CHUNK].into_boxed_slice());
        }
    }

    /// Set the length, zero-filling a growth and dropping a tail.
    ///
    /// The invariant is that every chunk but the last is whole, and the last is
    /// whatever the length says - so growing a file also *completes* the chunk
    /// that used to end it, which is the part that was easy to get wrong: an
    /// interior 5-byte chunk makes `len` lie about every chunk after it.
    fn set_len(&mut self, n: usize) {
        let full = n / CHUNK;
        let tail = n % CHUNK;
        let want = full + usize::from(tail > 0);
        if self.chunks.len() > want {
            self.chunks.truncate(want);
        }
        // What chunk `i` holds at the new length: whole before the last, and
        // the odd remainder on it.
        let desired = |i: usize| if i < full { CHUNK } else { tail };
        for i in 0..want {
            if self.chunks.len() <= i {
                self.chunks.push(vec![0u8; desired(i)].into_boxed_slice());
                continue;
            }
            let have = self.chunks[i].len();
            let want_len = desired(i);
            if have != want_len {
                // Grow or shrink the same way: a fresh buffer, the old bytes
                // kept up to whichever end is shorter, the rest zero.
                let mut fresh = vec![0u8; want_len].into_boxed_slice();
                let copy = have.min(want_len);
                fresh[..copy].copy_from_slice(&self.chunks[i][..copy]);
                self.chunks[i] = fresh;
            }
        }
    }

    fn read_at(&self, at: usize, dst: &mut [u8]) -> usize {
        let size = self.len();
        if at >= size {
            return 0;
        }
        let n = dst.len().min(size - at);
        let mut done = 0;
        while done < n {
            let pos = at + done;
            let src = &self.chunks[pos / CHUNK];
            let from = pos % CHUNK;
            let take = (CHUNK - from).min(n - done).min(src.len() - from);
            dst[done..done + take].copy_from_slice(&src[from..from + take]);
            done += take;
        }
        n
    }

    fn write_at(&mut self, at: usize, src: &[u8]) {
        if src.is_empty() {
            return;
        }
        if at + src.len() > self.len() {
            self.set_len(at + src.len());
        }
        self.ensure(at + src.len() - 1);
        let mut done = 0;
        while done < src.len() {
            let pos = at + done;
            let chunk = pos / CHUNK;
            let from = pos % CHUNK;
            let take = (CHUNK - from).min(src.len() - done);
            // Sized by `set_len` above, so the range is always there.
            self.chunks[chunk][from..from + take].copy_from_slice(&src[done..done + take]);
            done += take;
        }
    }
}

/// A ramfs node's attribute flags: the `FS_*_FL` word `lsattr` prints and
/// `FS_IOC_SETFLAGS` may change.
///
/// An atomic rather than a critical section, because it is one word that nothing
/// else in the node shares, and because the checks that read it run on every
/// write and every unlink: taking a lock to ask "is this file immutable?" would
/// be a lock for a load.
///
/// It is carried by the nodes that keep a mode and an owner, and deliberately
/// not by the ones that keep neither. A node with no metadata has no attributes
/// to hold, and the trait's own refusal is the answer Linux gives a filesystem
/// with no `fileattr` half. So `RamFile`, `RamDir` and `RamSocket` carry this,
/// while `RamDev` (a placeholder until a devtmpfs mount covers the directory)
/// and `RamSymlink` (which stores only its target) answer `ENOTTY` for flags
/// exactly as they already answer "no mode" and "no owner".
struct NodeFlags(AtomicU32);

impl NodeFlags {
    fn new() -> NodeFlags {
        NodeFlags(AtomicU32::new(0))
    }

    fn get(&self) -> u32 {
        self.0.load(Ordering::Relaxed)
    }

    /// Move the flags to `flags`, refusing what a memory filesystem cannot
    /// keep. Nothing here is *owned* by a layout: there is no on-disk extent
    /// tree or directory index for a flag to describe, so the owned mask is
    /// empty and every bit a caller may name is one this can hold.
    fn set(&self, flags: u32) -> VfsResult<()> {
        let old = self.get();
        let stored = crate::vfs::apply_flag_change(old, flags, 0)?;
        self.0.store(stored, Ordering::Relaxed);
        Ok(())
    }
}

/// A file: bytes with no structure of their own.
struct RamFile {
    /// The mount these bytes are charged to. Not the filesystem's: a file
    /// outlives its directory's unlink, and the room is only back when *this*
    /// reference goes.
    fs: Arc<Usage>,
    ino: Ino,
    /// How many names reach it - one at creation, one more per `link`. The
    /// last name going leaves `0`, which the trait's getter reads as "counts
    /// nothing"... and which `abi::nlink_of` reads as "does not track one" and
    /// answers `1` for. An unlinked file still held open therefore reports the
    /// one name it was created under rather than Linux's `0`, the price of a
    /// getter whose zero is a sentinel.
    links: Mutex<Cell<u64>>,
    /// The permission bits, once anybody set them. `None` until then - which
    /// `abi::mode_of` reads as "this filesystem keeps none" and answers the
    /// kind's default for - and `Some(0)` after a `chmod 000`, which is a
    /// mode like any other and denies. The initramfs unpacker is the caller
    /// that sets them: an archive's whole `/bin` is executable, and a kernel
    /// that forgets that answers `EACCES` to the `test -x` an init asks
    /// before running anything.
    mode: Mutex<Cell<Option<u32>>>,
    /// Who owns it, when anybody said. `(0, 0)` - root - until a create or a
    /// `chown` writes it, which is what a memory filesystem the kernel built
    /// for itself used to answer for everything.
    owner: Mutex<Cell<(u32, u32)>>,
    /// The file's three clocks, in unix seconds, stamped at creation and moved
    /// by the door each one belongs to: a write moves mtime and ctime, a read
    /// moves atime under `relatime`, `utimensat` moves the two it names and
    /// stamps ctime. A *regular file* keeps all three because Linux's tmpfs
    /// does, and a fresh file's first read is exactly the case that tells it
    /// from the old one-clock shape (`.workbuddy/probe_atime.c`: the three are
    /// equal at creation, and a read moves only atime).
    atime: Mutex<Cell<u64>>,
    mtime: Mutex<Cell<u64>>,
    ctime: Mutex<Cell<u64>>,
    /// The attribute flags: `lsattr`'s word, and what makes a file immutable
    /// or append-only.
    flags: NodeFlags,
    data: Mutex<RefCell<Chunked>>,
}

impl RamFile {
    /// Bytes held, which is what the mount's room is charged in.
    fn len(&self) -> usize {
        critical_section::with(|cs| self.data.borrow(cs).borrow().len())
    }
}

/// The room a file holds comes back when the file's last reference goes - not
/// when its name is unlinked, because an open descriptor keeps a deleted file's
/// bytes alive, and charging the mount for bytes nobody can reach any more is
/// how a limit turns into a slow leak.
impl Drop for RamFile {
    fn drop(&mut self) {
        self.fs.release(self.len() as u64);
    }
}

/// How many names reach a node, and the writing half of that count.
///
/// A hard link is a second edge to the same `Arc`, so the node is shared by
/// both names and the number belongs to it. The *directory* is what makes the
/// edge and the only node that sees both ends, which is why the trait carries
/// a setter for the count as well as a getter for it. A directory is not
/// linkable, so `RamDir` has neither and keeps the trait's fallback.
macro_rules! counts_its_names {
    () => {
        fn nlink(&self) -> u64 {
            critical_section::with(|cs| self.links.borrow(cs).get())
        }
        fn set_nlink(&self, links: u64) {
            critical_section::with(|cs| self.links.borrow(cs).set(links));
        }
    };
}

/// The operations that make a name, answered by a node that cannot hold one.
///
/// A path is walked before a name is made in it, and a walk that reaches
/// something which is not a directory is `ENOTDIR` on Linux. The trait's own
/// default is `Unsupported` instead, which reads to a caller as "this
/// filesystem cannot make sockets" rather than "this component is a file" -
/// `bind` under a plain file and `open(O_CREAT)` under one both answered
/// `EOPNOTSUPP` where Linux answers `ENOTDIR`. The resolver already says
/// `NotDir` for a non-directory in the *middle* of a path; this is the same
/// fact for the component that ends it. `RamDir` is the only node here that
/// holds names, so every other node says so in one word through this.
macro_rules! names_live_in_dirs {
    () => {
        fn create(
            &self,
            name: &str,
            kind: NodeKind,
            rdev: Option<(u32, u32)>,
        ) -> VfsResult<Arc<dyn Inode>> {
            let _ = (name, kind, rdev);
            Err(VfsError::NotDir)
        }
        fn create_tmpfile(&self) -> VfsResult<Arc<dyn Inode>> {
            Err(VfsError::NotDir)
        }
        fn symlink(&self, name: &str, target: &str) -> VfsResult<Arc<dyn Inode>> {
            let _ = (name, target);
            Err(VfsError::NotDir)
        }
        fn unlink(&self, name: &str) -> VfsResult<()> {
            let _ = name;
            Err(VfsError::NotDir)
        }
        fn rmdir(&self, name: &str) -> VfsResult<()> {
            let _ = name;
            Err(VfsError::NotDir)
        }
        fn link(&self, name: &str, target: &Arc<dyn Inode>) -> VfsResult<()> {
            let _ = (name, target);
            Err(VfsError::NotDir)
        }
    };
}

impl Inode for RamFile {
    fn flags(&self) -> VfsResult<u32> {
        Ok(self.flags.get())
    }

    fn set_flags(&self, flags: u32) -> VfsResult<()> {
        self.flags.set(flags)
    }

    fn metadata(&self) -> Metadata {
        Metadata {
            ino: self.ino,
            kind: NodeKind::File,
            size: critical_section::with(|cs| self.data.borrow(cs).borrow().len() as u64),
        }
    }

    fn mode(&self) -> u32 {
        critical_section::with(|cs| self.mode.borrow(cs).get()).unwrap_or(0) & MODE_BITS
    }

    fn set_mode(&self, mode: u32) {
        critical_section::with(|cs| {
            self.mode.borrow(cs).set(Some(mode & MODE_BITS));
        });
    }

    /// A mode is kept only once somebody set one: `chmod 000` and "nobody
    /// said" are different answers, and the cell has to hold both.
    fn keeps_modes(&self) -> bool {
        critical_section::with(|cs| self.mode.borrow(cs).get()).is_some()
    }

    fn owner(&self) -> (u32, u32) {
        critical_section::with(|cs| self.owner.borrow(cs).get())
    }

    fn set_owner(&self, uid: u32, gid: u32) {
        critical_section::with(|cs| self.owner.borrow(cs).set((uid, gid)));
    }

    fn mtime(&self) -> u64 {
        critical_section::with(|cs| self.mtime.borrow(cs).get())
    }

    fn atime(&self) -> u64 {
        critical_section::with(|cs| self.atime.borrow(cs).get())
    }

    fn ctime(&self) -> u64 {
        critical_section::with(|cs| self.ctime.borrow(cs).get())
    }

    /// `utimensat`: both clocks are kept, and the metadata change stamps ctime -
    /// Linux's `notify_change`. The caller has already resolved
    /// `UTIME_NOW`/`UTIME_OMIT` against what this node reports.
    fn set_times(&self, atime: u64, mtime: u64) -> VfsResult<()> {
        let at = now();
        critical_section::with(|cs| {
            self.atime.borrow(cs).set(atime);
            self.mtime.borrow(cs).set(mtime);
            self.ctime.borrow(cs).set(at);
        });
        Ok(())
    }

    /// The access clock of a read that moved bytes - **every** read, which is
    /// Linux's `strictatime` and what a tmpfs does. Measured on a current host
    /// (`.workbuddy/probe_atime.c`, its `/tmp` being a tmpfs): a future atime
    /// set with `utimensat` is pulled back to "now" by the next read, where an
    /// ext4 mount (relatime) leaves it standing. The two filesystems answer
    /// differently, so the driver spells the rule rather than reusing ext4's.
    ///
    /// The inode's own `A` flag still wins: `chattr +A` is a promise about this
    /// file, wherever it is mounted.
    fn touch_atime(&self, now: u64) -> VfsResult<()> {
        if self.flags.get() & crate::vfs::FS_NOATIME_FL == 0 {
            critical_section::with(|cs| self.atime.borrow(cs).set(now));
        }
        Ok(())
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> VfsResult<usize> {
        critical_section::with(|cs| {
            let data = self.data.borrow(cs).borrow();
            // An offset at or past the end reads nothing, which is how EOF
            // looks; the chunk walk clamps the same way by refusing to start.
            Ok(data.read_at(offset as usize, buf))
        })
    }

    fn write_at(&self, offset: u64, buf: &[u8]) -> VfsResult<usize> {
        let at = now();
        // Room first, bytes second: asking the kernel heap for a write the
        // mount has no room for is the failure this ordering exists to prevent.
        let grows_to = (offset as usize) + buf.len();
        let current = self.len();
        if grows_to > current && !self.fs.charge((grows_to - current) as u64) {
            return Err(VfsError::NoSpace);
        }
        critical_section::with(|cs| {
            // The chunks cover the hole a write past the end leaves, zero-
            // filled as they are made - POSIX's "offset past end of file" read.
            self.data
                .borrow(cs)
                .borrow_mut()
                .write_at(offset as usize, buf);
            self.mtime.borrow(cs).set(at);
            self.ctime.borrow(cs).set(at);
            Ok(buf.len())
        })
    }

    /// `truncate(2)`: shrinking drops the tail, growing adds zeros - the same
    /// contract `Chunked::set_len` keeps, one chunk at a time.
    fn truncate(&self, len: u64) -> VfsResult<()> {
        let len = usize::try_from(len).map_err(|_| VfsError::FileTooBig)?;
        let at = now();
        let current = self.len();
        match len.cmp(&current) {
            core::cmp::Ordering::Greater => {
                if !self.fs.charge((len - current) as u64) {
                    return Err(VfsError::NoSpace);
                }
            }
            core::cmp::Ordering::Less => self.fs.release((current - len) as u64),
            core::cmp::Ordering::Equal => {}
        }
        critical_section::with(|cs| {
            self.data.borrow(cs).borrow_mut().set_len(len);
            self.mtime.borrow(cs).set(at);
            self.ctime.borrow(cs).set(at);
        });
        Ok(())
    }

    counts_its_names!();
    names_live_in_dirs!();
}

/// A directory: name edges, aka the dentries of this filesystem.
/// A symbolic link: nothing but the path it stands for.
struct RamSymlink {
    ino: Ino,
    /// How many names reach it: one, plus one per `link` of the link itself.
    links: Mutex<Cell<u64>>,
    /// The link's clock: the moment it was made, and then whatever
    /// `utimensat` was asked for - Linux keeps a symlink's clocks, and
    /// `touch -h` is a caller that names one.
    mtime: Mutex<Cell<u64>>,
    target: String,
    /// Linux's birth mode (`0777` under `S_IFLNK`), stored rather than left
    /// unset so `stat` of the link reads what an ext2 link carries and a
    /// check that ever consults a link's word sees permission bits, not a
    /// `chmod 000`. Nothing follows a link asking its mode; `chmod` on one
    /// writes through to the target, as it does on Linux.
    mode: Mutex<Cell<Option<u32>>>,
    owner: Mutex<Cell<(u32, u32)>>,
}

impl Inode for RamSymlink {
    fn metadata(&self) -> Metadata {
        Metadata {
            ino: self.ino,
            kind: NodeKind::Symlink,
            size: self.target.len() as u64,
        }
    }

    fn mode(&self) -> u32 {
        critical_section::with(|cs| self.mode.borrow(cs).get()).unwrap_or(0) & MODE_BITS
    }

    fn set_mode(&self, mode: u32) {
        critical_section::with(|cs| {
            self.mode.borrow(cs).set(Some(mode & MODE_BITS));
        });
    }

    /// A mode is kept only once somebody set one - the same rule `RamFile`
    /// states, and the reason `chmod 000` on a link stays distinguishable
    /// from "a node born before this field had an answer".
    fn keeps_modes(&self) -> bool {
        critical_section::with(|cs| self.mode.borrow(cs).get()).is_some()
    }

    /// The creator's pair, which `stat` used to read as root's because no
    /// link stored anything. Linux stamps a symlink like any other inode;
    /// `lchown` on one is the call that would change it, and it now has a
    /// place to land (`Inode::set_owner`'s default still keeps nothing).
    fn owner(&self) -> (u32, u32) {
        critical_section::with(|cs| self.owner.borrow(cs).get())
    }

    fn set_owner(&self, uid: u32, gid: u32) {
        critical_section::with(|cs| self.owner.borrow(cs).set((uid, gid)));
    }

    fn mtime(&self) -> u64 {
        critical_section::with(|cs| self.mtime.borrow(cs).get())
    }

    /// `utimensat`, on the shape Linux keeps: a symlink's and a socket's
    /// clocks are settable (`.workbuddy/utimensat_host.c`), and the atime
    /// is accepted and dropped here for the same reason a file's is - one
    /// clock stands for all three in this filesystem.
    fn set_times(&self, _atime: u64, mtime: u64) -> VfsResult<()> {
        critical_section::with(|cs| self.mtime.borrow(cs).set(mtime));
        Ok(())
    }

    fn readlink(&self) -> VfsResult<String> {
        Ok(self.target.clone())
    }

    counts_its_names!();
    names_live_in_dirs!();
}

/// A device node a `mknod` made: a name carrying a kind **and a number**.
///
/// No driver stands behind it. Linux's does stand behind its own, because there
/// a number reaches the driver that registered it - measured, a node made with
/// `mknod(path, S_IFCHR|0666, makedev(1, 3))` in any tree opens the *real*
/// `/dev/null` on this host - so the same node made there and here is the same
/// node doing different things, and the difference is the number's meaning
/// rather than the number's existence. What this kernel can report honestly is
/// one half of it: the node keeps what `mknod` said - its kind, its number, its
/// word - and `open(2)` of it refuses `ENXIO`, which is Linux's answer for a
/// number no driver owns and therefore the answer for **every** number here
/// (`.workbuddy/probe_mknod_rdev.c`, Linux 7.2.11).
///
/// Answering anything else would be the lie this replaced: the node used to
/// open, read end-of-file and swallow writes, which is exactly what
/// `/dev/null` does - so a caller could not tell a device that is not there
/// from the one device that is.
struct RamDev {
    ino: Ino,
    /// Which of the two device types `mknod` asked for. They are different
    /// nodes on Linux and were the same node here: every placeholder reported
    /// a *character* device, so `stat` of a node made with `S_IFBLK` named a
    /// kind the caller never asked for.
    kind: NodeKind,
    /// The number `mknod`'s `dev` argument named, decoded from its word.
    dev: (u32, u32),
    /// The permission bits and the ids, which `mknod` hands the node it makes
    /// exactly as `open(O_CREAT)` hands them a file - masked by the caller's
    /// umask, and reported back by `stat` rather than invented from the kind.
    mode: Mutex<Cell<Option<u32>>>,
    owner: Mutex<Cell<(u32, u32)>>,
    /// The clock `utimensat` hands it - a device node made by `mknod` keeps one
    /// on Linux, the same as one devtmpfs published does. Like every node in
    /// this filesystem it keeps *one* word: the atime is accepted and dropped
    /// into the mtime, the rule the file and the directory already follow.
    mtime: Mutex<Cell<u64>>,
    /// How many names reach it: a `mknod` placeholder can be linked like any
    /// other node.
    links: Mutex<Cell<u64>>,
}

impl Inode for RamDev {
    fn metadata(&self) -> Metadata {
        Metadata {
            ino: self.ino,
            kind: self.kind,
            size: 0,
        }
    }

    /// What `st_rdev` reports: the number this node was *made* with. The other
    /// three stat doors reach it through here, so none of them can disagree
    /// with what `mknod` wrote.
    fn rdev(&self) -> Option<(u32, u32)> {
        Some(self.dev)
    }

    fn mode(&self) -> u32 {
        critical_section::with(|cs| self.mode.borrow(cs).get()).unwrap_or(0) & MODE_BITS
    }

    fn set_mode(&self, mode: u32) {
        critical_section::with(|cs| {
            self.mode.borrow(cs).set(Some(mode & MODE_BITS));
        });
    }

    fn keeps_modes(&self) -> bool {
        critical_section::with(|cs| self.mode.borrow(cs).get()).is_some()
    }

    fn owner(&self) -> (u32, u32) {
        critical_section::with(|cs| self.owner.borrow(cs).get())
    }

    fn set_owner(&self, uid: u32, gid: u32) {
        critical_section::with(|cs| self.owner.borrow(cs).set((uid, gid)));
    }

    /// None of the file doors reach a node no driver answers for, and `open(2)`
    /// of one is Linux's `ENXIO` - the refusal a socket node already gives for
    /// the same reason seen from the other side (there, the name is real and
    /// the bytes are not; here, neither is).
    fn read_at(&self, _offset: u64, _buf: &mut [u8]) -> VfsResult<usize> {
        Err(VfsError::NoDevice)
    }

    fn write_at(&self, _offset: u64, _buf: &[u8]) -> VfsResult<usize> {
        Err(VfsError::NoDevice)
    }

    fn open_replacement(&self, _flags: usize) -> VfsResult<Option<Arc<dyn Inode>>> {
        Err(VfsError::NoDevice)
    }

    fn mtime(&self) -> u64 {
        critical_section::with(|cs| self.mtime.borrow(cs).get())
    }

    /// `utimensat`: kept rather than refused, which is what Linux's `mknod`ed
    /// device nodes do (measured on a host, `.workbuddy/utimensat_host.c`) -
    /// and folded into one clock, the shape every node here has.
    fn set_times(&self, _atime: u64, mtime: u64) -> VfsResult<()> {
        critical_section::with(|cs| self.mtime.borrow(cs).set(mtime));
        Ok(())
    }

    counts_its_names!();
    names_live_in_dirs!();
}

/// A socket node: a name `bind(2)` made for a connection endpoint. It
/// carries the namespace presence - stat type, directory listing, unlink -
/// and deliberately no bytes: reading the node through the file doors is
/// the question a socket never answers, and `open(2)` on the path gets
/// Linux's `ENXIO` from the read/write refusals below.
struct RamSocket {
    ino: Ino,
    /// How many names reach it - the bound path, and any `link` of it.
    links: Mutex<Cell<u64>>,
    /// The socket's clock: the moment `bind` planted the name, and then
    /// whatever `utimensat` was asked for.
    mtime: Mutex<Cell<u64>>,
    mode: Mutex<Cell<Option<u32>>>,
    owner: Mutex<Cell<(u32, u32)>>,
    flags: NodeFlags,
}

impl Inode for RamSocket {
    fn flags(&self) -> VfsResult<u32> {
        Ok(self.flags.get())
    }

    fn set_flags(&self, flags: u32) -> VfsResult<()> {
        self.flags.set(flags)
    }

    fn metadata(&self) -> Metadata {
        Metadata {
            ino: self.ino,
            kind: NodeKind::Socket,
            size: 0,
        }
    }

    fn mtime(&self) -> u64 {
        critical_section::with(|cs| self.mtime.borrow(cs).get())
    }

    /// `utimensat`, on the shape Linux keeps: a symlink's and a socket's
    /// clocks are settable (`.workbuddy/utimensat_host.c`), and the atime
    /// is accepted and dropped here for the same reason a file's is - one
    /// clock stands for all three in this filesystem.
    fn set_times(&self, _atime: u64, mtime: u64) -> VfsResult<()> {
        critical_section::with(|cs| self.mtime.borrow(cs).set(mtime));
        Ok(())
    }

    fn mode(&self) -> u32 {
        critical_section::with(|cs| self.mode.borrow(cs).get()).unwrap_or(0) & MODE_BITS
    }

    fn set_mode(&self, mode: u32) {
        critical_section::with(|cs| {
            self.mode.borrow(cs).set(Some(mode & MODE_BITS));
        });
    }

    fn keeps_modes(&self) -> bool {
        critical_section::with(|cs| self.mode.borrow(cs).get()).is_some()
    }

    fn owner(&self) -> (u32, u32) {
        critical_section::with(|cs| self.owner.borrow(cs).get())
    }

    fn set_owner(&self, uid: u32, gid: u32) {
        critical_section::with(|cs| self.owner.borrow(cs).set((uid, gid)));
    }

    fn read_at(&self, _offset: u64, _buf: &mut [u8]) -> VfsResult<usize> {
        Err(VfsError::NotConnected)
    }

    fn write_at(&self, _offset: u64, _buf: &[u8]) -> VfsResult<usize> {
        Err(VfsError::NotConnected)
    }

    /// The file doors do not reach the queue behind a socket name:
    /// `open(2)` answers Linux's ENXIO, the same refusal `/dev/tty`
    /// gives a process it has no answer for.
    fn open_replacement(&self, _flags: usize) -> VfsResult<Option<Arc<dyn Inode>>> {
        Err(VfsError::NoDevice)
    }

    counts_its_names!();
    names_live_in_dirs!();
}

/// A FIFO: a *name* two processes meet at, whose bytes cross through a
/// [`Channel`](crate::vfs::pipe::Channel) rather than through storage.
///
/// The node carries the name's identity - `stat` type, mode, owner, clock,
/// unlink - and the channel it hands out. It holds no bytes itself: `open(2)`
/// runs the rendezvous (an `open(O_RDONLY)` with no writer parks, and the
/// reverse) and returns the end the direction asks for, so the reads and writes
/// behind the name are a pipe's. `docs/uapi/` §5.2 carries the measured
/// contract; `.workbuddy/probe_fifo.c` is where it was measured.
struct RamFifo {
    ino: Ino,
    /// How many names reach it - the `mknod`ed name, and any `link` of it.
    links: Mutex<Cell<u64>>,
    mode: Mutex<Cell<Option<u32>>>,
    owner: Mutex<Cell<(u32, u32)>>,
    mtime: Mutex<Cell<u64>>,
    flags: NodeFlags,
    /// The channel every open of this name joins. It lives exactly as long as
    /// the name does; each open takes a count on the way in and gives it back
    /// on close, and the buffer is discarded when both counts reach zero.
    channel: Arc<crate::vfs::pipe::Channel>,
}

impl Inode for RamFifo {
    fn flags(&self) -> VfsResult<u32> {
        Ok(self.flags.get())
    }

    fn set_flags(&self, flags: u32) -> VfsResult<()> {
        self.flags.set(flags)
    }

    fn metadata(&self) -> Metadata {
        Metadata {
            ino: self.ino,
            kind: NodeKind::Fifo,
            size: 0,
        }
    }

    fn mtime(&self) -> u64 {
        critical_section::with(|cs| self.mtime.borrow(cs).get())
    }

    fn set_times(&self, _atime: u64, mtime: u64) -> VfsResult<()> {
        critical_section::with(|cs| self.mtime.borrow(cs).set(mtime));
        Ok(())
    }

    fn mode(&self) -> u32 {
        critical_section::with(|cs| self.mode.borrow(cs).get()).unwrap_or(0) & MODE_BITS
    }

    fn set_mode(&self, mode: u32) {
        critical_section::with(|cs| {
            self.mode.borrow(cs).set(Some(mode & MODE_BITS));
        });
    }

    fn keeps_modes(&self) -> bool {
        critical_section::with(|cs| self.mode.borrow(cs).get()).is_some()
    }

    fn owner(&self) -> (u32, u32) {
        critical_section::with(|cs| self.owner.borrow(cs).get())
    }

    fn set_owner(&self, uid: u32, gid: u32) {
        critical_section::with(|cs| self.owner.borrow(cs).set((uid, gid)));
    }

    /// Opening a FIFO's name is opening one end of its channel, and the open's
    /// direction decides which: `O_RDONLY` a reader, `O_WRONLY` a writer,
    /// `O_RDWR` one description that is both (`Role::Both`), and `O_NONBLOCK`
    /// decides whether a peerless open parks or answers `ENXIO`. The whole
    /// shape is Linux's, measured in `docs/uapi/` §5.2.
    fn open_replacement(&self, flags: usize) -> VfsResult<Option<Arc<dyn Inode>>> {
        // The access mode is the low two bits of the flag word - 0 read, 1
        // write, 2 read-write - the same three-way choice `File::set_access`
        // records. Not `O_ACCMODE` from the syscall layer: the filesystem has
        // no business naming the ABI, and the numbers are POSIX's.
        let role = match flags & 0o3 {
            1 => crate::vfs::pipe::Role::Write,
            2 => crate::vfs::pipe::Role::Both,
            _ => crate::vfs::pipe::Role::Read,
        };
        let nonblocking = flags as u32 & crate::vfs::file::O_NONBLOCK != 0;
        let end = self.channel.open(role, nonblocking)?;
        Ok(Some(end as Arc<dyn Inode>))
    }

    counts_its_names!();
    names_live_in_dirs!();
}

struct RamDir {
    /// Passed down to every node this directory creates, so the whole mount
    /// shares one room.
    fs: Arc<Usage>,
    ino: Ino,
    mode: Mutex<Cell<Option<u32>>>,
    owner: Mutex<Cell<(u32, u32)>>,
    /// Stamped when an entry is added or removed, as a directory's mtime is
    /// on every real filesystem.
    mtime: Mutex<Cell<u64>>,
    /// The attribute flags. A directory's matter as much as a file's here: an
    /// append-only directory takes new names and gives none up, and an
    /// immutable one takes nothing at all.
    flags: NodeFlags,
    entries: Mutex<RefCell<BTreeMap<String, Arc<dyn Inode>>>>,
}

impl Inode for RamDir {
    fn flags(&self) -> VfsResult<u32> {
        Ok(self.flags.get())
    }

    fn set_flags(&self, flags: u32) -> VfsResult<()> {
        self.flags.set(flags)
    }

    fn metadata(&self) -> Metadata {
        Metadata {
            ino: self.ino,
            kind: NodeKind::Dir,
            size: 0,
        }
    }

    fn mode(&self) -> u32 {
        critical_section::with(|cs| self.mode.borrow(cs).get()).unwrap_or(0) & MODE_BITS
    }

    fn set_mode(&self, mode: u32) {
        critical_section::with(|cs| {
            self.mode.borrow(cs).set(Some(mode & MODE_BITS));
        });
    }

    /// A directory is in the same position as a file: the bits are kept once
    /// somebody sets them, and the initramfs unpacker is what sets them.
    fn keeps_modes(&self) -> bool {
        critical_section::with(|cs| self.mode.borrow(cs).get()).is_some()
    }

    fn owner(&self) -> (u32, u32) {
        critical_section::with(|cs| self.owner.borrow(cs).get())
    }

    fn set_owner(&self, uid: u32, gid: u32) {
        critical_section::with(|cs| self.owner.borrow(cs).set((uid, gid)));
    }

    fn mtime(&self) -> u64 {
        critical_section::with(|cs| self.mtime.borrow(cs).get())
    }

    fn set_times(&self, _atime: u64, mtime: u64) -> VfsResult<()> {
        critical_section::with(|cs| self.mtime.borrow(cs).set(mtime));
        Ok(())
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

    fn create(
        &self,
        name: &str,
        kind: NodeKind,
        rdev: Option<(u32, u32)>,
    ) -> VfsResult<Arc<dyn Inode>> {
        let at = now();
        // The birth mode, as every filesystem that stores modes gives one: a
        // new node is born with the kind's default minus the caller's mask
        // (`init_user_ns`'s umask for a kernel creation), so `mkdir(2)` and
        // `open(O_CREAT)` answer success *and* leave a traversable directory
        // behind. Without it the node keeps no mode - which `abi::mode_of`
        // still reports as the kind's default, so `stat` looked right while
        // `may_as` read zero triads and refused root the search of a
        // directory the same kernel had just made. Measured on the sweep
        // machine: `t-unix`'s bind under `/tmp/t_u/x/y`, where both
        // directories came from this call, answered `EACCES` where Linux
        // answers `ENOTDIR`. A device placeholder is the exception - see
        // `birth_mode`.
        let birth = birth_mode(kind);
        let owner = crate::sched::current_fs_owner();
        let node: Arc<dyn Inode> = match kind {
            NodeKind::File => Arc::new(RamFile {
                fs: self.fs.clone(),
                ino: fresh_ino(),
                links: Mutex::new(Cell::new(1)),
                mode: Mutex::new(Cell::new(birth)),
                owner: Mutex::new(Cell::new(owner)),
                atime: Mutex::new(Cell::new(at)),
                mtime: Mutex::new(Cell::new(at)),
                ctime: Mutex::new(Cell::new(at)),
                flags: NodeFlags::new(),
                data: Mutex::new(RefCell::new(Chunked::new())),
            }),
            NodeKind::Dir => Arc::new(RamDir {
                fs: self.fs.clone(),
                ino: fresh_ino(),
                mode: Mutex::new(Cell::new(birth)),
                owner: Mutex::new(Cell::new(owner)),
                mtime: Mutex::new(Cell::new(at)),
                flags: NodeFlags::new(),
                entries: Mutex::new(RefCell::new(BTreeMap::new())),
            }),
            // A device node `mknod` made. The number is the whole of why this
            // arm exists rather than being a node with nothing in it: `mknod`
            // *names* a device, so a node that keeps neither its number nor its
            // word is a name that answers `stat` about a device nobody made.
            // The pair is the argument's, decoded at the door, and the mode and
            // ids are the call's own - see `RamDev`.
            NodeKind::CharDev | NodeKind::BlockDev => Arc::new(RamDev {
                ino: fresh_ino(),
                kind,
                dev: rdev.unwrap_or((0, 0)),
                links: Mutex::new(Cell::new(1)),
                mode: Mutex::new(Cell::new(birth)),
                owner: Mutex::new(Cell::new(owner)),
                mtime: Mutex::new(Cell::new(at)),
            }),
            // A socket name, made by `bind` alone. Its clock is the bind;
            // `connect` looks up the node's identity, never its bytes. The
            // mode is Linux's birth mode for a unix socket because the DAC
            // check reads the stored word, not `mode_of`'s default.
            NodeKind::Socket => Arc::new(RamSocket {
                ino: fresh_ino(),
                links: Mutex::new(Cell::new(1)),
                mtime: Mutex::new(Cell::new(now())),
                mode: Mutex::new(Cell::new(birth)),
                owner: Mutex::new(Cell::new(owner)),
                flags: NodeFlags::new(),
            }),
            // A FIFO: a name whose channel a `mknod(S_IFIFO)` made. The channel
            // is born with the node and outlives every open of it; the birth
            // mode below is a default the caller's `mknod` mode overrides.
            NodeKind::Fifo => {
                let ino = fresh_ino();
                Arc::new(RamFifo {
                    ino,
                    links: Mutex::new(Cell::new(1)),
                    mode: Mutex::new(Cell::new(birth)),
                    owner: Mutex::new(Cell::new(owner)),
                    mtime: Mutex::new(Cell::new(at)),
                    flags: NodeFlags::new(),
                    channel: Arc::new(crate::vfs::pipe::Channel::new(NodeKind::Fifo, ino)),
                })
            }
            // A link is made with `Inode::symlink`, which carries the one thing
            // a link cannot exist without: what it points at.
            NodeKind::Symlink => return Err(VfsError::Unsupported),
            // Anonymous inodes are minted by their owning door (`pidfd_open`),
            // never by a filesystem name; nothing a caller can `mknod` is one.
            NodeKind::Anon { .. } => return Err(VfsError::Unsupported),
        };
        critical_section::with(|cs| {
            let mut entries = self.entries.borrow(cs).borrow_mut();
            if entries.contains_key(name) {
                return Err(VfsError::Exists);
            }
            entries.insert(String::from(name), node.clone());
            self.mtime.borrow(cs).set(at);
            Ok(node)
        })
    }

    fn symlink(&self, name: &str, target: &str) -> VfsResult<Arc<dyn Inode>> {
        let at = now();
        // Linux's birth mode for a link (`S_IFLNK | 0777`, mask applied to the
        // permission half): nothing consults a link's own bits - resolution
        // follows it - but `stat` of the link itself reads them back, and an
        // unset word there is a stored `chmod 000`, not "no mode". ext4's
        // `new_inode` gives its links the same number.
        let node: Arc<dyn Inode> = Arc::new(RamSymlink {
            ino: fresh_ino(),
            links: Mutex::new(Cell::new(1)),
            mtime: Mutex::new(Cell::new(at)),
            target: String::from(target),
            mode: Mutex::new(Cell::new(birth_mode(NodeKind::Symlink))),
            owner: Mutex::new(Cell::new(crate::sched::current_fs_owner())),
        });
        critical_section::with(|cs| {
            let mut entries = self.entries.borrow(cs).borrow_mut();
            if entries.contains_key(name) {
                return Err(VfsError::Exists);
            }
            entries.insert(String::from(name), node.clone());
            self.mtime.borrow(cs).set(at);
            Ok(node)
        })
    }

    /// Drop the name edge. The node the name pointed at lives on while any
    /// open descriptor or other edge holds it; the name is free at once.
    ///
    /// A directory is refused, as it is on every other filesystem and as Linux
    /// answers: `unlink` is for names that hold contents, and a directory holds
    /// `.` and its children. Removing one through this call would be a
    /// recursive delete arriving through the wrong door - it is what `rmdir`
    /// exists to say no to.
    fn unlink(&self, name: &str) -> VfsResult<()> {
        let child = self.lookup(name)?;
        if child.metadata().kind == NodeKind::Dir {
            return Err(VfsError::IsDir);
        }
        let at = now();
        critical_section::with(|cs| {
            let mut entries = self.entries.borrow(cs).borrow_mut();
            entries.remove(name).map(|_| ()).ok_or(VfsError::NoEntry)?;
            self.mtime.borrow(cs).set(at);
            Ok(())
        })?;
        // The name is gone, so one fewer reaches the node - `st_nlink` follows
        // the edge count, which is what tells `ln` and `rm -i` a file has
        // another name. Floored at the trait's zero, "counts nothing".
        child.set_nlink(child.nlink().saturating_sub(1));
        Ok(())
    }

    /// A hard link: a second name edge to the node another name already
    /// reaches. Here that is literally all one is - the same `Arc` under a
    /// second key - so both names read, write and truncate the same bytes,
    /// and the data survives either name being unlinked while one edge
    /// remains. Directories are refused, as `link(2)` does: a cycle of name
    /// edges is not something a tree this shape can survive.
    fn link(&self, name: &str, target: &Arc<dyn Inode>) -> VfsResult<()> {
        if target.metadata().kind == NodeKind::Dir {
            return Err(VfsError::Invalid);
        }
        let at = now();
        critical_section::with(|cs| {
            let mut entries = self.entries.borrow(cs).borrow_mut();
            if entries.contains_key(name) {
                return Err(VfsError::Exists);
            }
            entries.insert(alloc::string::String::from(name), target.clone());
            self.mtime.borrow(cs).set(at);
            Ok(())
        })?;
        // The second edge is up, so `st_nlink` is one more for both names -
        // the target's own count, since both names reach the same node. A
        // node that does not track one is not told (the setter's default).
        target.set_nlink(target.nlink() + 1);
        Ok(())
    }

    /// Drop the name edge to an **empty** directory.
    ///
    /// Nothing here counts links, so the whole of an `rmdir` is: the name must
    /// be a directory, and it must have nothing under it. The child's own
    /// listing is the only witness for the second, which is why this asks the
    /// node rather than the table.
    fn rmdir(&self, name: &str) -> VfsResult<()> {
        let child = self.lookup(name)?;
        if child.metadata().kind != NodeKind::Dir {
            return Err(VfsError::NotDir);
        }
        if !child.read_dir()?.is_empty() {
            return Err(VfsError::NotEmpty);
        }
        let at = now();
        critical_section::with(|cs| {
            let mut entries = self.entries.borrow(cs).borrow_mut();
            entries.remove(name).map(|_| ()).ok_or(VfsError::NoEntry)?;
            self.mtime.borrow(cs).set(at);
            Ok(())
        })
    }

    fn read_dir(&self) -> VfsResult<Vec<(String, NodeKind)>> {
        critical_section::with(|cs| {
            let entries = self.entries.borrow(cs).borrow();
            // The map's order is sorted, which is as good an order as any.
            Ok(entries
                .iter()
                .map(|(name, node)| (name.clone(), node.metadata().kind))
                .collect())
        })
    }
}

/// A mounted ramfs instance: one root directory and everything under it.
pub struct RamFs {
    root: Arc<RamDir>,
}

impl RamFs {
    /// The mount's room and how much of it is held - what `df`-style reporting
    /// reads, and what the tests assert on.
    pub fn usage(&self) -> Arc<Usage> {
        self.root.fs.clone()
    }
}

impl RamFs {
    pub fn new() -> Arc<RamFs> {
        RamFs::with_usage(Usage::new())
    }

    /// A mount with a named amount of room - what the tests use to reach an
    /// `ENOSPC` without filling a machine first.
    pub fn with_usage(fs: Arc<Usage>) -> Arc<RamFs> {
        Arc::new(RamFs {
            root: Arc::new(RamDir {
                fs,
                ino: fresh_ino(),
                mode: Mutex::new(Cell::new(None)),
                owner: Mutex::new(Cell::new(crate::sched::current_fs_owner())),
                mtime: Mutex::new(Cell::new(now())),
                flags: NodeFlags::new(),
                entries: Mutex::new(RefCell::new(BTreeMap::new())),
            }),
        })
    }
}

impl FileSystem for RamFs {
    fn root(&self) -> Arc<dyn Inode> {
        self.root.clone()
    }

    fn name(&self) -> &'static str {
        "ramfs"
    }
}

#[cfg(all(test, not(target_os = "none")))]
mod tests {
    use alloc::sync::Arc;
    use alloc::vec;

    use super::{CHUNK, Chunked, RamFs, Usage};
    use crate::vfs::{Caller, FileSystem, NodeKind, R_OK, VfsError, X_OK, may_as};

    /// The flags are kept, and a caller reads back what it set.
    ///
    /// Which is all `lsattr` needs from a memory filesystem, and the reason
    /// `GETFLAGS` answers *something* here rather than refusing: Linux's tmpfs
    /// answers it too - measured on the host, where `lsattr` on a tmpfs file
    /// prints its flags line rather than "not supported" - and the zero word a
    /// file nobody has flagged carries is that answer.
    #[test]
    fn the_attribute_flags_are_kept_and_read_back() {
        use crate::vfs::{FS_APPEND_FL, FS_IMMUTABLE_FL, FS_NODUMP_FL};
        let fs = RamFs::new();
        let file = fs.root().create("f", NodeKind::File, None).unwrap();
        assert_eq!(file.flags().unwrap(), 0, "nothing set on a fresh file");

        file.set_flags(FS_IMMUTABLE_FL | FS_NODUMP_FL).unwrap();
        assert_eq!(file.flags().unwrap(), FS_IMMUTABLE_FL | FS_NODUMP_FL);
        // The caller passes the *whole* word - which is what `chattr` does,
        // having read it first and edited one bit - so the word it passes is
        // the word kept, and the old flags do not linger.
        file.set_flags(FS_APPEND_FL).unwrap();
        assert_eq!(file.flags().unwrap(), FS_APPEND_FL);
        file.set_flags(0).unwrap();
        assert_eq!(file.flags().unwrap(), 0);
    }

    /// A flag this cannot keep is refused *and not stored*.
    ///
    /// The counterfactual is the whole point of the refusal: a `SETFLAGS` that
    /// recorded `FS_COMPR_FL` and went on writing uncompressed bytes would be a
    /// caller told something false, and `chattr +c` succeeding while doing
    /// nothing is how a directory someone believed was compressed fills the
    /// disk anyway.
    #[test]
    fn a_flag_this_cannot_keep_is_refused_and_not_stored() {
        use crate::vfs::{FS_COMPR_FL, FS_ENCRYPT_FL, FS_NODUMP_FL};
        let fs = RamFs::new();
        let file = fs.root().create("f", NodeKind::File, None).unwrap();
        assert_eq!(
            file.set_flags(FS_COMPR_FL).err(),
            Some(VfsError::Unsupported)
        );
        assert_eq!(file.flags().unwrap(), 0, "and nothing was recorded");
        // The whole word is checked, not the first bit that cannot be kept.
        assert_eq!(
            file.set_flags(FS_NODUMP_FL | FS_ENCRYPT_FL).err(),
            Some(VfsError::Unsupported)
        );
        assert_eq!(file.flags().unwrap(), 0, "not even the part that could be");
    }

    /// An immutable file refuses the write, the metadata change and the eager
    /// removal - and clearing the flag gives all three back.
    ///
    /// Each refusal is asserted at the layer that owns it rather than through a
    /// syscall, because that is where the rule lives and the syscall layer is
    /// machine-only: [`may_modify`] is what `write`, `chmod`, `chown` and
    /// `utimensat` ask, [`may_alter`] what `truncate` and `linkat` ask, and
    /// [`may_remove`] what `unlink`, `rmdir` and the copy-and-unlink that
    /// `rename` is made of ask.
    #[test]
    fn an_immutable_file_refuses_everything_the_flag_promises() {
        use crate::vfs::{FS_IMMUTABLE_FL, may_alter, may_modify, may_remove};
        let fs = RamFs::new();
        let dir = fs.root();
        let file = dir.create("f", NodeKind::File, None).unwrap();
        let handle = crate::vfs::file::File::new(file.clone());

        // First with no flags set, so each refusal below is the flag's doing
        // and not the fixture's.
        assert_eq!(handle.write(b"x").unwrap(), 1);
        may_modify(&file).unwrap();
        may_alter(&file).unwrap();
        may_remove(&dir, &file).unwrap();

        file.set_flags(FS_IMMUTABLE_FL).unwrap();
        assert_eq!(
            handle.write(b"x").err(),
            Some(VfsError::NotPermitted),
            "a write to an immutable file"
        );
        assert_eq!(may_modify(&file).err(), Some(VfsError::NotPermitted));
        assert_eq!(may_alter(&file).err(), Some(VfsError::NotPermitted));
        assert_eq!(may_remove(&dir, &file).err(), Some(VfsError::NotPermitted));

        file.set_flags(0).unwrap();
        assert_eq!(handle.write(b"x").unwrap(), 1, "and all of it comes back");
        may_modify(&file).unwrap();
        may_alter(&file).unwrap();
        may_remove(&dir, &file).unwrap();
    }

    /// An append-only file takes the writes that go to its end and refuses the
    /// rest - including the one that *could* have been made in the middle.
    #[test]
    fn an_append_only_file_takes_only_appends() {
        use crate::vfs::FS_APPEND_FL;
        use crate::vfs::may_alter;
        let fs = RamFs::new();
        let file = fs.root().create("log", NodeKind::File, None).unwrap();
        let handle = crate::vfs::file::File::new(file.clone());
        file.set_flags(FS_APPEND_FL).unwrap();

        // Without `O_APPEND` the write is refused, which is Linux's
        // `rw_verify_area`: the point of the flag is that the file *grows*, and
        // a write at a stored cursor is what it exists to prevent.
        assert_eq!(
            handle.write(b"x").err(),
            Some(VfsError::NotPermitted),
            "a write with no O_APPEND"
        );
        handle.set_status(crate::vfs::file::O_APPEND as u32);
        assert_eq!(
            handle.write(b"x").unwrap(),
            1,
            "an append is what it is for"
        );
        // And it may not be cut back, whichever way it is asked: this is the
        // one thing the two flags agree about, because shortening a file is
        // exactly the operation `a` exists to prevent.
        assert_eq!(may_alter(&file).err(), Some(VfsError::NotPermitted));
    }

    /// An append-only *directory* takes new names and gives none up.
    #[test]
    fn an_append_only_directory_takes_names_and_gives_none_up() {
        use crate::vfs::{FS_APPEND_FL, may_create_in, may_remove};
        let fs = RamFs::new();
        let dir = fs.root().create("d", NodeKind::Dir, None).unwrap();
        let old = dir.create("old", NodeKind::File, None).unwrap();

        // With no flags, both doors are open.
        may_create_in(&dir).unwrap();
        may_remove(&dir, &old).unwrap();

        dir.set_flags(FS_APPEND_FL).unwrap();
        may_create_in(&dir).unwrap();
        assert_eq!(
            may_remove(&dir, &old).err(),
            Some(VfsError::NotPermitted),
            "a name may be added to a directory that may only be added to, and not taken away"
        );
    }

    /// The whole point of the mount's room: a write that would take the bytes
    /// the machine does not have is refused, not attempted.
    #[test]
    fn a_full_mount_answers_enospc_instead_of_growing() {
        let fs = RamFs::with_usage(Usage::with_limit(4096));
        let root = fs.root();
        let file = root.create("big", NodeKind::File, None).unwrap();
        assert_eq!(file.write_at(0, &vec![0u8; 2048]).unwrap(), 2048);
        assert_eq!(file.write_at(2048, &vec![0u8; 2048]).unwrap(), 2048);
        assert_eq!(file.write_at(4096, &[0u8]).err(), Some(VfsError::NoSpace));
        // The refusal changed nothing: the bytes already there are still there,
        // and the accounting says so.
        assert_eq!(file.metadata().size, 4096);
        assert_eq!(fs_used(&fs), 4096);
    }

    /// A file that gives room back has to give it back on both paths - a
    /// truncate down, and the last reference going away - or the limit becomes a
    /// one-way ratchet that kills the machine a little later instead of sooner.
    #[test]
    fn shrinking_and_unlinking_return_the_room() {
        let usage = Usage::with_limit(4096);
        let fs = RamFs::with_usage(usage.clone());
        let root = fs.root();

        let file = root.create("a", NodeKind::File, None).unwrap();
        file.write_at(0, &vec![7u8; 3000]).unwrap();
        assert_eq!(usage.used(), 3000);
        file.truncate(100).unwrap();
        assert_eq!(usage.used(), 100, "the truncated tail came back");
        // And it is usable again: the same write that would have been refused
        // now fits.
        assert_eq!(file.write_at(100, &vec![7u8; 2900]).unwrap(), 2900);

        // A deleted-but-open file still occupies its room - the bytes are
        // reachable through the descriptor - and only the last reference gives
        // them up.
        let dropped = root.create("b", NodeKind::File, None).unwrap();
        dropped.write_at(0, &vec![0u8; 500]).unwrap();
        root.unlink("b").unwrap();
        assert!(
            usage.used() >= 100,
            "the unlink alone keeps b's bytes alive"
        );
        let before = usage.used();
        drop(dropped);
        assert!(
            usage.used() < before,
            "and letting go of them returns the room"
        );
        let _ = fs_used(&fs);
    }

    fn fs_used(fs: &Arc<RamFs>) -> u64 {
        // Reached through the root's own handle: the accounting is the mount's,
        // and every node carries the one it was created under.
        fs.usage().used()
    }

    /// The chunk store's own contract, because the interesting failures are at
    /// its boundaries: a chunk that stops being the last must become whole
    /// again, and a hole left by a write past the end must read back as zeros.
    #[test]
    fn a_file_is_chunks_and_not_one_contiguous_run() {
        let mut c = Chunked::new();
        c.write_at(0, b"small");
        assert_eq!(c.len(), 5);

        // Grow past the end of the first chunk: the 5-byte chunk is now an
        // interior one and has to read as 64 KiB of which the first five bytes
        // are `small` and the rest zero.
        c.write_at(CHUNK + 10, b"later");
        assert_eq!(c.len(), CHUNK + 15);
        let mut head = [0u8; 5];
        assert_eq!(c.read_at(0, &mut head), 5);
        assert_eq!(&head, b"small");
        let mut hole = [0xffu8; CHUNK];
        assert_eq!(c.read_at(5, &mut hole), CHUNK);
        assert!(hole.iter().all(|&b| b == 0), "the hole reads as zeros");
        let mut tail = [0u8; 5];
        assert_eq!(c.read_at(CHUNK + 10, &mut tail), 5);
        assert_eq!(&tail, b"later");

        // A read spanning both chunks returns all of it at once.
        let mut across = [0u8; 12];
        assert_eq!(c.read_at(CHUNK - 4, &mut across), 12);
        assert!(across[..8].iter().all(|&b| b == 0));

        // Shrinking drops the tail; growing again zero-fills from where it was.
        c.set_len(3);
        assert_eq!(c.len(), 3);
        let mut now = [0u8; 3];
        assert_eq!(c.read_at(0, &mut now), 3);
        assert_eq!(&now, b"sma");
        c.set_len(CHUNK * 2 + 7);
        assert_eq!(c.len(), CHUNK * 2 + 7);
        // Five bytes short of the end, so a nine-byte read stops at five - and
        // all of it is the zero-fill the growth brought in.
        let mut late = [0x5au8; 9];
        assert_eq!(c.read_at(CHUNK * 2 + 2, &mut late), 5);
        assert!(
            late[..5].iter().all(|&b| b == 0),
            "grown bytes read as zeros"
        );

        // Zero length, and the empty write that must not disturb anything.
        c.set_len(0);
        assert_eq!(c.len(), 0);
        assert_eq!(c.read_at(0, &mut now), 0);
        c.write_at(0, b"");
        assert_eq!(c.len(), 0);
    }

    #[test]
    fn create_and_lookup_round_trips_names() {
        let fs = RamFs::new();
        let root = fs.root();
        root.create("etc", NodeKind::Dir, None).unwrap();
        let etc = root.lookup("etc").unwrap();
        assert_eq!(etc.metadata().kind, NodeKind::Dir);
        assert_eq!(root.lookup("nope").err(), Some(VfsError::NoEntry));
    }

    #[test]
    fn creating_an_existing_name_is_an_error_and_keeps_the_old_node() {
        let fs = RamFs::new();
        let root = fs.root();
        let first = root.create("f", NodeKind::File, None).unwrap();
        assert_eq!(
            root.create("f", NodeKind::File, None).err(),
            Some(VfsError::Exists)
        );
        let again = root.lookup("f").unwrap();
        assert!(Arc::ptr_eq(&first, &again));
    }

    #[test]
    fn files_read_and_write_at_offsets_and_track_size() {
        let fs = RamFs::new();
        let file = fs.root().create("f", NodeKind::File, None).unwrap();

        assert_eq!(file.write_at(0, b"hello").unwrap(), 5);
        assert_eq!(file.metadata().size, 5);
        assert_eq!(file.write_at(7, b"!").unwrap(), 1);
        assert_eq!(file.metadata().size, 8);

        let mut buf = [0u8; 16];
        assert_eq!(file.read_at(0, &mut buf).unwrap(), 8);
        assert_eq!(&buf[..8], b"hello\0\0!");
        // A read starting past the end is simply empty.
        assert_eq!(file.read_at(100, &mut buf).unwrap(), 0);
    }

    #[test]
    fn short_reads_report_where_the_file_ends() {
        let fs = RamFs::new();
        let file = fs.root().create("f", NodeKind::File, None).unwrap();
        file.write_at(0, b"abc").unwrap();

        let mut buf = [0u8; 8];
        assert_eq!(file.read_at(0, &mut buf).unwrap(), 3);
        assert_eq!(file.read_at(2, &mut buf).unwrap(), 1);
    }

    #[test]
    fn truncate_shrinks_drops_the_tail_and_grows_a_hole() {
        let fs = RamFs::new();
        let file = fs.root().create("f", NodeKind::File, None).unwrap();
        file.write_at(0, b"0123456789").unwrap();

        file.truncate(4).unwrap();
        assert_eq!(file.metadata().size, 4);
        let mut buf = [0u8; 16];
        assert_eq!(file.read_at(0, &mut buf).unwrap(), 4);
        assert_eq!(&buf[..4], b"0123", "the tail is gone, not just hidden");

        file.truncate(8).unwrap();
        assert_eq!(file.metadata().size, 8);
        assert_eq!(file.read_at(0, &mut buf).unwrap(), 8);
        assert_eq!(&buf[..4], b"0123");
        assert_eq!(&buf[4..8], b"\0\0\0\0", "the new space reads as zeros");

        file.truncate(0).unwrap();
        assert_eq!(file.metadata().size, 0);
        assert_eq!(file.read_at(0, &mut buf).unwrap(), 0);
    }

    #[test]
    fn rmdir_takes_an_empty_directory_and_refuses_the_rest() {
        let fs = RamFs::new();
        let root = fs.root();

        // A file is not a directory, and `rmdir` is not `unlink` by another
        // name: the two have to stay apart or a recursive delete is one typo
        // away from a file.
        root.create("f", NodeKind::File, None).unwrap();
        assert_eq!(root.rmdir("f").err(), Some(VfsError::NotDir));
        assert!(root.lookup("f").is_ok(), "and the file is untouched");

        let dir = root.create("d", NodeKind::Dir, None).unwrap();
        dir.create("inner", NodeKind::File, None).unwrap();
        assert_eq!(root.rmdir("d").err(), Some(VfsError::NotEmpty));
        assert!(root.lookup("d").is_ok(), "the directory is still there");

        // And the converse, from the other side: `unlink` refuses a directory,
        // so neither call can be made to do the other's work by name.
        assert_eq!(root.unlink("d").err(), Some(VfsError::IsDir));
        assert!(root.lookup("d").is_ok(), "the directory is still there");

        dir.unlink("inner").unwrap();
        root.rmdir("d").unwrap();
        assert_eq!(root.lookup("d").err(), Some(VfsError::NoEntry));

        // A name that was never there answers for itself, not for the
        // directory it would have been in.
        assert_eq!(root.rmdir("gone").err(), Some(VfsError::NoEntry));
    }

    #[test]
    fn directories_reject_file_operations() {
        let fs = RamFs::new();
        let root = fs.root();
        assert_eq!(root.read_at(0, &mut []).unwrap_err(), VfsError::Unsupported);
        assert_eq!(root.write_at(0, b"x").unwrap_err(), VfsError::Unsupported);
    }

    #[test]
    fn read_dir_names_every_entry_with_its_kind() {
        let fs = RamFs::new();
        let root = fs.root();
        root.create("b", NodeKind::Dir, None).unwrap();
        root.create("a", NodeKind::File, None).unwrap();

        let listed = root.read_dir().unwrap();
        assert_eq!(listed.len(), 2, "every entry appears exactly once");
        assert!(listed.iter().any(|(n, k)| n == "a" && *k == NodeKind::File));
        assert!(listed.iter().any(|(n, k)| n == "b" && *k == NodeKind::Dir));

        // A file has no entries to list.
        let file = root.lookup("a").unwrap();
        assert_eq!(file.read_dir().unwrap_err(), VfsError::NotDir);
    }

    #[test]
    fn a_second_name_raises_the_link_count_and_unlinking_lowers_it() {
        let fs = RamFs::new();
        let root = fs.root();
        let file = root.create("a", NodeKind::File, None).unwrap();
        assert_eq!(file.nlink(), 1, "a created file has the one name");

        root.link("b", &file).unwrap();
        assert_eq!(file.nlink(), 2, "the second name reaches the same node");
        assert_eq!(
            root.lookup("b").unwrap().nlink(),
            2,
            "and both names report it"
        );

        root.unlink("b").unwrap();
        assert_eq!(file.nlink(), 1, "dropping a name drops the count");

        // A `mknod` placeholder and a bound socket name are counted too: they
        // are nodes like any other, and a link to one is a real second name.
        let dev = root.create("d", NodeKind::CharDev, None).unwrap();
        assert_eq!(dev.nlink(), 1);
        root.link("d2", &dev).unwrap();
        assert_eq!(dev.nlink(), 2);
        let sock = root.create("s", NodeKind::Socket, None).unwrap();
        assert_eq!(sock.nlink(), 1);
        root.link("s2", &sock).unwrap();
        assert_eq!(sock.nlink(), 2);
    }

    #[test]
    fn a_node_that_is_not_a_directory_refuses_names_with_not_dir() {
        // The trait's own default for these answers `Unsupported`, which reads
        // as "this filesystem cannot make that kind of thing". Linux refuses
        // the *walk* at the component that is not a directory, so a `bind` or
        // an `open(O_CREAT)` under a plain file has to hear `ENOTDIR` - and
        // this is the one place the distinction can be pinned.
        let fs = RamFs::new();
        let root = fs.root();
        let file = root.create("f", NodeKind::File, None).unwrap();
        assert_eq!(
            file.create("x", NodeKind::File, None).err(),
            Some(VfsError::NotDir)
        );
        assert_eq!(file.create_tmpfile().err(), Some(VfsError::NotDir));
        assert_eq!(file.symlink("x", "y").err(), Some(VfsError::NotDir));
        assert_eq!(file.unlink("f").err(), Some(VfsError::NotDir));
        assert_eq!(file.rmdir("f").err(), Some(VfsError::NotDir));
        assert_eq!(file.link("x", &file).err(), Some(VfsError::NotDir));

        // A link is refused the same way, and so is a bound socket's name.
        let link = root.symlink("l", "nowhere").unwrap();
        assert_eq!(
            link.create("x", NodeKind::File, None).err(),
            Some(VfsError::NotDir)
        );
        assert_eq!(link.unlink("l").err(), Some(VfsError::NotDir));
        let sock = root.create("s", NodeKind::Socket, None).unwrap();
        assert_eq!(
            sock.create("x", NodeKind::File, None).err(),
            Some(VfsError::NotDir)
        );
        assert_eq!(sock.unlink("s").err(), Some(VfsError::NotDir));
    }

    #[test]
    fn unlink_removes_the_name_and_frees_it_for_reuse() {
        let fs = RamFs::new();
        let root = fs.root();
        let file = root.create("f", NodeKind::File, None).unwrap();
        file.write_at(0, b"x").unwrap();

        root.unlink("f").unwrap();
        assert_eq!(root.lookup("f").err(), Some(VfsError::NoEntry));
        // The name is gone but the node survives behind the Arc, writable.
        assert_eq!(file.write_at(0, b"y").unwrap(), 1);
        // The freed name can be created again, as a fresh node.
        let fresh = root.create("f", NodeKind::File, None).unwrap();
        assert!(!Arc::ptr_eq(&file, &fresh));
        // Removing a name that is not there answers NoEntry.
        assert_eq!(root.unlink("f").err(), None);
        assert_eq!(root.unlink("gone").err(), Some(VfsError::NoEntry));
    }

    #[test]
    fn nodes_carry_a_clock_that_writes_and_utimensat_move() {
        let fs = RamFs::new();
        let root = fs.root();
        let file = root.create("f", NodeKind::File, None).unwrap();

        let born = file.mtime();
        assert_ne!(born, 0, "a new file is not from 1970");
        assert!(root.mtime() >= born, "the directory knows it gained a name");

        file.write_at(0, b"x").unwrap();
        assert!(file.mtime() > born, "a write moves the clock");

        file.set_times(111, 222).unwrap();
        assert_eq!(file.mtime(), 222, "utimensat takes the caller's value");
        // A regular file keeps the three clocks apart now - the read clock is
        // the caller's 111, and the metadata change moved ctime past `born`.
        assert_eq!(file.atime(), 111, "utimensat takes the access clock too");
        assert_ne!(file.ctime(), born, "and stamps the metadata change");

        // A symlink answers with its creation time; it has no write path.
        let link = root.symlink("l", "/f").unwrap();
        assert_ne!(link.mtime(), 0);

        // A symlink, a socket name and a `mknod` device node take a
        // `utimensat` where they used to answer the trait's default
        // `EOPNOTSUPP` - and fold it into the one clock this filesystem keeps,
        // exactly as the file above does (`NOTES.md` §15.35). The refusal, not
        // the folding, was the divergence a caller tripped over.
        for (what, node) in [
            ("symlink", link.clone()),
            ("socket", root.create("s", NodeKind::Socket, None).unwrap()),
            ("device", root.create("d", NodeKind::CharDev, None).unwrap()),
        ] {
            node.set_times(111, 222).unwrap();
            assert_eq!(node.mtime(), 222, "a ramfs {what} takes the caller's value");
            assert_eq!(
                node.atime(),
                222,
                "and folds the atime into it, its one clock"
            );
        }
    }

    /// A directory is born traversable by everyone, and a file is not: the
    /// two kinds start from different defaults, and only one of them is what
    /// makes `/tmp` usable. Counterfactual both ways - give the directory the
    /// file's `0644` and every name below it becomes unreachable for a caller
    /// that is not its owner (measured on the sweep machine as `t-unix`'s bind
    /// under `/tmp/…` answering EACCES before it reached the plain file it was
    /// meant to report); give the *file* the directory's bits and an unpacked
    /// tree arrives world-writable.
    #[test]
    fn a_directory_is_born_traversable_and_a_file_is_not() {
        // The birth mode reads the process-global mask, and this test *sets*
        // it, so it takes its turn with the others that read one.
        let _lock = crate::sched::test_lock::take();
        let fs = RamFs::new();
        let root = fs.root();
        let dir = root.create("d", NodeKind::Dir, None).unwrap();
        let file = root.create("f", NodeKind::File, None).unwrap();
        // Host umask is the default 022, so 0777 -> 0755 and 0644 unchanged.
        assert_eq!(dir.mode(), 0o755, "mkdir(2)'s S_IALLUGO less the mask");
        assert_eq!(file.mode(), 0o644, "a regular file keeps rw-r--r--");
        // The consequence the numbers exist for: uid 1000 may search *and* list
        // the directory - both are the other triad's bits, and `0644` would
        // have neither. That is the difference between a `/tmp` names can be
        // reached through and one they cannot.
        assert_eq!(
            may_as(Caller::ids(1000, 1000), &dir, X_OK | R_OK).ok(),
            Some(())
        );
        // And the mask is honoured rather than hardcoded: at 0077 the same
        // create leaves a directory nobody but its owner can walk into or list.
        crate::sched::set_current_umask(0o077);
        let private = root.create("p", NodeKind::Dir, None).unwrap();
        crate::sched::set_current_umask(crate::abi::DEFAULT_UMASK);
        assert_eq!(private.mode(), 0o700, "the creator's mask decides");
        assert_eq!(
            may_as(Caller::ids(1000, 1000), &private, X_OK | R_OK).err(),
            Some(VfsError::Access)
        );
    }

    /// A socket node: the kind stat reports, the doors it refuses, and the
    /// one clock it carries. `bind(2)` is the only maker; the reads that
    /// answer ENOTCONN are what keep `open(2)`-style callers out.
    #[test]
    fn a_socket_node_is_a_name_with_no_bytes() {
        // Its birth mode is the process-global mask's answer, so it takes its
        // turn with the test that sets one.
        let _lock = crate::sched::test_lock::take();
        let fs = RamFs::new();
        let root = fs.root();
        let sock = root.create("x.sock", NodeKind::Socket, None).unwrap();
        assert_eq!(sock.metadata().kind, NodeKind::Socket);
        assert_ne!(sock.mtime(), 0, "the bind moment is the node's birthday");
        // The birth mode, not an unset word: `create` stamps Linux's default
        // minus the mask (host umask `022` → `0755`), because a stored zero
        // *is* a `chmod 000` to the DAC check, and a socket nobody can even
        // be looked up through is not what `bind` made. `bind_path` restates
        // the same number after this call; here it is the filesystem's own
        // answer that shows.
        assert_eq!(sock.mode(), 0o755, "a socket node is born traversable");
        assert_eq!(
            sock.read_at(0, &mut [0u8; 4]).unwrap_err(),
            VfsError::NotConnected
        );
        assert_eq!(sock.write_at(0, b"x").unwrap_err(), VfsError::NotConnected);
        // The name lives and dies with the directory like any other.
        root.unlink("x.sock").unwrap();
        assert!(root.lookup("x.sock").is_err());
    }

    /// A device node `mknod` made: the number it was made with, the kind it was
    /// made with, and the word the caller stamps afterwards. Three answers this
    /// node used to invent - `st_rdev` was zero for every node, a `S_IFBLK`
    /// node answered as a *character* device, and the mode came from the kind
    /// rather than from the call.
    ///
    /// Measured on a current host (`.workbuddy/probe_mknod_rdev.c`, Linux
    /// 7.2.11, a tmpfs and an ext4 agreeing on every row): `mknod(p,
    /// S_IFCHR|0666, makedev(136, 300))` leaves `st_rdev` `0x0010882c`, which
    /// decodes back to `136:300`; `S_IFBLK` leaves a **block** node; and `0666`
    /// under `umask 027` leaves `0640`.
    #[test]
    fn a_device_node_keeps_the_number_its_kind_and_the_callers_mode() {
        let fs = RamFs::new();
        let root = fs.root();
        let chr = root
            .create("null", NodeKind::CharDev, Some((1, 3)))
            .unwrap();
        let wide = root
            .create("pts-300", NodeKind::CharDev, Some((136, 300)))
            .unwrap();
        let blk = root
            .create("sda", NodeKind::BlockDev, Some((254, 70_000)))
            .unwrap();

        // The pair survives whole - which is the part keeping the *word*
        // instead of the numbers would lose: a minor above eight bits was
        // folded onto another device's number rather than reported as its own.
        assert_eq!(chr.rdev(), Some((1, 3)));
        assert_eq!(wide.rdev(), Some((136, 300)));
        assert_eq!(blk.rdev(), Some((254, 70_000)));
        // And the pair is the answer `st_rdev` is built from rather than the
        // answer itself: the word those two bytes *become* is
        // `abi::new_encode_dev`'s business, and `abi`'s own tests pin every row
        // of it. This node's claim stops at the pair - which is the layer's own
        // edge, the graph a memory filesystem may name (see xtask's depgraph
        // ratchet), and the honest place to stop: the encoding is a question
        // about the ABI, not about a node.
        assert_eq!(chr.rdev(), Some((1, 3)));
        assert_eq!(wide.rdev(), Some((136, 300)));
        assert_eq!(blk.rdev(), Some((254, 70_000)));
        // The two device kinds are two nodes, not one: a name made with
        // `S_IFBLK` reporting a character device to `stat` was a kind nobody
        // asked this kernel for.
        assert_eq!(chr.metadata().kind, NodeKind::CharDev);
        assert_eq!(wide.metadata().kind, NodeKind::CharDev);
        assert_eq!(blk.metadata().kind, NodeKind::BlockDev);
        // The mode: none at birth, the caller's afterwards. There is no default
        // a device node could be born with that would be the caller's answer,
        // so it is born keeping none and takes the word `mknodat` stamps -
        // `0000` included, which is not the same thing as keeping none.
        assert!(!chr.keeps_modes(), "born with no mode of its own");
        chr.set_mode(0o640);
        assert_eq!(chr.mode(), 0o640, "0666 less a caller's 027 mask");
        assert!(chr.keeps_modes());
        blk.set_mode(0);
        assert_eq!(blk.mode(), 0, "a mknod asked for no bits gets none");
        assert!(blk.keeps_modes(), "the word is empty, not absent");
    }

    /// A device node no driver answers for: `open(2)` of it is Linux's `ENXIO`,
    /// and none of the file doors reaches it either.
    ///
    /// Which is what makes the number above honest rather than decorative. On
    /// Linux a number reaches the driver that registered it - measured beside
    /// the rows above (`.workbuddy/probe_mknod_rdev.c`, Linux 7.2.11): in a
    /// tree that permits devices, a node made with `makedev(1, 3)` opens the
    /// **real** `/dev/null`, one made with `makedev(4095, 4095)` answers
    /// `ENXIO`, and a block node nobody owns is `ENXIO` too. Behind every
    /// number this kernel can be given there stands nothing, so `ENXIO` -
    /// [`VfsError::NoDevice`] - is the answer for all of them, `0:0` included:
    /// even the whiteout's number names a device no driver here owns.
    ///
    /// Opening, reading end-of-file and swallowing writes was what this node
    /// did before, which is what `/dev/null` does - so a caller could not tell
    /// the device that is not there from the one that is.
    #[test]
    fn a_device_node_no_driver_answers_for_refuses_every_file_door() {
        let fs = RamFs::new();
        let root = fs.root();
        for (name, kind, rdev) in [
            ("whiteout", NodeKind::CharDev, (0, 0)),
            ("null-shape", NodeKind::CharDev, (1, 3)),
            ("blk-big", NodeKind::BlockDev, (254, 70_000)),
        ] {
            let node = root.create(name, kind, Some(rdev)).unwrap();
            assert_eq!(node.rdev(), Some(rdev));
            assert_eq!(
                node.open_replacement(0).err(),
                Some(VfsError::NoDevice),
                "open(2) of {name} is ENXIO"
            );
            let mut buf = [0u8; 4];
            assert_eq!(
                node.read_at(0, &mut buf).err(),
                Some(VfsError::NoDevice),
                "read(2) of {name}"
            );
            assert_eq!(
                node.write_at(0, b"x").err(),
                Some(VfsError::NoDevice),
                "write(2) of {name}"
            );
        }
        // The one door a name always answers: it can be taken away again.
        assert!(root.unlink("null-shape").is_ok());
        assert!(root.lookup("null-shape").is_err());
    }

    #[test]
    fn deep_trees_nest_through_the_trait_alone() {
        let fs = RamFs::new();
        let root = fs.root();
        let a = root.create("a", NodeKind::Dir, None).unwrap();
        let b = a.create("b", NodeKind::Dir, None).unwrap();
        let f = b.create("f", NodeKind::File, None).unwrap();
        f.write_at(0, b"deep").unwrap();
        assert_eq!(
            root.lookup("a")
                .unwrap()
                .lookup("b")
                .unwrap()
                .lookup("f")
                .unwrap()
                .read_at(0, &mut [0u8; 4])
                .unwrap(),
            4
        );
    }
}
