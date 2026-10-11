# misc

**Home:** `src/{rand,uevent,uaccess,kernel_services}.rs` · **Owns:** the kernel's odd corners: entropy, kobject events, user-memory access, and the service table the machine layer calls back into

## What it owns

This page collects four small kernel-wide concerns that each earn a file at the crate root rather
than a module directory. They share a shape rather than a subject: each is a service the rest of the
kernel calls by name, each sits at a boundary (to userland, to the machine layer, to a caller's
pointer), and none of them is a subsystem with state worth a directory. Grouping them here is a
documentation decision; in the source they are four independent modules, and the sub-headings below
keep them apart.

### Entropy (`src/rand.rs`)

The kernel's random source, and the reason it exists is concrete: `/dev/urandom` is the only entropy
source a busybox userland has - its TLS implementation reads that one file and nothing else, not even
`getrandom(2)` - so a kernel with no `/dev/urandom` cannot run an HTTPS client however correct its TCP
is. The same bytes back `getrandom(2)`, so a program that reaches for either gets the same stream.
What it replaces is documented in the file itself: a xorshift64 seeded from the tick counter *on every
call*, where two calls inside one tick returned the same bytes - repeated randomness, not merely weak
randomness.

It is one `ChaCha20Rng` behind the usual `critical_section::Mutex<RefCell<Option<...>>>` (`RNG`),
seeded once and re-keyed on demand. `init` seeds idempotently; `fill` seeds on first use so a syscall
or a device read need not know the boot order; `reseed` is what `RNDRESEEDCRNG` asks for and mixes a
draw from the *current* stream into the new seed so the re-key is not a reset to a fixed point. The
seed itself (`seed()`) is the machine's own sources through `hal::Entropy` (via `hal::mix_entropy`),
opting the boot instant and tick counter on top so a machine whose sources are absent still gets a
seed an outside caller cannot predict. The machine's rng *device* is injected through
`set_device_entropy` rather than named, because a kernel service naming `crate::drivers` would close a
cycle with the edge that already runs the other way - which device a machine has is the wiring
layer's fact (`docs/notes/` §14.3). On the host build `seed()` is a fixed constant on purpose, so a
unit test seeds the same every run.

### kobject events (`src/uevent.rs`)

The device-event bus: Linux narrates its device tree over `NETLINK_KOBJECT_UEVENT`, and every piece
of userland that has to *find* a device listens there - udev, mdev's hotplug mode, and mkinitfs's
`nlplug-findfs`, which is what decides where the root filesystem is. A kernel that publishes no
events is one whose initramfs cannot find its root, however correct its devices are. This module owns
both halves of the family: the kernel's publish side (`Event`, `encode`/`encode_seq`, `publish`) and
the socket side userland drives (`open`, `close`, `bind`, `send`, `recv`, `ready`, `exists`,
`port_of`, `set_passcred`/`passcred`) for the unicast handoff udev's manager makes to a worker.

The message format is the envp block - one line naming the event, then NUL-terminated `KEY=VALUE`
entries - and the encoding is careful about two separators (`Split::AtAndNul` vs `Split::Nul`) so a
device name cannot forge a line boundary. `SEQNUM` is the line that is easy to omit and expensive to:
udevd orders its queue by it, so a kernel that sends no number tells udevd every event is still
waiting behind another and it queues them all and starts none. The sequence counter is shared with
the netlink header (`encode_seq`) so `nlmsg_seq` and `SEQNUM=` are one fact. Also here is the
*numbering* (`Numbering`, `BlockEntry`, `makedev`/`major_of`/`minor_of`), because a device's
`major:minor` is what `/sys` reports, what `/dev` nodes are made from, and what a listener matches
events against - one function computes it so the three cannot disagree. The write side's rules
(unicast by port id, broadcast taking the lowest set group as Linux's `ffs` does, `ECONNREFUSED` for
an unknown port, `ENOBUFS` for a full queue, the sender never receiving its own message) are each
measured against Linux and the module comment says so. The publisher and the send path deliberately
do their allocation *before* the critical section and share one `Arc<Vec<u8>>` among recipients: a
clone inside a section with interrupts masked could spin on the allocator's lock while its holder
cannot run, which is how a cold-plug stalled on the first device it announced.

### User-memory validation (`src/uaccess.rs`)

