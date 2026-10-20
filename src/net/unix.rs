//! AF_UNIX path sockets: the bind table, the listener, the handshake, and
//! the datagram queue.
//!
//! A dbus daemon owns `/run/dbus/system_bus_socket`; busybox syslog owns
//! `/dev/log`; an X client finds the server through `/tmp/.X11-unix/X0`.
//! Neither relationship begins at a network address - it begins at a *name
//! in the filesystem*, planted by `bind(2)` and answered by `connect(2)`
//! or a `sendto`. That is this module's whole subject: the steps between
//! `socket(AF_UNIX, ...)` and a working channel, which `socketpair`
//! already provides by other means.
//!
//! The division of labor follows that fact. A *connected* unix stream is
//! the same anonymous byte channel a socketpair end is - a
//! [`DuplexEnd`](crate::vfs::pipe::DuplexEnd), whose read/write/poll/
//! half-close machinery is not duplicated here at all. A *datagram* socket
//! is not a channel at all: it is a name and a queue - messages arrive
//! whole from anywhere the name resolves, and leave whole to the peer a
//! `connect` pinned - so it keeps its own [`Dgram`] queue here rather
//! than borrowing the pipe's. What this module owns either way is the
//! state before bytes move: the [`UnixSock`] an fd names, the registry
//! that maps a bound node's identity `(dev, ino)` to the socket listening
//! behind it, and the accept queue a `connect` feeds and an `accept`
//! drains. The registry keys on node identity for the same reason the lock
//! table and inotify watches do (`src/vfs/locks.rs`, `src/vfs/inotify.rs`):
//! a path can be renamed over or unlinked and rebound, and only the
//! inode's own number says which socket answered.
//!
//! Why none of this lives in the network registry (`src/net/mod.rs`): that
//! registry is smoltcp's, and it answers `ENETDOWN` before a machine has a
//! network card. AF_UNIX is the family a machine with no network at all
//! still runs dbus on; netlink took the same detour for the same reason
//! (`socket.rs`'s uevent socket).
//!
//! Network namespaces split this module's two tables, and the split is not
//! the one the family's name suggests. An **abstract** name belongs to the
//! namespace of the socket that holds it: a name planted in one is
//! `ECONNREFUSED` from a socket in another, and the same name binds again
//! there. A **path** is a filesystem object and is not namespaced at all - a
//! socket bound at a path answers that path from every namespace, both
//! directions. And what keys the abstract half is the *socket's* namespace
//! rather than the calling task's, so a socket made before an `unshare` goes
//! on binding and dialling where it was made. Measured, not read off a page:
//! `.workbuddy/probe_unix_netns{,2,3}.c`, whose transcript is `docs/notes/`
//! §6.3. `by_name` carries the namespace; `by_node` deliberately does not.
//!
//! Deliberate departures from Linux, each visible from userland:
//! * `SOCK_SEQPACKET` over AF_UNIX opens - udevd cannot start without it -
//!   but carries no message boundaries until a `socketpair` connects it: a
//!   *named* one is served as the byte stream this family has (the type's
//!   own note in `net::socket` tells the trade).
//! * the abstract namespace (`sun_path[0] == '\0'`) is **served** for the
//!   names a caller spells, and for the `addrlen` that spells none at all
//!   (`bind` makes a name up, the way Linux's `unix_autobind` does). It was
//!   refused until `xwayland-satellite` - which binds
//!   `@/tmp/.X11-unix/X0` - reached for it, which is the measurement that
//!   retired the old reason ("no target daemon reaches for it"); the rules
//!   come from `.workbuddy/probe_unix_abstract.c` rather than from a man
//!   page. Two narrow rules remain this kernel's own: a path with bytes
//!   *after* its terminator is `EINVAL` here where Linux ignores them, and
//!   the name is `String`-free bytes for the abstract case but a `String`
//!   for a path, so the *path* half still refuses non-UTF-8.
//! * a bound *stream* socket that was never `listen`ed answers
//!   `ECONNREFUSED`; Linux's implicit rendezvous at a socket nobody
//!   listened on is state this kernel declines to model, and both daemons
//!   call `listen`. A `listen` with a *zero* backlog is a different thing
//!   and is modelled: Linux's admission test is `queue_len > backlog`, so
//!   `listen(fd, 0)` is a queue of one - which is the call eudev's control
//!   socket makes ([`admits_one_more`] has the measurement). A
//!   bound *datagram* socket needs no `listen` - there is nothing to
//!   queue up to; its queue is open from the bind.
//! * a datagram queue holds [`RECVQ_MAX`] messages and refuses more with
//!   `EAGAIN` to the sender; Linux grows the queue against the receiver's
//!   socket buffer, and one fixed, generous cap is this kernel's shape.
//! * a socket path on ext4 cannot be bound (`EOPNOTSUPP` at the node
//!   level): the on-disk inode type is unwired, and `/run` and
//!   `/tmp/.X11-unix` - where every real socket lives - are tmpfs.
//! * `connect(2)` never reports `EINPROGRESS`: over a byte pipe the
//!   handshake is not a process, it is a push.

use alloc::collections::{BTreeMap, VecDeque};
use alloc::string::{String, ToString};
use alloc::sync::{Arc, Weak};
use core::cell::RefCell;

use critical_section::Mutex;

use crate::sched::TaskId;
use crate::vfs::pipe::DuplexEnd;
use crate::vfs::{Ino, Inode, Metadata, NodeKind, Ready, VfsError, VfsResult};

/// The largest backlog Linux's `SOMAXCONN` clamps a `listen` to.
pub const SOMAXCONN: usize = 4096;
/// The size of `struct sockaddr_un`'s `sun_path`, `linux/un.h`.
pub const SUN_PATH_MAX: usize = 108;

/// `struct ucred`, the credential payload `SO_PEERCRED` and the
/// `SCM_CREDENTIALS` control message carry. Linux fills `pid` with the peer's
/// thread-group id, and `uid`/`gid` with its *real* ids.
///
/// These are the numbers the machine **stores** - the parent namespace's
/// numbering, where a task's `pid` is its own slot. What a reader sees is this
/// mapped through the reader's own namespaces at the render, which is
/// `socket.rs`'s `observed_cred`: the ids through the reader's **user**
/// namespace and the `pid` through its **pid** namespace (0 when the peer is
/// not a member of it). A task that never unshares is in the identity map of
/// both and reads exactly these back.
#[derive(Clone, Copy, Default)]
pub struct UCred {
    pub pid: u32,
    pub uid: u32,
    pub gid: u32,
}

/// The running task's credentials, in the shape `ucred` wants. Read outside any
/// socket lock - the scheduler table wants the same critical section, so
/// capturing this first and carrying the plain words in is what keeps the two
/// locks from nesting. On the host there is no task table, so the answer is the
/// zero the queue logic never depends on.
pub(crate) fn self_cred() -> UCred {
    #[cfg(target_os = "none")]
    {
        let (uid, gid) = crate::sched::current_real_owner();
        UCred {
            pid: crate::sched::current_tgid().0 as u32,
            uid,
            gid,
        }
    }
    #[cfg(not(target_os = "none"))]
    {
        UCred::default()
    }
}

// ---------------------------------------------------------------------------
// One AF_UNIX socket, as the fd table sees it
// ---------------------------------------------------------------------------

/// A unix socket: one descriptor's worth of state for `socket`, `bind`,
/// `listen`, `connect`, `accept`, `getsockname` and the file doors to ask
/// about. The connected channel delegates its whole byte face to the
/// `DuplexEnd` it holds; the unconnected states answer what the man pages
/// say an unconnected socket answers.
pub struct UnixSock {
    id: usize,
    /// `SOCK_STREAM`, `SOCK_DGRAM` or `SOCK_SEQPACKET`, as `socket` was
    /// told: the streams connect through a listener's queue and carry
    /// bytes; the datagrams connect to a name and carry whole messages;
    /// a named SEQPACKET is served as the stream and a paired one as the
    /// datagram (`carries_messages` picks the branch).
    typ: u16,
    /// The network namespace this socket was **made in** - Linux's `sk_net`
    /// on a unix socket, and the key exactly one of this module's two tables
    /// reads.
    ///
    /// Which table is a measurement rather than a reading of the man page, and
    /// the two halves of the family differ (`.workbuddy/probe_unix_netns{,2,3}.c`;
    /// §6.3):
    ///
    /// * an **abstract** name belongs to the namespace of the socket that holds
    ///   it - a name bound in one is `ECONNREFUSED` from a socket in another,
    ///   and the *same* name binds again there. That is `by_name`;
    /// * a **path** name is a filesystem object and is not namespaced at all: a
    ///   socket bound at a path is reachable by that path from every namespace,
    ///   in both directions. That is `by_node`, which is machine-wide;
    /// * and it is the *socket's* namespace, not the calling task's: a socket
    ///   made in one namespace and used from another binds and dials in the
    ///   one it was made in, which is the same rule the IP stack's handle map
    ///   keeps (`net`'s `HOMES`) - and the reason this is a field rather than a
    ///   lookup at the call.
    ///
    /// `None` is the machine's own namespace, normalized on the way in, so a
    /// task that `setns`ed back into it is the same key as one that never left.
    net_ns: Option<u64>,
    state: Mutex<RefCell<UnixState>>,
}

/// One connection waiting in a listener's queue, in the shape its family
/// carries.
enum Pending {
    /// A byte-stream connection: the server half of a `DuplexEnd` pair and
    /// the connector's credentials for `SO_PEERCRED`.
    Stream(Arc<DuplexEnd>, UCred),
    /// A sequenced-packet connection: the server socket, already paired with
    /// the connector's socket as a message peer (each end has its own queue,
    /// so there is no shared channel to hand over).
    Messages(Arc<UnixSock>),
}

