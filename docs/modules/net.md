# net

**Home:** `src/net/` · **Owns:** smoltcp driven by the machine's adapter, the socket registry that gives every userspace socket a stable handle the fd table can name, and the families that run without an IP stack (`AF_UNIX`, `AF_PACKET`, `AF_NETLINK`, `AF_VSOCK`)

## What it owns

The protocol machinery — IPv4, ICMP, ARP, TCP and its retransmission and window
logic, UDP — is smoltcp's and not this module's. What `net` adds is the two seams
around it. The first is `NicDevice`, a `smoltcp::phy::Device` that moves frames
between smoltcp and whatever the boot found behind `drivers::virtio::NetDevice`;
the second is a registry that turns the handle an `Arc<dyn Inode>` carries back
into the endpoint state a syscall needs. Everything the stack owns sits in one
`Stack` behind `critical_section::Mutex<RefCell<…>>` (`NET`, `mod.rs`), the same
one-lock discipline as the rest of the kernel, and the rule that follows from it
is that no network operation may sleep holding the lock — a blocking call is a
loop of *lock, poll, try, unlock, park* (`mod.rs`'s header, `socket.rs`'s
`park_until`). Parking ends on the stack's own wakeup rather than the next clock
tick: every task waiting on a socket registers through `subscribe`, and `service`
wakes the whole registered list after every poll.

