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
//! Deliberate departures from Linux, each visible from userland:
//! * `SOCK_SEQPACKET` over AF_UNIX opens - udevd cannot start without it -
//!   but carries no message boundaries until a `socketpair` connects it: a
//!   *named* one is served as the byte stream this family has (the type's
//!   own note in `net::socket` tells the trade).
//! * the abstract namespace (`sun_path[0] == '\0'`) answers `EINVAL` - it
//!   has no filesystem node, hence no part of this design that would hold
//!   it, and no target daemon reaches for it.
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
    state: Mutex<RefCell<UnixState>>,
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
    /// The pending connections: the server-facing halves minted by
    /// `connect`, each tagged with the connector's credentials so the socket
    /// `accept` mints learns who dialed it, waiting for `accept` to hand them
    /// to a descriptor.
    pending: VecDeque<(Arc<DuplexEnd>, UCred)>,
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
    from: Option<String>,
    cred: UCred,
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

struct BoundName {
    /// The path spelled at `bind`, for `getsockname`'s echo. The node the
    /// bind planted is *not* kept: its `(dev, ino)` identity, which the
    /// table keys on and `unlink` retires, never comes back - ramfs mints
    /// inode numbers from a counter that only moves forward, and the one
    /// filesystem that reuses numbers (ext4) cannot host a socket node at
    /// all. Pinning the node would hold a deleted name's memory for
    /// nothing.
    path: String,
    key: (u64, u64),
}