struct UnixState {
    /// The name `bind` planted: the filesystem node it made (the identity
    /// the bind table keys on) and the path spelled at the call, which is
    /// what `getsockname` echoes.
    bound: Option<BoundName>,
    /// `listen`'s backlog, once `listen` was called. `Some` is what a
    /// `connect` looks for; a bound socket without it is refused
    /// (`ECONNREFUSED`, the header's divergence list).
    backlog: Option<usize>,
    /// The pending connections, each tagged with the connector's credentials
    /// so the socket `accept` mints learns who dialed it, waiting for
    /// `accept` to hand them to a descriptor. A stream connection is the
    /// server half of a `DuplexEnd` pair; a sequenced-packet one is the
    /// server socket itself, already wired to the connector as a message
    /// peer, because that shape has a queue per end rather than one shared
    /// byte channel.
    pending: VecDeque<Pending>,
    /// Tasks parked in `accept`, and tasks parked in a `connect` that
    /// found the backlog full. Stale ids are harmless - every wait
    /// re-checks its condition inside its own predicate - the rule
    /// `pipe.rs` documents for its two lists.
    accept_waiters: Vec<TaskId>,
    room_waiters: Vec<TaskId>,
    /// The channel, once connected (by `accept` or by `connect`). `None`
    /// is "not connected", which is what the file doors and the send path
    /// refuse with `ENOTCONN`.
    end: Option<Arc<DuplexEnd>>,
    /// The credentials of the task that created this socket (`socket`, or the
    /// `accept` that minted a connected one) - what a peer sees as ours.
    owner: UCred,
    /// The peer's credentials, captured when the connection was made (the
    /// connector's at `connect`, the listener's on the client side). This is
    /// what `SO_PEERCRED` reports and `SCM_CREDENTIALS` delivers, and it is
    /// `Some` exactly when the socket is connected.
    peer: Option<UCred>,
    /// `SO_PASSCRED`: the owner wants the peer's credentials attached, as an
    /// `SCM_CREDENTIALS` message, to every read that has room for one.
    passcred: bool,
    /// A `SOCK_DGRAM` socket's received datagrams, whole, in send order -
    /// the queue a connected writer pushes onto and `read` pops. A
    /// listener's `pending` holds *channels*; this holds *messages*, which
    /// is the whole difference between the two shapes of unix socket.
    recvq: VecDeque<Dgram>,
    /// Tasks parked in a `read` that found the queue empty. Same contract
    /// as the accept waiters: the delivery wakes them, and every wake
    /// re-checks its own condition.
    read_waiters: Vec<TaskId>,
    /// The socket a `connect` aimed this datagram socket at. Weak: a
    /// connected sender must not keep the recipient alive; the upgrade
    /// failing is exactly the "peer is gone" the send answers with.
    dgram_peer: Option<Weak<UnixSock>>,
    /// Whether the descriptor's `O_NONBLOCK` has reached the socket - the
    /// pipe's rule, carried by the same `F_SETFL`/`FIONBIO` sync.
    nonblocking: bool,
}

/// One received datagram: the bytes, the sender's name if it had a bound one
/// - the `recvfrom` answer - and the credentials of the task that sent it.
///
/// The credentials ride per message rather than coming from the socket's
/// `SO_PEERCRED` because that is what a listener with `SO_PASSCRED` is
/// entitled to: `sendmsg`/`write` on a datagram records who was running, and
/// the read hands that back as `SCM_CREDENTIALS`. udevd's manager runs its
/// whole worker protocol on it - it forked the socketpair before forking each
/// worker, so the *socket's* peer is the manager for every message and only
/// the sender's own pid tells a worker's reply from another's. A message
/// without it is "ignoring worker message without valid PID", and a manager
/// that never hears a worker return considers its event still running - which
/// is how every child device of a busy parent path, `/dev/input/event0` among
/// them, waits behind an event that already finished.
struct Dgram {
    data: Vec<u8>,
    /// The sender's address, in whichever namespace it lives - which is what
    /// `recvfrom` hands back.
    from: Option<UnixAddr>,
    cred: UCred,
    /// Descriptors the send carried as `SCM_RIGHTS`, kept with the message
    /// they belong to. A message is a boundary for control data too: the
    /// receive that takes it installs these, and a peek hands out clones so
    /// the record stays for the next reader.
    fds: Vec<Arc<crate::vfs::File>>,
}

/// Queued datagrams per socket before a send answers `EAGAIN`, and the
/// largest datagram accepted. Linux sizes these from socket buffers; one
/// fixed, generous number is the teaching-kernel shape: a syslog line is
/// a few hundred bytes, and a caller whose peer is not reading gets an
/// explicit no instead of an unbounded queue.
const RECVQ_MAX: usize = 64;
const GRAM_MAX: usize = 64 * 1024;

/// `SOCK_DGRAM` as `socket(2)` names it.
const SOCK_DGRAM: u16 = 2;

/// `SOCK_SEQPACKET` as `socket(2)` names it: message boundaries over a
/// connection, the shape a named listener and an accepted pair carry.
const SOCK_SEQPACKET: u16 = 5;

enum BoundName {
    /// A name in the filesystem: the path spelled at `bind`, and the node's
    /// identity. The node itself is *not* kept - `(dev, ino)` is what the
    /// table keys on and `unlink` retires, and ramfs mints inode numbers from
    /// a counter that only moves forward, so pinning the node would hold a
    /// deleted name's memory for nothing.
    Path { path: String, key: (u64, u64) },
    /// An abstract name: bytes that no filesystem holds, so nothing outside
    /// this module can retire them. The name lives exactly as long as a
    /// descriptor does, which is what [`Drop`] is for (Linux has no
    /// `unlink` on this namespace either).
    Abstract(Vec<u8>),
}

impl UnixSock {
    fn new(id: usize, typ: u16, owner: UCred, net_ns: Option<u64>) -> Arc<UnixSock> {
        Arc::new(UnixSock {
            id,
            typ,
            net_ns,
            state: Mutex::new(RefCell::new(UnixState {
                bound: None,
                backlog: None,
                pending: VecDeque::new(),
                accept_waiters: Vec::new(),
                room_waiters: Vec::new(),
                end: None,
                owner,
                peer: None,
                passcred: false,
                recvq: VecDeque::new(),
                read_waiters: Vec::new(),
                dgram_peer: None,
                nonblocking: false,
            })),
        })
    }

    pub fn id(&self) -> usize {
        self.id
    }

    /// The peer's credentials this socket learned at connect, if it is
    /// connected - `SO_PEERCRED`'s answer and `SCM_CREDENTIALS`' payload.
    pub fn peer_cred(&self) -> Option<UCred> {
        critical_section::with(|cs| self.state.borrow(cs).borrow().peer)
    }

    /// Whether `SO_PASSCRED` is set - whether a read should attach the peer's
    /// credentials.
    pub fn passcred(&self) -> bool {
        critical_section::with(|cs| self.state.borrow(cs).borrow().passcred)
    }

    /// Set `SO_PASSCRED`, so the next `recvmsg` with room carries a
    /// `SCM_CREDENTIALS`.
    pub fn set_passcred(&self, on: bool) {
        critical_section::with(|cs| self.state.borrow(cs).borrow_mut().passcred = on);
    }

    /// Whether this is a datagram socket - the branch the *name* rules route
    /// by (`bind` to a name, `connect` to a name, `sendto` a path).
    pub fn is_dgram(&self) -> bool {
        self.typ == SOCK_DGRAM
    }

    /// Whether this socket's data is carried as *messages* rather than as a
    /// byte stream - the branch the byte doors and the message syscalls route
    /// by.
    ///
    /// True of a datagram socket, and of a sequenced-packet one that is
    /// connected to a peer: a `socketpair` half, or either end of a named
    /// connection a listener accepted.
    pub fn carries_messages(&self) -> bool {
        self.typ == SOCK_DGRAM || self.dgram_connected()
    }

    /// Whether this is a sequenced-packet socket with no peer to carry a
    /// message to - unconnected, or bound and listening, alike.
    ///
    /// Such a socket's send doors are message doors, but there is no peer:
    /// Linux drops the destination before the shared send looks at it and
    /// answers `ENOTCONN` (measured 107 on Linux 7.2.11 for no name, a live
    /// name, a dead name, a regular file and a family-only address alike), while
    /// the syscall layer's own copy still runs first. `docs/uapi/` §5.1 item 81
    /// serves that split.
    pub fn unconnected_seqpacket(&self) -> bool {
        self.typ == SOCK_SEQPACKET && !self.carries_messages()
    }

    /// Whether a send ignores the destination it is given.
    ///
    /// True of a connected sequenced-packet socket, and of nothing else:
    /// Linux drops `msg_name` before the shared datagram send looks at it, so
    /// the address is neither validated nor resolved - the message goes to the
    /// peer whatever the name says (measured on Linux 7.2.11: a family-only
    /// address, a dead name, a different live socket and a `SOCK_STREAM`
    /// listener's name all send, while a datagram socket given the same
    /// address answers `EINVAL`/`ENOENT`/`ECONNREFUSED`).
    pub fn ignores_send_address(&self) -> bool {
        self.typ == SOCK_SEQPACKET && self.dgram_connected()
    }

    /// The descriptor's `O_NONBLOCK`, carried onto the socket the way the
    /// pipe carries it: the doors answer EAGAIN instead of parking.
    pub fn set_nonblocking(&self, on: bool) {
        critical_section::with(|cs| self.state.borrow(cs).borrow_mut().nonblocking = on);
    }

    /// The receiver's half of a datagram send: queue one whole message and
    /// wake whoever sits in `read`. `false` is the queue at its cap - the
    /// sender's `EAGAIN`, never a silent drop of a message someone asked
    /// to send.
    fn push_dgram(
        &self,
        data: &[u8],
        from: Option<UnixAddr>,
        cred: UCred,
        fds: Vec<Arc<crate::vfs::File>>,
    ) -> bool {
        let waiters = critical_section::with(|cs| {
            let mut st = self.state.borrow(cs).borrow_mut();
            if st.recvq.len() >= RECVQ_MAX {
                return None;
            }
            st.recvq.push_back(Dgram {
                data: data.to_vec(),
                from,
                cred,
                fds,
            });
            Some(st.read_waiters.clone())
        });
        match waiters {
            Some(ids) => {
                wake_all(ids);
                true
            }
            None => false,
        }
    }

    /// The socket `connect` aimed a datagram sender at, while it lives.
    fn dgram_peer(&self) -> Option<Arc<UnixSock>> {
        critical_section::with(|cs| {
            self.state
                .borrow(cs)
                .borrow()
                .dgram_peer
                .as_ref()
                .and_then(|w| w.upgrade())
        })
    }

    /// Whether a datagram socket has a peer pinned, live or not - the
    /// `EISCONN` a second `connect` answers by.
    fn dgram_connected(&self) -> bool {
        critical_section::with(|cs| self.state.borrow(cs).borrow().dgram_peer.is_some())
    }

