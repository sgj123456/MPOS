# §4 Phase 4: Networking Enhancements (Medium Priority)

Part of the split `docs/notes/` evidence library — routing for every `§N.M` pointer is in
[index.md](index.md). Section numbers are unchanged from the original `NOTES.md`.

## Phase 4: Networking Enhancements (Medium Priority)

### 4.1 AF_UNIX Domain Sockets — **ANSWERED** (SOCK_STREAM)

The path family is real: `src/net/unix.rs` carries `socket(AF_UNIX,
SOCK_STREAM)` through `bind` (which creates a genuine `S_IFSOCK` node in
ramfs, visible to `stat`/`ls -l`/`find`, and `open` on which answers
`ENXIO`), `listen` with a clamped backlog, `connect` by resolved path
(symlinks and mounts ride the normal resolution), `accept`/`accept4`,
`getsockname` echoing the bound path, `shutdown`, and the full data path -
a connected unix socket *is* a `pipe::DuplexEnd` half, so `read`/`write`/
`sendmsg`/`recvmsg`/`poll`/`epoll` all speak to it unchanged. The bind
table is keyed by `(fs_id, ino)` like locks and inotify watches, so
`unlink` of the name retires the binding (hooked in `unlinkat`) while the
listener itself survives - exactly how a daemon outlives its own stale
socket file. Permissions: bind needs write+search on the parent (the
mkdirat rule), connect needs `W_OK` on the node.

Deliberate divergences from Linux (each asserted in `t-unix`/
`t-unix-rustix`): a connect to a
bound-but-never-listened name is `ECONNREFUSED` rather than Linux's
implicit rendezvous at a socket nobody listened on; ext4 refuses socket
nodes (`EOPNOTSUPP` - `/run` is ramfs); `mknod(S_IFSOCK)` still refuses, so
only `bind` creates a socket name; `getpeername` answers `AF_UNIX` with an
empty path (accepted peers carry no name). `SOCK_DGRAM` unix sockets are
served whole-message (`t-unix-dgram`), and so is a `SOCK_SEQPACKET` *pair*;
a *named* `SOCK_SEQPACKET` connection kept no message boundary until item 78,
which now serves it as the same message pair a `socketpair` half is
(`t-unix-seqpacket`): a send is one message, a short read discards the rest of
that message, `MSG_PEEK`/`MSG_TRUNC` are the message family's answers, and
`SCM_RIGHTS` rides the message's own record.

**`SCM_RIGHTS` / `SCM_CREDENTIALS` / `SO_PEERCRED` are now answered** (see
`t-unix-scm`): a unix (or socketpair) `sendmsg` can hand live descriptors
across, `recvmsg` installs them into the reader's table (respecting
`RLIMIT_NOFILE`, and `MSG_CMSG_CLOEXEC`), `SO_PASSCRED` makes every read
carry an `SCM_CREDENTIALS`, and `getsockopt(SO_PEERCRED)` returns the peer's
pid/uid/gid - the identity a dbus broker authenticates on. The handover rides
the connection's `PipeState` beside the bytes it travels with, so a descriptor
never taken is closed when the queue drops, and the peer's credentials are
captured at `connect`/`accept`/`socketpair`. v1 divergences: on `SOCK_STREAM`
the kernel keeps one cmsg boundary per send rather than per skb, a sender's
`SCM_CREDENTIALS` opt-in is accepted but the kernel always fills the peer's
real ids (no impersonation), and `SO_PEERCRED`/`SO_PASSCRED` are the unix
`connect` path's - a `socketpair` carries descriptors but not credentials.

A *data-less* `sendmsg` used to be the one deadlock left in the family: the
record rode a byte boundary and there was no byte, so the read parked on a
descriptor it already had. `PipeEnd::scm_due` now lets the read see that the
front record is due before it decides to wait, and the read returns the record
with zero bytes rather than waiting for the stream to advance - which is the
one case where a `recvmsg` on a stream can answer 0 and not mean end-of-stream,
so a receiver that takes a short read has to look at its control buffer before
concluding the peer is gone. That is the price of the record, and the
alternative was a hang; `t-unix-scm` pins both the delivery and the
non-blocking second read that follows it. **Measured 2026-10-20, that
deadlock-avoidance is this kernel's and not Linux's for a *stream*:** a
zero-byte `sendmsg` that carried a descriptor answers `EAGAIN` on `recv`,
`recvfrom` **and** `recvmsg` on the host - every door - until a byte arrives
(`.workbuddy/probe_unix_peek6.c`, `probe_unix_peek7.c`), while the caller the
rule was written for - eudev's worker channel - is a **datagram** pair, whose
zero-byte message is a message a reader does reach. The `empty_handover` path
stays (a deadlock is worse), and §4.1.2 records the divergence rather than
claiming Linux's shape.

**A message may be zero bytes long, and receiving one may offer no room.** The
rule sounds like a corner and is not: eudev's worker channel is a
`socketpair(AF_LOCAL, SOCK_DGRAM|SOCK_CLOEXEC, 0, ...)` whose whole message is
zero bytes - `struct worker_message` has no members, so the worker's
`loop_write` is a zero-length `write(2)`, and the credentials that arrive with
it *are* the payload the manager looks its worker up by - and the manager's
`recvmsg` therefore offers an iovec whose length is zero. That shape answered
`EINVAL`, which left udevd's loop stuck on the failing read and every device on
the machine untagged (the board's `t-udev-worker` entry carries the whole
story). What the family now does is Linux's, measured one call at a time before
it was written: a `recv`/`recvfrom`/`recvmsg` with a zero-byte data buffer
**consumes** the front message and answers 0, still delivering
`SCM_CREDENTIALS` when the reader asked and there is room (`MSG_CTRUNC` when
there is not), and `MSG_TRUNC` when the message was longer than the buffer -
the message's own length is its boundary, not the caller's buffer. `read(2)` is
the exception, and it is a *file-level* one rather than a socket rule:
`vfs_read` short-circuits a zero-length count, so `read(fd, buf, 0)` touches
nothing at all while `recv(fd, buf, 0)` takes a message. `netlink` follows the
same rule (a `NETLINK_USERSOCK` socket that sent itself a message was the
measurement).

**And a `listen` backlog is one deeper than the number it names.** Linux's
admission test is `queue_len > backlog`, so `listen(fd, 0)` is a queue of one -
which this kernel read as "never admits" until eudev's control socket turned up
created with exactly `listen(uctrl->sock, 0)` (`udev-ctrl.c`), leaving `udevadm`
unable to reach the daemon at all. Measured on the host: backlogs 0..=3
admitted 1, 2, 3 and 4 connections before the next `connect` answered `EAGAIN`.
`t-unix` pins the depth and the room an `accept` returns; the divergence that
remains is the *unlistened* name, which is still `ECONNREFUSED`.

**And a *negative* backlog is `SOMAXCONN`, not a refusal.** `clamp_backlog` used
to answer `EINVAL` for one, which reads as the man page's rule and is not
Linux's: `__sys_listen` compares the argument to `SOMAXCONN` as an *unsigned*
int, so every negative value lands on the ceiling rather than being refused.
Measured on the host before the rule was written: `listen(fd, -1)` returns 0 for
`AF_UNIX` and `AF_INET` alike, and `ss -lx`/`ss -ltn` then report the backlog as
4096 - `net.core.somaxconn`, not the zero a "negative means a closed queue"
reading would leave. Nor is it a number anybody has to go looking for: `-1` is
what Rust's std passes on Linux from `UnixListener::bind` *and*
`TcpListener::bind`, so every Rust server and compositor asks for it on every
bind - and the `EINVAL` it got here is where niri stopped (§7.4). The rule lives
in one place, `net::unix::clamp_backlog`, which the IPv4 pool reads as well: that
arm had read `-1` as **zero**, a queue one deep, which is the same defect in the
other family's clothes. Pinned from both sides, `t-unix` and `t-tcp-listen`.

#### 4.1.1 The abstract namespace — **SERVED** (2026-10-13)