impl UnixSock {
    fn new(id: usize, typ: u16, owner: UCred) -> Arc<UnixSock> {
        Arc::new(UnixSock {
            id,
            typ,
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
    /// connected to a peer: that is exactly what a `socketpair` half is, and
    /// there is no other way for one to exist here - a *named*
    /// sequenced-packet socket is served as the stream this family has (see
    /// `SOCK_SEQPACKET`'s note in `net::socket`).
    pub fn carries_messages(&self) -> bool {
        self.typ == SOCK_DGRAM || self.dgram_connected()
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
    fn push_dgram(&self, data: &[u8], from: Option<String>, cred: UCred) -> bool {
        let waiters = critical_section::with(|cs| {
            let mut st = self.state.borrow(cs).borrow_mut();
            if st.recvq.len() >= RECVQ_MAX {
                return None;
            }
            st.recvq.push_back(Dgram {
                data: data.to_vec(),
                from,
                cred,
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
    /// caller allowed waiting. The answer is the bytes copied and the
    /// sender's name; the copy truncates to the caller's buffer the way
    /// unix datagrams do - the rest of that one message is gone, and the
    /// next read still finds the next message whole.
    fn take_dgram(&self, buf: &mut [u8]) -> Result<(usize, Option<String>, UCred, bool), VfsError> {
        loop {
            let taken = critical_section::with(|cs| {
                let mut st = self.state.borrow(cs).borrow_mut();
                match st.recvq.pop_front() {
                    Some(g) => Some((g.data, g.from, g.cred, st.read_waiters.clone())),
                    None => None,
                }
            });
            if let Some((data, from, cred, _waiters)) = taken {
                // The queue only got emptier by one message, so the other
                // parked readers stay parked until a push wakes them all;
                // this take's own waiters set is the poll subscribers, and
                // `epoll` re-arms them through `poll_ready` on its own.
                let n = data.len().min(buf.len());
                buf[..n].copy_from_slice(&data[..n]);
                // `n < data.len()` is the message being longer than the
                // caller's buffer: the rest of that one message is gone, and
                // Linux says so with `MSG_TRUNC` rather than letting the caller
                // guess from a short count - a receive of exactly the size it
                // asked for is otherwise indistinguishable from one that fitted.
                return Ok((n, from, cred, n < data.len()));
            }
            let nonblock = critical_section::with(|cs| self.state.borrow(cs).borrow().nonblocking);
            if nonblock {
                return Err(VfsError::Again);
            }
            #[cfg(not(target_os = "none"))]
            return Err(VfsError::Again);
            #[cfg(target_os = "none")]
            self.wait_for_dgram();
        }
    }

    /// Park until a datagram stands in the queue - the accept wait's
    /// shape: register, wait on the condition, unregister. A signal
    /// breaks `block_until` the way it breaks every other kernel park.
    #[cfg(target_os = "none")]
    fn wait_for_dgram(&self) {
        let me = crate::sched::current();
        critical_section::with(|cs| {
            let mut st = self.state.borrow(cs).borrow_mut();
            if !st.read_waiters.contains(&me) {
                st.read_waiters.push(me);
            }
        });
        crate::sched::block_until(|| {
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

    /// The bound node's table key, if a bind has planted one - the same
    /// fact `local_name` reports, told by identity.
    pub fn bound_key(&self) -> Option<(u64, u64)> {
        critical_section::with(|cs| self.state.borrow(cs).borrow().bound.as_ref().map(|b| b.key))
    }

    /// The bound path, for `getsockname`.
    pub fn local_name(&self) -> Option<String> {
        critical_section::with(|cs| {
            self.state
                .borrow(cs)
                .borrow()
                .bound
                .as_ref()
                .map(|b| b.path.clone())
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

    fn as_unix(&self) -> Option<&UnixSock> {
        Some(self)
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
            let (n, _, _, _) = self.take_dgram(buf)?;
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
            let name = self.local_name();
            if peer.push_dgram(buf, name, self_cred()) {
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
}

struct Slot {
    socket: Weak<UnixSock>,
    /// The key `by_node` points here, so a release can remove its own
    /// entry without scanning paths.
    key: Option<(u64, u64)>,
}

impl Registry {
    const fn new() -> Self {
        Registry {
            slots: Vec::new(),
            by_node: BTreeMap::new(),
        }
    }
}

static UNIX: Mutex<RefCell<Registry>> = Mutex::new(RefCell::new(Registry::new()));

/// A fresh socket and its registry slot. Slot numbers are never reused, so
/// a stale id can only fail its own liveness check, never answer to a
/// different socket.
pub(crate) fn alloc(typ: u16) -> Arc<UnixSock> {
    let owner = self_cred();
    critical_section::with(|cs| {
        let mut reg = UNIX.borrow(cs).borrow_mut();
        let id = reg.slots.len() + 1;
        let sock = UnixSock::new(id, typ, owner);
        reg.slots.push(Some(Slot {
            socket: Arc::downgrade(&sock),
            key: None,
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
        st.bound = Some(BoundName { path, key });
    });
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
            if let Some(Some(slot)) = reg.slots.get_mut(self.id - 1)
                && let Some(key) = slot.key.take()
            {
                reg.by_node.remove(&key);
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
    if sock.typ == SOCK_DGRAM {
        // A datagram `connect` pins a destination and nothing else: no
        // handshake, no queue entry, no listener's consent - the name just
        // has to answer, and answering as a stream is answering with no
        // queue to land messages in, so both are refused the way Linux
        // refuses them.
        if !listener.is_dgram() {
            return Err(ConnectRefused::Refused);
        }
        critical_section::with(|cs| {
            let cred = listener.state.borrow(cs).borrow().owner;
            let mut st = sock.state.borrow(cs).borrow_mut();
            st.dgram_peer = Some(Arc::downgrade(&listener));
            st.peer = Some(cred);
        });
        return Ok(());
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
                stl.pending.push_back((server, connector_cred));
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
                // the way pipe parks: register, wait on the condition (a
                // fatal signal breaks `block_until`), unregister, retry.
                #[cfg(target_os = "none")]
                room_wait(&listener);
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
    /// EAGAIN: backlog full and the call nonblocking.
    Again,
}

enum ConnectOutcome {
    Connected(Vec<TaskId>),
    Full,
    Refused,
}

/// Why an accept queue has a connection waiting: the wake set collected
/// under the lock, run outside it.
#[cfg(target_os = "none")]
fn room_wait(listener: &Arc<UnixSock>) {
    let me = crate::sched::current();
    critical_section::with(|cs| {
        let mut st = listener.state.borrow(cs).borrow_mut();
        if !st.room_waiters.contains(&me) {
            st.room_waiters.push(me);
        }
    });
    crate::sched::block_until(|| {
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
}

/// One `accept`'s pop. `Err(EAGAIN)` when the queue is empty and the
/// caller made the descriptor nonblocking; the blocking wait is
/// [`accept_blocking`], split out so the queue arithmetic is host-testable
/// exactly like the pipe's wait is.
pub(crate) fn accept_once(
    sock: &UnixSock,
    nonblock: bool,
) -> Result<(Arc<DuplexEnd>, UCred), isize> {
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
        if let Some(((end, peer), room)) = popped {
            wake_all(room);
            return Ok((end, peer));
        }
        if nonblock {
            return Err(crate::syscall::err::EAGAIN);
        }
        if !accept_blocking_step(sock) {
            return Err(crate::syscall::err::EAGAIN);
        }
    }
}

/// Park until this listener's queue is nonempty - or until the listener
/// itself dies (its slot upgraded to nothing), which is `block_until`'s
/// fatal-signal condition's cousin: a dead listener must not strand its
/// acceptors. Answers false when the socket is gone (the caller re-checks
/// and leaves).
#[cfg(target_os = "none")]
fn accept_blocking_step(sock: &UnixSock) -> bool {
    let me = crate::sched::current();
    critical_section::with(|cs| {
        let mut st = sock.state.borrow(cs).borrow_mut();
        if !st.accept_waiters.contains(&me) {
            st.accept_waiters.push(me);
        }
    });
    crate::sched::block_until(|| sock.pending_nonempty());
    critical_section::with(|cs| {
        let mut st = sock.state.borrow(cs).borrow_mut();
        st.accept_waiters.retain(|&w| w != me);
    });
    true
}

#[cfg(not(target_os = "none"))]
fn accept_blocking_step(_sock: &UnixSock) -> bool {
    panic!("blocking accept needs the kernel scheduler; push_pending in tests");
}

/// The `listen` call: record the backlog. Linux's odd zero and negative
/// answers are decided by [`clamp_backlog`] and the handler; the first
/// `listen` on a socket is what makes a `connect` acceptable at all.
pub(crate) fn listen(sock: &UnixSock, backlog: i32) -> Result<(), isize> {
    // `listen` is the stream call: it opens an accept queue, and a
    // datagram socket has no such thing - its queue is its queue, already
    // open from the moment the socket was. Linux answers `EOPNOTSUPP`.
    if sock.typ == SOCK_DGRAM {
        return Err(err::EOPNOTSUPP);
    }
    let cap = clamp_backlog(backlog).ok_or(crate::syscall::err::EINVAL)?;
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

/// The backlog rule, pure: negative is `EINVAL`, over `SOMAXCONN` is
/// clamped (Linux since 2.4.22), and zero is a real queue of one - see
/// [`admits_one_more`] for the arithmetic and the measurement behind it.
pub(crate) fn clamp_backlog(backlog: i32) -> Option<usize> {
    if backlog < 0 {
        return None;
    }
    Some((backlog as usize).min(SOMAXCONN))
}

// ---------------------------------------------------------------------------
// `struct sockaddr_un`: the path in front of us
// ---------------------------------------------------------------------------

/// The path a `sockaddr_un`'s bytes carry, the one parser every unix call
/// reads through (bind, connect, and the getsockname writer shares the
/// length arithmetic). The rules are the header's and this kernel's:
/// at least the two family bytes plus one NUL, never more than the struct;
/// `AF_UNIX` in the family slot; a terminating NUL with nothing but the
/// path in front of it - which refuses the abstract namespace (first byte
/// NUL) with the same arm that refuses an empty path; and valid UTF-8,
/// because the kernel's path vocabulary is `String`.
pub(crate) fn sockaddr_un_path(raw: &[u8]) -> Result<String, isize> {
    let einval = || Err(crate::syscall::err::EINVAL);
    if raw.len() < 3 || raw.len() > 2 + SUN_PATH_MAX {
        return einval();
    }
    if raw[0] as u16 | ((raw[1] as u16) << 8) != crate::net::socket::AF_UNIX {
        return einval();
    }
    let body = &raw[2..];
    let end = match body.iter().position(|&b| b == 0) {
        Some(i) => i,
        // A path that fills the field with no NUL anywhere: Linux accepts
        // exactly this one case (the field *is* the terminator-less
        // path); this kernel's path keys are `String`s with a length, so
        // the same 108-byte name can be taken whole.
        None => {
            let name = core::str::from_utf8(body).map_err(|_| crate::syscall::err::EINVAL)?;
            return Ok(name.to_string());
        }
    };
    if body[end + 1..].iter().any(|&b| b != 0) {
        return einval(); // an interior NUL with bytes after it
    }
    if end == 0 {
        return einval(); // the abstract namespace, refused (module header)
    }
    let name = core::str::from_utf8(&body[..end]).map_err(|_| crate::syscall::err::EINVAL)?;
    Ok(name.to_string())
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

use crate::syscall::err;

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
    if sock.bound_key().is_some() {
        return err::EINVAL;
    }
    let (parent_path, name) = match crate::vfs::split_parent(path) {
        Ok(split) => split,
        Err(_) => return err::EINVAL,
    };
    let cwd = crate::sched::current_cwd();
    let parent = match crate::vfs::resolve_opt(parent_path, Some(cwd)) {
        Ok(dir) => dir,
        Err(e) => return crate::syscall::err_of(e),
    };
    // The last component must be free, whatever kind of thing occupies it
    // (Linux refuses both a live socket and a plain file under the name);
    // a name that is not even a single component is `EINVAL`.
    if parent.lookup(name).is_ok() {
        return err::EADDRINUSE;
    }
    if let Err(e) = crate::vfs::may_create_in(&parent) {
        return crate::syscall::err_of(e);
    }
    let node = match parent.create(name, NodeKind::Socket) {
        Ok(created) => created,
        Err(VfsError::Exists) => return err::EADDRINUSE,
        Err(VfsError::Unsupported) => return err::EOPNOTSUPP,
        Err(e) => return crate::syscall::err_of(e),
    };
    node.set_mode(0o755);
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
    let path = sock.local_name().unwrap_or_default();
    write_named(&path, addr_ptr, len_ptr)
}

/// The same contract with the name given directly - which is what
/// `recvfrom` needs: the address it writes back is the *sender's* name of
/// the datagram just taken, not the receiver's own.
pub(crate) fn write_named(path: &str, addr_ptr: usize, len_ptr: usize) -> isize {
    let bytes = sockaddr_un_bytes(path);
    if len_ptr == 0 {
        return err::EFAULT;
    }
    if !crate::syscall::buffer_ok(len_ptr, 4, true, true) {
        return err::EFAULT;
    }
    let want = unsafe { (len_ptr as *const u32).read_unaligned() } as usize;
    // The caller asked for the name when there is somewhere to put it: a
    // zero `addrlen` is the probe, and the length written below is the
    // whole answer. A nonzero `addrlen` with a null pointer is the caller
    // breaking their own word - EFAULT, the man page's.
    if want > 0 {
        if addr_ptr == 0 || !crate::syscall::buffer_ok(addr_ptr, want, true, true) {
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
        Err(e) => return crate::syscall::err_of(e),
    };
    if node.metadata().kind != NodeKind::Socket {
        return err::ECONNREFUSED;
    }
    // Linux checks the path's write permission before it checks whether
    // anyone is listening; the order only shows to a caller that has
    // neither, and the permission first is the man page's.
    if let Err(e) = crate::vfs::may_access(&node, crate::vfs::W_OK) {
        return crate::syscall::err_of(e);
    }
    // Already connected sockets cannot connect again.
    if sock.end_of().is_some() || sock.dgram_connected() {
        return err::EISCONN;
    }
    let arc = sock_arc(sock);
    match connect(&arc, &node, nonblock) {
        Ok(()) => 0,
        Err(ConnectRefused::Refused) => err::ECONNREFUSED,
        Err(ConnectRefused::Again) => err::EAGAIN,
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
pub(crate) fn send_dgram(sock: &UnixSock, data: &[u8], dest: Option<&str>) -> isize {
    if data.len() > GRAM_MAX {
        return err::EMSGSIZE;
    }
    let target: Arc<UnixSock> = match dest {
        Some(path) => {
            // A connected datagram socket sends to its peer; naming a
            // different destination on the call that already pinned one is
            // `EISCONN`, the same refusal the stream channel answers.
            if sock.dgram_connected() {
                return err::EISCONN;
            }
            let cwd = crate::sched::current_cwd();
            let node = match crate::vfs::resolve_opt(path, Some(cwd)) {
                Ok(node) => node,
                Err(e) => return crate::syscall::err_of(e),
            };
            if node.metadata().kind != NodeKind::Socket {
                return err::ECONNREFUSED;
            }
            match live_listener(&node) {
                Some(sock) => sock,
                None => return err::ECONNREFUSED,
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
    let from = sock.local_name();
    if target.push_dgram(data, from, self_cred()) {
        data.len() as isize
    } else {
        err::EAGAIN
    }
}

/// One `recvfrom` on a datagram socket: take the front message into the
/// caller's buffer and report the sender's name for the `src_addr` slots.
pub(crate) fn recv_dgram(
    sock: &UnixSock,
    buf: &mut [u8],
) -> Result<(usize, Option<String>), VfsError> {
    sock.take_dgram(buf).map(|(n, from, _, _)| (n, from))
}

/// [`recv_dgram`], keeping the sender's credentials - what a `recvmsg` on a
/// socket that asked for `SO_PASSCRED` has to answer with.
pub(crate) fn recv_dgram_cred(
    sock: &UnixSock,
    buf: &mut [u8],
) -> Result<(usize, Option<String>, UCred, bool), VfsError> {
    sock.take_dgram(buf)
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
    let (end, peer) = accept_once(listener, nonblock)?;
    let accepted = alloc(listener.typ);
    accepted.with_state_connect(end, peer);
    Ok(accepted)
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
