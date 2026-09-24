//! AF_UNIX path sockets: the bind table, the listener, the handshake.
//!
//! A dbus daemon owns `/run/dbus/system_bus_socket`; an X client finds the
//! server through `/tmp/.X11-unix/X0`. Neither relationship begins at a
//! network address - it begins at a *name in the filesystem*, planted by
//! `bind(2)` and answered by `connect(2)`. That is this module's whole
//! subject: the two steps between `socket(AF_UNIX, SOCK_STREAM)` and a
//! working byte channel, which `socketpair` already provides by other
//! means.
//!
//! The division of labor follows that fact. A *connected* unix stream is
//! the same anonymous byte channel a socketpair end is - a
//! [`DuplexEnd`](crate::vfs::pipe::DuplexEnd), whose read/write/poll/
//! half-close machinery is not duplicated here at all. What this module
//! owns is the state before a connection exists: the [`UnixSock`] an fd
//! names, the registry that maps a bound node's identity `(dev, ino)` to
//! the socket listening behind it, and the accept queue a `connect` feeds
//! and an `accept` drains. The registry keys on node identity for the same
//! reason the lock table and inotify watches do (`src/vfs/locks.rs`,
//! `src/vfs/inotify.rs`): a path can be renamed over or unlinked and
//! rebound, and only the inode's own number says which socket answered.
//!
//! Why none of this lives in the network registry (`src/net/mod.rs`): that
//! registry is smoltcp's, and it answers `ENETDOWN` before a machine has a
//! network card. AF_UNIX is the family a machine with no network at all
//! still runs dbus on; netlink took the same detour for the same reason
//! (`socket.rs`'s uevent socket).
//!
//! Deliberate departures from Linux, each visible from userland:
//! * `SOCK_DGRAM` and `SOCK_SEQPACKET` over AF_UNIX answer `EOPNOTSUPP`,
//!   the refusal `socketpair` already makes; STREAM serves both target
//!   daemons.
//! * the abstract namespace (`sun_path[0] == '\0'`) answers `EINVAL` - it
//!   has no filesystem node, hence no part of this design that would hold
//!   it, and no target daemon reaches for it.
//! * a bound socket that was never `listen`ed answers `ECONNREFUSED`;
//!   Linux's implicit backlog-zero rendezvous is state this kernel
//!   declines to model, and both daemons call `listen`.
//! * a socket path on ext4 cannot be bound (`EOPNOTSUPP` at the node
//!   level): the on-disk inode type is unwired, and `/run` and
//!   `/tmp/.X11-unix` - where every real socket lives - are tmpfs.
//! * `connect(2)` never reports `EINPROGRESS`: over a byte pipe the
//!   handshake is not a process, it is a push.

use alloc::collections::{BTreeMap, VecDeque};
use alloc::string::{String, ToString};
use alloc::sync::Arc;
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
fn self_cred() -> UCred {
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
    /// `SOCK_STREAM` in this kernel's v1 (the module header's refusal
    /// list); carried so a future datagram form reaches the same downcast.
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
}

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

    /// The byte doors: a connected socket answers through its channel -
    /// the same reads a socketpair end gives - and an unconnected one
    /// answers `ENOTCONN`, listening or fresh.
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> VfsResult<usize> {
        match self.end_of() {
            Some(end) => end.read_at(offset, buf),
            None => Err(VfsError::NotConnected),
        }
    }

    fn write_at(&self, offset: u64, buf: &[u8]) -> VfsResult<usize> {
        match self.end_of() {
            Some(end) => end.write_at(offset, buf),
            None => Err(VfsError::NotConnected),
        }
    }

    fn poll_ready(&self) -> Ready {
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
        });
    }
}

// ---------------------------------------------------------------------------
// The bind table: node identity in front of every listener
// ---------------------------------------------------------------------------

struct Registry {
    /// Slot `id - 1`, the socket's own number. Strong, and retired by the
    /// close hook (`release`) - the uevent socket's pattern, not timerfd's
    /// weak list: a socket must outlive every transient borrow the syscall
    /// handlers take through its descriptor, and it dies exactly when its
    /// last fd does. A slot freed to `None` makes later lookups answer
    /// `ECONNREFUSED`, Linux's dead-socket-file verdict.
    slots: Vec<Option<Slot>>,
    /// Which live socket answers each bound node. `unlink` removes the key
    /// by node identity (`unbind_node`); a connect resolves a path to a
    /// node and asks this map, which is how symlinks, `..` and mounts all
    /// come for free.
    by_node: BTreeMap<(u64, u64), usize>,
}

struct Slot {
    socket: Arc<UnixSock>,
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
            socket: sock.clone(),
            key: None,
        }));
        sock
    })
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