    /// Take the front datagram, parked for if the queue is empty and the
    /// caller allowed waiting. The answer carries the whole message, so the
    /// caller can copy what its buffer holds and still know the message's own
    /// length for `MSG_TRUNC`.
    pub(crate) fn take_dgram(&self) -> Result<RecvDgram, VfsError> {
        loop {
            let taken = critical_section::with(|cs| {
                let mut st = self.state.borrow(cs).borrow_mut();
                st.recvq.pop_front().map(|g| RecvDgram {
                    data: g.data,
                    from: g.from,
                    cred: g.cred,
                    fds: g.fds,
                })
            });
            if let Some(got) = taken {
                // The queue only got emptier by one message, so the other
                // parked readers stay parked until a push wakes them all;
                // this take's own waiters set is the poll subscribers, and
                // `epoll` re-arms them through `poll_ready` on its own.
                return Ok(got);
            }
            let nonblock = critical_section::with(|cs| self.state.borrow(cs).borrow().nonblocking);
            if nonblock {
                return Err(VfsError::Again);
            }
            #[cfg(not(target_os = "none"))]
            return Err(VfsError::Again);
            #[cfg(target_os = "none")]
            self.wait_for_dgram()?;
        }
    }

    /// Look at the front datagram without taking it - `MSG_PEEK` on a message
    /// socket.
    ///
    /// Linux's peek is a full copy of the queue's front that leaves it for the
    /// next receive, repeatable as often as the caller asks; a short peek
    /// still reports the message's own length under `MSG_TRUNC` and sets
    /// `MSG_TRUNC` in a `recvmsg`'s `msg_flags` without being asked, exactly
    /// as a consuming receive does (measured on a current host,
    /// `.workbuddy/probe_unix_peek{,2}.c`). The wait is the take's wait, so a
    /// peek of an empty queue parks and a signal ends it the same way.
    pub(crate) fn peek_dgram(&self) -> Result<RecvDgram, VfsError> {
        loop {
            let front = critical_section::with(|cs| {
                self.state
                    .borrow(cs)
                    .borrow()
                    .recvq
                    .front()
                    .map(|g| RecvDgram {
                        data: g.data.clone(),
                        from: g.from.clone(),
                        cred: g.cred,
                        fds: g.fds.clone(),
                    })
            });
            if let Some(got) = front {
                return Ok(got);
            }
            let nonblock = critical_section::with(|cs| self.state.borrow(cs).borrow().nonblocking);
            if nonblock {
                return Err(VfsError::Again);
            }
            #[cfg(not(target_os = "none"))]
            return Err(VfsError::Again);
            #[cfg(target_os = "none")]
            self.wait_for_dgram()?;
        }
    }

    /// Park until a datagram stands in the queue - the accept wait's
    /// shape: register, wait on the condition, unregister.
    ///
    /// Interruptible, so a catchable signal ends it with `EINTR` and the queue
    /// untouched: a receiver that owns this end of a pair is usually also
    /// waiting on signals, and a wait that swallowed them would leave it deaf
    /// to its own alarm until the peer happened to send.
    #[cfg(target_os = "none")]
    fn wait_for_dgram(&self) -> VfsResult<()> {
        let me = crate::sched::current();
        critical_section::with(|cs| {
            let mut st = self.state.borrow(cs).borrow_mut();
            if !st.read_waiters.contains(&me) {
                st.read_waiters.push(me);
            }
        });
        let end = crate::sched::block_until_interruptible(|| {
            critical_section::with(|cs| {
                let st = self.state.borrow(cs).borrow();
                !st.recvq.is_empty()
            })
        });
        critical_section::with(|cs| {
            self.state
                .borrow(cs)
                .borrow_mut()
                .read_waiters
                .retain(|&w| w != me);
        });
        match end {
            crate::sched::WaitEnd::Interrupted => Err(VfsError::Interrupted),
            _ => Ok(()),
        }
    }

    /// The connected channel, if there is one - the downcast the file
    /// doors and the message syscalls route by.
    fn end_of(&self) -> Option<Arc<DuplexEnd>> {
        critical_section::with(|cs| self.state.borrow(cs).borrow().end.clone())
    }

    /// Whether `connect` has reached this listener's door since the last
    /// `accept` - `poll_ready`'s whole answer for a listening socket.
    fn pending_nonempty(&self) -> bool {
        critical_section::with(|cs| !self.state.borrow(cs).borrow().pending.is_empty())
    }

    /// Whether `listen` has been called on this socket - `FIONREAD` tells a
    /// listener (which refuses) from a fresh, unconnected socket (which
    /// answers 0), the two both having no channel.
    pub fn is_listening(&self) -> bool {
        critical_section::with(|cs| self.state.borrow(cs).borrow().backlog.is_some())
    }

    /// Whether a `bind` has planted a name on this socket. Linux refuses a
    /// second bind on the grounds that the first is what the fd answers by.
    pub fn is_bound(&self) -> bool {
        critical_section::with(|cs| self.state.borrow(cs).borrow().bound.is_some())
    }

    /// The bound address, for `getsockname` - in whichever namespace it lives.
    pub fn local_addr(&self) -> Option<UnixAddr> {
        critical_section::with(|cs| match self.state.borrow(cs).borrow().bound.as_ref()? {
            BoundName::Path { path, .. } => Some(UnixAddr::Path(path.clone())),
            BoundName::Abstract(name) => Some(UnixAddr::Abstract(name.clone())),
        })
    }
}

impl Inode for UnixSock {
    /// A socket has no position; `lseek` is `ESPIPE` the way every other
    /// channel answers it.
    fn seekable(&self) -> bool {
        false
    }

    fn metadata(&self) -> Metadata {
        Metadata {
            // The anonymous-inode trick the network sockets use
            // (`SocketInode`'s `0x7000_0000 + handle`): this kernel's
            // sockets report `CharDev` to `stat` and a reserved number no
            // file claims - `ls -l /proc/self/fd` is where anyone ever
            // looks, and the bound *filesystem* node is the one that
            // answers `S_IFSOCK`.
            ino: 0x7100_0000 + self.id as Ino,
            kind: NodeKind::CharDev,
            size: 0,
        }
    }

    fn unix_id(&self) -> usize {
        self.id
    }

    fn unix_end(&self) -> Option<Arc<DuplexEnd>> {
        self.end_of()
    }

    /// `FIONREAD` on a unix socket. A **message-carrying** one (`SOCK_DGRAM`,
    /// `SOCK_SEQPACKET`) reports its *first* pending datagram's length, which is
    /// the front of the queue rather than the sum - measured on the host
    /// (`probe_fionread2.c`: 5 then 9 queued answers 5, and an empty queue
    /// answers 0). A **stream** one with a channel reports through it, the same
    /// `DuplexEnd` a socketpair end is, so its queued bytes are the peer's
    /// unread writes. A stream one with *no* channel is either a **listener**,
    /// which Linux refuses with `EINVAL` before any value, or a fresh,
    /// unconnected socket, which holds nothing and answers 0 - both measured
    /// (`probe_unixinq.c`, Linux 7.2.9).
    fn fionread(&self) -> Option<VfsResult<usize>> {
        if self.carries_messages() {
            let queued = critical_section::with(|cs| {
                self.state
                    .borrow(cs)
                    .borrow()
                    .recvq
                    .front()
                    .map(|d| d.data.len())
                    .unwrap_or(0)
            });
            return Some(Ok(queued));
        }
        if let Some(end) = self.end_of() {
            return end.fionread();
        }
        if self.is_listening() {
            return Some(Err(VfsError::Invalid));
        }
        Some(Ok(0))
    }

    /// The byte doors. A stream answers through its channel - the same
    /// reads a socketpair end gives - and answers `ENOTCONN` when there is
    /// no channel, listening or fresh. A datagram socket answers through
    /// its queue instead: one `read` takes one whole datagram (bytes past
    /// the caller's buffer are discarded, which is the unix rule, not a
    /// loss to paper over), and one `write` is one whole datagram to the
    /// connected peer.
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> VfsResult<usize> {
        if self.carries_messages() {
            // `read(2)` with a zero-length buffer is a no-op on Linux: it
            // answers 0 and leaves the front message where it is, where
            // `recvfrom`/`recvmsg` with a zero-length room are receives that
            // take it (measured on a named sequenced-packet connection; a
            // datagram pair answers the same).
            if buf.is_empty() {
                return Ok(0);
            }
            let got = self.take_dgram()?;
            let n = got.data.len().min(buf.len());
            buf[..n].copy_from_slice(&got.data[..n]);
            return Ok(n);
        }
        let _ = offset;
        match self.end_of() {
            Some(end) => end.read_at(offset, buf),
            None => Err(VfsError::NotConnected),
        }
    }

    fn write_at(&self, offset: u64, buf: &[u8]) -> VfsResult<usize> {
        if self.carries_messages() {
            let _ = offset;
            let peer = self.dgram_peer().ok_or(VfsError::NotConnected)?;
            let name = self.local_addr();
            if peer.push_dgram(buf, name, self_cred(), Vec::new()) {
                Ok(buf.len())
            } else {
                Err(VfsError::Again)
            }
        } else {
            match self.end_of() {
                Some(end) => end.write_at(offset, buf),
                None => Err(VfsError::NotConnected),
            }
        }
    }

    fn poll_ready(&self) -> Ready {
        // A datagram socket is readable when a message stands in its queue,
        // and writable while the queue has room to send into the peer's -
        // the peer's cap is the only backpressure, so `write: true` states
        // what is true of a socket whose peer reads at all; a full peer
        // answers the send itself with `EAGAIN`.
        if self.carries_messages() {
            let queued =
                critical_section::with(|cs| !self.state.borrow(cs).borrow().recvq.is_empty());
            return Ready {
                read: queued,
                write: true,
            };
        }
        // Connected first, by fact not by position: the client end of a
        // completed handshake has an empty accept queue of its own, and
        // reading it through the listener branch below would report it
        // readable forever - the channel's readiness is the only truth.
        if let Some(end) = self.end_of() {
            return end.poll_ready();
        }
        // A listener is readable exactly when something stands in its
        // queue; a fresh, unbound socket is ready for nothing.
        Ready {
            read: self.pending_nonempty(),
            write: false,
        }
    }

    /// Listening sockets keep their accept waiters themselves, so a
    /// `poll(2)` on a bare listener is woken by a `connect` rather than
    /// waiting out its tick budget. A connected socket forwards to the
    /// channel - both of its halves, the socketpair rule.
    fn poll_subscribe(&self, id: TaskId) {
        // A message reader waits on the message queue, not the accept one.
        if self.carries_messages() {
            critical_section::with(|cs| {
                let mut st = self.state.borrow(cs).borrow_mut();
                if !st.read_waiters.contains(&id) {
                    st.read_waiters.push(id);
                }
            });
            return;
        }
        // Connected: both halves of the channel, the socketpair rule.
        // Listening (or nothing): the accept waiters, so a `connect`
        // reaching the queue is what wakes the poll.
        if let Some(end) = self.end_of() {
            end.poll_subscribe(id);
            return;
        }
        critical_section::with(|cs| {
            let mut st = self.state.borrow(cs).borrow_mut();
            if st.end.is_none() && !st.accept_waiters.contains(&id) {
                st.accept_waiters.push(id);
            }
        });
    }

