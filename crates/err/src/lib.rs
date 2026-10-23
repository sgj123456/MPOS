#![no_std]

//! The errno values the kernel answers with, already negated for `rax`.
//!
//! These numbers are ABI, not policy: `EBADF` is `-9` because Linux says it
//! is, and every layer that has to say "no" in a caller's language needs the
//! same spelling. That is the whole reason the table lives here at the crate
//! root rather than inside `syscall`.
//!
//! The `syscall` layer is the only one that *returns* these numbers to userland
//! through `rax`, so putting them there looked defensible. What it actually did
//! was make the ABI layer a dependency of everyone who refuses: `vfs/epoll.rs`,
//! `vfs/timerfd.rs`, `net/*`, `drivers/virtio/sound.rs` and `uevent.rs` all
//! reached *up* into `syscall` for a minus sign, which is the single most
//! avoidable edge in the tree - a constant has no business needing a dispatch
//! table underneath it. The direction the layering aims for is that nothing
//! below `syscall` names `syscall` at all; a leaf of numbers is the one thing
//! that can satisfy that without a compromise, because it has no behaviour to
//! duplicate and no state to own.
//!
//! `syscall` re-exports the module so the paths its own handlers already use
//! keep working. New callers name `crate::err`.

pub const ENOENT: isize = -2;
pub const ESRCH: isize = -3;
/// EIO: an I/O error. `ptrace` answers it for a request the kernel does
/// not know or a tracee address it cannot reach, which is exactly how Linux
/// spells both - and a debugger distinguishes them from `EFAULT`, which
/// means the *caller's* pointer was bad.
pub const EIO: isize = -5;
pub const ENXIO: isize = -6;
/// ECANCELED: the operation was undone by something outside it. The
/// one user is a `TFD_TIMER_CANCEL_ON_SET` timer whose real-time clock
/// was set underneath it - timerfd_create(2) says the next read(2) fails
/// with exactly this, and a caller distinguishes it from EINVAL, which
/// would mean the buffer it handed over was the problem.
pub const ECANCELED: isize = -125;
/// EACCES: permission denied. What `access(2)`'s family answers for the
/// one check this kernel can refuse - an execute asked of a file whose
/// stored mode has no execute bit - which is the question a shell's
/// `test -x` is really asking.
pub const EACCES: isize = -13;
pub const ENOEXEC: isize = -8;
pub const EBADF: isize = -9;
pub const ECHILD: isize = -10;
pub const EFAULT: isize = -14;
pub const EBUSY: isize = -16;
pub const EEXIST: isize = -17;
/// EAGAIN: try again later. What a `wait` that was asked not to block
/// answers when the child it was asked about is still running.
pub const EAGAIN: isize = -11;
pub const ENOTDIR: isize = -20;
/// ENODEV: no such device. What an `mmap` that asked for a mapping mpos
/// has no way to build - a file-backed one - is answered with.
pub const ENODEV: isize = -19;
pub const EISDIR: isize = -21;
pub const EINVAL: isize = -22;
pub const ENOMEM: isize = -12;
pub const EFBIG: isize = -27;
/// EMFILE: the process has run out of its descriptor allowance - the
/// `RLIMIT_NOFILE` ceiling. Linux's errno for an fd allocation that would
/// land at or above the soft limit, and what `open`/`dup`/`pipe` answer once
/// a program has used up its file descriptors.
pub const EMFILE: isize = -24;
pub const ENOSPC: isize = -28;
/// ENOBUFS: a kernel buffer with a fixed bound is full. rtnetlink's
/// answer for an address or route table that cannot take another entry -
/// the table sizes are compile-time bounds, not tunables.
pub const ENOBUFS: isize = -105;
/// ESPIPE: a seek on something that has no position - a pipe, a socket, a
/// terminal. It is `lseek(2)`'s own answer, and a load-bearing one: a
/// reader that has taken more bytes than it wanted pushes the rest back by
/// seeking, so a kernel that answers "fine" instead makes the caller drop
/// them.
pub const ESPIPE: isize = -29;
pub const ERANGE: isize = -34;
pub const EPIPE: isize = -32;
pub const ENOSYS: isize = -38;
pub const EPERM: isize = -1;
/// EIDRM: the object a System V call named has been removed, and this is the
/// answer to a caller that was already waiting on it. A `semop` asleep on a set
/// another process `IPC_RMID`s wakes with this (measured on a current host,
/// `docs/notes/` §6.14) - it is the one refusal that is about the *object's*
/// death rather than about the caller or the argument.
pub const EIDRM: isize = -43;
pub const EOPNOTSUPP: isize = -95;
pub const EROFS: isize = -30;
/// EXDEV: a hard link across two filesystems. The one request `link(2)`
/// cannot honor by design, and the answer that tells a caller to fall
/// back to copy-and-delete.
pub const EXDEV: isize = -18;
pub const ENOIOCTLCMD: isize = -515;
pub const EWOULDBLOCK: isize = -11;
pub const EINTR: isize = -4;
pub const ENOTTY: isize = -25;
pub const E2BIG: isize = -7;
pub const ENAMETOOLONG: isize = -36;
/// ELOOP: a chain of `#!` interpreters that leads back into itself. The
/// bound that produces it is what ends the chain; nothing else can.
pub const ELOOP: isize = -40;
/// ENOTBLK: a mount source that is not a block device.
pub const ENOTBLK: isize = -15;
/// ENOTEMPTY: `rmdir` on a directory that still names something. Linux
/// has a separate call from `unlink` precisely so that this can be said
/// instead of "the directory was removed, and so was everything in it".
pub const ENOTEMPTY: isize = -39;
// --- socket family ---
/// EMSGSIZE: a datagram is larger than the socket can carry as one packet.
pub const EMSGSIZE: isize = -90;
/// EDESTADDRREQ: a send on an unconnected datagram socket named no address.
pub const EDESTADDRREQ: isize = -89;
/// ENOTSOCK: the descriptor names something that is not a socket.
pub const ENOTSOCK: isize = -88;
/// ENOPROTOOPT: the option level or name is not one the kernel knows.
pub const ENOPROTOOPT: isize = -92;
/// EPROTONOSUPPORT: the protocol number does not match the socket type.
pub const EPROTONOSUPPORT: isize = -93;
/// ESOCKTNOSUPPORT: the socket type itself is one the kernel does not know.
pub const ESOCKTNOSUPPORT: isize = -94;
/// EAFNOSUPPORT: the address family is not one this kernel speaks. What
/// musl's syslog gets for its `AF_UNIX` probe, so it falls back to writing
/// the console directly.
pub const EAFNOSUPPORT: isize = -97;
/// EADDRINUSE: the local address is already bound by another socket.
pub const EADDRINUSE: isize = -98;
/// EADDRNOTAVAIL: the requested local address cannot be assigned here.
pub const EADDRNOTAVAIL: isize = -99;
/// ENETDOWN: the network interface is not up.
pub const ENETDOWN: isize = -100;
/// ENETUNREACH: the network named is not reachable from here. What the
/// IPv6 stub answers at `connect`, so a caller walking a getaddrinfo
/// list falls through to the IPv4 answer the way Linux lets it.
pub const ENETUNREACH: isize = -101;
/// EHOSTUNREACH: a route exists but no host answered for it. The raw-IP
/// send's answer when an ARP probe for the next hop goes unanswered: the
/// frame did not leave, and this kernel has nowhere to queue it for a
/// later retry the way Linux's async neighbour path does (`docs/notes/`
/// §4.5).
pub const EHOSTUNREACH: isize = -113;
/// ENOTCONN: the socket is not connected, and the call needs it to be.
pub const ENOTCONN: isize = -107;
/// EISCONN: the socket is already connected, and `connect` was asked again.
pub const EISCONN: isize = -106;
/// ESHUTDOWN: the socket has been shut down for that direction.
pub const ESHUTDOWN: isize = -108;
/// ETIMEDOUT: the peer never answered in time.
pub const ETIMEDOUT: isize = -110;
/// ECONNREFUSED: the peer reset the connection attempt.
pub const ECONNREFUSED: isize = -111;
/// EPROTOTYPE: the destination socket carries a different kind of data than
/// the caller's - `connect` on a path name whose socket is a different
/// `SOCK_*` type. Linux's `unix_find_other` answers this for a filesystem
/// name, while an abstract name misses its type-keyed lookup and reports
/// `ECONNREFUSED` instead (both measured on a current host).
pub const EPROTOTYPE: isize = -91;
/// ECONNRESET: an established connection was reset by the peer, or by this
/// side's own `SO_LINGER{on, 0}` close. A read of such a socket reports this,
/// where one that never carried a connection reports `ENOTCONN` - the two are
/// different answers to different questions, and both measured on this host's
/// kernel (`docs/notes/` §4.3).
pub const ECONNRESET: isize = -104;
/// EALREADY: a nonblocking connect is still in flight.
pub const EALREADY: isize = -114;
/// EINPROGRESS: a nonblocking connect has been started and is not done.
pub const EINPROGRESS: isize = -115;
