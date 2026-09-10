//! Open file descriptions and the fd table.
//!
//! A [`File`] pairs an inode with a cursor. It is the thing fds name; several
//! fds can share one `File` (fork semantics, at M9), and several `File`s can
//! share one inode (independent cursors over the same bytes).
//!
//! The table is one global array for now, because every task is a kernel task
//! and every fd means the same thing. M9 gives processes tables of their own;
//! the `File` type will not change.

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

/// One slot per fd; `None` is free. Slots are reused lowest-first, which is
/// what the fd contract promises callers may assume.
static FDS: Mutex<RefCell<Vec<Option<Arc<File>>>>> = Mutex::new(RefCell::new(Vec::new()));

fn alloc(file: Arc<File>) -> RawFd {
    critical_section::with(|cs| {
        let mut fds = FDS.borrow(cs).borrow_mut();
        match fds.iter().position(Option::is_none) {
            Some(slot) => {
                fds[slot] = Some(file);
                slot as RawFd
            }
            None => {
                fds.push(Some(file));
                (fds.len() - 1) as RawFd
            }
        }
    })
}

fn take(fd: RawFd) -> VfsResult<Arc<File>> {
    critical_section::with(|cs| {
        FDS.borrow(cs)
            .borrow()
            .get(usize::try_from(fd).unwrap_or(usize::MAX))
            .cloned()
            .flatten()
            .ok_or(VfsError::BadFd)
    })
}

/// Resolve `path` and open a fresh description on it.
pub fn open(path: &str) -> VfsResult<RawFd> {
    let inode = resolve(path)?;
    Ok(alloc(Arc::new(File::new(inode))))
}

/// Put the standard descriptors in the table: three open descriptions of the
/// console tty, fds 0 through 2, one cursor each. Every POSIX program assumes
/// they exist before it runs - and without them, the first `pipe()` would be
/// handed fd 0. Call once, at start-up, before anything opens a file.
pub fn init_standard(tty: Arc<dyn Inode>) {
    for _ in 0..3 {
        alloc(Arc::new(File::new(tty.clone())));
    }
}

/// Open a fresh description on an inode already in hand - a pipe end, whose
/// channel is anonymous and has no name to resolve.
pub fn open_inode(inode: Arc<dyn Inode>) -> RawFd {
    alloc(Arc::new(File::new(inode)))
}

/// Read at the description's cursor, advancing it.
pub fn read(fd: RawFd, buf: &mut [u8]) -> VfsResult<usize> {
    take(fd)?.read(buf)
}

/// Write at the description's cursor, advancing it.
pub fn write(fd: RawFd, buf: &[u8]) -> VfsResult<usize> {
    take(fd)?.write(buf)
}

/// Release an fd. The description dies when the last fd naming it does.
pub fn close(fd: RawFd) -> VfsResult<()> {
    critical_section::with(|cs| {
        let mut fds = FDS.borrow(cs).borrow_mut();
        let slot = usize::try_from(fd).map_err(|_| VfsError::BadFd)?;
        fds.get_mut(slot)
            .filter(|entry| entry.is_some())
            .ok_or(VfsError::BadFd)?
            .take();
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::{File, close, open, read, write};
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
}