    fn poll_unsubscribe(&self, id: TaskId) {
        if let Some(end) = self.end_of() {
            end.poll_unsubscribe(id);
        }
        critical_section::with(|cs| {
            let mut st = self.state.borrow(cs).borrow_mut();
            st.accept_waiters.retain(|&w| w != id);
            st.read_waiters.retain(|&w| w != id);
        });
    }
}

// ---------------------------------------------------------------------------
// The bind table: node identity in front of every listener
// ---------------------------------------------------------------------------

struct Registry {
    /// Slot `id - 1`, the socket's own number. Weak: the socket lives
    /// exactly as long as something holds a descriptor to it - an fd
    /// table's entry, an epoll interest list's reference, a handler's
    /// transient clone - and dies the moment the last one lets go. The
    /// registry is a *directory of names*, not an owner: an owner would be
    /// a socket whose last reference went away while some cleanup hook
    /// still owed it a call, and that socket would answer `connect` for
    /// ever - a listener nothing will ever accept, the shape of a connect
    /// that hangs where Linux answers `ECONNREFUSED`.
    ///
    /// The death is [`UnixSock::drop`]'s doing, and it is *the* mechanism
    /// rather than a backstop because the alternative - a strong slot
    /// retired by an explicit close hook - was tried here and failed a
    /// count gate: `close_sockets_at_exit` compared the batch's references
    /// against the socket's whole `strong_count`, an epoll entry watching
    /// its own listener is one of those references (every daemon that
    /// polls its listening socket - dbus first - holds exactly that
    /// shape), and the gate read the pair as "someone still owns this" on
    /// the day the owner died. A reference the batch does not hold can
    /// only honestly mean "the socket is not this batch's to close", and
    /// the only rule that is true for *every* holder is the count itself.
    slots: Vec<Option<Slot>>,
    /// Which live socket answers each bound node. `unlink` removes the key
    /// by node identity (`unbind_node`); a connect resolves a path to a
    /// node and asks this map, which is how symlinks, `..` and mounts all
    /// come for free.
    by_node: BTreeMap<(u64, u64), usize>,
    /// Which live socket holds each abstract name, **per network namespace**.
    ///
    /// Separate from `by_node` because the two are separate namespaces - and
    /// separate *from each other* in scope, which is the part the family's own
    /// shape hides: `by_node` is machine-wide (a path is a filesystem object,
    /// reachable from every namespace) while this one is keyed by the namespace
    /// of the socket that holds the name. Both halves measured; the field's own
    /// note carries the numbers.
    ///
    /// Keyed on the *socket type* as well as the name because Linux is
    /// (measured: a datagram `bind` on a stream socket's abstract name answers
    /// 0). A path cannot be shared that way - it is one node.
    by_name: BTreeMap<(Option<u64>, Vec<u8>, u16), usize>,
    /// The counter behind `autobind`'s generated names. Linux mints an abstract
    /// name for a `bind` that names nothing; the *shape* is what the caller can
    /// observe (five hex characters, `addrlen` 8 from `getsockname`), not the
    /// value.
    autobinds: u32,
}

struct Slot {
    socket: Weak<UnixSock>,
    /// The key `by_node` points here, so a release can remove its own
    /// entry without scanning paths.
    key: Option<(u64, u64)>,
    /// The `by_name` key this socket holds, if any - a socket has at most one
    /// name, so at most one of these two is set. The namespace is part of the
    /// key, so the release removes the entry the bind made rather than scanning
    /// the namespaces for one that matches.
    name: Option<(Option<u64>, Vec<u8>, u16)>,
}

impl Registry {
    const fn new() -> Self {
        Registry {
            slots: Vec::new(),
            by_node: BTreeMap::new(),
            by_name: BTreeMap::new(),
            autobinds: 0,
        }
    }
}

static UNIX: Mutex<RefCell<Registry>> = Mutex::new(RefCell::new(Registry::new()));

/// A fresh socket and its registry slot. Slot numbers are never reused, so
/// a stale id can only fail its own liveness check, never answer to a
/// different socket.
///
/// The socket is born in the **caller's** network namespace - `socket(2)` is a
/// creation, and what it makes belongs to whoever asked (Linux's
/// `unix_create` sets `sock_net` from the same caller). Everything after that
/// reads the field, so a socket that outlives an `unshare` keeps answering in
/// the namespace it was made in.
pub(crate) fn alloc(typ: u16) -> Arc<UnixSock> {
    alloc_in(crate::net::normalize(crate::sched::current_net_ns()), typ)
}

/// The same, for a socket that belongs to a namespace the caller is not in:
/// `accept` mints one in the **listener's** namespace, which is whose name it
/// answers and whose table a name of its own would land in. Linux's
/// `unix_accept` does the same (`unix_create1(sock_net(sk), ...)`), and the
/// difference is observable the moment a namespace's server accepts and the
/// accepted socket binds: with the accepter's namespace it would plant the name
/// in the wrong table.
fn alloc_in(net_ns: Option<u64>, typ: u16) -> Arc<UnixSock> {
    let owner = self_cred();
    critical_section::with(|cs| {
        let mut reg = UNIX.borrow(cs).borrow_mut();
        let id = reg.slots.len() + 1;
        let sock = UnixSock::new(id, typ, owner, net_ns);
        reg.slots.push(Some(Slot {
            socket: Arc::downgrade(&sock),
            key: None,
            name: None,
        }));
        sock
    })
}

/// A pair of anonymous, connected, message-carrying sockets: `socketpair(2)`
/// for the two types that carry messages.
///
/// The two ends point at each other and at nothing else - no name, no bind
/// table entry, no listener, which is what a pair is. What one end sends is
/// one *message* in the other's queue, boundaries and all: the same machinery
/// a named datagram socket's `connect` sets up, with the name left out. That
/// is the whole difference from a stream pair, whose ends are the two halves
/// of one byte channel.
///
/// The peer's credentials are captured the way a named connection captures
/// them, so `SO_PEERCRED` on either end answers the other's maker - here, the
/// process that made the pair.
pub(crate) fn pair(typ: u16) -> (Arc<UnixSock>, Arc<UnixSock>) {
    let first = alloc(typ);
    let second = alloc(typ);
    critical_section::with(|cs| {
        let first_cred = first.state.borrow(cs).borrow().owner;
        let second_cred = second.state.borrow(cs).borrow().owner;
        {
            let mut st = first.state.borrow(cs).borrow_mut();
            st.dgram_peer = Some(Arc::downgrade(&second));
            st.peer = Some(second_cred);
        }
        {
            let mut st = second.state.borrow(cs).borrow_mut();
            st.dgram_peer = Some(Arc::downgrade(&first));
            st.peer = Some(first_cred);
        }
    });
    (first, second)
}

/// The outcome `bind` answers: the errno an already-taken name gets, the
/// table key a successful one records.
pub(crate) fn bind(sock: &Arc<UnixSock>, _node: Arc<dyn Inode>, path: String, key: (u64, u64)) {
    critical_section::with(|cs| {
        {
            let mut reg = UNIX.borrow(cs).borrow_mut();
            if let Some(Some(slot)) = reg.slots.get_mut(sock.id - 1) {
                slot.key = Some(key);
            }
            reg.by_node.insert(key, sock.id);
        }
        let mut st = sock.state.borrow(cs).borrow_mut();
        st.bound = Some(BoundName::Path { path, key });
    });
}

/// Bind an **abstract** name: bytes that no filesystem holds, so the whole of
/// the binding is this module's table plus the socket's own record of what it
/// answers `getsockname` with.
///
/// Linux's rules, measured (`.workbuddy/probe_unix_abstract.c`): a name already
/// held **by the same socket type** is `EADDRINUSE` - a datagram socket may hold
/// the name a stream socket holds, because the two keys are different entries -
/// and a socket that already has a name answers `EINVAL` whatever the second
/// name is, which is the refusal `bind_path` makes as well.
///
/// The table is the **socket's** namespace's, not the caller's: a socket made in
/// one namespace and carried into another binds its name at home (measured -
/// `.workbuddy/probe_unix_netns3.c` bound on a socket carried across an
/// `unshare` and a socket of the new namespace could not see the name, while a
/// second socket of the old one could).
pub(crate) fn bind_name(sock: &UnixSock, name: Vec<u8>) -> isize {
    if sock.is_bound() {
        return err::EINVAL;
    }
    critical_section::with(|cs| {
        let mut reg = UNIX.borrow(cs).borrow_mut();
        let key = (sock.net_ns, name.clone(), sock.typ);
        if reg.by_name.contains_key(&key) {
            return err::EADDRINUSE;
        }
        reg.by_name.insert(key.clone(), sock.id);
        if let Some(Some(slot)) = reg.slots.get_mut(sock.id - 1) {
            slot.name = Some(key);
        }
        let mut st = sock.state.borrow(cs).borrow_mut();
        st.bound = Some(BoundName::Abstract(name));
        0
    })
}

/// `bind` with an `addrlen` of just the family: Linux *makes a name up* rather
/// than refusing it (measured: success, and `getsockname` answers a leading NUL
/// and five hex characters). The generated name is this module's own, because
/// the value is not part of the contract - the *shape* is, and it is what the
/// caller can see.
pub(crate) fn bind_autobind(sock: &UnixSock) -> isize {
    let name = critical_section::with(|cs| {
        let mut reg = UNIX.borrow(cs).borrow_mut();
        let mut n = reg.autobinds;
        loop {
            n = n.wrapping_add(1);
            let candidate = alloc::format!("{n:05x}").into_bytes();
            if !reg
                .by_name
                .contains_key(&(sock.net_ns, candidate.clone(), sock.typ))
            {
                reg.autobinds = n;
                return candidate;
            }
        }
    });
    bind_name(sock, name)
}

/// The live socket holding an abstract name **in namespace `ns`**, or `None` -
/// which every caller turns into the answer Linux gives for a name nobody
/// holds.
///
/// The namespace is the *dialling socket's* own, not the calling task's
/// (measured: a socket made before an `unshare` and used after it reaches the
/// names of the namespace it was made in), and the socket *type* is part of the
/// lookup, so a datagram connector cannot find a stream listener here at all;
/// Linux refuses that pair too.
fn live_named(ns: Option<u64>, name: &[u8], typ: u16) -> Option<Arc<UnixSock>> {
    critical_section::with(|cs| {
        let reg = UNIX.borrow(cs).borrow();
        let id = *reg.by_name.get(&(ns, name.to_vec(), typ))?;
        reg.slots
            .get(id - 1)?
            .as_ref()
            .and_then(|slot| slot.socket.upgrade())
    })
}