It used to answer `EINVAL`, and the reason was written down in the module
header: it "has no filesystem node, hence no part of this design that would hold
it, and **no target daemon reaches for it**". The last clause is what retired the
refusal: `xwayland-satellite` - which the image now carries, because `niri` is
installed - binds `@/tmp/.X11-unix/X0` and, refused, logs `error opening X11
sockets, disabling xwayland-satellite integration`. A refusal whose stated
reason has been measured false is a wrong shape, and the wrong shape is the work
(`docs/uapi/` §5's list).

**The rules were measured before they were written**
(`.workbuddy/probe_unix_abstract.c`, run on the host; three of the numbers are
ones a guess gets wrong):

| call | Linux |
|---|---|
| `bind` an explicit abstract name | 0 |
| `bind` it again | `EADDRINUSE` |
| — but from a **datagram** socket on a *stream* socket's name | 0 — the key is the name **and the socket type** |
| `getsockname` | family, the leading NUL, the name; `addrlen` = `3 + name.len()` |
| `listen`/`connect`/`accept` | all served |
| `connect` to a name nobody bound (stream *and* datagram) | `ECONNREFUSED` (111, not `ENOENT`) |
| a datagram `sendto` to a name nobody bound | `ECONNREFUSED` (111) |
| `addrlen` of just the family (`bind`) | 0 — **auto-binds**: `getsockname` answers a five-hex-character abstract name |
| the same `addrlen` for `connect`/`sendto` | `EINVAL` (Linux's `unix_mkname` refuses `len <= offsetof(sun_path)`) |
| a name with an interior NUL | 0 — the abstract name is length-delimited, not NUL-terminated |
| a name that is only the NUL (`addrlen == 3`) | 0 — a name of zero bytes is a name |

**What landed**: `UnixAddr` in `src/net/unix.rs` is the two namespaces as one
type - a path resolves to the node the table keys on, an abstract name is bytes
and nothing else - and the registry carries a second map, `by_name`, keyed on
`(name, socket_type)` because Linux is. The lifetime is the descriptor's:
nothing can `unlink` an abstract name, so the last close is what releases it
(the same `Drop` that retires a path's table key). One handshake serves both,
which is why `connect` was split into "find the listener" (by node, by name) and
`connect_to`. `bind`'s `addrlen == 2` generates a name, in Linux's *shape* rather
than its value; `sendto`/`sendmsg` carry an address rather than a path, so the
sender's abstract name reaches the receiver's `recvfrom`; and the reader's lower
bound moved from `3` to `2`, which is the gate that made the autobind case
unreachable at first (it was written for the old parser's "family + one NUL").

**One narrow rule stays named**: a *path* is still a `String`, so the path half
refuses non-UTF-8 (the abstract half does not - its name is bytes). The other
rule this section used to carry - "a path with bytes *after* its terminator is
`EINVAL` here where Linux ignores them" - is **gone** (§4.1.5): the `EINVAL`
was a refusal of a name Linux accepts, and the bytes are the caller's storage.

**Pinned by `t-unix`**, whose old claim said the opposite: the program asserted
"the abstract namespace answers `EINVAL`" (with a host branch naming Linux's
service), and now makes thirteen claims - each measured on the host first, and
all thirteen green on the machine. The counterfactual is the old kernel's answer
and the old claim: the program cannot pass against a kernel that refuses the
namespace.

**What it bought, measured**: `niri` on the same image now logs
`INFO niri: listening on X11 socket: :0` and no abstract-socket warning at all,
where before it logged `WARN niri::utils::xwayland::satellite: error opening X11
sockets, disabling xwayland-satellite integration: error binding to abstract
socket … Invalid argument`. The renderer warnings beside it are a different gap
(§7.4's Mesa chain) and are untouched.

#### 4.1.2 `MSG_PEEK` and `MSG_TRUNC` on the receive doors — **SERVED** (2026-10-20)

Both flags used to answer `EOPNOTSUPP` on every unix receive door, and the module's own comment
carried the reason: a pipe could not look at its front without taking it. The flags are what a
supervisor composes with - peek to see whether a whole message has arrived, `MSG_TRUNC` to learn
a datagram's length without room for it - and the reasons turned out to be two different
contracts, measured whole on a current host first (`.workbuddy/probe_unix_peek{,2,3,4,5,6,7}.c`,
Linux 7.2.x, `.workbuddy/mpos` working tree):

* **a stream peek copies and takes nothing.** Four bytes of ten answer 4, fill the caller's
  buffer with `abcd`, and the next read still returns all ten; the look is repeatable, a
  zero-length look answers 0 at once, and a look at end-of-stream answers 0.
* **a stream's `MSG_TRUNC` is not a datagram's.** A four-byte read with the flag answers 4,
  copies the four bytes and consumes them - byte for byte the read without it. (The TCP receive
  path is the count-only rule, which is why the two families cannot share an arm.)
* **a datagram peek is a full copy of the front**, and under `MSG_TRUNC` the return is the
  message's own length while the copy stays short; `recvmsg` reports the cut in `msg_flags`
  whether or not the flag was asked for. A zero-length peek leaves the message, a zero-length
  **non-peek** receive consumes it, and a peek names the sender like any other receive.
* **`SCM_RIGHTS` survives a peek.** Linux installs the descriptors a peeked send carried and
  leaves the record queued, so the read that consumes its bytes installs a second set and both
  stay live. A zero-byte handover is the exception: `recv`/`recvfrom` answer `EAGAIN` until a
  byte arrives (`probe_unix_peek6.c`), while the kernel's existing `empty_handover` path makes
  `recvmsg` receive it - a deliberate trade against the deadlock that shape once caused, and
  now a named divergence (`docs/uapi/` §5.2).

The landing keeps the look at the bottom of the family rather than in the syscall layer:
`PipeEnd::peek_once` copies the ring's front without moving the cursor, `PipeEnd::peek_scm`
clones the front handover (so a peek's installed descriptors share the queued files'
descriptions, Linux's `get_file` shape), and `DuplexEnd` forwards both. `UnixSock::peek_dgram`
clones the front `Dgram` and leaves the queue alone; `take_dgram`/`peek_dgram` answer a
`RecvDgram` that carries the message's whole length, which is what `MSG_TRUNC` reports. The
receive arms route the two flags and nothing else: the stream arms deliberately ignore
`MSG_TRUNC`, because on this family it changes nothing.

**Pin**: `t-unix-peek`, 63 claims, green on a current host first and all 63 green on both
machines - stream, datagram and seqpacket, both `MSG_TRUNC` shapes on each, `recvmsg`'s
`msg_flags`, an abstract sender name, and the handover a peek installs twice. The counterfactual
(the four peek refusals restored) is x86_64 `pass=191 fail=1`, only `t-unix-peek` red, 18 of its
63 claims by name. Gate: x86_64 `pass=192 fail=0`, `mpos-kunit 32 passed, 0 failed`, exit 33;
aarch64 `pass=191 fail=0`, `mpos-kunit 32 passed, 0 failed`, reaching `=== END`. Host suite 584
(two new `vfs::pipe` tests), `xtask` 123; fmt/clippy clean on the touched files and the depgraph
ratchet unchanged. Named rather than served: `NETLINK`/`AF_PACKET`/vsock peeks, the vsock
`recvmsg`'s first-segment chain, and the zero-byte stream handover above.

#### 4.1.3 A named `SOCK_SEQPACKET` connection keeps its messages — **SERVED** (2026-10-21)

The one divergence left in the family until item 78: a listener at a path (or an abstract name)
handed `accept` a `DuplexEnd` byte channel, so the connection the family calls "message
boundaries over a stream" carried a byte stream. A `socketpair(AF_UNIX, SOCK_SEQPACKET, ...)`
half was already a message queue; a *named* one was not, and the two shapes disagreed about the
same type name.

Measured whole on a current host first (`.workbuddy/probe_unix_seqpacket{,2,3}.c`, Linux 7.2.11,
over abstract and path names):

| The call | Linux 7.2.11 |
|---|---|
| `write` 3 then 5, `read` 64 twice | 3 then 5 - one send is one message |
| `sendmsg` iovec 4+4+2, `read` 32 | 10, one message |
| ten-byte message through a four-byte `read` | 4, the rest of that message discarded; next read `EAGAIN` |
| `recvmsg(MSG_TRUNC, iov 4)` | 10, `msg_flags` `MSG_TRUNC`, consumed |
| `recvmsg(MSG_PEEK, iov 16)` | 10, message left; the read after is the same ten |
| zero-length send, `read` 8 | 0 - a zero-length message, consumed by that read |
| `read(fd, buf, 0)` | 0, the front message left where it was |
| `recvfrom`/`recvmsg` with a zero-length room | 0, `MSG_TRUNC` for `recvmsg`, message **consumed** |
| `sendmsg` with `SCM_RIGHTS` | the receive installs the descriptor; a peek installs its own set and the take installs a second, distinct one |
| zero-length message with `SCM_RIGHTS` | 0 and the descriptor is installed, message consumed |
| stream dialing a seqpacket path name (and the reverse) | `EPROTOTYPE` (91); an abstract name misses its type-keyed lookup and answers `ECONNREFUSED` |

The landing keeps the existing message machinery rather than adding a second one: the listener's
queue now holds a `Pending` that is either a `DuplexEnd` server half (stream) or a server
`UnixSock` already paired with the connector's (seqpacket), `connect_messages` mints and wires
that pair outside the listener's critical section (dropping an unqueued socket under the lock
would re-enter the registry's), and `accept` hands the queued socket straight out. Both ends then
carry messages through the same `recvq`/`push_dgram` a `socketpair` half uses. The type refusal
is `EPROTOTYPE` for a path name, which needed a new `ConnectRefused` arm and errno constant.

Three details the measurement forced, each in the doors rather than in the family: `read(2)`
with a zero-length buffer is a no-op (it used to take a message and answer 0, which consumed a
zero-length message and, on a nonblocking empty queue, answered `EAGAIN` where Linux answers 0);
`Dgram` now carries the message's `SCM_RIGHTS` descriptors, so `sendmsg` on a message socket
collects them and `recvmsg` installs them (a peek clones the `Arc` list, so the record stays
complete) - without that, moving the connection off `DuplexEnd` would have silently dropped the
descriptor handover udev's own control socket can use; and `recvfrom` drops them, because it has
no control buffer to install them into.

**Pin**: `t-unix-seqpacket`, 58 claims, green on a current host first. `t-unix-dgram`'s
"a named SOCK_SEQPACKET socket is served as a stream" claim became "…listens", which is what it
was ever measuring. Gate: x86_64 `pass=193 fail=0`; aarch64 `pass=192 fail=0`; `mpos-kunit` 32/0
on both. Named at landing and served by item 79: `sendto` with a destination on an
already-connected seqpacket socket answered `EISCONN` where Linux sends it to the peer.

#### 4.1.4 A send's destination is the socket type's rule — **SERVED** (2026-10-21)

Item 78's named residual was two rules wearing one sentence. Both message families answered
`EISCONN` when a connected socket's send named a destination; Linux decides by the type, and a
connected *datagram* socket does the opposite of a connected seqpacket one. Measured on a
current host first (`.workbuddy/probe_unix_sendto_connected{,2}.c`,
`probe_unix_sendto_family_only.c`, `probe_unix_sendto_long_name.c`,
`probe_unix_sendto_wrongtype.c`, `probe_unix_sendto_huge_name.c`, Linux 7.2.11):

| The send | Linux 7.2.11 |
|---|---|
| seqpacket, connected, the listener's own name | sent to the peer, boundary kept |
| seqpacket, connected, a different live socket's name | sent to the peer; that socket gets nothing |
| seqpacket, connected, a dead path / dead abstract name | sent to the peer - the name is not resolved |
| seqpacket, connected, a stream listener's name | sent to the peer - the name is not validated |
| seqpacket, connected, a family-only `sockaddr_un` | sent to the peer (`EINVAL` for a datagram sender) |
| seqpacket, connected, `addrlen` 110 or 128 | sent to the peer; > 128 is `EINVAL`, a bad pointer `EFAULT` |
| seqpacket, unconnected or listening, any name | `ENOTCONN` (107) - served by item 81; the name is not looked at, and a NULL name pointer with a length is still "no name" |
| seqpacket, unconnected or listening, `sendto` `addrlen` 129 / a bad pointer | `EINVAL` (22) / `EFAULT` (14) - the syscall layer's copy still runs |
| seqpacket, unconnected or listening, `sendmsg` any name | `ENOTCONN`; 129 bytes is not `EINVAL`, a bad iovec base is not `EFAULT` |
| dgram, connected, a different live socket's name | that socket receives; the pinned peer does not |
| dgram, connected, a dead path | `ENOENT` (2) |
| dgram, connected, a dead abstract name / a regular file | `ECONNREFUSED` (111) |
| dgram, connected, a live stream listener's name | `EPROTOTYPE` (91) - served by item 80 (a *path*; abstract names of another type stay `ECONNREFUSED`) |
| dgram, connected, a family-only `sockaddr_un` | `EINVAL` (22) |
| stream pair, any name | `EISCONN` (106), unchanged |

Served by `UnixSock::ignores_send_address` (connected sequenced-packet only), a
`copy_send_address` that performs the syscall layer's bounds and readability check without
interpreting the bytes, and `send_dgram` resolving a datagram's name whether or not it is
connected. **Pins**: `t-unix-seqpacket` 58 -> 66 claims (the three ignored destinations, each
read back whole, the `EINVAL` a 129-byte address still earns, plus the empty queue);
`t-unix-dgram` gains six and loses its `EISCONN` divergence claim. Gate: x86_64 `pass=193
fail=0`; aarch64 `pass=192 fail=0`; `mpos-kunit` 32/0 on both. Counterfactual (the old rule
restored): `t-unix-dgram`'s first named-send claim is red with `EISCONN` (106), and the following
blocking read parks for a message that was never sent, so the arm was stopped rather than counted.
Served since: item 80 made a *path* naming another socket type `EPROTOTYPE` while an abstract
name of another type stays `ECONNREFUSED`, and item 81 made the no-peer seqpacket rows
`ENOTCONN`, with `sendto`'s 129-byte `EINVAL` and `sendmsg`'s import-only validation pinned
separately (`t-unix-dgram` 49 -> 57, `t-unix-seqpacket` 69 -> 88 host-run claims).

#### 4.1.5 A path's bytes after its terminator are the caller's storage — **SERVED** (2026-10-22)

§4.1.1's last named divergence, and the one shape a socket library gets wrong by
refusing rather than by omitting. Linux's `unix_mkname` is `strnlen(sun_path, len)`
over the caller's `addrlen`: a path **ends** at the first NUL, and whatever the
caller left behind it is storage, not part of the name. This kernel answered
`EINVAL` - an error for a name Linux serves, which is the wrong shape a caller
cannot fall back from (AGENTS.md §3).

**Measured whole on a current host first** (`.workbuddy/probe_unix_path_tail.c`,
Linux 7.2.11), one fresh name per scenario:

| The address | Linux 7.2.11 |
|---|---|
| `"abc"` + NUL + `"XYZ"`, `addrlen` covers all 7 | `bind` 0; `getsockname` answers `family + "abc" + NUL`, `addrlen` 6 |
| `"abc2"` + NUL + a zero tail, `addrlen` 12 | `bind` 0; `getsockname` `family + "abc2" + NUL`, `addrlen` 7 |
| `"abc3"`, no NUL, `addrlen` exactly its 4 bytes | `bind` 0; `getsockname` `family + "abc3" + NUL`, `addrlen` 7 |
| a 107-byte name with no NUL, `addrlen` 2+107 | `bind` 0; `addrlen` answered 110 |
| the whole 108-byte field non-NUL, `addrlen` 2+108 | `bind` 0; `addrlen` answered **111** - one past the field |
| `addrlen` 2+109 (past the struct) | `EINVAL` |
| `connect` to `"abc"` after `"abc\0XYZ"` was bound | `ECONNREFUSED` - the *name* is the prefix, and nothing listened |
| a datagram `sendto` naming `"abc\0tail"` | `ECONNREFUSED`, the same key |
| abstract `"\0abc\0XYZ"`, `addrlen` 8 | 0, and `getsockname` echoes **all eight** name bytes - the abstract half is length-delimited, so its NULs *are* the name |

**The rule that fell out is one deletion**: a path is `sun_path` up to its first
NUL (or the whole field when there is none), and nothing after it is looked at.
The interior-NUL check in `sockaddr_un_addr` was the only thing standing between
the parser and that rule; `UnixAddr::Path` already stores the prefix,
`sockaddr_un_bytes` already writes the prefix plus its own terminator, and
`bind_path` already keys the table on the resolved *node* - so the node, the
`EADDRINUSE` arbitration, the `getsockname` echo and the connect all followed
without a second change. What the deletion did *not* move is the parser's own
bound: an `addrlen` past the struct is still `EINVAL`, and it is pinned as its
own claim.

**Pinned by `t-unix`**, 74 → 80 claims: the tail bind, its short echo, the node
being the prefix's (and *not* `…sockXYZ`), a second bind of the bare prefix
answering `EADDRINUSE`, the listener accepting a connection spelled with the same
trailing bytes, and the 108-byte field's whole-field echo with its 111-byte
`addrlen`. The last one is what a host run is for: a first draft claimed the
reported terminator was *written*, and Linux writes only `min(len, ulen)` bytes
while still reporting 111 - so the echoed bytes are the field and only the count
is one longer. Every claim green on a current host first (`MPOS_ON_HOST=1`, 80 of
80), and the same 80 `ok:` lines on both machines.

**What stays named** is the one thing this item did not touch: the path half is
still a `String`, so a path that is not UTF-8 is refused here where Linux takes
any bytes. That is a type change (`UnixAddr::Path(Vec<u8>)` and every path door
under it), not a rule, and §4.1.1 names it beside the parser.

### 4.2 ICMP Improvements — **DONE**

`socket(AF_INET, SOCK_RAW, IPPROTO_ICMP)` is served by a raw smoltcp socket
(`alloc_socket` → `add_raw` + `ip_to_smoltcp_protocol`), giving `ping` its echo
request/reply path; the `t-udp` resolver test exercises the raw/UDP read path.
The socket now speaks Linux's exact shape both ways: `sendto` takes the
protocol payload and the kernel builds the IPv4 header (no `IP_HDRINCL`
needed), and receives deliver the **complete IP packet** - the earlier
header-stripped delivery is what made busybox's `ping` print a ttl wandering
0..255 (it reads `iphdr->ttl` at offset 8, which fell inside the echo data).
`IP_RECVTTL` is honoured too (setsockopt stored, the answer attached as a
24-byte cmsg, and `msg_controllen` zeroed when a message carries no control
data). `t-rawicmp` pins the delivered bytes; `t-dns`/`t-stdresolve` pin the
resolver's UDP path (12/12 raw queries, 20/20 musl `getaddrinfo`).

Current: auto echo reply only. No user-facing ICMP socket.

**Changes**:
- `socket(AF_INET, SOCK_RAW, IPPROTO_ICMP)` creates raw ICMP socket
- `sendto`/`recvfrom` on raw ICMP socket
- Enable `ping` command to work

### 4.3 TCP: the loopback, the listener, and the options — **PARTIAL**

Three things live in this section, and the **loopback** (4.3.1) and
**`listen`/`accept`** (4.3.2) are done. The **socket options** (4.3.3) are the
part still open, and of the six, five now steer the stack - `TCP_NODELAY`,
`SO_KEEPALIVE`, `SO_REUSEADDR`'s bind arbitration, `SO_LINGER`'s reset and
`SO_SNDBUF`'s datagram queue - while one is stored and read back without changing
a packet (`SO_RCVBUF`).

#### 4.3.1 The loopback is a real wire — **DONE**

`lo` used to be a name the link dump filled in: `rtnl` reported `127.0.0.1/8`
because a reader expects it there, and a segment sent to that address left by the
NIC and was never seen again. It is now an interface over **smoltcp's own
`Loopback` device** (gated on `alloc`, which this crate already enables)
carrying `127.0.0.0/8`, with no routes and its own up flag - so the device this
section kept promising as "fifty lines" turned out to be zero lines, and what was
left was the shape.

The shape is that **smoltcp cannot route it with one socket set.** Its route
lookup is *destination-only* - `Interface::route` answers `in_same_network(dst)`
or the best matching route (`iface/interface/mod.rs:1010,1014`) - and
`Interface::poll` walks every socket in the set it is handed, so a shared set
would have eth0 claim a `127.0.0.1` socket and put the segment on the wire,
whether or not `127.0.0.0/8` was added to eth0. Two interfaces therefore mean two
socket sets, and smoltcp has no operation that moves a socket between them.

So `Stack` is two `Fabric`s (interface + device + set + its own up flag, with
`sockets_mut(home)`/`poll_home(home)` resolving a handle), a socket's channel
carries a `home`, and the home is decided by the first call that names an
address - `bind`, `connect`, `sendto` - through one rule: `127.0.0.0/8` is the
loopback's, everything else (the wildcard included) is eth0's. A socket created
before then is *relocated* by swapping it for a fresh one in the right set, which
is only ever done to a socket nothing has used yet (`Closed` for TCP, `!bound`
for UDP) and always before the call that would consume it.

Two boundaries, recorded rather than hidden. A **raw socket never moves**: its
destination is a per-call argument, so one socket cannot serve two sets - which
is what keeps `ping 127.0.0.1` behaving exactly as it did, travelling the virtual
wire where slirp answers it, as `t-netlat` measures. And a **wildcard listener**
lives on eth0's fabric, so a loopback client does not reach it; the bound
`127.0.0.1` case is the supported one.

`t-udp-loopback` is the instrument: two datagram sockets in one process
exchanging a payload over `127.0.0.1`, an unbound sender relocated by its
destination (with the sender named back *on the loopback*, which is the assertion
a stack that forgot the move fails), and a datagram to a port nobody holds
answered by nothing at all.

#### 4.3.2 `listen` and `accept` — **DONE**

smoltcp has no listening socket: a `tcp::Socket` is *one* connection, and one
that completes a handshake stops listening. Its own `listen` doc points at the
answer - a pool, re-listed as each connection completes - so a listener here is a
**pool** of sockets in `Listen` (at most `LISTEN_POOL_MAX = 4`, because each is
80 KiB of buffers) plus the **queue** of the ones that completed, kept in
`Stack::listeners` and serviced after every poll: a socket that left `Listen` for
`Established` or `CloseWait` becomes a queued connection, one whose handshake
died is dropped and replaced, and the pool is refilled from what the queue leaves
room for. A full queue refills nothing, so the next `SYN` finds no listener and
is dropped - Linux's own answer to a full backlog - rather than being answered
and then abruptly refused.

The contract is Linux's, and each rule was measured on this host's kernel before
it was written:

| Call | Answer |
|---|---|
| `listen` on an unbound socket | succeeds: it implicitly binds `0.0.0.0` and an ephemeral port, and `getsockname` reports `0.0.0.0:port` |
| `listen` on a bound socket | succeeds; `getsockname` reports the bound name |
| `listen` again | succeeds, re-setting the backlog |
| `listen` on a connected socket | `EINVAL` |
| `bind` on a listening socket | `EINVAL` |
| `accept` with nothing queued, blocking | waits, woken by the poll that fills the queue |
| `accept` with nothing queued, non-blocking | `EAGAIN` |
| `accept` on a bound-but-not-listening socket | `EINVAL` |
| `recv` on a listener | `ENOTCONN` |
| `send` on a listener | `EPIPE` (recorded, not pinned: Linux also raises `SIGPIPE` here, and this kernel raises no signal for a broken channel at all) |
| `getpeername` on a listener | `ENOTCONN` |
| `shutdown` on a listener | succeeds for either half, and the queue is left alone |
| accepted `getsockname`/`getpeername` | the connection's own and its peer's names |
| `connect` where nothing listens | `ECONNREFUSED` - smoltcp resets an unclaimed segment, so the loopback answers rather than timing out |
| backlog admission | `queue_len > backlog`, one deeper than the number it names - the rule §4.1 measured on the unix family |

`accept4`'s `SOCK_CLOEXEC`/`SOCK_NONBLOCK` are honoured for this family too; they
had been accepted and dropped while the refusal was the whole implementation.
`/proc/net/tcp` reports a listener as one `LISTEN` row on its bound address (the
pool behind it is not a connection and has no row), and `poll`/`select`/`epoll`
report a listener readable exactly when the queue is non-empty - the same
question `accept` parks on, so a waiter that is woken cannot find the queue empty.

Closing a listener **resets** the connections still in its queue, which is
Linux's answer - the client's next read is `ECONNRESET` and its next write
`EPIPE` (measured, `.workbuddy/probe-linger.c`) - and this used to be a recorded
divergence because the note that once justified it was wrong (see 4.3.4): the
connections are `abort()`ed, and smoltcp's poll turns that into the `RST`. A
connection the listener *did* hand out is untouched, exactly as on Linux.

`t-tcp-listen` pins the whole shape: a server bound to `127.0.0.1:0`, a client
that connects over the loopback, the accept with both names read back, bytes both
ways, the client's *half*-close read as end-of-stream while the other direction
still carries, a second sequential connection (which is what fails if the pool
never refills), and every refusal in the table above.

#### 4.3.3 The options — **PARTIAL** (the receive buffer is a report, not a window)

`setsockopt`/`getsockopt` are no longer a no-op: each socket's registry `Entry`
carries a `SockOpts`, and the common options round-trip with the correct ABI —
`SO_REUSEADDR`, `SO_KEEPALIVE`, `SO_SNDBUF`, `SO_RCVBUF`, `TCP_NODELAY` (`SOL_TCP`)
as 4-byte ints, `SO_LINGER` as an 8-byte `struct linger` (in-out length honoured),
plus `SO_TYPE`/`SO_ERROR`. The two buffer sizes are Linux's numbers, not this
kernel's taste (`net::sock_buffer_size`): a fresh socket reports its family's own
defaults — 212992 for both datagram options, 16384 send / 131072 receive for a
stream — a `setsockopt` stores **twice** the ask, clamped from below at 4608
(send) or 2304 (receive) and capped at 4 MiB before the doubling, and a negative
ask means "the largest" rather than an error. Unknown options are accepted and
read back as 0.

**`SO_REUSEADDR` is applied at `bind`/`listen`, and the rule it gates is not the
one the man page suggests.** That is the finding, so the measurement comes
first. On this host's kernel, `127.0.0.1`, with the holder's own
`SO_REUSEADDR` and the new socket's varied independently, and with the holder in
each of the states a name can be in:

| the name is held by | holder's `SO_REUSEADDR` | new socket's | `bind` |
|---|---|---|---|
| a listening socket, open | either | either | `EADDRINUSE` |
| a `TIME-WAIT` 4-tuple | no | **yes** | `EADDRINUSE` |
| a `TIME-WAIT` 4-tuple | yes | no | `EADDRINUSE` |
| a `TIME-WAIT` 4-tuple | yes | yes | **succeeds** |
| an established connection, listener closed | either | not both | `EADDRINUSE` |
| an established connection, listener closed | yes | yes | **succeeds** |
| a bound, never-connected socket | yes | yes | **succeeds** |

Three things fall out of that, and two of them are corrections to what this
option is usually described as doing:

* **The new socket's word is not enough.** The row where the holder did not set
  the option is `EADDRINUSE` *however loudly the new socket asks*. What makes a
  restart work is the other direction: Linux copies `SO_REUSEADDR` onto a socket
  when it is **accepted** (measured, `inet_csk_clone_lock`), so the listener's
  option is inherited by the connections that will later sit in `TIME-WAIT` on
  its port. A restart comes back because the *dead connections* had the option,
  not because the new socket does.
* **The exemption is not about `TIME-WAIT`.** A bound-but-unconnected socket and
  a live established connection both yield their name to a caller when both
  sides asked - so a port can be rebound while a connection is still open on it,
  and a listener can then *share* that name with it (measured: the new listener
  answers a fresh client while the old connection keeps its own). `TIME-WAIT` is
  simply the state this is usually noticed in.
* **A listener never yields.** Two live listeners on one name is what
  `SO_REUSEPORT` is for - it is not implemented here, and setting `SO_REUSEADDR`
  on *both* is not a substitute on Linux either (measured).

So `net::Stack::tcp_name_taken` is one clause over one table of holders: a
listening holder always conflicts; every other holder conflicts unless both it
and the caller set the option. The names it compares are the registry's recorded
ones plus, for a connection that recorded none (an accepted socket answers
`getsockname` from its own 4-tuple), the socket's own endpoint - and
`names_overlap` is the address half of the rule: the same port and either the
same address or the wildcard on one side, so `127.0.0.1:80` and `127.0.0.2:80`
coexist while either is refused beside `0.0.0.0:80` (measured). The holder's own
flag has to outlive its descriptor, so `Stack::teardown` carries it beside the
socket it named. The ephemeral pick behind a `bind(port 0)` (and behind
`listen`'s implicit bind) goes through the same predicate, which is what keeps an
implicit bind from landing on a `TIME-WAIT` port it would then have to refuse for
a name the caller never chose.

`t-tcp-listen` carries all of it - thirteen checks, every one of which is a line
of the table, plus "the restarted listener is *usable*" (a fresh client connects
to the taken-back name and its bytes arrive) and "a listener can share a name
with an open connection, and both serve". The whole test also runs on a Linux
host unchanged, which is the instrument: the same assertions that the guest must
satisfy are the ones a real kernel satisfies.

*Remaining (real effect, into smoltcp):* one. `SO_RCVBUF` is the number a caller
reads and not a window this kernel resizes: the receive ring is fixed at 16 KiB,
so a smaller ask does not shrink it, where Linux drops at delivery once the queue
passes the buffer. Five that used to be on this list are not: `SO_SNDBUF` governs
a datagram socket's queue (`docs/uapi/` §5.1 item 58), `TCP_NODELAY` reaches the
live socket (`set_nagle_enabled`), `SO_KEEPALIVE` arms smoltcp's probe timer at
Linux's default interval, `SO_REUSEADDR` is the rule above, and `SO_LINGER{on, 0}`
makes the last `close` a reset (4.3.4).

Two more boundaries ride with the buffer number rather than the option. The
datagram-too-big line is still the ring's rather than Linux's 65507 — this stack
has no IP fragmentation, so a wider datagram could not leave the machine — and a
floor-sized buffer admits three 1400-byte datagrams here against Linux's two,
because this kernel charges the payload where Linux charges each skb's truesize.

Two named gaps ride with the rule rather than the option. The predicate lives in
`net/mod.rs`, which is gated `target_os = "none"` (`src/lib.rs:48`), so it has no
host unit test - the guest program is the pin, the same situation `AF_PACKET`'s
parsers are in (§4.5). And **smoltcp's `TIME-WAIT` is ten seconds**
(`CLOSE_DELAY`) where Linux's is sixty (`TCP_TIMEWAIT_LEN`), so this machine
gives a name back six times sooner. The rule about *who* may take it is
identical; only the wait is short.

#### 4.3.4 `close(2)`, and the teardown the descriptor could not wait for — **DONE**

Measured rather than assumed: a plain `close(2)` reached `net::free_entry`, which
called `sockets.remove(handle)` on the spot, while smoltcp only puts the `FIN` out
from its poll loop after the socket is marked closing (`shutdown(2)` did call
`close()`; the last close did not). A closing socket is now marked and parked in
`Stack::teardown` - bounded, because a peer that stops answering may cost a slot
but not the set - and `reap_teardown` frees it after the poll that carries its
last segment, so the descriptor is gone when the caller asks and the wire still
gets its four packets. `t-tcp-close` + a host-side listener through slirp
(`10.0.2.2:4599` → host `127.0.0.1:4599`) shows the peer receiving the bytes
*and* end-of-stream instead of sitting until it gives up. (The socket to reap
lives in one of two sets now, so the list carries its home - see 4.3.1.)

That producer went missing once, with the consumer, the bounded list and this
paragraph all still in place - `TEARDOWN_MAX` sitting unread was the only clue,
and a suite that passes says nothing, because `t-tcp-close` reports `skip:` and
passes whenever no host listener is up. Re-measured with the listener running:
its connection (the orderly close) reaches `GOT-FIN`. The lesson is that the
probe is only an instrument when somebody holds it: run it with the listener up
before believing this section.

`SO_LINGER{l_onoff=1, l_linger=0}` - "reset, discard unread" - **is served now
(2026-10-16).** It used to be left orderly on purpose, and the reason written
down was that "smoltcp 0.13's `abort()` only moves the socket to `Closed` and
transmits nothing". That is **false**, and it was measured false before anything
was built: smoltcp's own `test_established_abort` asserts the `RST` that
`abort()` dispatches, `dispatch` emits one for a socket in `Closed` with its
endpoint still named (`repr.control = TcpControl::Rst`), and `poll_at` returns
`PollAt::Now` for that state - so the poll the socket is reaped after always
carries it. What the note was probably seeing is the shape that *does* go
silent: `abort()` and then removing the socket before the poll, which is exactly
what `Stack::teardown` exists to prevent. The last close of a socket carrying
`{on, 0}` is now an `abort()`, and the peer's observation was measured on this
host's kernel first (`.workbuddy/probe-linger.c`):

| the close | the peer's read | the peer's next write |
|---|---|---|
| `{0, 0}` (the default) | `0`, end-of-stream | succeeds |
| `{1, 0}` | `ECONNRESET` | `EPIPE` |
| `{1, 0}`, with bytes already sent | the bytes, then `ECONNRESET` | `EPIPE` |
| `{1, 5}` | `0`, end-of-stream | succeeds |
| a *listener* closed with an unaccepted connection | `ECONNRESET` | `EPIPE` |

Two things fall out of that table and neither is the `RST` itself. **A reset is
not the same read as a socket that never connected.** Linux answers `ENOTCONN`
for a socket that never carried a connection (one that was never connected, and
one whose `connect` was refused) and `ECONNRESET` for one that was reset, and
this kernel used to answer `ENOTCONN` for both - because smoltcp spells both
`RecvError::InvalidState`: an incoming `RST` moves the socket to `Closed` *and
clears its tuple*, so it is indistinguishable from a fresh one. The fact that
the connection existed is therefore remembered on the syscall side, in
`Channel::Tcp`'s `connected`, set by a completed `connect`, by `accept`, and by
any read or write that finds the socket open. **A linger with a non-zero timeout
is not a reset** (`{1, 5}`): Linux waits for the send buffer to drain and then
closes orderly, and only the zero timeout is the reset, so `free_entry` makes
`abort()` conditional on `linger_sec == 0` alone. And the bytes of a `{1, 0}`
connection that were *already transmitted* still reach the peer before the
reset - smoltcp keeps the receive buffer readable while it holds data (`may_recv`
is true then), which is Linux's answer too.

`t-tcp-listen` pins all of it in-guest, over the loopback, where both ends are
the same process: an ordinary close reads end-of-stream, `{on, 0}` reads
`ECONNRESET` and then writes `EPIPE`, `{on, 5}` reads end-of-stream, and a
listener closed with an unaccepted connection resets that client. The same
program runs on a Linux host unchanged, which is the instrument.

*Remaining after those:* `SO_KEEPALIVE` probe packets. `SO_RCVBUF`/`SO_SNDBUF`
cannot be honoured per socket as things stand: smoltcp sizes its buffers when
the socket set is built, so a real answer needs either a set rebuilt per
requested size or a limit applied in the mpos read/write queue - and until one
of those exists the options should say so rather than round-trip a number that
changes nothing. `SO_REUSEADDR` used to be on this line and is not any more: it
is applied at `bind`/`listen` now (§3.3.3), and the rebind allowance it gates
turned out to be a rule about the *holder's* option as much as the caller's.

### 4.4 Netlink Write Operations — **DONE**

The write side is real: `ip addr add/del`, `ip route add/del/replace`,
`ip link set eth0 up|down|mtu N|name X` are parsed (`ifaddrmsg`/`rtmsg`/
`ifinfomsg` + `rtattr` walks) and applied to the live stack - smoltcp's own
address and route tables, so a route added is a route the packet path
follows. `ip link set eth0 down` stops the interface from being polled:
frames neither leave nor arrive, and a `ping` through it loses 100% until
`up` restores it. Dumps read the same live state, so what was written is
what `ip addr`/`ip route`/`ip link` print - with the Linux shapes filled in
(`IFLA_BROADCAST`, `IFLA_MTU`, `IFLA_QDISC "noqueue"`, `IFLA_TXQLEN`,
`IFA_F_PERMANENT`, secondary-address labels `eth0:1`).

Errnos match Linux: `EEXIST` on a duplicate, `EADDRNOTAVAIL`/`ESRCH` on a
delete of something absent, `ENODEV` on an unknown index, `EAFNOSUPPORT`
off-family, `ENOBUFS` when the fixed route table (four entries) is full.
The error rides back in `nlmsgerr.error` **negated** - the kernel's own
convention; writing it positive made busybox print the message for an errno
no table knows ("No error information") instead of "File exists".

Deliberate v1 divergences (each asserted in `t-netlink`): an exact-
duplicate `ip addr add` succeeds instead of `EEXIST` (the boot's own
idempotent re-configuration stays quiet); a route with no gateway is
`EINVAL` (smoltcp delivers the interface's own subnets implicitly, so an
on-link prefix has no second expression); `RTM_DELLINK` and `ip link add`
are `EOPNOTSUPP` (no dynamic interfaces); `lo`'s address and MTU are fixed
(flags and read-back work, the stack has no loopback device behind them).
The `ip link set` flags path also works through the **ioctls** busybox
reaches it by - `SIOCGIFFLAGS`/`SIOCSIFFLAGS`/`SIOCSIFMTU`/`SIOCGIFINDEX`
(the one musl's `if_nametoindex` needs), `SIOCGIFMTU`, `SIOCGIFTXQLEN`/
`SIOCGIFHWADDR` (whose numbers 0x8942/0x8927 had been swapped, answering
the MAC for a txqueue-length request). Two helper gaps this opened: AF_UNIX
`SOCK_DGRAM` had to *open* (musl's `if_nametoindex` asks for it as the bare
handle for its own `SIOCGIFINDEX`; the datagram transfer itself has since
landed - see §4.1); and `IFLA_TXQLEN`/`IFLA_MTU` are native-order words, the
endianness addresses are not.

**Changes**:
- `RTM_NEWADDR`/`RTM_DELADDR`: `ip addr add`/`del`, applied to the stack
- `RTM_NEWROUTE`/`RTM_DELROUTE`: `ip route add`/`del`/`replace`
- `RTM_NEWLINK`/`RTM_SETLINK`: link up/down (a real poll gate), MTU, name
- `ip addr`/`ip route`/`ip link` dumps read the live tables, not constants
- The `ifreq` ioctl set `ip link set` and `if_nametoindex` ride on

**The other netlink protocol has no write side. That was the last blocker on the
udevd path, and it is written now.** `NETLINK_ROUTE` was already applied to the
live stack; `NETLINK_KOBJECT_UEVENT` was a read-only channel the kernel
publishes into, and udev needs the userspace half of it: the device manager
hands an event to one of its workers with `udev_monitor_send_device(monitor,
worker->monitor, dev)` - a `sendmsg(2)` on a uevent socket carrying a 12-byte
`struct sockaddr_nl` - and publishes the processed device to the group every
listener joined. The send path here read that address as a `sockaddr_in`
(`len < SOCKADDR_IN_LEN`) and answered `EINVAL`, so the handoff failed and udevd
killed the worker that refused it: `worker [11] did not accept message -22
(Invalid argument), kill it`, with no device tagged while it happened.

What the family grew, and it is measured rather than recalled - `libudev-monitor.c`
for the shape udev uses, this host's kernel for the semantics:

| Question | Measured answer |
|---|---|
| where a port id comes from | the process's pid when it is free, a hash-derived id when it is not (libudev's own comment: "usually, but not necessarily the pid"); an unbound socket is given one by its first send |
| binding a port id another socket holds | `EADDRINUSE`; binding twice (even to add a group) is `EINVAL` |
| a unicast | delivered to the port id; the receiver's `msg_name` is the *sender's* `nl_pid` with `nl_groups == 0`; an unknown or closed port id is `ECONNREFUSED`; the sender never receives its own |
| a group send | goes to the **lowest** group in the mask (Linux computes `ffs` of it: `nl_groups = 6` reaches group 2 and not group 4); the receiver is told the *destination group* as `msg_name.nl_groups`; the sender is not told; an empty group is not an error - the byte count comes back anyway, which is what libudev's "a multicast `ECONNREFUSED` would be expected" tolerates |
| a send with no port id and no group | accepted, and addressed to the kernel |
| a 12-byte `sockaddr_nl` | the shape that works; a short address or a family that is not `AF_NETLINK` is `EINVAL` (not `EAFNOSUPPORT`) |
| credentials | with `SO_PASSCRED` every received message carries `SCM_CREDENTIALS` with the sender's real ids; without it there is no control data at all |
| `getsockname` before bind | the all-zero address, not an error |

The policy is `uevent`'s and is host-tested - nine tests over port ids, unicast,
groups (including the `ffs` artifact), the full-queue `ENOBUFS` a handoff must
hear rather than lose, and the autobind - while `t-uevent` pins the shape
end-to-end in the guest: two sockets, `SO_PASSCRED`, a `sendmsg` to the port id
`getsockname` reported, the payload, the source address and the credentials, and
the three refusals. **And the boot says the same:** `/dev/input/event0` carries
`ID_INPUT=1`, `ID_INPUT_KEY=1` and `ID_INPUT_KEYBOARD=1`, and
`/run/udev/data/c13:64` holds them - which is the tag libinput classifies a
keyboard by, and the end of the chain that began with a worker's zero-byte
message.

#### 4.4.1 The receive doors look without taking — **SERVED** (2026-10-21)

Item 76 closed `AF_UNIX`'s receive flags and named the families beside it. Two of those were
reading the flag word as nothing at all, and one of the two is this section's: **netlink**.
`recvfrom` and `recvmsg` on a `NETLINK_ROUTE` socket popped the front answer whatever `MSG_PEEK`
said, and returned `min(len, message)` whatever `MSG_TRUNC` said; the uevent socket's `recvmsg`
did the same behind its block. A peek that takes is worse than a peek refused - the caller gets
a number and loses the message it asked about - which is why this was a *wrong shape* and not a
smaller feature.

Measured whole on a current host first (`.workbuddy/probe_netlink_peek.c`, Linux 7.2.11, over a
`RTM_GETLINK` dump, so the queue holds several answers and "was the front taken?" is answered by
*which interface* the next answer names, not by an empty-queue `EAGAIN` alone):

| The call | Linux 7.2.11 |
|---|---|
| `recv(MSG_PEEK, 8192)` on the dump | 3020 (the first answer), and the plain receive after it is the same 3020 bytes |
| `recvmsg` with `MSG_PEEK`+`MSG_TRUNC`, iov 8 | **3020** - the answer's own length - with `msg_flags` `MSG_TRUNC`; the front is still there |
| `recvmsg(MSG_TRUNC, iov 8)` | 3020, copies 8, `msg_flags` `MSG_TRUNC`, and **consumes** it (the next plain receive is the second answer, 3436) |
| `recvmsg(iov 8)`, no flags | 8, `msg_flags` `MSG_TRUNC` **unasked**, consumes |
| zero-length room, `MSG_PEEK` | 0, `MSG_TRUNC` in `msg_flags`, front still there |
| zero-length room, plain | 0, `MSG_TRUNC`, front consumed |
| `MSG_DONTWAIT` on a drained queue | `EAGAIN` |

The same two flags on the same family's *write-half* socket are the rows this section's uevent
table already carries; the receive half is this landing. The fix is one look per layer, not one
in the syscall: `rtnl::peek` clones the queue's front and `recv_blocking(handle, look)` gives the
look the same interruptible park a receive has (`MSG_PEEK` must not turn a blocking read into
`EAGAIN`); `uevent::peek` clones a `Message`, whose bytes are already an `Arc` the send shared;
and the doors read the flag word - `handle_recvfrom`'s netlink branch, `handle_recvmsg`'s
netlink and uevent arms, and `recvmsg_netlink`, which now returns the block's own length when
`MSG_TRUNC` asked while the scatter wrapper still gets the copy's count, and reports the cut in
`msg_flags` either way.

**Pins**: `t-netlink`'s 14 new claims on the default sweep (both doors' looks, repeatability,
the short receive's consume and its `msg_flags`, `MSG_TRUNC`'s return, both zero-length shapes,
the drained-queue `EAGAIN`) and `t-uevent`'s 5 (the block's own length through 8 bytes, the cut
in `msg_flags`, the room that fits, a zero-length peek, a zero-length receive that consumes).
**Gate**: x86_64 `pass=192 fail=0`, `mpos-kunit 32 passed, 0 failed`, exit 33; aarch64
`pass=191 fail=0`, `mpos-kunit 32 passed, 0 failed`. The external group (which carries the
`AF_PACKET` door's own four claims in `t-dhcp`) is green at `pass=199 fail=0` (199 built).

---

### 4.5 `AF_PACKET` — the link layer as a socket — **CLOSED**

The kernel had been the last word on the machine's address with no door for
userland to overrule it: `10.0.2.15` was a boot-time constant and `EAFNOSUPPORT`
was the answer to every client that wanted to negotiate one. `src/net/packet.rs`
opens that door in the shape Linux spells: `socket(AF_PACKET, SOCK_DGRAM,
htons(proto))`, a tap hung on the NIC seam (`NicDevice::receive` hands every
frame to the sockets *before* the stack decides whose business it is — which is
the point: a BOOTP reply for an addressless machine is nobody's business in the
IP sense), `sendto` with a `sockaddr_ll` whose Ethernet header this kernel
builds from the device's own MAC, per-socket bounded rings with the oldest-frame
drop, and `bind`/`getsockname`/`recvfrom` answering through the same
`sockaddr_ll` (`linux/if_packet.h`'s real offsets: 20 bytes, address at 12).
`SOL_PACKET` membership requests succeed because the tap already sees every
frame of the socket's type; every other level is `ENOPROTOOPT`, Linux's own
answer. Both `AF_PACKET` types are served now: the paragraph above is the
`SOCK_DGRAM` shape, and `SOCK_RAW` — whole frames in, the caller's own header
out — is recorded in the closing bullet below.

The named gaps, each with what it waits on:

* **Closed (2026-10-14): `SOCK_RAW` — frames *with* their header on both sides.**
  The old `EOPNOTSUPP` was wrong twice over: it refused a shape a caller (an ARP
  resolver, a bridge, `busybox`'s own raw paths) legitimately wants, and it was
  not even Linux's refusal for an unsupported *type* — measured on the host
  (`.workbuddy/probe_afpacket_type.c`, run under `sudo` for `CAP_NET_RAW`), an
  `AF_PACKET` of any type but `SOCK_DGRAM`/`SOCK_RAW` answers `ESOCKTNOSUPPORT`
  (94), while `SOCK_RAW` itself *opens*. The shape that landed, each rule
  measured first (`.workbuddy/probe_afpacket_raw.c`, `probe_afpacket_raw2.c`,
  `probe_afpacket_addr.c`, over a veth pair, so the wire is real):

  * On **receive** a `SOCK_RAW` socket is handed the **whole** frame, its
    Ethernet header in front, where `SOCK_DGRAM`'s is the payload half (measured:
    the same 8-byte payload reads back as 22 bytes, the leading 14 the two MACs
    and `88 b5`). The seam therefore stores the frame whole and slices at the
    reader (`PacketSock::received`, `link::frame_payload`), so the tap — which
    knows the frame and not each socket's type — does not split per listener.
    `sll_protocol` still reports the frame's own ethertype and `sll_halen` is 6.
  * On **send** a `SOCK_RAW` socket carries the caller's own header, and the
    bytes go to the wire **untouched**. `sll_addr` names nothing (measured: the
    header's destination won when the two disagreed), `sll_protocol` is ignored
    (the frame's own type is the frame's), the only check is that a header is
    actually there — shorter than 14 is `EINVAL` (14 passes, 13 does not) — and
    the `sockaddr_ll` is still required because the frame alone cannot name the
    interface (`ENXIO` without one, measured).

  `t-dhcp` pins both halves on both machines: it opens a `SOCK_RAW` socket
  beside the `SOCK_DGRAM` one, reads the same DHCP answer through both — equal
  but for the fourteen-byte header, and that header names the device as the
  sender — and then builds a DISCOVER octet by octet with its *own* Ethernet
  header, sends it through the raw socket, and gets the server to answer the
  transaction id it wrote, which is the whole-frame path measured end to end.
  The unsupported-type refusal is pinned there too (`SOCK_STREAM` is
  `ESOCKTNOSUPPORT`). `link::frame_payload` carries the host unit test for the
  slice.
* **Closed (2026-10-21): the receive doors read the flag word — `MSG_PEEK` looks and
  `MSG_TRUNC` names the frame's own length.** Both were ignored here: every packet door
  refused `MSG_PEEK` with `EOPNOTSUPP`, and `MSG_TRUNC` changed nothing (the `recvmsg` door set
  the bit in `msg_flags` but still returned the copy). Measured whole on a current host first
  (`.workbuddy/probe_packet_peek.c`, Linux 7.2.11), inside
  `unshare(CLONE_NEWUSER | CLONE_NEWNET)` — the capability the host lacks made the way item
  74's probe made it — with `lo` up and one frame the socket sends to itself, since loopback
  hands the skb back through the receive path:

  | The call | Linux 7.2.11 |
  |---|---|
  | `recv(MSG_PEEK, 2048)` with a 16-byte frame queued | 16, and the plain receive after it is the same 16 bytes |
  | `recvmsg` with `MSG_PEEK`+`MSG_TRUNC`, iov 8 | **16** — the frame's own length — with `msg_flags` `MSG_TRUNC`, frame still queued |
  | `recvmsg(MSG_TRUNC, iov 8)` | 16, copies 8, consumes |
  | `recvmsg(iov 8)`, no flags | 8, `msg_flags` `MSG_TRUNC` **unasked** |
  | zero-length room, `MSG_PEEK` / plain | 0 with `MSG_TRUNC`; leaves it / consumes it |
  | `recv` with `MSG_PEEK`+`MSG_DONTWAIT`, frame queued | 16 |

  Served by `link::Frames::peek` (the ring is `mpos-link`'s, so the look belongs beside `pop`)
  and `PacketSock::try_peek`/`peek_blocking` in the kernel, with the two doors reading the flag
  word. **Pinned** by `t-dhcp`'s four new claims — one `recvfrom` and one `recvmsg` peek, each
  naming the frame's length and each leaving it for the read that follows — and by the
  `mpos-link` unit test `a_peek_shows_the_front_and_leaves_the_queue_alone`. `t-dhcp` is in the
  `external` group, so those four claims are the program's own gate rather than the default
  sweep's; run there, both looks name 576 bytes (the frame's own length) and both leave the same
  frame for the read after them (`pass=199 fail=0`, 199 built).

* The boot still has no *working* client, and the two halves of that sentence are
  measured separately now (2026-10-12). **The wiring is in the image and runs**:
  `assets/rootfs/core/usr/share/udhcpc/default.script` is the script `ifup` hands
  to `udhcpc -s` (without it a client leases and configures *nothing* - the
  interface keeps whatever address it had and the boot looks successful),
  `xtask/src/rootfs.rs`'s `SERVICES` enables `networking`, and a product boot
  shows both working: `* Starting networking ...` / `*   eth0 ...`. The client then
  stops on its **first** syscall:

  ```
  udhcpc: socket(AF_INET,3,255): Protocol not supported
  ifup: failed to change interface eth0 state to 'up'
  ```

  `AF_INET, SOCK_RAW, IPPROTO_RAW` — the door a DHCP client uses to *send* a
  DISCOVER before it has an address of its own, building the IP header itself.
  (The *receive* half needs no new door: it is `AF_PACKET` - and which *call*
  the client reads it with turned out to be the whole of what was left; see the
  bullet below.) This kernel serves `AF_INET`+`SOCK_RAW` for ICMP
  (`t-rawicmp`) and refuses the wildcard protocol with `EPROTONOSUPPORT`, so what
  is left of this gap is that one door, and its shape is named rather than
  guessed: the caller's own IP header under Linux's `IP_HDRINCL` rules (the total
  length, the checksum and the source address filled in when it is left zero),
  `CAP_NET_RAW` as the gate, and `EPROTONOSUPPORT` kept for every other protocol.
  The door is open now, and with the receive call settled (below) a product boot
  leases: `udhcpc: broadcasting discover` / `broadcasting select for 10.0.2.15,
  server 10.0.2.2` / `lease of 10.0.2.15 obtained from 10.0.2.2, lease time
  86400` (2026-10-13) - the address is the client's own negotiation from here,
  over a door userland drives, and not a number this kernel decided. The
  raw-IP door's unicast half, once named below, is closed (2026-10-15).
* **What a first attempt at that door measured, and why it was reverted** (2026-10-12).
  The rules were measured on the host first (`.workbuddy/probe_rawip.c`): `IP_HDRINCL`
  reads **1** on a fresh `SOCK_RAW`/`IPPROTO_RAW` socket (the protocol implies the
  option rather than the caller setting it); `sendto` with no destination is
  `EDESTADDRREQ`; a header shorter than its IHL is `EINVAL`; the **header's**
  destination wins over the argument and its total length is overwritten by the sent
  length; the source, the identification and the header checksum are filled in when the
  caller leaves them zero; `recv` on such a socket gets nothing, ever; and the whole
  raw family - `AF_PACKET` included, both its types - is `EPERM` for a caller without
  `CAP_NET_RAW`, asked *before* the protocol is judged. A draft that served the door
  took the boot past its first syscall — `udhcpc: started, v1.38.0` /
  `udhcpc: broadcasting discover` — and then the server never answered
  (`FAIL: the server answers through the raw door too (Ok(0))`, a poll timeout, while
  the same DISCOVER through `AF_PACKET` answers as it always did). So the packet was
  *queued* (`sendto` returned the full 292 bytes) and did not leave as built. The draft
  was reverted rather than landed, because a door that accepts a packet and drops it
  silently is worse than one that refuses.

  **The cause, found by reading smoltcp rather than by guessing.** The header is *not*
  rebuilt from the socket's field - the packet is **dropped**, and by name:
  `socket/raw.rs`'s `dispatch` carries

  ```rust
  if ip_protocol.is_some_and(|next_header| next_header != packet.next_header()) {
      net_trace!("raw: sent packet with wrong ip protocol, dropping.");
      return Ok(());
  }
  ```

  so a queued packet whose IP protocol byte is not the *socket's* is discarded silently
  (`Ok(())`), which is exactly the shape measured above: `sendto` returns the full
  length because it only queued, and nothing leaves. This kernel's `Channel::Raw`
  builds its socket with the channel's protocol - `255` for `IPPROTO_RAW` - while a
  DHCP client's packet carries `17`, so the door as written can never send UDP.

  **And the two obvious ways out are both wrong, for reasons worth writing down.**
  Creating the socket with *no* protocol filter makes smoltcp emit anything (the check
  is `is_some_and`), but `accepts` then matches every IPv4 packet and the interface's
  `raw_socket_filter` reports them *handled* - so such a socket would swallow the
  machine's UDP and TCP delivery. And binding the channel's socket to the header's
  protocol (per send, or lazily at the first one) breaks the other direction the same
  way: a socket bound to `17` receives UDP and steals it from the real UDP sockets.

  A raw-IP send therefore has to leave through the **device**, the way an `AF_PACKET`
  send already does - the frame built from the header plus the link address - and that
  is a door with its own prerequisite: a broadcast destination (all `dhcp` needs) needs
  no neighbour resolution, while a *unicast* one needs the ARP/next-hop lookup smoltcp
  would have done, so the honest first version serves broadcast and names unicast.
  The `CAP_NET_RAW` half **was** verified and landed: both raw doors ask it before the
  protocol, pinned by `t-caps` on both machines (a lowered caller answers `EPERM` for
  ICMP, for a protocol nothing has, and for `AF_PACKET`; the same calls succeed once the
  capability is back).
* **What the working client measured, and the two bugs it took to get there**
  (2026-10-13). With the door open, the boot reached `udhcpc: broadcasting
  discover` and then answered itself `udhcpc: read error: Not a socket,
  reopening socket` on every retry - while the NIC seam printed a 590-byte
  frame per DISCOVER, which is the server replying. So the client's own shape
  was measured on the host first, the way the door's rules were:

  ```
  socket(AF_INET, SOCK_RAW, IPPROTO_RAW) = 5      opened twice, closed both times
  socket(AF_PACKET, SOCK_DGRAM, htons(ETH_P_IP)) = 5   the *listen* socket
  bind(5, {AF_PACKET, ETH_P_IP, ifindex}) = 0
  setsockopt(5, SOL_PACKET, PACKET_AUXDATA, [1]) = 0
  sendto(6, "E\0\1H...", 328, ...)                DISCOVER, its own temp socket
  recvmsg(5, {msg_iov=656, msg_control=PACKET_AUXDATA}, 0) = 576
  ```

  (`sudo strace -f -e trace=network,read busybox udhcpc -i lo`; the host has
  1.36.1, the image 1.38.0, and the 1.38 binary carries the same strings.)
  `d4_recv_raw_packet` moved off `safe_read` onto `recvmsg` so that it could
  read `PACKET_AUXDATA` and skip a UDP checksum a VM never computed; the
  `AF_INET` raw socket is the door the client *dies* on when it is refused, and
  carries no byte. Two things were wrong on this side of that measurement:

  * `handle_recvmsg` had no `AF_PACKET` branch, so the call fell through to the
    registry's `ENOTSOCK`. `t-dhcp` could not see it: that program reads the
    same socket with `recvfrom`, which does have a branch - which is why "the
    receive half works" was true of one door and not the other. It reads one
    answer with each now, and a green pair is the claim that both answer.
  * The NIC seam's copy into the wildcard door (`echo_to_raw_doors`) compared
    the frame's two type bytes against a *second* spelling of the ethertype
    written with the wrong endian detour - `0x0800u16.to_le()`, where
    `from_le_bytes` of the wire's `08 00` is `0x0008`. Every frame returned at
    that line, which measured as "the seam runs and no door is ever filled".
    The constant is gone; the seam uses `packet::IPV4_ETHERTYPE`, the spelling
    that was measured. A constant written twice is §15's rule, and this is it
    again.

  And one the fix would have walked straight into: the seam took the **stack's
  lock** to find the doors, while `NicDevice::receive` is called from inside
  `poll_once`, which every caller reaches *already* holding `NET` borrowed - a
  re-entered `RefCell` is a panic, not a `None`. The doors are a static list of
  their own now (`socket::RAW_DOORS`, the shape `packet.rs`'s tap uses), each
  holding a clone of the queue's `Arc`, and a door whose channel is freed is
  dropped on the next frame. The queue holds `Vec<u8>` at the length the packet
  arrived with: the 512-byte slot it replaced dropped QEMU's **576**-byte
  BOOTREPLY whole (14 + 20 + 8 + 548 - `struct bootp_t` with `DHCP_OPT_LEN`
  312, the `len=590` the seam was printing) and made a short packet read back
  as a full slot. Measured green: `the door itself reads one whole packet (got
  576)`.
* **Closed (2026-10-13): the parsers over `sockaddr_ll` and the Ethernet frame
  have a host-side pin now.** They had none: `AGENTS.md` §3 asks every new parser
  for a fuzz target, and `net/packet.rs` carried neither a target nor a host test
  — not from oversight but from the §6 trap that gates the whole `net` module on
  `target_os = "none"`, so nothing host-compiled could reach it, and
  `net/socket.rs`'s annotated test module stood as the standing proof of that
  (V-11). The wire-format halves — the `sockaddr_ll` decode and encode, the
  Ethernet header, and the packet socket's bounded frame ring — moved to the
  `mpos-link` leaf crate (`crates/link`, re-exported as `crate::link`), the shape
  `evdev` and `uinput` took before them; `src/net/packet.rs` keeps what needs the
  machine (the tap on the NIC seam, the device's own MAC, the user-memory
  boundary). Ten host unit tests run in the gate's leaf stage
  (`cargo xtask test`'s stage 1), and `fuzz/link_wire` is the target — 60 000
  libFuzzer runs clean. What the crate can now *assert* is the point:
  `IPV4_ETHERTYPE == u16::from_le_bytes([0x08, 0x00])` is a pinned test, which is
  exactly the spelling a second copy of that constant got wrong two bullets up —
  so the parse that could not be tested is now the parse whose endianness cannot
  drift silently again.
* **Closed (2026-10-15): the raw-IP door's unicast half — the next hop, and its
  link address.** The door's broadcast shape (all a DHCP client sends before it
  has an address) needs no neighbour: the frame goes to the broadcast MAC. A
  **unicast** destination is the gap two bullets up named: it needs the route
  and the neighbour's MAC, the lookup smoltcp performs for every socket it sends
  and which this direct-to-device door has to do for itself. The routing rule was
  measured on the host first (`.workbuddy/arp_probe.c`, an `IPPROTO_RAW` socket
  over a veth pair and a route-less netns):

  * **No route is `ENETUNREACH`**, synchronously. An `IPPROTO_RAW` `sendto` in a
    namespace whose only interface shares no subnet with the destination (and has
    no default route) answers `-1`/`ENETUNREACH` (101); with a route - the
    destination on-link, or a default route - the same call returns the length.
  * **A route with an unresolved neighbour returns the length anyway.** Linux
    queues the frame and resolves ARP in the background, so `sendto` is
    fire-and-forget for the neighbour. A synchronous stack has nowhere to queue,
    so this door resolves in place and answers `EHOSTUNREACH` if no reply comes.
    That is the door's one divergence from Linux, and it is the honest half of
    the pair: a length returned for a frame that never left is the one answer
    this kernel may not give (`AGENTS.md` §3.2).

  The lookup is Linux's routing rule, not a guess: a destination inside one of
  eth0's own subnets is on the wire (the next hop is the destination itself),
  anything else leaves by the matching route with the longest prefix (the default
  route's gateway, `10.0.2.2`), and no route at all is `ENETUNREACH`. The link
  address is ARP, in `src/net/arp.rs`: a bounded cache the NIC seam fills from
  every ARP frame (`arp::observe`, called beside `packet::deliver` and
  `echo_to_raw_doors` - the same frame that fills smoltcp's own `NeighborCache`,
  which is private and which this door, bypassing smoltcp, cannot reach), and a
  cache miss runs a bounded probe (`arp::resolve`: three one-second requests, the
  count Linux's own `ucast_solicit` uses) that drives the stack until the reply
  arrives. A subnet's *directed* broadcast (`10.0.2.255`) is also served, by the
  broadcast MAC and no probe. The ARP wire format is smoltcp's `ArpRepr`/
  `ArpPacket` - the parser the IP layer already trusts - so this adds a cache and
  a probe, not a parser, and owes no new fuzz target; the only wire fact this side
  adds is `link::ARP_ETHERTYPE`, pinned in `crates/link` beside `IPV4_ETHERTYPE`.

  `t-dhcp` pins it on both machines: the unicast DISCOVER aimed at the server now
  returns its whole length (a broken next-hop or ARP path would answer
  `EHOSTUNREACH` there instead), and a unicast ICMP echo - built here, so the door
  carries the caller's packet untouched - is answered by the peer `t-rawicmp`
  pings, which proves the frame left for the resolved link address (the router's
  MAC, since that peer is off-link). `link`'s host test pins `ARP_ETHERTYPE ==
  u16::from_le_bytes([0x08, 0x06])`.

---

