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
use core::sync::atomic::{AtomicU64, Ordering};

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
#[cfg(target_os = "none")]
fn now() -> u64 {
    crate::arch::rtc::boot_epoch()
        + crate::arch::timer::ticks() / crate::arch::timer::TICK_HZ as u64
}

#[cfg(not(target_os = "none"))]
fn now() -> u64 {
    use core::sync::atomic::AtomicU32;
    static HOST_NOW: AtomicU32 = AtomicU32::new(1_700_000_000);
    HOST_NOW.fetch_add(1, Ordering::Relaxed) as u64
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

/// A file: bytes with no structure of their own.
struct RamFile {
    /// The mount these bytes are charged to. Not the filesystem's: a file
    /// outlives its directory's unlink, and the room is only back when *this*
    /// reference goes.
    fs: Arc<Usage>,
    ino: Ino,
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
    /// The one clock this filesystem keeps, in unix seconds - stamped at
    /// creation and moved by every write. `stat`'s atime and ctime report it
    /// too, which is the trait's stated answer for a filesystem with a single
    /// clock.
    mtime: Mutex<Cell<u64>>,
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

impl Inode for RamFile {
    fn metadata(&self) -> Metadata {
        Metadata {
            ino: self.ino,
            kind: NodeKind::File,
            size: critical_section::with(|cs| self.data.borrow(cs).borrow().len() as u64),
        }
    }

    fn mode(&self) -> u32 {
        critical_section::with(|cs| self.mode.borrow(cs).get()).unwrap_or(0)
    }

    fn set_mode(&self, mode: u32) {
        critical_section::with(|cs| self.mode.borrow(cs).set(Some(mode)));
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

    /// `utimensat`: the atime is accepted and dropped - one clock stands for
    /// all three here - and the mtime is what `stat` will report.
    fn set_times(&self, _atime: u64, mtime: u64) -> VfsResult<()> {
        critical_section::with(|cs| self.mtime.borrow(cs).set(mtime));
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
        let grows_to = (offset as usize).max(0) + buf.len();
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
        });
        Ok(())
    }
}

/// A directory: name edges, aka the dentries of this filesystem.
/// A symbolic link: nothing but the path it stands for.
struct RamSymlink {
    ino: Ino,
    /// The moment the link was made - all the clock a link, which has no
    /// contents to rewrite, can have.
    mtime: u64,
    target: String,
}

impl Inode for RamSymlink {
    fn metadata(&self) -> Metadata {
        Metadata {
            ino: self.ino,
            kind: NodeKind::Symlink,
            size: self.target.len() as u64,
        }
    }

    fn mtime(&self) -> u64 {
        self.mtime
    }

    fn readlink(&self) -> VfsResult<String> {
        Ok(self.target.clone())
    }
}

/// A device node a `mknod` made: a name carrying a device kind.
///
/// No driver stands behind it - on this machine the nodes an initramfs's init
/// pre-seeds (`/dev/null`, `/dev/ptmx`) are placeholders that live exactly as
/// long as it takes the `devtmpfs` mount to cover them with real devices.
/// Reads answer end of file and writes vanish, which is the honest behaviour
/// of a null device and a harmless one for anything nobody has opened yet.
struct RamDev {
    ino: Ino,
}

impl Inode for RamDev {
    fn metadata(&self) -> Metadata {
        Metadata {
            ino: self.ino,
            kind: NodeKind::CharDev,
            size: 0,
        }
    }

    fn read_at(&self, _offset: u64, _buf: &mut [u8]) -> VfsResult<usize> {
        Ok(0)
    }

    fn write_at(&self, _offset: u64, buf: &[u8]) -> VfsResult<usize> {
        Ok(buf.len())
    }
}

/// A socket node: a name `bind(2)` made for a connection endpoint. It
/// carries the namespace presence - stat type, directory listing, unlink -
/// and deliberately no bytes: reading the node through the file doors is
/// the question a socket never answers, and `open(2)` on the path gets
/// Linux's `ENXIO` from the read/write refusals below.
struct RamSocket {
    ino: Ino,
    /// The moment `bind` planted the name; a socket's own clock has nothing
    /// else to say about it.
    mtime: u64,
    mode: Mutex<Cell<Option<u32>>>,
    owner: Mutex<Cell<(u32, u32)>>,
}

impl Inode for RamSocket {
    fn metadata(&self) -> Metadata {
        Metadata {
            ino: self.ino,
            kind: NodeKind::Socket,
            size: 0,
        }
    }

    fn mtime(&self) -> u64 {
        self.mtime
    }

    fn mode(&self) -> u32 {
        critical_section::with(|cs| self.mode.borrow(cs).get()).unwrap_or(0)
    }

    fn set_mode(&self, mode: u32) {
        critical_section::with(|cs| self.mode.borrow(cs).set(Some(mode)));
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
    fn open_replacement(&self) -> VfsResult<Option<Arc<dyn Inode>>> {
        Err(VfsError::NoDevice)
    }
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
    entries: Mutex<RefCell<BTreeMap<String, Arc<dyn Inode>>>>,
}

impl Inode for RamDir {
    fn metadata(&self) -> Metadata {
        Metadata {
            ino: self.ino,
            kind: NodeKind::Dir,
            size: 0,
        }
    }

    fn mode(&self) -> u32 {
        critical_section::with(|cs| self.mode.borrow(cs).get()).unwrap_or(0)
    }

    fn set_mode(&self, mode: u32) {
        critical_section::with(|cs| self.mode.borrow(cs).set(Some(mode)));
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

    fn create(&self, name: &str, kind: NodeKind) -> VfsResult<Arc<dyn Inode>> {
        let at = now();
        let node: Arc<dyn Inode> = match kind {
            NodeKind::File => Arc::new(RamFile {
                fs: self.fs.clone(),
                ino: fresh_ino(),
                mode: Mutex::new(Cell::new(None)),
                owner: Mutex::new(Cell::new(crate::sched::current_owner())),
                mtime: Mutex::new(Cell::new(at)),
                data: Mutex::new(RefCell::new(Chunked::new())),
            }),
            NodeKind::Dir => Arc::new(RamDir {
                fs: self.fs.clone(),
                ino: fresh_ino(),
                mode: Mutex::new(Cell::new(None)),
                owner: Mutex::new(Cell::new(crate::sched::current_owner())),
                mtime: Mutex::new(Cell::new(at)),
                entries: Mutex::new(RefCell::new(BTreeMap::new())),
            }),
            // A device node mknod made: a placeholder until a devtmpfs mount
            // covers the directory with driver-backed devices.
            NodeKind::CharDev | NodeKind::BlockDev => Arc::new(RamDev { ino: fresh_ino() }),
            // A socket name, made by `bind` alone. Its clock is the bind;
            // `connect` looks up the node's identity, never its bytes.
            NodeKind::Socket => Arc::new(RamSocket {
                ino: fresh_ino(),
                mtime: now(),
                mode: Mutex::new(Cell::new(None)),
                owner: Mutex::new(Cell::new(crate::sched::current_owner())),
            }),
            // A link is made with `Inode::symlink`, which carries the one thing
            // a link cannot exist without: what it points at.
            NodeKind::Symlink => return Err(VfsError::Unsupported),
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
        let node: Arc<dyn Inode> = Arc::new(RamSymlink {
            ino: fresh_ino(),
            mtime: at,
            target: String::from(target),
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
        })
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
        })
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
                owner: Mutex::new(Cell::new(crate::sched::current_owner())),
                mtime: Mutex::new(Cell::new(now())),
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
    use crate::vfs::{FileSystem, NodeKind, VfsError};

    /// The whole point of the mount's room: a write that would take the bytes
    /// the machine does not have is refused, not attempted.
    #[test]
    fn a_full_mount_answers_enospc_instead_of_growing() {
        let fs = RamFs::with_usage(Usage::with_limit(4096));
        let root = fs.root();
        let file = root.create("big", NodeKind::File).unwrap();
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

        let file = root.create("a", NodeKind::File).unwrap();
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
        let dropped = root.create("b", NodeKind::File).unwrap();
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
        root.create("etc", NodeKind::Dir).unwrap();
        let etc = root.lookup("etc").unwrap();
        assert_eq!(etc.metadata().kind, NodeKind::Dir);
        assert_eq!(root.lookup("nope").err(), Some(VfsError::NoEntry));
    }

    #[test]
    fn creating_an_existing_name_is_an_error_and_keeps_the_old_node() {
        let fs = RamFs::new();
        let root = fs.root();
        let first = root.create("f", NodeKind::File).unwrap();
        assert_eq!(
            root.create("f", NodeKind::File).err(),
            Some(VfsError::Exists)
        );
        let again = root.lookup("f").unwrap();
        assert!(Arc::ptr_eq(&first, &again));
    }

    #[test]
    fn files_read_and_write_at_offsets_and_track_size() {
        let fs = RamFs::new();
        let file = fs.root().create("f", NodeKind::File).unwrap();

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
        let file = fs.root().create("f", NodeKind::File).unwrap();
        file.write_at(0, b"abc").unwrap();

        let mut buf = [0u8; 8];
        assert_eq!(file.read_at(0, &mut buf).unwrap(), 3);
        assert_eq!(file.read_at(2, &mut buf).unwrap(), 1);
    }

    #[test]
    fn truncate_shrinks_drops_the_tail_and_grows_a_hole() {
        let fs = RamFs::new();
        let file = fs.root().create("f", NodeKind::File).unwrap();
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
        root.create("f", NodeKind::File).unwrap();
        assert_eq!(root.rmdir("f").err(), Some(VfsError::NotDir));
        assert!(root.lookup("f").is_ok(), "and the file is untouched");

        let dir = root.create("d", NodeKind::Dir).unwrap();
        dir.create("inner", NodeKind::File).unwrap();
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
        root.create("b", NodeKind::Dir).unwrap();
        root.create("a", NodeKind::File).unwrap();

        let listed = root.read_dir().unwrap();
        assert_eq!(listed.len(), 2, "every entry appears exactly once");
        assert!(listed.iter().any(|(n, k)| n == "a" && *k == NodeKind::File));
        assert!(listed.iter().any(|(n, k)| n == "b" && *k == NodeKind::Dir));

        // A file has no entries to list.
        let file = root.lookup("a").unwrap();
        assert_eq!(file.read_dir().unwrap_err(), VfsError::NotDir);
    }

    #[test]
    fn unlink_removes_the_name_and_frees_it_for_reuse() {
        let fs = RamFs::new();
        let root = fs.root();
        let file = root.create("f", NodeKind::File).unwrap();
        file.write_at(0, b"x").unwrap();

        root.unlink("f").unwrap();
        assert_eq!(root.lookup("f").err(), Some(VfsError::NoEntry));
        // The name is gone but the node survives behind the Arc, writable.
        assert_eq!(file.write_at(0, b"y").unwrap(), 1);
        // The freed name can be created again, as a fresh node.
        let fresh = root.create("f", NodeKind::File).unwrap();
        assert!(!Arc::ptr_eq(&file, &fresh));
        // Removing a name that is not there answers NoEntry.
        assert_eq!(root.unlink("f").err(), None);
        assert_eq!(root.unlink("gone").err(), Some(VfsError::NoEntry));
    }

    #[test]
    fn nodes_carry_a_clock_that_writes_and_utimensat_move() {
        let fs = RamFs::new();
        let root = fs.root();
        let file = root.create("f", NodeKind::File).unwrap();

        let born = file.mtime();
        assert_ne!(born, 0, "a new file is not from 1970");
        assert!(root.mtime() >= born, "the directory knows it gained a name");

        file.write_at(0, b"x").unwrap();
        assert!(file.mtime() > born, "a write moves the clock");

        file.set_times(111, 222).unwrap();
        assert_eq!(file.mtime(), 222, "utimensat takes the caller's value");
        assert_eq!(file.atime(), 222, "one clock stands for the read clock too");

        // A symlink answers with its creation time; it has no write path.
        let link = root.symlink("l", "/f").unwrap();
        assert_ne!(link.mtime(), 0);
    }

    /// A socket node: the kind stat reports, the doors it refuses, and the
    /// one clock it carries. `bind(2)` is the only maker; the reads that
    /// answer ENOTCONN are what keep `open(2)`-style callers out.
    #[test]
    fn a_socket_node_is_a_name_with_no_bytes() {
        let fs = RamFs::new();
        let root = fs.root();
        let sock = root.create("x.sock", NodeKind::Socket).unwrap();
        assert_eq!(sock.metadata().kind, NodeKind::Socket);
        assert_ne!(sock.mtime(), 0, "the bind moment is the node's birthday");
        assert_eq!(
            sock.mode(),
            0,
            "no mode was said yet - stat fills the default"
        );
        assert_eq!(
            sock.read_at(0, &mut [0u8; 4]).unwrap_err(),
            VfsError::NotConnected
        );
        assert_eq!(sock.write_at(0, b"x").unwrap_err(), VfsError::NotConnected);
        // The name lives and dies with the directory like any other.
        root.unlink("x.sock").unwrap();
        assert!(root.lookup("x.sock").is_err());
    }

    #[test]
    fn deep_trees_nest_through_the_trait_alone() {
        let fs = RamFs::new();
        let root = fs.root();
        let a = root.create("a", NodeKind::Dir).unwrap();
        let b = a.create("b", NodeKind::Dir).unwrap();
        let f = b.create("f", NodeKind::File).unwrap();
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