The stack comes up with a single static configuration — the shape QEMU's user
network always hands out, recorded in `LOCAL_IP`/`GATEWAY_IP` — because userland
owns addressing from there: `apk`, `udhcpc` and `ip` negotiate and mutate through
the stack's own write side (`add_ipv4_addr`, `del_ipv4_addr`, `add_route`,
`del_route`, `set_link_up`, `set_mtu`, `set_eth_name`). DNS deliberately is *not*
served here; musl resolves in userspace over UDP sockets, which is why the UDP
path must work and no resolver lives in the kernel (`mod.rs`'s header).

The loopback is a *second* interface with a *second* socket set, and that is not
tidiness: smoltcp's `Interface::route` answers from the address prefix and
otherwise consults the route table, while `Interface::poll` walks only the set it
is handed, so one interface cannot serve both wires. `Home` (`Eth` or `Lo`)
records which set a socket lives in, and it is decided once — by the first call
that names an address (`bind`, `connect`, `sendto`) — because smoltcp has no
operation that moves a socket between sets. A raw socket cannot choose (its
destination is a per-call argument), so it stays on `eth0`; that is why
`ping 127.0.0.1` still travels the virtual wire.

Three of the families take a deliberate detour around the smoltcp registry,
because each must answer on a machine with no NIC — where the smoltcp registry
would say `ENETDOWN`. `unix.rs` serves `AF_UNIX` (a dbus socket, `/dev/log`, an X
socket: names in the filesystem, not addresses) over `vfs::pipe::DuplexEnd` for
streams and its own datagram queue; `rtnl.rs` serves the route half of
`AF_NETLINK`; `vsock.rs` serves `AF_VSOCK` over the `VsockDevice` the device tree
or PCI probe opened. `packet.rs` is the opposite case — it is deliberately welded
to the NIC seam, tapping every frame in `NicDevice::receive` before the IP layer
decides the frame is any of its business, which is how a DHCP client can read the
wire before the machine owns an address.

## Key files

| File | What lives there |
|---|---|
| `mod.rs` | The `Stack` (the `eth` **`Wire`** and the `lo` fabric, the `entries` registry, the `listeners` pool, the `teardown` list, the `waiters` list), `Channel`/`Home`/`Entry`/`SockOpts`, `init`, `service`, `irq_service`, `subscribe`/`unsubscribe`/`wake_waiters`, `alloc_socket`/`adopt_socket`/`with`/`with_stack`, and the rtnetlink write-side helpers the ioctl/link calls drive. The **network namespaces**: `mint_namespace` and the `NAMESPACES` registry of stacks keyed by the number `vfs::ns` minted, `Stack::deviceless()` (a namespace's own stack: `lo`, and no wire), and the `Wire` whose device and interface are optional while its socket set is not - a namespace has `lo` and no `eth0`, which is the difference the link dump keeps with `LinkFacts::eth_present` |
| `socket.rs` | The socket syscalls (`handle_socket`, `handle_bind`, `handle_connect`, `handle_listen`, `handle_accept`/`accept4`, `handle_sendto`/`handle_sendmsg`/`handle_sendmmsg`, `handle_recvfrom`/`handle_recvmsg`/`handle_recvmmsg`, `handle_shutdown`, `handle_getsockname`/`handle_getpeername`, `handle_setsockopt`/`handle_getsockopt`, `handle_socketpair`), and the `SocketInode`/`NlRouteInode` `Inode` impls that carry a socket through the fd table |
| `unix.rs` | `AF_UNIX`: the `UnixSock` — which records the **network namespace it was made in** (Linux's `sk_net`, and the key of the family's name tables) — the bind table keyed on node identity `(dev, ino)` for **paths** (machine-wide: a path is not namespaced at all, measured both ways) and on `(namespace, name, type)` for **abstract** names (the namespace's, and the *socket's* rather than the calling task's), `listen`/`connect`/`accept_once` (which mints the accepted socket in the listener's namespace), the datagram queue, `SCM_RIGHTS`, `SO_PEERCRED`/`SO_PASSCRED`, and `unbind_node` |
| `rtnl.rs` | The route half of `AF_NETLINK`: `RTM_GET*` dumps built from the live stack and `RTM_NEW*/DEL*/SET*` mutations parsed and applied to it |
| `packet.rs` | `AF_PACKET` (`SOCK_DGRAM` only): the `PacketSock`, `deliver` tapped from the NIC seam, and the user-memory boundary. The wire format itself — `sockaddr_ll` encode/decode, the Ethernet header, the bounded frame ring — sits below the gate in the `mpos-link` leaf crate (`docs/modules/leaf-crates.md`), so it is host-tested and fuzzed (`docs/notes/` §4.5) |
| `vsock.rs` | `AF_VSOCK`: the `VsockSock`, its id registry, `bind`/`listen`/`connect`/`accept`, credit-checked `send`/`recv` over the virtio socket device |

## Contracts

The bring-up pair is `init(dev: Arc<dyn NetDevice>)` — which builds both fabrics
and stamps the static configuration — and `spawn_service()`, which starts one
permanent background task that polls once a tick; both are called from
`board::steps::bring_up_net`, so the two machines' boot code shares the same
wiring. `service()` is the poll-and-wake entry a syscall or the background task
uses; `irq_service()` is the interrupt-shaped entry, and the one step it adds is
acknowledging the device through `NetDevice::ack_interrupt` so the next arrival
can raise the vector again (`crate::kernel_services` routes here). The wakeup
contract is `subscribe`/`unsubscribe`/`wake_waiters`: a wake only means "look
again", each waiter re-reads its own condition and sleeps on if it is unchanged,
which is what licenses waking the whole list and lets a device event (a uevent
becoming readable) wake a listener that no frame will ever reach.

The registry contract is the handle: `alloc_socket`, `adopt_socket`, `with` and
`with_stack` index `Stack.entries`, and every syscall resolves an fd back through
`socket_handle()` on the `Inode` (`SocketInode`/`NlRouteInode`). A handle comes from **one**
machine-wide counter, and `HOMES` records where its entry lives - which namespace, and which slot
in that namespace's table. That mapping is this layer's `struct sock::sk_net`: **a socket remembers
the stack it was made in**. The creation paths resolve the *caller's* namespace (`with_stack`),
and every door that acts on an existing descriptor - `with`, `free_entry`, the teardown sweep,
`/proc/net`'s collector - resolves the *socket's*. So a socket bound before `unshare(CLONE_NEWNET)`
keeps answering its own address, an inherited descriptor works in a cloned child, and one handed
over by `SCM_RIGHTS` arrives intact and answers the sender's. The machine's own stack is reached
only by `with_machine_stack`, `init` and `with_namespace(None, ..)`. The invariant
that holds it together is that a socket belongs to exactly one `Home` from the
first address-naming call, and that `close(2)` unbinds the descriptor immediately
while teardown still rides the wire via the bounded `teardown` list. The socket
handlers are called from `syscall::linux::dispatch`; the module validates its own
user pointers through the `crate::uaccess` leaf rather than reaching up into
`syscall` (the design rule recorded in `docs/ARCHITECTURE.md` §3). Non-syscall
callers: `net::unix::unbind_node` on `unlink`, `net::socket::close_last_socket`
on `close`, `collect_net_sockets` for the `/proc/net` rows, and the
`syscall/linux/fs/ioctl.rs` link calls (`link_facts`, `ipv4_addrs`, `lo_addrs`,
`routes`, `primary_ipv4`, …). The device seam the whole module names is
`drivers::virtio::{NetDevice, VsockDevice}` — never a transport — so a machine
whose card is MMIO runs the same stack. Who may call this module, and its two-way
edges with `sched`/`syscall`/`uevent`/`vfs`, are `docs/ARCHITECTURE.md` §2/§3/§6;
they are linked here, not copied.

## What userspace sees

The rows this module owns are in `docs/uapi/`: the `socket`/`bind`/`connect`/
`listen`/`accept`/`send*`/`recv*`/`getsock{name,opt}` family in §1, the `SIOC*`
(`0x89xx`) interface-ioctl row that routes through `fs/ioctl.rs`, and the
structure-layout rows for `struct sockaddr_ll`, `struct nlmsghdr`/`rtattr`/
`ifa*msg` and `struct ifreq` in §4. The gaps — record-only `SO_LINGER`/buffer
sizes, `AF_PACKET` `SOCK_RAW`, the named `SOCK_SEQPACKET` stream, `MSG_PEEK` — are
listed in docs/uapi/ §5.2, with the module gate's consequence for host
testing in §5.4. That file is the inventory; this page does not duplicate its
rows.

## Pins

`src/net/` has **no host tests**, and that is a documented property rather than
an omission: `src/lib.rs` gates `pub mod net;` on `target_os = "none"`, so a
`#[cfg(test)]` module inside it would never run — the reason `net/socket.rs` ends
with a comment explaining why it has none, and the reason `AGENTS.md` §5 lists
"a `net/*` host test never runs" as a known trap. The exception the rule allowed
is now taken: the `AF_PACKET` wire format — `sockaddr_ll`, the Ethernet header,
the frame ring — moved to the `mpos-link` leaf crate (`crates/link`), so those
parsers do run in the gate's leaf stage and are fuzzed by `fuzz/link_wire`
(`docs/notes/` §4.5). The module's own files still have none, for the reason
above. The pins are the guest programs, all in `user/tests/src/bin/` now:
`t-socket`, `t-sock-flags`, `t-sockmsg`, `t-sockopt`, `t-socketpair`, `t-tcp`,
`t-tcp-listen`, `t-tcp-close`, `t-udp`, `t-udp-loopback`, `t-unix`,
`t-unix-dgram`, `t-unix-rustix`, `t-unix-scm`, `t-epoll-socket`, `t-uevent`,
`t-dhcp`, `t-netlink`, `t-dns`, `t-rawicmp`, `t-netlat`, `t-udev-worker`.
`tests.toml` decides
which of these the default sweep runs; several network-facing ones sit in its
`external` group because they need the host's network or a host listener, and a
red one there is a statement about the internet before it is one about the
kernel.

## Evidence and open work

The reasoning and measurements behind each landing are `docs/notes/` §4.1
(`AF_UNIX`), §4.2 (ICMP and `IP_RECVTTL`), §4.4 (the netlink write side), §4.5
(`AF_PACKET`, still partial) and §10.1 (network latency); the aarch64 stack flip
and the vsock host-gating are §13.4 and §13.28. `docs/ROADMAP.md`'s Phase 4 is
where the status and the remaining networking work are tracked. `AF_VSOCK` has no
conformance program at all, because a test boot deliberately gets no host vsock
bus (§13.28) — that gap, the `AF_PACKET` parsers' lack of a host test or fuzz
target, and the `net` gate's other consequences are named in docs/uapi/ §5.2
and §5.4.