/// `connect` by abstract name.
///
/// There is no node and no filesystem, so there is nothing to resolve and no
/// permission to check: a name nobody holds is the answer a dead node gives,
/// `ECONNREFUSED` (measured: both a stream and a datagram `connect` to an
/// unbound abstract name answer 111).
pub(crate) fn connect_named(sock: &UnixSock, name: &[u8], nonblock: bool) -> isize {
    if sock.end_of().is_some() || sock.dgram_connected() {
        return err::EISCONN;
    }
    let Some(listener) = live_named(sock.net_ns, name, sock.typ) else {
        return err::ECONNREFUSED;
    };
    let arc = sock_arc(sock);
    match connect_to(&arc, &listener, nonblock) {
        Ok(()) => 0,
        Err(ConnectRefused::Refused) => err::ECONNREFUSED,
        Err(ConnectRefused::WrongType) => err::EPROTOTYPE,
        Err(ConnectRefused::Again) => err::EAGAIN,
        Err(ConnectRefused::Interrupted) => err::EINTR,
    }
}

/// The unlink hook: the name is gone from the namespace, so no new
/// `connect` may find this socket by it. The listener itself is untouched
/// (Linux's behavior: a listening socket survives its own unlink); only
/// the table key retires.
pub(crate) fn unbind_node(node: &Arc<dyn Inode>) {
    let key = (crate::vfs::mount::fs_id_of_inode(node), node.metadata().ino);
    critical_section::with(|cs| {
        let mut reg = UNIX.borrow(cs).borrow_mut();
        reg.by_node.remove(&key);
        for slot in reg.slots.iter_mut().flatten() {
            if slot.key == Some(key) {
                slot.key = None;
            }
        }
        // An abstract name is not a filesystem name: nothing outside this
        // module can unlink one, so this hook is about paths alone.
    });
}

/// The last reference is gone: whatever held a descriptor - an fd table's
/// entry, an epoll interest list's reference, a handler's transient clone -
/// has let go, and the socket retires itself. The name's table key goes
/// first, so no later `connect` resolves the node to this socket; the slot
/// follows; the pending connections drop outside every borrow (the
/// `DuplexEnd` drop path EOFs whoever already held the other half - a
/// client parked on a dead daemon's queue wakes to end-of-file, not to
/// nothing); and anyone parked in `accept` or in a backlog-full `connect`
/// wakes to find the socket gone.
///
/// This is *the* mechanism, not a backstop. A registry that owned its
/// sockets would need every death path to remember a call, and the one
/// death path that did not - the exit batch's count gate seeing an epoll
/// entry's reference and reading "someone still owns this" - left the
/// listener answering `connect` for ever: a daemon that polls its own
/// listening socket (dbus first) is exactly that shape. A weak slot and
/// this drop are the same rule told once: the socket is alive exactly
/// while something names it.
impl Drop for UnixSock {
    fn drop(&mut self) {
        let (pending, waiters) = critical_section::with(|cs| {
            let mut reg = UNIX.borrow(cs).borrow_mut();
            let (node_key, name_key) = match reg.slots.get_mut(self.id - 1) {
                Some(Some(slot)) => (slot.key.take(), slot.name.take()),
                _ => (None, None),
            };
            if let Some(key) = node_key {
                reg.by_node.remove(&key);
            }
            if let Some(name) = name_key {
                reg.by_name.remove(&name);
            }
            reg.slots[self.id - 1] = None;
            let mut st = self.state.borrow(cs).borrow_mut();
            let pending = core::mem::take(&mut st.pending);
            let mut waiters = core::mem::take(&mut st.accept_waiters);
            waiters.append(&mut st.room_waiters);
            waiters.append(&mut st.read_waiters);
            (pending, waiters)
        });
        // Outside every borrow: a queued half's drop can wake whoever
        // blocked on it, and a wake needs locks no drop may hold.
        drop(pending);
        wake_all(waiters);
    }
}

/// Look up a connect's resolved node in the table, returning the live
/// socket behind it. `None` answers `ECONNREFUSED` at the call site: no
/// entry, an entry whose socket has died - the upgrade is the liveness
/// test - or a stale key all mean the same thing to a connector.
fn live_listener(node: &Arc<dyn Inode>) -> Option<Arc<UnixSock>> {
    let key = (crate::vfs::mount::fs_id_of_inode(node), node.metadata().ino);
    critical_section::with(|cs| {
        let reg = UNIX.borrow(cs).borrow();
        let id = *reg.by_node.get(&key)?;
        reg.slots
            .get(id - 1)?
            .as_ref()
            .and_then(|slot| slot.socket.upgrade())
    })
}

// ---------------------------------------------------------------------------
// The connect handshake and the accept queue
// ---------------------------------------------------------------------------

/// One `connect`'s whole effect: mint the pair, queue the server half,
/// install the client half, wake the accept queue. The backlog is the
/// queue's cap; at the cap a blocking caller waits for room (and a signal
/// makes that `EINTR`, the man page's), a nonblocking one gets `EAGAIN`.
pub(crate) fn connect(
    sock: &Arc<UnixSock>,
    node: &Arc<dyn Inode>,
    nonblock: bool,
) -> Result<(), ConnectRefused> {
    let listener = live_listener(node).ok_or(ConnectRefused::Refused)?;
    connect_to(sock, &listener, nonblock)
}

/// The handshake itself, once a listener has been found - by node for a path,
/// by name for an abstract address. Everything below is about the two sockets
/// and nothing about how the second one was named, which is why it is one body
/// and not two.
fn connect_to(
    sock: &Arc<UnixSock>,
    listener: &Arc<UnixSock>,
    nonblock: bool,
) -> Result<(), ConnectRefused> {
    if sock.typ == SOCK_DGRAM {
        // A datagram `connect` pins a destination and nothing else: no
        // handshake, no queue entry, no listener's consent - the name just
        // has to answer, and answering as another type is the disagreement
        // Linux calls `EPROTOTYPE` (measured 91 for a path naming either a
        // stream or a sequenced-packet socket; an abstract name whose type
        // differs is not found by the type-keyed table and answers
        // `ECONNREFUSED` before it reaches this line).
        if !listener.is_dgram() {
            return Err(ConnectRefused::WrongType);
        }
        critical_section::with(|cs| {
            let cred = listener.state.borrow(cs).borrow().owner;
            let mut st = sock.state.borrow(cs).borrow_mut();
            st.dgram_peer = Some(Arc::downgrade(listener));
            st.peer = Some(cred);
        });
        return Ok(());
    }
    // The two ends must agree on what the connection carries. Linux refuses
    // a stream dialing a sequenced-packet listener and the other way round
    // with `ECONNREFUSED` (measured: -111 in both directions), and the
    // stream handshake below would happily make a byte channel out of
    // either.
    if sock.typ != listener.typ {
        // A path name is found by its node whatever the type, and Linux
        // answers that disagreement with `EPROTOTYPE` (measured 91, both
        // directions). An *abstract* name never reaches here: its table key
        // includes the type, so the lookup misses and `connect_named`
        // answers `ECONNREFUSED` - also measured.
        return Err(ConnectRefused::WrongType);
    }
    // A sequenced-packet connection is a *message* pair: each end owns a
    // queue, and a send is one message in the other's. The stream handshake
    // is the byte shape only.
    if sock.typ == SOCK_SEQPACKET {
        return connect_messages(sock, listener, nonblock);
    }
    loop {
        let outcome = critical_section::with(|cs| {
            {
                let mut stl = listener.state.borrow(cs).borrow_mut();
                let backlog = match stl.backlog {
                    Some(b) => b,
                    None => return ConnectOutcome::Refused,
                };
                if !admits_one_more(backlog, stl.pending.len()) {
                    return ConnectOutcome::Full;
                }
                // The pair's own credential slot stays empty: this conduit is
                // never the inode a descriptor names - the `UnixSock` halves
                // wrap it, and they are who `SO_PEERCRED` answers from, with
                // the `peer` recorded just below.
                let (client, server) = DuplexEnd::pair((0, 0, 0));
                let listener_cred = stl.owner;
                let waiters = stl.accept_waiters.clone();
                // The connector owns its half immediately: connect is not
                // a rendezvous, and an unaccepted connection is a live
                // socket on both sides (Linux's behavior once backlog
                // space existed). It also records who it dialed into - the
                // listener's credentials are this socket's peer from now on.
                let connector_cred = {
                    let mut stc = sock.state.borrow(cs).borrow_mut();
                    let cred = stc.owner;
                    stc.end = Some(client);
                    stc.peer = Some(listener_cred);
                    cred
                };
                // The connector's own credentials ride the queued half, so the
                // socket `accept` mints can report them as its peer.
                stl.pending
                    .push_back(Pending::Stream(server, connector_cred));
                ConnectOutcome::Connected(waiters)
            }
        });
        match outcome {
            ConnectOutcome::Connected(waiters) => {
                wake_all(waiters);
                return Ok(());
            }
            ConnectOutcome::Refused => return Err(ConnectRefused::Refused),
            ConnectOutcome::Full => {
                if nonblock {
                    return Err(ConnectRefused::Again);
                }
                // Room appears when an accept pops. Park on the listener
                // the way pipe parks: register, wait on the condition
                // (a catchable signal ends it), unregister, retry.
                #[cfg(target_os = "none")]
                if room_wait(&listener).is_err() {
                    // `EINTR`: nothing was connected, and whether to retry is
                    // the caller's decision - which is the difference between
                    // this and the `Again` above, the one the caller asked for.
                    return Err(ConnectRefused::Interrupted);
                }
                #[cfg(not(target_os = "none"))]
                return Err(ConnectRefused::Again);
            }
        }
    }
}