The userland-boundary primitives: validate a caller's pointer, and turn a filesystem error into the
errno a caller reads. `MAX_IO` bounds a single read or write, `buffer_ok` is the length-and-access
check, `user_accessible` is its page-walk half (through `hal::user_accessible` on the machine; a stub
that answers "accessible" on the host, where there are no page tables and the tests hand real memory
in), and `err_of` maps `VfsError` to the errno the dispatcher returns.

The file's own header explains why it sits at the crate root and not in `syscall`: `net/*` and
`drivers/drm.rs` copy bytes to and from a caller's buffer, so they must ask whether a range is the
caller's and, for a fill, writable - and neither question is a *syscall* concern. The helpers happened
to live in `syscall`, so every below-`syscall` caller had to reach *up* into it, which is the inversion
`docs/ARCHITECTURE.md` §11.1.5 (V-9) records. `syscall` re-exports them so the paths its handlers
already use keep working; new callers outside the ABI layer name `crate::uaccess`. The bodies were
unchanged by the move - the semantics are the contract.

### The kernel service table (`src/kernel_services.rs`)

`arch/**` has to ask the kernel whether a faulting page is one to populate, where a `svc` goes, and
what a line of console input means, and it asks through `hal::kernel::Kernel` - a trait the *kernel*
crate implements - so the machine layer never names `sched`, `syscall` or `mm` (`docs/notes/`
§14.2). This file is the one implementation of that trait (`struct Services`), registered once by
`register()` from `src/main.rs::_start` before anything that can take a trap is armed on either
machine (before the x86_64 IDT, before the aarch64 vector table). The trait is deliberately a *fact*
interface rather than a policy one: each method answers a question or performs one step, and which of
them a machine calls and in what order stays the machine's.

It is **one** table, and the file says why: it used to be written twice, once per machine, and that
duplication is what made it look machine-shaped - the two impls were the same methods with the same
bodies, and the only difference (`device_not_available`) is a *capability*, gated on `mpos_fpu_trap`,
not an architecture. A machine that needs a different answer here is a machine with a different
*kernel*, which is the thing the seam exists to say is impossible.

## Key files

| File | What lives there |
|---|---|
| `src/rand.rs` | the `ChaCha20Rng`, `init`/`fill`/`reseed`, the injected device source (`set_device_entropy`) and `seed()` |
| `src/uevent.rs` | `Event` and its envp encoding, the netlink socket table, `publish`, and the `Numbering`/`makedev` device-number model |
| `src/uaccess.rs` | `MAX_IO`, `buffer_ok`, `user_accessible`, `err_of` |
| `src/kernel_services.rs` | `struct Services` (the one `impl Kernel`) and `register()` |
| `arch/src/hal/kernel.rs` | the `Kernel` trait and the forwards the machine layer actually calls (`started`, `resolve_cow_fault`, `dispatch_syscall`, ...) |
| `arch/src/hal/mod.rs` | `Entropy` and `mix_entropy`/`boot_epoch` (what `rand::seed` draws on), and `user_accessible` (what `uaccess` walks on the machine) |

## Contracts

**Entropy:** `init`, `fill(&mut [u8])`, `reseed`, `set_device_entropy(fn() -> Option<[u8; 32]>)`; the
one stream is shared by `getrandom(2)` and `/dev/urandom`, which is the invariant a caller relies on.
`fingerprint()` is test-only and takes its read from a clone so it does not advance the stream.

**kobject events:** the userspace contract is the family's Linux shape - `open`/`close`/`bind`(with
`EINVAL` for a second bind, `EADDRINUSE` for a held port, `EBADF` for a socket this module does not
own)/`send`/`recv`/`ready`/`exists`, and the `Event` fields; the kernel contract is `publish(&Event)`,
called from device registration and from a write to a device's `uevent` attribute. `Numbering` is the
one place a `major:minor` is computed.

**uaccess:** `buffer_ok(buf, len, write, from_user)` and `err_of(VfsError) -> isize` are the contract
every syscall handler that copies to or from a caller relies on; `MAX_IO` is the bound.

