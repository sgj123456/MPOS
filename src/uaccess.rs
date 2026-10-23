//! The userland-boundary primitives: validate a caller's pointer, and turn a
//! filesystem error into the errno a caller reads.
//!
//! These two helpers were the last thing that made an ABI-layer module a
//! dependency of layers that sit *below* it. `net/*` and `drivers/drm/` copy
//! bytes to and from a caller's buffer, so they must ask "is this range the
//! caller's, and - for a fill - writable?"; and they turn a `VfsError` into the
//! negative errno userland expects. Neither question is a *syscall* concern - it
//! is a question about a user pointer, identical whether the answer is reached
//! through `read`, `sendmsg` or an ioctl - yet the helpers happened to live in
//! `syscall`, so every below-`syscall` caller had to reach *up* into it. That is
//! the inversion `ARCHITECTURE.md` §11.1.5 (V-9) records, and the reason they
//! now live here at the crate root: a leaf has no dispatch table underneath it
//! to drag along, and the errno table (`crate::err`) was given the same
//! treatment for the same reason.
//!
//! The function bodies are unchanged by the move - the semantics are the
//! contract, and a refactor must not quietly edit them. `syscall` re-exports
//! them so the paths its own handlers already use keep working; new callers
//! outside the ABI layer name `crate::uaccess`.

/// Upper bound on a single read or write.
pub(crate) const MAX_IO: usize = 1 << 20;

/// Map a VFS error onto the errno [`crate::syscall::linux::dispatch`] returns.
#[cfg(target_os = "none")]
pub(crate) fn err_of(e: crate::vfs::VfsError) -> isize {
    use crate::err;
    use crate::vfs::VfsError;
    match e {
        VfsError::NoEntry => err::ENOENT,
        VfsError::NotDir => err::ENOTDIR,
        VfsError::IsDir => err::EISDIR,
        VfsError::Exists => err::EEXIST,
        VfsError::NotEmpty => err::ENOTEMPTY,
        VfsError::Busy => err::EBUSY,
        VfsError::BadFd => err::EBADF,
        VfsError::NoRoot | VfsError::Invalid => err::EINVAL,
        VfsError::NoSpace => err::ENOSPC,
        VfsError::FileTooBig => err::EFBIG,
        VfsError::BrokenPipe => err::EPIPE,
        VfsError::Unsupported => err::EOPNOTSUPP,
        // EROFS: what Linux answers a write to a filesystem mounted
        // read-only, and what userland checks for before remounting.
        VfsError::ReadOnly => err::EROFS,
        // EIO: damage the filesystem can see. Reported rather than panicked on
        // for the same reason Linux reports it - a caller can be told, and a
        // machine that stops on one bad directory entry cannot tell anyone.
        VfsError::Corrupt => err::EIO,
        // EXDEV: a hard link whose two ends live on different filesystems.
        VfsError::CrossDevice => err::EXDEV,
        // EAGAIN, which is also EWOULDBLOCK: they are one errno here.
        VfsError::Again => err::EWOULDBLOCK,
        // EMFILE: the descriptor allowance is spent.
        VfsError::TooManyDescriptors => err::EMFILE,
        // ECANCELED: a timerfd whose CANCEL_ON_SET clock was set - the
        // read that would have counted its expirations reports the undoing
        // instead, which is timerfd_create(2)'s wording of the case.
        VfsError::Canceled => err::ECANCELED,
        // ENXIO: the device's name answers nothing for this caller. `/dev/tty`
        // without a controlling terminal is the whole case - POSIX's own
        // answer, and the one a shell reads as "job control off".
        // EACCES and EPERM, the two answers a permission check gives: the
        // first says the caller's identity may not, the second says the
        // operation is refused for a reason the bits cannot express.
        VfsError::Access => err::EACCES,
        VfsError::NotPermitted => err::EPERM,
        VfsError::NoDevice => err::ENXIO,
        // ENODEV: the descriptor's access to its device was revoked by
        // `EVIOCREVOKE`. A different fact from `NoDevice` - the device is there,
        // this reader is no longer allowed to it - but the same errno, because
        // Linux's `evdev_read`/`evdev_ioctl` answer a revoked client with
        // `ENODEV` and a caller branches on the number, not the reason.
        VfsError::Revoked => err::ENODEV,
        // ENOTCONN: the door is a socket's, and no connection stands
        // behind it - Linux's read(2) answer on an unconnected socket.
        VfsError::NotConnected => err::ENOTCONN,
        // ECONNREFUSED: a connected datagram's peer is gone. Linux answers
        // that send once with ECONNREFUSED and forgets the connection.
        VfsError::ConnectionRefused => err::ECONNREFUSED,
        // EINTR: the node was about to block and a catchable signal ended the
        // wait instead, having consumed nothing. This is what lets a full-screen
        // program's own timeout (SIGALRM from `alarm`/`setitimer`) or a resize
        // (SIGWINCH) break a terminal read - the measured Linux answer, and the
        // reason `vim` stops redrawing on a kernel where a blocked read sleeps
        // through the signal instead.
        VfsError::Interrupted => err::EINTR,
    }
}

/// Validate a user buffer: length within bounds and pages accessible.
pub(crate) fn buffer_ok(buf: usize, len: usize, write: bool, from_user: bool) -> bool {
    len <= MAX_IO && (!from_user || user_accessible(buf, len, write))
}

/// The page-walk half of [`buffer_ok`]: is the range mapped and, for a read
/// performed by the kernel on the caller's behalf, writable?
///
/// On the machine this is the page table's answer. The host build has no
/// page tables - the tests that reach this layer hand it real memory and
/// call with `from_user = false` where the distinction matters - so the stub
/// answers "accessible" and the length bound above stays the real gate.
#[cfg(target_os = "none")]
fn user_accessible(buf: usize, len: usize, write: bool) -> bool {
    crate::hal::user_accessible(buf, len, write)
}

#[cfg(not(target_os = "none"))]
fn user_accessible(_buf: usize, _len: usize, _write: bool) -> bool {
    true
}