/// The `SOCK_SEQPACKET` handshake: two sockets pointing at each other as
/// message peers, the server half queued for `accept`.
///
/// It is the stream handshake's shape with the channel replaced by a pair of
/// queues - which is what the family already uses for an unnamed
/// `socketpair` half. The server socket is minted *outside* the listener's
/// critical section because registering it takes the registry's own section,
/// and dropping an unqueued one under the listener's lock would re-enter it
/// from `UnixSock::drop`.
fn connect_messages(
    sock: &Arc<UnixSock>,
    listener: &Arc<UnixSock>,
    nonblock: bool,
) -> Result<(), ConnectRefused> {
    loop {
        let server = alloc_in(listener.net_ns, SOCK_SEQPACKET);
        let (outcome, unqueued) = critical_section::with(|cs| {
            let mut stl = listener.state.borrow(cs).borrow_mut();
            let backlog = match stl.backlog {
                Some(b) => b,
                None => return (ConnectOutcome::Refused, Some(server)),
            };
            if !admits_one_more(backlog, stl.pending.len()) {
                return (ConnectOutcome::Full, Some(server));
            }
            let listener_cred = stl.owner;
            let waiters = stl.accept_waiters.clone();
            let connector_cred = sock.state.borrow(cs).borrow().owner;
            // The connector owns its end immediately, as a stream connector
            // does: connect is not a rendezvous. Its peer is the listener.
            {
                let mut stc = sock.state.borrow(cs).borrow_mut();
                stc.dgram_peer = Some(Arc::downgrade(&server));
                stc.peer = Some(listener_cred);
            }
            // The accepted end belongs to the listener, not to whichever
            // task happened to call `connect`: `SO_PEERCRED` on the client
            // must read the listener's owner, the same number a stream
            // accept leaves there.
            {
                let mut sts = server.state.borrow(cs).borrow_mut();
                sts.owner = listener_cred;
                sts.dgram_peer = Some(Arc::downgrade(sock));
                sts.peer = Some(connector_cred);
            }
            stl.pending.push_back(Pending::Messages(server));
            (ConnectOutcome::Connected(waiters), None)
        });
        // Outside every borrow: an unqueued socket's drop takes the registry
        // section, and a wake needs locks no drop may hold.
        drop(unqueued);
        match outcome {
            ConnectOutcome::Connected(waiters) => {
                wake_all(waiters);
                return Ok(());
            }
            ConnectOutcome::Refused => return Err(ConnectRefused::Refused),
            ConnectOutcome::Full => {
                if nonblock {
                    return Err(ConnectRefused::Again);
                }
                #[cfg(target_os = "none")]
                if room_wait(listener).is_err() {
                    return Err(ConnectRefused::Interrupted);
                }
                #[cfg(not(target_os = "none"))]
                return Err(ConnectRefused::Again);
            }
        }
    }
}

/// Run a collected wake set outside every borrow, on the machine only -
/// the pipe/eventfd discipline the module docs repeat.
fn wake_all(ids: Vec<TaskId>) {
    #[cfg(target_os = "none")]
    for t in ids {
        crate::sched::wake(t);
    }
    #[cfg(not(target_os = "none"))]
    let _ = ids;
}

/// How a `connect` ended, told in the call site's errno vocabulary.
pub(crate) enum ConnectRefused {
    /// ECONNREFUSED: nothing listening behind the name (D4's list).
    Refused,
    /// EPROTOTYPE: the name's socket carries a different kind of data than
    /// the connector's (`SOCK_STREAM` dialing a `SOCK_SEQPACKET` listener,
    /// the reverse, and a `SOCK_DGRAM` dialing either), which Linux says only
    /// for a path name - the abstract table is keyed by type, so a name of
    /// another type is not found there at all.
    WrongType,
    /// EAGAIN: backlog full and the call nonblocking.
    Again,
    /// EINTR: a catchable signal ended the wait for room in a full backlog,
    /// having connected nothing.
    Interrupted,
}

enum ConnectOutcome {
    Connected(Vec<TaskId>),
    Full,
    Refused,
}

/// Why an accept queue has a connection waiting: the wake set collected
/// under the lock, run outside it.
#[cfg(target_os = "none")]
fn room_wait(listener: &Arc<UnixSock>) -> VfsResult<()> {
    let me = crate::sched::current();
    critical_section::with(|cs| {
        let mut st = listener.state.borrow(cs).borrow_mut();
        if !st.room_waiters.contains(&me) {
            st.room_waiters.push(me);
        }
    });
    let end = crate::sched::block_until_interruptible(|| {
        critical_section::with(|cs| {
            let st = listener.state.borrow(cs).borrow();
            match st.backlog {
                Some(b) => admits_one_more(b, st.pending.len()),
                None => true, // the listener died while we waited
            }
        })
    });
    critical_section::with(|cs| {
        listener
            .state
            .borrow(cs)
            .borrow_mut()
            .room_waiters
            .retain(|&w| w != me);
    });
    match end {
        crate::sched::WaitEnd::Interrupted => Err(VfsError::Interrupted),
        _ => Ok(()),
    }
}

/// One `accept`'s pop. `Err(EAGAIN)` when the queue is empty and the
/// caller made the descriptor nonblocking; the blocking wait is
/// [`accept_blocking`], split out so the queue arithmetic is host-testable
/// exactly like the pipe's wait is.
pub(crate) fn accept_once(sock: &UnixSock, nonblock: bool) -> Result<Pending, isize> {
    loop {
        let popped = critical_section::with(|cs| {
            let mut st = sock.state.borrow(cs).borrow_mut();
            match st.pending.pop_front() {
                Some(conn) => {
                    // Room for a blocked connect, if any waits.
                    Some((conn, st.room_waiters.clone()))
                }
                None => None,
            }
        });
        if let Some((conn, room)) = popped {
            wake_all(room);
            return Ok(conn);
        }
        if nonblock {
            return Err(crate::err::EAGAIN);
        }
        match accept_blocking_step(sock) {
            AcceptStep::Woken => {}
            // `EINTR`, with the queue untouched: a signal that arrives while a
            // program is waiting for a client is the program's own news, and
            // Linux's `accept` reports it rather than swallowing it.
            AcceptStep::Interrupted => return Err(crate::err::EINTR),
        }
    }
}

/// What one park in an `accept` loop ended on.
///
/// The "listener is gone" case the older comment here promised is not
/// expressible: the acceptor holds an `Arc` to the socket, so nothing can drop
/// it under a park, and what the wait ends on is the queue it was asked about
/// (or a signal). A name for a state that cannot happen would be a second
/// answer to a question with one.
#[cfg(target_os = "none")]
enum AcceptStep {
    /// Something may be queued now: look again.
    Woken,
    /// A catchable signal ended the wait, and nothing was consumed.
    Interrupted,
}

/// Park until this listener's queue is nonempty, or until a catchable signal
/// ends the wait. See [`AcceptStep`] for what the two answers mean.
#[cfg(target_os = "none")]
fn accept_blocking_step(sock: &UnixSock) -> AcceptStep {
    let me = crate::sched::current();
    critical_section::with(|cs| {
        let mut st = sock.state.borrow(cs).borrow_mut();
        if !st.accept_waiters.contains(&me) {
            st.accept_waiters.push(me);
        }
    });
    let end = crate::sched::block_until_interruptible(|| sock.pending_nonempty());
    critical_section::with(|cs| {
        let mut st = sock.state.borrow(cs).borrow_mut();
        st.accept_waiters.retain(|&w| w != me);
    });
    match end {
        crate::sched::WaitEnd::Interrupted => AcceptStep::Interrupted,
        _ => AcceptStep::Woken,
    }
}

#[cfg(not(target_os = "none"))]
fn accept_blocking_step(_sock: &UnixSock) -> AcceptStep {
    panic!("blocking accept needs the kernel scheduler; push_pending in tests");
}

/// The `listen` call: record the backlog. Linux's odd zero answer is decided
/// by [`clamp_backlog`] and the handler; the first `listen` on a socket is
/// what makes a `connect` acceptable at all.
pub(crate) fn listen(sock: &UnixSock, backlog: i32) -> Result<(), isize> {
    // `listen` is the stream call: it opens an accept queue, and a
    // datagram socket has no such thing - its queue is its queue, already
    // open from the moment the socket was. Linux answers `EOPNOTSUPP`.
    if sock.typ == SOCK_DGRAM {
        return Err(err::EOPNOTSUPP);
    }
    let cap = clamp_backlog(backlog);
    critical_section::with(|cs| {
        let mut st = sock.state.borrow(cs).borrow_mut();
        st.backlog = Some(cap);
    });
    Ok(())
}

/// Whether a listener whose queue is `pending` deep admits one more
/// connection, given the backlog its `listen` named.
///
/// Linux's rule, and it is an off-by-one worth writing down: `unix_recvq_full`
/// compares the queue's length to `sk_max_ack_backlog` with `>`, so `listen(fd,
/// n)` admits `n + 1` connections before the next one is refused, and
/// `listen(fd, 0)` admits exactly one. That is not a curiosity - it is the
/// number eudev's control socket is created with (`listen(uctrl->sock, 0)` in
/// `udev-ctrl.c`), and a kernel that read "a zero backlog never admits" out of
/// the man page left `udevadm` unable to reach the daemon at all. Measured
/// against the host kernel before being written: backlogs 0, 1, 2 and 3
/// admitted 1, 2, 3 and 4 connections, with the next `connect` answering
/// `EAGAIN`.
fn admits_one_more(backlog: usize, pending: usize) -> bool {
    pending <= backlog
}

/// The backlog rule, pure, and it is `__sys_listen`'s rather than any one
/// family's - both this module and the IPv4 pool read it. Linux compares
/// the argument to `SOMAXCONN` as an *unsigned* int, so a negative backlog
/// is not refused: every one of them lands on `SOMAXCONN`, and anything
/// above the ceiling is clamped to it (Linux since 2.4.22). Zero, in
/// between, is a real queue of one - see [`admits_one_more`] for the
/// arithmetic and the measurement behind it.
///
/// The negative half is the one a caller actually sends, which is why it
/// is spelled out: Rust's std passes `-1` on Linux, in both families
/// (`UnixListener::bind` and `TcpListener::bind` call `listen(fd, -1)`,
/// "silently capped to `/proc/sys/net/core/somaxconn`"), and that is the
/// call `wayland-server`'s `ListeningSocket::bind_absolute` makes for a
/// compositor's socket. Refusing it stopped niri at
/// `ListeningSocketSource::new_auto` (`src/niri.rs:2435`), `EINVAL` from a
/// call Linux answers 0 to. Measured on the host before it was written:
/// `listen(fd, -1)` returns 0 for `AF_UNIX` and `AF_INET` alike, and
/// `ss -lx`/`ss -ltn` then report the backlog as 4096 - this host's
/// `net.core.somaxconn`, not the zero a "negative means empty queue"
/// reading would leave.
pub(crate) fn clamp_backlog(backlog: i32) -> usize {
    (backlog as u32).min(SOMAXCONN as u32) as usize
}

// ---------------------------------------------------------------------------
// `struct sockaddr_un`: the path in front of us
// ---------------------------------------------------------------------------

