//! The Linux file/syscall ABI surface, split along its real seam
//! (PLAN.md §11.2 Step E): the path-carrying handlers in `file`, and the
//! device-`ioctl` entry point in `ioctl`. The two submodules re-export through
//! here, so `crate::syscall::linux`'s dispatch still names `fs::<handler>`
//! byte-for-byte as when it was one `fs.rs`.

#[cfg(target_os = "none")]
use crate::drivers::virtio::input::EvdevNode;
#[cfg(target_os = "none")]
use crate::net::unix::UnixSock;

/// Push the stored `O_NONBLOCK` bit down to where the blocking behaviour
/// actually lives.
///
/// `fcntl(F_SETFL)` and `ioctl(FIONBIO)` both set the flag on the description,
/// but a pipe's and an eventfd's blocking is the *inode's* own: the bit has to
/// be mirrored there too, or a `fcntl(F_SETFL, O_NONBLOCK)` on a plain
/// `pipe()` would read and write exactly as a blocking one. A terminal is the
/// same story with two ends: the slave's bit belongs to its `Tty` and the
/// master's to the pair, and the master is checked *first* because
/// `PtyMaster::as_tty` answers the slave's terminal - it has to, the pair
/// being one terminal for every termios question - so reaching the `as_tty`
/// arm with a master's descriptor would set the wrong end's flag.
pub(crate) fn sync_nonblocking(file: &crate::vfs::File, status: u32) {
    let on = status & crate::vfs::file::O_NONBLOCK != 0;
    if let Some(pty) = file.inode().as_pty() {
        pty.set_master_nonblocking(on);
    } else if let Some(tty) = file.inode().as_tty() {
        tty.set_nonblocking(on);
    } else if let Some(pipe) = file.inode().as_pipe() {
        pipe.set_nonblocking(on);
    } else if let Some(ev) = file.inode().as_any().downcast_ref::<EvdevNode>() {
        ev.set_nonblocking(on);
    } else if let Some(eventfd) = file.inode().as_eventfd() {
        eventfd.set_nonblocking(on);
    } else if let Some(inotify) = file.inode().as_inotify() {
        inotify.set_nonblocking(on);
    } else if let Some(sock) = file.inode().as_any().downcast_ref::<UnixSock>() {
        sock.set_nonblocking(on);
    }
}

mod file;
mod ioctl;

pub(super) use file::*;
pub(super) use ioctl::*;