**The service table:** `hal::kernel::Kernel` (see `arch/src/hal/kernel.rs` for the method list and
each one's contract) plus `register`. Registration is once and panics on a second call, because
registering twice is a boot bug rather than a runtime condition.

The edges - who may call these modules and whom they may call - are `docs/ARCHITECTURE.md` §2/§3.
`kernel_services` is the inverted face of the machine seam; `rand` and `uevent` are the two places
that were most at risk of an upward edge and explicitly avoided one (the injected entropy source and
the network-stack wake call, which is `#[cfg(mpos_net)]`).

## What userspace sees

- `src/rand.rs`: the `'R'` ioctl row in `docs/uapi/` - `RNDGETENTCNT`, the `RNDADDENTROPY`
  credit, and `RNDRESEEDCRNG` (a real re-key) served; `RNDZAPENTCNT`/`RNDCLEARPOOL` refused
  `EOPNOTSUPP`; `FIONREAD` answering `EINVAL` as Linux does. The `getrandom` flag row is the same
  file's other face.
- `src/uevent.rs`: the `struct nlmsghdr`/`rtattr`/`ifa*msg` row (shared with netlink route) and the
  `struct ifreq` row.
- `src/uaccess.rs`: no row of its own. It is the boundary *through which* other rows' `EFAULT`
  answers travel, so it is pinned indirectly (see below) rather than listed.
- `src/kernel_services.rs`: nothing userspace can name; it is the machine layer's bridge into the
  kernel.

The `RNDRESEEDCRNG` provenance and the `FIONREAD` divergence are docs/uapi/ §5.1; the
`PROT_READ`-destination `EFAULT` case is §5.2.

## Pins

| Pin | Kind | What it would fail on |
|---|---|---|
| `src/rand.rs` tests | host | `two_fills_do_not_repeat`, `a_fill_actually_changes_the_buffer`, `a_stream_keeps_going_across_calls`, `a_reseed_re_keys_the_generator`, `a_rekeyed_generator_still_produces_bytes` |
| `src/uevent.rs` tests | host | `an_event_encodes_as_the_envp_block_listeners_parse`, `a_subsystems_own_keys_travel_in_the_block`, `every_event_carries_a_later_number_than_the_last`, `only_bound_sockets_receive_events_and_the_queue_drains`, `a_bounded_queue_keeps_the_newest_events`, `a_bound_socket_gets_an_address_and_the_rules_around_it`, `a_unicast_reaches_one_socket_and_names_its_sender`, `a_broadcast_goes_to_the_group_and_never_to_its_sender`, `a_first_send_names_a_socket_that_never_bound`, `a_full_unicast_queue_is_refused_rather_than_dropped`, `the_device_number_is_packed_the_way_libc_decodes_it` |
| `t-getrandom` | `user/tests` | the count contract on both `getrandom` and large fills |
| `t-uevent` | `user/tests` | the event socket's blocking read, the legacy block's bytes, and the `SEQNUM` ordering |
| `t-udev-worker` | `user/abitest` integration | the socket shape udevd's worker channel needs |
| `t-netlink` | `user/abitest` integration | the netlink message layouts (its `nlmsghdr`/attribute vocabulary is the same leaf `uevent` uses) |
| `src/syscall/linux/fs/ioctl.rs` etc. | host/build | the `buffer_ok`/`err_of` callers in the ABI layer, and `t-stat-buffer` / `t-ioctl` for the writable-destination cases |

`src/uaccess.rs` has no `#[cfg(test)]` module of its own (grep), and `src/kernel_services.rs` has
none; `uaccess` is pinned through its callers and `kernel_services` is compiled into every boot, so a
wrong body is a boot failure on a machine rather than a host test. `t-uevent`'s own header is explicit
that its udev-handoff half is not measurable on the host, so that row's coverage is the machine
sweep, not the host suite.

## Evidence and open work

The measurements and the lessons behind each corner are in `docs/notes/`: the entropy device
injection and the machine-seam rule in §14.2/§14.3, the aarch64 virtio-disk `DEVPATH` defect that
`VIRTIO_BLK_DEVPATH` fixes in §15.10, and the `rand`-in-stage-1 flake that the trap table warns about
in §12.4a. The userspace verdicts are docs/uapi/ §5.1/§5.2. The remaining work is
`docs/ROADMAP.md`'s: the `RNDRESEEDCRNG` entropy-count provenance noted in `docs/uapi/`, and the
`uaccess` `user_accessible` stub's limits on the host build.