/// A `sockaddr_un`'s address: the two namespaces Linux has.
///
/// They are not two spellings of one thing. A *path* names a node in the
/// filesystem, and the node - not the string - is what the table keys on, so a
/// symlink, a `..` and a mount all come for free. An *abstract* name is bytes
/// and nothing else: no node, no filesystem, no `unlink`. Linux keys those on
/// the name **and the socket type**, so a stream and a datagram socket may hold
/// one name between them (measured on the host: `bind` from a datagram socket on
/// a stream socket's name answers 0), while a path is one node and cannot be
/// shared that way at all.
#[derive(Clone)]
pub(crate) enum UnixAddr {
    /// A filesystem path, for the caller to resolve to the node that holds it.
    Path(String),
    /// An abstract name: the bytes after the leading NUL, to the end of the
    /// caller's `addrlen` - length-delimited rather than NUL-terminated, so a
    /// name may contain NULs of its own (measured: Linux binds one).
    Abstract(Vec<u8>),
}

/// The address a `sockaddr_un`'s bytes carry, the one parser every unix call
/// reads through (bind, connect, sendto and the `getsockname` writer shares the
/// length arithmetic).
///
/// The rules, each measured on the host rather than read off a page
/// (`.workbuddy/probe_unix_abstract.c`):
///
/// * fewer than the two family bytes, or more than the struct, is `EINVAL`, and
///   the family must be `AF_UNIX`;
/// * **`addrlen` of exactly the family** is the caller naming nothing at all.
///   Linux *auto*-binds it (a generated abstract name comes back from
///   `getsockname`) and refuses a `connect` with `EINVAL`, so this is
///   [`UnixAddrParse::Autobind`] and the call decides;
/// * a first `sun_path` byte of NUL is the abstract namespace: the **whole**
///   remainder of the caller's `addrlen` is the name, NULs and all, and an empty
///   one is a name of zero bytes (also measured: `bind` serves it);
/// * anything else is a path: NUL-terminated, or the full field with no NUL -
///   Linux's one exception for a path that fills the struct.
///
/// A path that has bytes *after* its terminator is still `EINVAL` here and
/// ignored on Linux; that divergence is named in the module header rather than
/// served, because the bytes after a terminator are not part of any name.
pub(crate) enum UnixAddrParse {
    Name(UnixAddr),
    Autobind,
}

pub(crate) fn sockaddr_un_addr(raw: &[u8]) -> Result<UnixAddrParse, isize> {
    let einval = || Err(crate::err::EINVAL);
    if raw.len() < 2 || raw.len() > 2 + SUN_PATH_MAX {
        return einval();
    }
    if raw[0] as u16 | ((raw[1] as u16) << 8) != crate::net::socket::AF_UNIX {
        return einval();
    }
    let body = &raw[2..];
    if body.is_empty() {
        return Ok(UnixAddrParse::Autobind);
    }
    if body[0] == 0 {
        return Ok(UnixAddrParse::Name(UnixAddr::Abstract(body[1..].to_vec())));
    }
    let end = match body.iter().position(|&b| b == 0) {
        Some(i) => i,
        // A path that fills the field with no NUL anywhere: Linux accepts
        // exactly this one case (the field *is* the terminator-less name);
        // this kernel's path keys are `String`s with a length, so the same
        // 108-byte name can be taken whole.
        None => {
            let name = core::str::from_utf8(body).map_err(|_| crate::err::EINVAL)?;
            return Ok(UnixAddrParse::Name(UnixAddr::Path(name.to_string())));
        }
    };
    if body[end + 1..].iter().any(|&b| b != 0) {
        return einval(); // an interior NUL with bytes after it
    }
    let name = core::str::from_utf8(&body[..end]).map_err(|_| crate::err::EINVAL)?;
    Ok(UnixAddrParse::Name(UnixAddr::Path(name.to_string())))
}

/// The bytes a `sockaddr_un` answering `getsockname` carries for an abstract
/// name: the family, the leading NUL, and the name - `addrlen` is `3 + len`,
/// which is what Linux answers (measured: 13 for a ten-byte name).
pub(crate) fn sockaddr_un_abstract_bytes(name: &[u8]) -> Vec<u8> {
    let mut out = alloc::vec::Vec::with_capacity(3 + name.len());
    out.push(crate::net::socket::AF_UNIX as u8);
    out.push((crate::net::socket::AF_UNIX >> 8) as u8);
    out.push(0);
    out.extend_from_slice(name);
    out
}

/// The bytes a `sockaddr_un` answering `getsockname` carries: family, the
/// path's bytes, one NUL; `min_len` is the caller's `addrlen` in-out,
/// which Linux answers with the *used* length.
pub(crate) fn sockaddr_un_bytes(path: &str) -> Vec<u8> {
    let mut out = alloc::vec::Vec::with_capacity(2 + path.len() + 1);
    out.extend_from_slice(&crate::net::socket::AF_UNIX.to_ne_bytes());
    out.extend_from_slice(path.as_bytes());
    out.push(0);
    out
}

use alloc::vec::Vec;
// ---------------------------------------------------------------------------
// The syscall faces: bind's node, the address writer
// ---------------------------------------------------------------------------

use crate::err;

/// Plant the filesystem half of a bind, then record the table half.
///
/// The order is the kernel promise in two acts: the name first - through
/// the same parent-split, permission gate and `create` call `mkdirat`
/// makes, so `may_create_in`, the sticky bit and the inotify `IN_CREATE`
/// news all arrive for free - the table second. `getsockname` echoes the
/// path exactly as `bind` spelled it, which is what Linux stores: the
/// bytes of the address, not a recomputed absolute name.
pub(crate) fn bind_path(sock: &UnixSock, path: &str) -> isize {
    // One socket, one name: Linux refuses a second bind on the grounds
    // that the first is what the fd answers by. The check is the stored
    // key's own - the node it pins is what keeps that key from ever
    // naming a different file behind this socket's back.
    if sock.is_bound() {
        return err::EINVAL;
    }
    let cwd = crate::sched::current_cwd();
    // The last component is a name, not a step: Linux's `user_path_parent`
    // resolves the directory and hands the final name to the operation, so a
    // bind under a plain file answers ENOTDIR from that lookup - never EACCES
    // from searching through something that cannot hold names.
    let (parent, name) = match crate::vfs::resolve_parent_at(path, cwd) {
        Ok(split) => split,
        Err(e) => return crate::uaccess::err_of(e),
    };
    let name = name.as_str();
    // The last component must be free, whatever kind of thing occupies it
    // (Linux refuses both a live socket and a plain file under the name). A
    // *stale* socket - one no descriptor holds any more - is retired first,
    // exactly as Linux's `unix_bind` does, and the retirement is an `unlink`:
    // write permission on the file being removed, not a search of the
    // directory through it. Measured against this machine's own userland:
    // asking the parent instead (`may_remove`'s shape) refused root's
    // replacement of its own leftover `/var/run` names with EACCES.
    if let Ok(existing) = parent.lookup(name) {
        if existing.metadata().kind != NodeKind::Socket || is_held(&existing) {
            return err::EADDRINUSE;
        }
        if let Err(e) = crate::vfs::may_write(&existing) {
            return crate::uaccess::err_of(e);
        }
        unbind_node(&existing);
        if let Err(e) = parent.unlink(name) {
            return crate::uaccess::err_of(e);
        }
        crate::vfs::inotify::emit(&parent, crate::vfs::inotify::IN_DELETE, name);
    }
    if let Err(e) = crate::vfs::may_create_in(&parent) {
        return crate::uaccess::err_of(e);
    }
    let node = match parent.create(name, NodeKind::Socket, None) {
        Ok(created) => created,
        Err(VfsError::Exists) => return err::EADDRINUSE,
        Err(VfsError::Unsupported) => return err::EOPNOTSUPP,
        Err(e) => return crate::uaccess::err_of(e),
    };
    // Linux's birth mode for a unix socket, mask applied (`0755` less the
    // default `022` is the `srwxr-xr--` an unmodified `bind` leaves). The
    // filesystem that made the node already stamped its own default; this
    // is what says the name is a socket's rather than whatever `create`
    // guessed.
    node.set_mode(0o755 & !crate::sched::current_umask());
    let key = (
        crate::vfs::mount::fs_id_of_inode(&node),
        node.metadata().ino,
    );
    let arc = sock_arc(sock);
    bind(&arc, node, path.into(), key);
    // The create news, said by the directory the name appeared in - the
    // same report fs.rs makes for every other successful create.
    crate::vfs::inotify::emit(&parent, crate::vfs::inotify::IN_CREATE, name);
    0
}

/// Whether a node is a socket name some live descriptor still holds - the
/// difference between a bound name (`EADDRINUSE`) and a stale one `bind` may
/// replace. The registry keys on `(fs id, ino)`, the same pair `unbind_node`
/// retires: an entry whose socket has died is exactly what `live_listener`
/// treats as no entry at all, so it holds nothing.
fn is_held(node: &Arc<dyn Inode>) -> bool {
    let key = (crate::vfs::mount::fs_id_of_inode(node), node.metadata().ino);
    critical_section::with(|cs| {
        let reg = UNIX.borrow(cs).borrow();
        match reg.by_node.get(&key) {
            Some(id) => reg
                .slots
                .get(*id - 1)
                .and_then(|slot| slot.as_ref())
                .and_then(|slot| slot.socket.upgrade())
                .is_some(),
            None => false,
        }
    })
}

/// The bind record wants an `Arc` for the registry's weak self-reference; the
/// handler holds only a borrow. The caller's own fd is what keeps the socket
/// alive - the upgrade cannot fail while the borrow it came from exists - so
/// the registry is the one place that can turn "the socket this fd names"
/// back into the Arc itself.
fn sock_arc(sock: &UnixSock) -> Arc<UnixSock> {
    critical_section::with(|cs| {
        let reg = UNIX.borrow(cs).borrow();
        reg.slots
            .get(sock.id - 1)
            .and_then(|slot| slot.as_ref())
            .and_then(|slot| slot.socket.upgrade())
            .expect("a unix fd outlives its own socket while borrowed")
    })
}

/// Write the bound name back as a `sockaddr_un`, honoring the caller
/// in-out `addrlen` the Linux way: copy what fits, report the used length
/// through, and answer the probe form (`addrlen` pointing at zero, asking
/// for the needed size) by storing the length without touching the
/// address.
pub(crate) fn write_name(sock: &UnixSock, addr_ptr: usize, len_ptr: usize) -> isize {
    write_addr(sock.local_addr().as_ref(), addr_ptr, len_ptr)
}

