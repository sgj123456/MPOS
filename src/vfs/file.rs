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
use core::sync::atomic::{AtomicU64, Ordering};

use critical_section::Mutex;

use super::{Inode, Metadata, VfsError, VfsResult, resolve};

/// What an fd number is.
pub type RawFd = i32;

/// An open file description: an inode plus where the next read/write lands.
pub struct File {
    inode: Arc<dyn Inode>,
    pos: AtomicU64,
}

impl File {
    /// A fresh description positioned at the start.
    pub fn new(inode: Arc<dyn Inode>) -> File {
        File {
            inode,
            pos: AtomicU64::new(0),
        }
    }

    pub fn metadata(&self) -> Metadata {
        self.inode.metadata()
    }

    pub fn read(&self, buf: &mut [u8]) -> VfsResult<usize> {
        let pos = self.pos.load(Ordering::Relaxed);
        let n = self.inode.read_at(pos, buf)?;
        self.pos.fetch_add(n as u64, Ordering::Relaxed);
        Ok(n)
    }

    pub fn write(&self, buf: &[u8]) -> VfsResult<usize> {
        let pos = self.pos.load(Ordering::Relaxed);
        let n = self.inode.write_at(pos, buf)?;
        self.pos.fetch_add(n as u64, Ordering::Relaxed);
        Ok(n)
    }
}

/// One process's descriptors: one slot per fd, `None` is free, and slots are
/// reused lowest-first, which is what the fd contract promises callers may
/// assume.
pub struct FdTable {
    slots: Mutex<RefCell<Vec<Option<Arc<File>>>>>,
}

impl FdTable {
    pub fn new() -> FdTable {
        FdTable {
            slots: Mutex::new(RefCell::new(Vec::new())),
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

    /// Add fds 0, 1 and 2 over one inode. Without them the first `pipe()`
    /// would be handed fd 0.
    fn push_standard(&self, tty: Arc<dyn Inode>) {
        for _ in 0..3 {
            self.install(Arc::new(File::new(tty.clone())));
        }
    }

    fn install(&self, file: Arc<File>) -> RawFd {
        critical_section::with(|cs| {
            let mut slots = self.slots.borrow(cs).borrow_mut();
            match slots.iter().position(Option::is_none) {
                Some(slot) => {
                    slots[slot] = Some(file);
                    slot as RawFd
                }
                None => {
                    slots.push(Some(file));
                    (slots.len() - 1) as RawFd
                }
            }
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
            while slots.len() <= slot {
                slots.push(None);
            }
            slots[slot] = Some(file);
        });
        Ok(())
    }

    /// The description `fd` names, for handing to another table.
    pub fn clone_at(&self, fd: RawFd) -> VfsResult<Arc<File>> {
        self.get(fd)
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
    Ok(current_table().install(Arc::new(File::new(inode))))
}

/// [`open`](fn.open.html) with a caller-supplied start for relative paths.
/// `None` is the root, which is what every kernel caller means.
pub fn open_opt(path: &str, start: Option<Arc<dyn Inode>>) -> VfsResult<RawFd> {
    let inode = match start {
        Some(dir) => super::resolve_at(path, dir)?,
        None => resolve(path)?,
    };
    Ok(current_table().install(Arc::new(File::new(inode))))
}

/// Open a fresh description on an inode already in hand - a pipe end, whose
/// channel is anonymous and has no name to resolve.
pub fn open_inode(inode: Arc<dyn Inode>) -> RawFd {
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
        crate::vfs::mount_root(&*RamFs::new());
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
        let fd = a.install(file_on("f"));

        // The same number in another process's table names nothing at all.
        assert_eq!(b.get(fd).err(), Some(VfsError::BadFd));
        b.close(fd).unwrap_err();
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
        assert_eq!(table.install(file_on("f")), 3);
    }

    #[test]
    fn dup2_points_a_second_number_at_the_same_description() {
        let table = FdTable::new();
        let fd = table.install(file_on("f"));

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
}