/// The close hook, beside `close_last_socket`: the socket's last
/// descriptor went. Its pending connections drop (the `PipeEnd` drop path
/// EOFs whoever already held the other half), its key retires, and anyone
/// parked on its accept queue wakes to find the slot dead and leave.
pub(crate) fn release(id: usize) {
    let (dead, waiters) = critical_section::with(|cs| {
        let mut reg = UNIX.borrow(cs).borrow_mut();
        // `take` the slot whole: the registry's strong reference ends here,
        // and the caller's descriptor Arc is what lets the socket live
        // exactly as long as the close path still means it.
        let Some(slot) = reg.slots.get_mut(id - 1).and_then(Option::take) else {
            return (None, Vec::new());
        };
        let dead = slot.socket;
        if let Some(key) = slot.key {
            reg.by_node.remove(&key);
        }
        let waiters = {
            let mut st = dead.state.borrow(cs).borrow_mut();
            let mut w = st.accept_waiters.clone();
            w.extend(st.room_waiters.iter().copied());
            st.pending.clear();
            w
        }; // the borrow of `dead` ends with the block; the Arc moves freely
        (Some(dead), waiters)
    });
    // The registry's released reference goes here, at the end: the caller's
    // fd Arc (or the exit batch's) is what let the socket live until this
    // moment, and the close path itself is what finishes the job.
    drop(dead);
    wake_all(waiters);
}

/// Look up a connect's resolved node in the table, returning the live
/// socket behind it. `None` answers `ECONNREFUSED` at the call site: no
/// entry, an entry behind a closed socket, or a stale key all mean the
/// same thing to a connector.
fn live_listener(node: &Arc<dyn Inode>) -> Option<Arc<UnixSock>> {
    let key = (crate::vfs::mount::fs_id_of_inode(node), node.metadata().ino);
    critical_section::with(|cs| {
        let reg = UNIX.borrow(cs).borrow();
        let id = *reg.by_node.get(&key)?;
        reg.slots
            .get(id - 1)?
            .as_ref()
            .map(|slot| slot.socket.clone())
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
    loop {
        let outcome = critical_section::with(|cs| {
            let lst = {
                let mut stl = listener.state.borrow(cs).borrow_mut();
                let backlog = match stl.backlog {
                    Some(b) => b,
                    None => return ConnectOutcome::Refused,
                };
                if stl.pending.len() >= backlog {
                    return ConnectOutcome::Full;
                }
                let (client, server) = DuplexEnd::pair();
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
            };
            lst
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
                Some(b) => st.pending.len() < b,
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
    let cap = clamp_backlog(backlog).ok_or(crate::syscall::err::EINVAL)?;
    critical_section::with(|cs| {
        let mut st = sock.state.borrow(cs).borrow_mut();
        st.backlog = Some(cap);
    });
    Ok(())
}

/// The backlog rule, pure: negative is `EINVAL`, over `SOMAXCONN` is
/// clamped (Linux since 2.4.22), zero means the smallest queue there is -
/// and in this kernel a zero backlog never admits a connection, which is
/// Linux's listen-backlog-zero behavior as closely as a push queue gets.
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

/// The bind record wants an `Arc` for the slot's weak self-reference; the
/// handler holds only a borrow. The slot registry is what upgrades it -
/// the fd the caller holds keeps that slot alive, so the borrow cannot
/// outrun the arc.
fn sock_arc(sock: &UnixSock) -> Arc<UnixSock> {
    critical_section::with(|cs| {
        let reg = UNIX.borrow(cs).borrow();
        reg.slots
            .get(sock.id - 1)
            .and_then(|slot| slot.as_ref())
            .map(|slot| slot.socket.clone())
            .expect("a unix fd outlives its own slot while borrowed")
    })
}

/// Write the bound name back as a `sockaddr_un`, honoring the caller
/// in-out `addrlen` the Linux way: copy what fits, report the used length
/// through, and answer the probe form (`addrlen` pointing at zero, asking
/// for the needed size) by storing the length without touching the
/// address.
pub(crate) fn write_name(sock: &UnixSock, addr_ptr: usize, len_ptr: usize) -> isize {
    let path = sock.local_name().unwrap_or_default();
    let bytes = sockaddr_un_bytes(&path);
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
    if sock.end_of().is_some() {
        return err::EISCONN;
    }
    let arc = sock_arc(sock);
    match connect(&arc, &node, nonblock) {
        Ok(()) => 0,
        Err(ConnectRefused::Refused) => err::ECONNREFUSED,
        Err(ConnectRefused::Again) => err::EAGAIN,
    }
}

/// The accept act, handler side: pop (or park), wrap the server half in a
/// fresh unix socket, and hand the caller a descriptor that is already
/// connected - the shape an accepted socket has on every system.
pub(crate) fn accept_face(listener: &UnixSock, nonblock: bool) -> Result<usize, isize> {
    let (end, peer) = accept_once(listener, nonblock)?;
    let accepted = alloc(listener.typ);
    accepted.with_state_connect(end, peer);
    Ok(accepted.id)
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
            .map(|slot| slot.socket.clone())
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