/// The writer both callers share: `getsockname` answers the socket's own bound
/// address, `recvfrom` the *sender's* of the datagram just taken. The two
/// namespaces differ only in the bytes they spell - a path is the string and a
/// NUL, an abstract name is the leading NUL and the bytes - and `addrlen` is
/// the length of whichever one is written, which is what Linux reports.
pub(crate) fn write_addr(addr: Option<&UnixAddr>, addr_ptr: usize, len_ptr: usize) -> isize {
    let bytes = match addr {
        Some(UnixAddr::Path(path)) => sockaddr_un_bytes(path),
        Some(UnixAddr::Abstract(name)) => sockaddr_un_abstract_bytes(name),
        // No address to answer with. Linux writes the bare family; this
        // kernel has always written an empty path (the same three bytes the
        // empty-path probe sees) and that divergence is `t-unix`'s.
        None => sockaddr_un_bytes(""),
    };
    if len_ptr == 0 {
        return err::EFAULT;
    }
    if !crate::uaccess::buffer_ok(len_ptr, 4, true, true) {
        return err::EFAULT;
    }
    let want = unsafe { (len_ptr as *const u32).read_unaligned() } as usize;
    // The caller asked for the name when there is somewhere to put it: a
    // zero `addrlen` is the probe, and the length written below is the
    // whole answer. A nonzero `addrlen` with a null pointer is the caller
    // breaking their own word - EFAULT, the man page's.
    if want > 0 {
        if addr_ptr == 0 || !crate::uaccess::buffer_ok(addr_ptr, want, true, true) {
            return err::EFAULT;
        }
        let n = bytes.len().min(want);
        unsafe {
            core::ptr::copy_nonoverlapping(bytes.as_ptr(), addr_ptr as *mut u8, n);
        }
    }
    unsafe {
        (len_ptr as *mut u32).write_unaligned(bytes.len() as u32);
    }
    0
}

/// The connect act for a unix socket whose fd the caller named: resolve
/// the path to its node, refuse the kinds that cannot answer (`ENOTSOCK`
/// for the socket's own connect, Linux's `ECONNREFUSED` for a name that
/// is there but not a socket), and hand the live listener's queue the
/// handshake. A name that is not there at all is `ENOENT`.
pub(crate) fn connect_path(sock: &UnixSock, path: &str, nonblock: bool) -> isize {
    let cwd = crate::sched::current_cwd();
    let node = match crate::vfs::resolve_opt(path, Some(cwd)) {
        Ok(node) => node,
        Err(e) => return crate::uaccess::err_of(e),
    };
    if node.metadata().kind != NodeKind::Socket {
        return err::ECONNREFUSED;
    }
    // Linux checks the path's write permission before it checks whether
    // anyone is listening; the order only shows to a caller that has
    // neither, and the permission first is the man page's.
    if let Err(e) = crate::vfs::may_access(&node, crate::vfs::W_OK) {
        return crate::uaccess::err_of(e);
    }
    // Already connected sockets cannot connect again.
    if sock.end_of().is_some() || sock.dgram_connected() {
        return err::EISCONN;
    }
    let arc = sock_arc(sock);
    match connect(&arc, &node, nonblock) {
        Ok(()) => 0,
        Err(ConnectRefused::Refused) => err::ECONNREFUSED,
        Err(ConnectRefused::WrongType) => err::EPROTOTYPE,
        Err(ConnectRefused::Again) => err::EAGAIN,
        Err(ConnectRefused::Interrupted) => err::EINTR,
    }
}

// ---------------------------------------------------------------------------
// The datagram face: a send to a name, a receive from the queue
// ---------------------------------------------------------------------------

/// One datagram's whole send, from either spelling of the call: with a
/// name the socket is aiming at it this time (the `sendto` shape, which
/// does not remember the aim); without one, the queue of the peer a
/// previous `connect` pinned is where the message goes, and a socket that
/// was never connected sends nowhere - `ENOTCONN`, the man page's answer
/// for a sender with no one to send to.
///
/// Delivery is the receiving socket's queue: a datagram never waits for a
/// reader the way a stream byte does, it waits for *room*. A full queue
/// answers `EAGAIN` to the sender - the receiver's slow read is the
/// sender's problem to hear about, which is the honest version of Linux's
/// socket-buffer accounting at this scale.
pub(crate) fn send_dgram(
    sock: &UnixSock,
    data: &[u8],
    dest: Option<&UnixAddr>,
    fds: Vec<Arc<crate::vfs::File>>,
) -> isize {
    if data.len() > GRAM_MAX {
        return err::EMSGSIZE;
    }
    let target: Arc<UnixSock> = match dest {
        // A sequenced-packet connection has no per-call destination, and the
        // door above usually drops the name before this function sees it.
        // Keeping the rule here too means a caller that still arrives with one
        // (the `socketpair` shape) cannot be sent anywhere but the peer.
        Some(_) if sock.ignores_send_address() => match sock.dgram_peer() {
            Some(peer) => peer,
            None => return err::ENOTCONN,
        },
        // A datagram socket honours the name it is given, connected or not:
        // Linux resolves and validates it exactly as an unconnected send does
        // (measured: naming a different live socket sends there, a dead name is
        // `ENOENT`, a regular file is `ECONNREFUSED`, and a path naming a live
        // socket of another type is `EPROTOTYPE`). Being connected only decides
        // where a send *without* a name goes.
        Some(addr) => {
            match addr {
                UnixAddr::Path(path) => {
                    let cwd = crate::sched::current_cwd();
                    let node = match crate::vfs::resolve_opt(path, Some(cwd)) {
                        Ok(node) => node,
                        Err(e) => return crate::uaccess::err_of(e),
                    };
                    if node.metadata().kind != NodeKind::Socket {
                        return err::ECONNREFUSED;
                    }
                    let target = match live_listener(&node) {
                        Some(sock) => sock,
                        None => return err::ECONNREFUSED,
                    };
                    // The node is a live socket, but not one that carries this
                    // sender's kind of data. Linux's path lookup finds the
                    // socket by inode whatever its type and answers that
                    // disagreement `EPROTOTYPE` (measured 91 for both a stream
                    // and a sequenced-packet name). The abstract half never
                    // reaches this check: its table key includes the type, so a
                    // different type is simply not found (`ECONNREFUSED`).
                    if target.typ != sock.typ {
                        return err::EPROTOTYPE;
                    }
                    target
                }
                // An abstract name has no node to resolve and no permission to
                // check; a name nobody holds answers what a dead node answers
                // (measured: 111 on Linux for a datagram `sendto`), and the
                // name is looked up in the **sender's** namespace, which is the
                // namespace the table it landed in belongs to.
                UnixAddr::Abstract(name) => match live_named(sock.net_ns, name, sock.typ) {
                    Some(sock) => sock,
                    None => return err::ECONNREFUSED,
                },
            }
        }
        None => match sock.dgram_peer() {
            Some(peer) => peer,
            None => return err::ENOTCONN,
        },
    };
    // A socket with no message queue to land in: the name is there, no datagram
    // lives behind it, and the refusal is the one `connect` to the same name
    // would answer - `ECONNREFUSED`.
    //
    // The test is "carries messages", not "is a datagram socket", and the
    // difference is the *sequenced-packet* pair: a `socketpair(AF_UNIX,
    // SOCK_SEQPACKET, ...)` half has exactly the queue this is looking for
    // while its type name is neither `SOCK_STREAM` nor `SOCK_DGRAM`, so a check
    // written against the one type name refused the message at the sender - and
    // `t-udev-worker` carries that case. (udevd's own *worker* channel is a
    // `SOCK_DGRAM` pair and would have been admitted either way; the seqpacket
    // shape is the one that would have kept failing.)
    if !target.carries_messages() {
        return err::ECONNREFUSED;
    }
    let from = sock.local_addr();
    if target.push_dgram(data, from, self_cred(), fds) {
        data.len() as isize
    } else {
        err::EAGAIN
    }
}

/// One receive on a message socket, as both the take and the peek answer it:
/// the message's bytes, the sender's name and the credentials the send
/// recorded. `data.len()` is the message's own length, which is what
/// `MSG_TRUNC` reports even when only part of it was copied.
pub(crate) struct RecvDgram {
    pub data: Vec<u8>,
    pub from: Option<UnixAddr>,
    pub cred: UCred,
    /// The message's `SCM_RIGHTS` descriptors, cloned for a peek so the
    /// queued record keeps its own set.
    pub fds: Vec<Arc<crate::vfs::File>>,
}

/// The accept act, handler side: pop (or park), wrap the server half in a
/// fresh unix socket, and hand the caller a descriptor that is already
/// connected - the shape an accepted socket has on every system.
///
/// The Arc leaves *by name*, not as a slot number: the registry holds its
/// slots weak, so a socket nothing owns is gone the moment the statement
/// that made it ends - and an id sent back through the handler would be
/// looked up in a slot that had already died. The caller receives the one
/// strong reference that keeps the accepted socket alive until the fd table
/// has taken its own.
pub(crate) fn accept_face(listener: &UnixSock, nonblock: bool) -> Result<Arc<UnixSock>, isize> {
    let pending = accept_once(listener, nonblock)?;
    match pending {
        // A message connection already *is* the server socket: `connect`
        // minted it, pointed both ends at each other and recorded the
        // credentials, so there is no channel left to wrap.
        Pending::Messages(server) => Ok(server),
        Pending::Stream(end, peer) => {
            // The listener's namespace, not the accepter's: the accepted
            // socket is the server's other end, and it is the server's table
            // an abstract name of its own would land in (Linux's
            // `unix_accept` uses `sock_net` of the listener).
            let accepted = alloc_in(listener.net_ns, listener.typ);
            accepted.with_state_connect(end, peer);
            Ok(accepted)
        }
    }
}

impl UnixSock {
    /// Install a channel the accept queue just released, and the credentials
    /// of the socket that dialed it - the accepted socket's peer, which
    /// `SO_PEERCRED` and `SCM_CREDENTIALS` report from here on. Inherent to
    /// the type; the handler borrows through the Arc it made for the fd.
    fn with_state_connect(&self, end: Arc<DuplexEnd>, peer: UCred) {
        critical_section::with(|cs| {
            let mut st = self.state.borrow(cs).borrow_mut();
            st.end = Some(end);
            st.peer = Some(peer);
        });
    }
}

/// The slot's live socket by its own id - the registry is the only place
/// that can turn "slot number" back into an `Arc`, and the accept path
/// needs the arc to queue onto and to hand out.
pub(crate) fn by_id(id: usize) -> Option<Arc<UnixSock>> {
    critical_section::with(|cs| {
        UNIX.borrow(cs)
            .borrow()
            .slots
            .get(id - 1)
            .and_then(|slot| slot.as_ref())
            .and_then(|slot| slot.socket.upgrade())
    })
}

/// Write-back for the accepted socket's peer name, which is the
/// listener's bound path - by slot id, the accept handler's number.
pub(crate) fn write_name_by_id(listener_id: usize, addr_ptr: usize, len_ptr: usize) -> isize {
    match by_id(listener_id) {
        Some(sock) => write_name(&sock, addr_ptr, len_ptr),
        None => err::EBADF,
    }
}
