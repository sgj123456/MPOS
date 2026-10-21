# §6 Phase 6: Process Management Enhancements (Low-Medium Priority)

Part of the split `docs/notes/` evidence library — routing for every `§N.M` pointer is in
[index.md](index.md). Section numbers are unchanged from the original `NOTES.md`.

## Phase 6: Process Management Enhancements (Low-Medium Priority)

### 6.1 ptrace (Debugger Support) — **DONE**

`PTRACE_DETACH` used to return 0 and leave the tracee stopped. The cause was not
the stop it was in but the one it had not reached: `PTRACE_ATTACH` injects a
`SIGSTOP`, and while the tracee is still inside its current syscall that signal
is merely *pending*. A detach that only ends an in-progress stop therefore hands
back a process which stops itself on the way out of that syscall - with no tracer
left, and nothing on the machine but a `SIGCONT` that could end it. The test
demonstrated it by hanging the machine for ten minutes with every earlier check
printing `ok:`, until a bounded `WNOHANG` poll replaced the blocking `waitpid`.

The three cases are Linux's, and were measured against it before being matched
(`t-ptrace-attach` pins all three):

| Sequence | Linux | Now |
|---|---|---|
| `attach`, `detach(0)`, never `wait`ing | target runs on | the pending stop signal is dropped by the detach |
| `attach`, report the stop, `detach(0)` | target runs on | the stop parked with `delivering` set is ended |
| `kill -STOP`, `attach`, `detach(0)` - or `detach(SIGCONT)` | stays stopped | the stop is left alone, and no wake is sent |

The third row is the one that decides the shape of the code: the signal a detach
injects goes to the *tracee*, not to its stop, so naming `SIGCONT` does not end a
group stop the user asked for - and a `wait` on that child stays empty. Which is
also why the wake is now conditional everywhere: a task parked in a group stop is
held there by the scheduler's sleep, so a detach that woke it would release the
very stop it had just decided to leave. The companion rule is in the signal
sender - a stop signal arriving for a task already in a group stop has nothing to
say to it, and must not wake it either.

Dropping the `SIGSTOP` *pending* bit in the third case is not an invention: after
`kill -STOP` + attach + `detach(0)`, one outside `kill -CONT` returns Linux's
process to running rather than re-stopping it, which is only possible if the
attach's own signal did not outlive the detach.

What is *not* modelled: Linux reports a `PTRACE_EVENT_STOP` to a tracer that
attaches to an already-stopped task, so that the tracer's first `wait` returns
before anything is detached. Here that stop stays the parent's news, which is why
the test attaches without waiting - the same thing `strace -p` on a suspended job
would see.

`handle_ptrace` (proc.rs) implements the debugger set: `TRACEME`, `ATTACH`,
`DETACH`, `CONT`, `SYSCALL`, `SINGLESTEP`, `KILL`, `INTERRUPT`,
`PEEK`/`POKE` TEXT/DATA/USER, `GET`/`SETREGS`, `GET`/`SETREGSET`,
`GET`/`SETFPREGS`, `GET`/`SETSIGINFO`, `SETOPTIONS`, `GETEVENTMSG`, wired to the
task's `Tracing` state and the `waitid` stop reports. Required for gdb/strace.

| Syscall | Nr | Description |
|---------|-----|-------------|
| `ptrace` | 101 | Process trace |

**Required for**: gdb, strace, ltrace

**Implementation plan**:
- `PTRACE_TRACEME`: child sets `traced = true`, sends `SIGSTOP` to self
- `PTRACE_ATTACH`/`PTRACE_SEIZE`: attach to target process
- `PTRACE_PEEKDATA`/`PTRACE_POKEDATA`: read/write target memory
- `PTRACE_GETREGS`/`PTRACE_SETREGS`: read/write target registers
- `PTRACE_SINGLESTEP`: set TF (trap flag) in RFLAGS, deliver `SIGTRAP` on next instruction
- `PTRACE_CONT`: resume execution, optionally delivering a signal
- On any stop, target enters `TASK_TRACED` state and parent is notified via `waitid`

### 6.2 Process Resource Limits (Real) — **DONE**

`getrlimit`/`setrlimit`/`prlimit64` now operate on a real per-process table
rather than returning constants:
- A `[Rlimit; RLIM_NLIMITS]` table is stored on `Task`, seeded by
  `default_rlimits()` (8 MiB stack, 1024/1M `NOFILE`, 4096 `NPROC`, infinity
  elsewhere) and **inherited whole across fork and clone** (exec leaves it).
- `setrlimit`'s rule is real: `rlimit_admit` rejects soft-above-hard with
  `EINVAL` and an unprivileged raise of the hard ceiling with `EPERM` (root may);
  the accepted pair is stored. `RLIM_INFINITY` is `u64::MAX`.
- `getrlimit`/`prlimit64(pid 0)` read the caller's own table; `prlimit64` on
  another live pid can read it (else `ESRCH`) but not write it (`EPERM`).
- `/proc/<pid>/limits` renders the table in the `ulimit` layout.
- The pure rule + defaults are host-tested (`setrlimit_admits_by_privilege`).

*Three of the resources are enforced, beyond the pair above:*
- **`RLIMIT_NOFILE`**: the descriptor table carries the soft ceiling (seeded 1024,
  synced from `setrlimit(7)` - 7 is `asm-generic/resource.h`'s number, and this
  said 5 until §15.13, which is `RLIMIT_RSS`'s; copied on fork);
  `install`/`install_at_least`
  refuse a descriptor at or above it with `EMFILE` instead of growing unbounded.
  Every fd-creating syscall propagates it, and `pipe`/`socketpair` return the
  first descriptor if the second is refused. Host-tested by
  `the_descriptor_table_honours_its_ceiling`.
- **`RLIMIT_STACK`**: `resolve_stack_growth` reads the faulting task's soft limit
  as the stack floor (SIGSEGV beyond it; capped at the reserved region for a
  raised/infinite one).
- **`RLIMIT_SIGPENDING`** (11, landed 2026-10-17, `docs/uapi/` §5.1 item 35): the
  queued real-time records a **user** may hold, counted by the scheduler's send
  path over every task of the *target's* real uid, against the *target's* soft
  limit - Linux's own shape, measured (`send_signal_info`'s `Sent::QueueFull`).
  `sigqueue`/`rt_sigqueueinfo` answer `EAGAIN` at it; `kill`/`tgkill` drop the
  copy and answer 0. The records themselves live in the per-task `RtQueue`, so
  the resource's default is that ring's size (`RT_QUEUE_LEN`, 16) rather than
  Linux's `max_threads / 2`: raising the limit past the ring buys no room, which
  is the one corner `docs/uapi/` §5.2 names.

### 6.3 Namespace Support — **UTS BUILT; THE OTHER THREE DESIGNED**

Two calls (`unshare`, `setns`), eight kinds of namespace, and one of each kind
until this pass. Most of this section is still the *decision record* rather than
a to-do list, because the decisions are what §6.3 kept being deferred for: which
of the eight this kernel can isolate observably, in what order they are worth
building, and what each one costs the layers that would have to grow a
per-namespace answer. **The first of the four decisions has since been built** -
`CLONE_NEWUTS`, in `src/uts.rs` and the `Task` field that holds it - so the
table below reads "build first" as done and the rest as designed. "Step 1,
landed", at the end of this section, is what the step actually cost, what it
measured, and what it deliberately did not do.

**The rule that decides the list is §4's rule 4** - refuse rather than
approximate. A namespace that isolates nothing is not a namespace, it is a
promise, and `EOPNOTSUPP` is what a promise is worth: `unshare(CLONE_NEWIPC)` on
a machine with no SysV IPC objects and no POSIX message queues would hand back a
"new IPC namespace" whose contents are exactly the old one's, and a caller that
believes it stops looking. So each of the eight is decided by *what a program
could observe*, and only the four with something observable behind them are on
the build list.

| flag | what it must observably isolate | verdict |
|---|---|---|
| `CLONE_NEWUTS` | `uname`'s `nodename`/`domainname`: `sethostname` in one namespace must not be visible in another, through **all three** doors | **built** - "Step 1, landed" below |
| `CLONE_NEWNS` | the mount table *and the root*: a `mount` in one namespace invisible in another, and `chroot`/`pivot_root` giving a container its own `/` | **build second** |
| `CLONE_NEWPID` | the numbers a process *sees*: a child of the new namespace is pid 1 to itself, and `/proc` is numbered per reader | **build third - the expensive one** |
| `CLONE_NEWNET` | interfaces, addresses, routes and the socket table: a socket bound in one namespace is not reachable from another | **build fourth** |
| `CLONE_NEWUSER` | uid/gid mapping, and the capability set *within* the namespace | **built** (the maps, the translation, the "root inside" grant, and both doors that create one) - "Step 6"/"Step 6e, landed" below |
| `CLONE_NEWIPC` | SysV IPC objects and POSIX message queues | **refused**: this kernel has neither, so it isolates nothing |
| `CLONE_NEWCGROUP` | the cgroup tree a process sees | **refused**: there is no cgroup tree here to see |
| `CLONE_NEWTIME` | `CLOCK_MONOTONIC`/`BOOTTIME` offsets per namespace | **built** - "Step 5, landed" below |

**UTS: the state is two strings, and the cost is that they have three doors.**
`nodename` and `domainname` are one process-wide pair today (`vfs::procfs`'s
`HOSTNAME`/`DOMAINNAME` statics), read by `sethostname`/`setdomainname`,
`uname`'s `nodename`, and `/proc/sys/kernel/{hostname,domainname}` - kept in one
home precisely so the three cannot disagree. A namespace makes that home *the
caller's*: a `Uts { nodename, domainname }` the task owns, with `fork`/`clone`
inheriting the reference (a child shares its parent's namespace until it
unshares) and `unshare` **copying** it, which is Linux's `copy_utsname` - the
new namespace starts with the old one's contents, so a `sethostname` afterwards
is invisible to the parent. This is the one whose whole cost is the plumbing;
it is first because it is small, and because it is the shape every other
namespace repeats.

**MNT: the one that costs the VFS — the root half has landed, the table has not.**
The per-process *root* — the prerequisite this whole item was deferred on — is
done. A task carries its own root (`sched::Task.root`, inherited on `fork`/`clone`,
set by `chroot(2)` through `sched::set_root`), and the one absolute-path walk
(`Vfs::resolve_walk`) begins, clamps `..`, and restarts absolute symlinks at *the
caller's* root rather than the machine's. A `chroot` now re-homes the calling
process and its future children — `t-chroot` has a child chroot, confirm its jail,
and prove the parent's view of the same `/tmp` is untouched, the `t-ns-uts` shape
for a per-task view. `set_global_root` is gone; what the boot's `switch_root` used
it for it now gets by inheritance (pid 1 is every process's ancestor). **The mount
table is still a single global**, and `pivot_root(2)`/`MS_MOVE` act on it
machine-wide — which *is* Linux's answer with one mount namespace, and stays
named until the next step. What is left: move the table onto the task too (an
`Arc<MountNamespace>` holding the entries, the root having already moved), have
resolution ask *the caller's* namespace for its mounts, and let
`unshare(CLONE_NEWNS)` clone the tree the way `copy_mnt_ns` does — a `mount` in the
child then invisible to the parent, and `pivot_root` per-namespace, which is what
gives a container its own `/`. Two things stay named refusals rather than silent half-answers: **mount
propagation** (`MS_SHARED`/`MS_PRIVATE`/`MS_SLAVE` - a mount event crossing
namespaces is a second mechanism, and ignoring the flag would silently change
what "unshare" meant), and **`/proc/mounts`** having to render the reader's
namespace rather than the machine's.

**PID: expensive because of the vocabulary, not the feature.** A pid here *is*
the task slot - `sched::policy::TaskId(pub usize)`, the number `getpid` returns,
the number `/proc/<pid>` is named after - and the slot is **reused**. A pid
namespace needs the thing Linux built for exactly this and this kernel does not
have: a `struct pid` whose lifetime is its own, a per-namespace number mapped to
it, and `/proc` rendering the numbers the *reader's* namespace gives. It is also
where the reusable-slot problem §7.1 already records for `VT_SETMODE`'s promise
("Linux holds a refcounted `struct pid`, so a promise can never be inherited by
an unrelated process later given the same number") would have to be solved once
and properly, rather than twice. The semantics to hold, none of them optional:
`unshare(CLONE_NEWPID)` renumbers the caller's *children* and not the caller
(the process that asked keeps its own pid - it is the first child that is 1);
`setns` into a pid namespace has the same "children only" effect; and `/proc`
must be per-namespace, which means the pid directory for a process is a
different name in two namespaces that both can see it.

**NET landed in part, and the part it refuses is written down beside it.** The fourth rung is
served for the one shape the design named first: **`clone(CLONE_NEWNET)` gives the child a stack of
its own** - `lo` and nothing else, a port space the machine's cannot see, and no wire. Three things
are refused by name, all with the same root; and the root is a measurement that came out of the
attempt itself.

**NET's first step, measured: a socket has to remember its namespace.** Designing the fourth
rung turned up a constraint that reorders it, and it is worth having in writing before anyone
starts. Two readings of `net`:

* `net::with(handle, f)` (`src/net/mod.rs:1032`) reaches the machine's stack *directly*
  (`NET.borrow(cs)`), not through `with_stack` - so an fd's socket would be resolved against
  whichever namespace is current, and a task that unshared would find its existing sockets answering
  from the wrong stack;
* and the handle is a **per-stack index** (`alloc_socket` -> `stack.add_tcp(..)`, and
  `Stack.entries` is that stack's own registry), so a fresh namespace numbers its sockets from the
  same low indices: an fd inherited across a namespace boundary would name one of the *new*
  namespace's sockets instead of its own. A silent aliasing rather than a stale handle, which is the
  worse half.

Linux's shape is the opposite and it is deliberate: a `struct sock` holds its `sk_net`, so a live
fd keeps working in the namespace it was made in, whoever holds it afterwards - which is what makes
`unshare(CLONE_NEWNET)` usable at all in a process that already has connections. So the step that
has to land **first** is the socket registry moving out of `Stack` and becoming machine-global,
each entry recording the namespace whose stack owns its addresses, routes and timers
(`entries`/`listeners`/`teardown` are referenced 41 times, measured, all inside
`socket.rs`/`mod.rs`/`rtnl.rs`). It is observable on its own - a machine that never unshares is
unchanged, and every socket test still passes - and no namespace may be entered before it.

**And that is not an argument any more: it was measured, by a booted machine.** The attempt at
the namespace itself - `Task::net_ns`, `with_stack` following the caller, a fresh `Stack` with `lo`
and no wire, `clone(CLONE_NEWNET)` served for a caller holding no socket - built and passed the
host suites, and the **gate panicked inside smoltcp**:

```
KERNEL PANIC: panicked at smoltcp-0.13.1/src/iface/socket_set.rs:113:
index out of bounds: the len is 0 but the index is 2
```

The reason is the second half of the measurement above, and it is worth stating because it fixes
the boundary of the step: **`alloc_socket` and `with(handle)` reach the machine's stack
directly** (`NET.borrow`, `mod.rs:1304`/`:1227` - six such sites in all), while `bind`, `listen`
and `connect` go through `with_stack`. In a namespace those two disagree about *which* stack a
socket lives in: the entry was registered in one and looked up in the other, and the child's
lookup landed on an empty socket set with an index from the machine's. Every socket test in the
suite had been passing because in the machine's namespace the two are the same stack - which is
exactly why this cannot be half-migrated.

So the step's boundary is firmer than it was: **all six** direct-`NET` sites move at once
(`with`, `alloc_socket`, and the three registry functions beside them), the entry records its
namespace, and `with(handle)` resolves the stack *from the entry* rather than from the caller -
because the caller's namespace is not where an inherited descriptor's socket lives. That last
point has a second face the clone precondition does not cover: `SCM_RIGHTS` can hand a socket
descriptor to a task in another namespace, and nothing in the sender's or the receiver's flags says
so.

**What landed on the near side of that boundary, and how the four doors were made to agree.** The
attempt above panicked because `alloc_socket` and `with(handle)` reached the machine's stack while
`bind`/`listen`/`connect` went through `with_stack`. The landed fix is the other direction from
the registry step: **every door that acts on a descriptor resolves the caller's namespace's
stack** - `with`, `alloc_socket`, `adopt_socket` and `free_entry` joined `with_stack`, and the
machine's own stack is now reached only by `with_machine_stack` (the init namespace) and `init`
itself. That closes the two-stacks-for-one-socket hole for every socket a task can *make*.

**And then the half that made the caller-relative rule unnecessary.** Resolving a descriptor in
the *caller's* namespace is right only while every socket a task holds was made where the task is.
Two cases break that, and both are ordinary: a socket made **before** an `unshare` (the caller
moves, the socket does not) and a descriptor **handed over** by `SCM_RIGHTS` (the receiver never
was in the sender's namespace, and no flag says so). The first version of this step therefore
refused both by name - `EOPNOTSUPP` for a `clone` from a caller holding a socket (asked through
`Inode::socket_handle`, and the pin is what corrected the first draft there: a socket inode's
`NodeKind` is `CharDev`, not `Socket`, so a `NodeKind` test passed every descriptor and the child
went on to re-run the test body, which is how the log showed every claim twice), and `MSG_CTRUNC`
for a handover that crossed a namespace.

What replaced both is the fact Linux keeps inside the socket: **a socket remembers the stack it was
made in.** `net`'s `HOMES` is that fact here - a side table from the registry handle to
`(namespace, slot)`, with the handles minted from **one** counter so two namespaces can never mint
the same number - and `with(handle)`, `free_entry` and the teardown path all resolve the stack
*from the entry* rather than from the caller. That is what makes the three refusals go away at
once, and each is now a claim rather than a refusal:

* `unshare(CLONE_NEWNET)` and `setns(fd, CLONE_NEWNET)` are **served**, and the pin holds the
  consequence: a socket bound *before* the move still answers its own port *after* it, because it
  is still resolved against the stack it was made in.
* `clone(CLONE_NEWNET)` from a caller holding a socket is served, and the child's inherited
  descriptors keep working against *their* stacks.
* A descriptor handed across a namespace arrives **intact**: the pin has the parent bind its gift
  in the machine's namespace and hand it over the machine-wide unix registry, and the child (in
  its own namespace) reads that socket's address back - the *sender's* port - which no resolution
  against the child's own stack could produce.

**Two divergences the pin pinned, both with their reason.** A fresh namespace's **`lo` comes up
carrying `127.0.0.1/8`**, where Linux's starts down and unaddressed - because this kernel answers
`EOPNOTSUPP` to `RTM_NEWADDR` naming `lo` (its one address is the kernel's, not a caller's to
change), so a namespace whose loopback started empty could never be given one and its private
network would be unusable rather than private. The pin holds both halves: the child's loopback
answers, and the request that would add the address is refused. And **`ip link set eth0 up` inside
a namespace is `ENODEV`**, because there is no device at that index - a namespace is born with no wire, and a wire arrives with `ip link set ... netns`, which is served
now - see the move's own paragraph below.

**And the move request itself is now refused rather than silently accepted (2026-10-15).** The
rtnetlink `link set` handler parsed only `IFLA_MTU` and `IFLA_IFNAME`, so `ip link set eth0 netns N`
- which carries `IFLA_NET_NS_PID` (or `IFLA_NET_NS_FD`) - fell through the attribute walk and the
request answered **`0`**: a success report for a wire that had not moved, the single worst answer
this ABI can give a script. The handler now recognises both attributes and returns `EOPNOTSUPP`
for the whole request, matching the honest-refusal rule (§3.2): the move is real on Linux and this
kernel cannot do it, so it says so. The pin is `t-ns-net`'s parent-side claim, which sends the
request from the machine's namespace with the child's own pid as the destination (a live task in a
namespace of its own, so the name resolves) and reads back `-EOPNOTSUPP`; before this change the
same request answered `0` and the claim fails, which is the counterfactual.

**And the move itself is served (2026-10-16).** `ip link set eth0 netns N` now moves the wire:
the device, the interface built around it and its name leave the stack that owns them and are
handed to the stack the request names - by pid (`IFLA_NET_NS_PID`) or by an open
`/proc/<pid>/ns/net` (`IFLA_NET_NS_FD`, resolved the way `setns` resolves one). What "served" has
to mean was measured on this developer's Linux *first*, because the shape is not the obvious one
(`.workbuddy/probe-netns-move.sh` and `.workbuddy/probe-netns-errno2.py`, kernel 7.2):

| question | Linux's answer |
|---|---|
| a device moved into another namespace | `0`; it keeps its name, MTU, hardware address and interface index, and arrives **down** (`IFF_UP` cleared) and **with no address and no route** |
| moved into the namespace it is already in | `0`, and nothing happens - the address even survives |
| `lo`, into another namespace | `EINVAL` (netns-local) |
| a pid that names no task | `ESRCH` |

The unconfigured arrival is the surprising half, and it is what an address *belongs to* says: an
address is the namespace's, not the device's, so both sides of a real move end up with none, which
is why a moved interface has to be configured again. Two traps in the measuring, both worth their
line: `ip` validates `netns <n>` in userland and refuses before the kernel ever sees it, so the
`ESRCH` above is only reachable with a hand-built message - and a first draft of that message
numbered `IFLA_NET_NS_PID` **27**, which is not what `linux/if_link.h` gives it (**19**, printed
by `.workbuddy/probe-ifla.c`). Every request answered a plain `0`, including the ones that should
have failed, which is exactly the silence this rung's earlier claim was written against.

The pin is `t-ns-net`'s `wire_moves_into_a_namespace`, a whole round trip: the machine moves its
wire into a child cloned with `CLONE_NEWNET`; the machine's next `ip link set eth0 up` is
`ENODEV` where it had just answered `0`; the child brings the wire up **and gives it the machine's
own address**, which answers `0` only because no address travelled with the device (`EEXIST` would
mean one had); the child hands it back by naming the parent, which is the caller Linux requires of
the request, since the device is in *its* namespace now and a request from outside it has no
standing; and the machine brings it up, re-addresses it and restores its default route. Nothing
is asserted until the wire is home and configured, because a claim that failed halfway would leave
the machine with no network for the ~180 programs sharing this boot. Counterfactual: with the two
`clear()`s in `Wire::take_in` removed, the three address/route claims are the red ones, answering
`-17` (`EEXIST`) where they print `0` - `pass=172 fail=1` with `t-ns-net` the one red; with them,
x86_64 is `pass=173 fail=0` with `mpos-kunit 32 passed, 0 failed` and exit 33, aarch64
`pass=172 fail=0`.

Not measured, and named rather than guessed: an `IFLA_NET_NS_FD` naming a descriptor that is *not*
a net namespace. This kernel answers `EINVAL`, the word `setns` measures for the same question, but
no probe of this one was run.

**Gate (2026-10-15).** x86_64 `=== SUMMARY pass=173 fail=0` with `mpos-kunit 32 passed, 0 failed`
and exit 33; aarch64 `=== SUMMARY pass=172 fail=0`, the same 32 cases. The program counts are
unchanged from the commit before - this adds a claim *inside* `t-ns-net`, not a program - and the
new claim prints `ok: ... refuses to hand its wire away, EOPNOTSUPP (got -95)` on both arms.
clippy reports 0 errors on `x86_64-unknown-none` (its warnings are the pre-existing ones), and
`cargo xtask depgraph` is within the ratchet: the change is inside `net`'s own `rtnl` module and
adds no `crate::` edge.

**Gate.** x86_64 `=== SUMMARY pass=173 fail=0 (173 built)` with the kernel's own
`mpos-kunit 32 passed, 0 failed` and exit 33; aarch64 `=== SUMMARY pass=172 fail=0`, the same 32
cases, exit 0. Each arm is one higher than the commit before this one, which is the new pin
(`t-ns-net`, twelve claims) joining both default sweeps. Host suites: 537 in the kernel, 7 in
`mpos-pidns`, 6 in `mpos-abi`, 121 in `xtask`; clippy reports 0 errors on `x86_64-unknown-none`,
`aarch64-unknown-none`, the host lib and the touched leaf crates; `cargo xtask depgraph` is within
the ratchet. Three references were added inside *existing* edges - three to `net -> sched` (the
namespace lookup, and the sender/receiver comparisons in the handover) and one to `net -> vfs`
(`vfs::ns::machine_net_id`) - so no edge is new and `ARCHITECTURE.md` §2's recorded weights stay
the snapshot they are dated (`2026-10-02`), which this change does not re-measure.

**And the drift the mapping itself caused, in the programs rather than the kernel.** Serving
`unshare`/`setns` for NET turned four programs red on the sweep, all for one reason: they used NET
as the *unserved* half of a mixed word, and the half is not unserved any more. `t-caps`'s
"`unshare(CLONE_NEWNET)` is `EOPNOTSUPP`" became `NEWIPC`, and `t-ns-mnt`, `t-ns-pid` and
`t-ns-uts` each moved their mixed word (`NEWNS|NET`, `NEWPID|NET`, `NEWUTS|NET`) to the same kind -
`t-ns-uts`'s *witness* claim moved with it, because a word that is now *served* really does unshare
and the witness stopped seeing the rename. That is the sixth time §6.3 records this drift, and it
is recorded rather than deleted for the reason the others are: the gate is what finds it, every
time, and a served set written into a test is the shape that makes it happen.

**TIME: the fifth rung, built on a shape that was measured whole first (2026-10-16).** The
paragraph that stood here was a measurement table rather than a plan, and the implementation
followed it row for row - which is worth recording, because "measure the shape before building
it" is the claim this whole section keeps testing. Where each piece went: `crates/timens` (the
object - two offsets and the freeze flag - and the offsets file's grammar, host-tested, seven
unit tests); `vfs::ns` (the kind, its flag, and the [`Entry`](../../src/vfs/ns.rs) that spells
the two slots apart); the task (`time_ns` and `time_ns_children`, read by the clock reader and
by the file respectively); `sched` (`unshare_time`, `setns_time`, `time_offsets`); procfs
(`/proc/<pid>/timens_offsets`, and `time`/`time_for_children` in the directory); four lines of
`clock_gettime`; and `t-ns-time`, 46 claims. Taken on this developer's Linux (7.2) with eleven
probes - the six `probe-timens{,1..6}.c` above plus `probe-timens{7,8,9,10}.c`,
`probe-timens-links.sh` and `probe-clone{3,}-timens.c`:

| question | Linux's answer |
|---|---|
| `unshare(CLONE_NEWTIME)` | `0`. The caller's `/proc/self/ns/time` is **unchanged** - it is `time_ns_for_children` that becomes new - and the caller's own clocks never move, whatever the offsets are set to afterwards |
| `clone(CLONE_NEWTIME)` | **`EINVAL`** by way of the *signal byte*: the flag is `0x80`, entirely inside the low byte `clone(2)` reads as the exit signal, so `CLONE_NEWTIME|SIGCHLD` arrives as `0x91` and 145 is not a signal. The control that settles it: `clone(0x80)` alone is `EINVAL` too, while `clone(SIGCHLD\|CLONE_NEWNET)` starts a child - and `clone3`, which carries the signal in its own field, *creates a task* for `CLONE_NEWTIME`. So the flag is not refused as a request, it is unexpressible through `clone(2)` |
| a child forked after the unshare | is in the new namespace (`time:[4026532873]` against the machine's `time:[4026531834]`) |
| the clocks that child sees | `CLOCK_MONOTONIC`, `CLOCK_MONOTONIC_RAW`, `CLOCK_MONOTONIC_COARSE` and `CLOCK_BOOTTIME` shifted; `CLOCK_REALTIME`, `CLOCK_TAI` and the two cputime clocks **not** |
| which offset moves which clock | `MONOTONIC`/`RAW`/`COARSE` read through the **monotonic** offset, `BOOTTIME` through its own - and the two are independent: a namespace whose monotonic clock has been moved *past* its boottime clock is accepted, there is no cross-check |
| `/proc/self/timens_offsets` of the unsharer | belongs to the *children's* namespace; reads two lines `monotonic <sec> <nsec>` / `boottime <sec> <nsec>`, 31 bytes each (`%-9s %11ld %9ld`) |
| the grammar | either line order; leading whitespace, tabs, a missing final newline and text after the two fields all accepted; a whole-seconds field may be **negative**; the nanosecond field may not (both `-1` and `1000000000` are `EINVAL`); an unknown clock name, a name with a suffix (`monotonicX`), a missing field, and an **empty write** are all `EINVAL`; the same line twice is accepted, last wins |
| a partial write | replaces the clock it names and **leaves the other where it was** (`monotonic 9 0` after setting both leaves `boottime` at 4) - so the parse starts from the current pair, not from zero |
| the first write | accepted; the second, **once a process has entered the namespace**, is `EACCES` - measured: a child forked first, then the write is refused; without the child, two writes in a row are both accepted and the later one wins |
| joining a frozen namespace | `setns(fd, CLONE_NEWTIME)` answers `0` **even when the namespace already has a process in it** - the freeze governs the offsets file, not who may come in |
| `timens_offsets` permissions | mode `0644 root:root`, so the mode is the gate rather than a capability (and a non-root writer is refused at the `open`) |
| `/proc/<pid>/ns` | ten names, `time` **and `time_for_children`** among them - the same for-children split `pid`/`pid_for_children` has, which is what the second row above is |

The model all of those fit is `nsproxy`'s two slots: `unshare` replaces `time_ns_for_children`
alone, a fork installs the child in `time_ns_for_children` (which is why the child's link differs
from its parent's), and the offsets are frozen by that first entry - which is the whole reason a
namespace's offsets have to be set *before* anything is created in it.

**Step 5, landed: TIME, and the pin's two counterfactuals.** `t-ns-time` walks the whole shape -
the machine's namespace frozen with two zero lines before anything is unshared, the caller's slot
unmoved and the children's moved, the offsets written and read back in a real kernel's 31-byte
shape, a forked child's three monotonic clocks and its boottime moved by exactly the offsets
while REALTIME, TAI and the cputime clock are not, the caller's own readings unmoved, the write
refused the instant the child exists, `clone`'s `EINVAL`, and a `setns` round trip that names a
*resident* rather than the keeper precisely because `unshare` does not move the keeper. 46 claims,
all green on x86_64 and aarch64.

Two counterfactuals, because there are two halves and each needs its own:

* **Drop the offset from `clock_gettime`** (the namespace is read and not applied) → `pass=173
  fail=1`, `t-ns-time` the one red, and exactly the five claims that depend on the offset being
  applied: the four moving clocks in the child and the child born after the join.
* **Give `unshare` one slot instead of two** (`time_ns = fresh` as well, so the caller lands in
  the very namespace it is about to write) → `pass=173 fail=1`, `t-ns-time` the one red, and the
  claims that go red are *different* ones: "the caller's own slot does not move", "the unshare and
  the write moved none of this process's own clocks" - whose failure prints the seven deltas,
  `[42008378261, 42008378280, 42008378294, 3600008378317, 8378341, 8378365, 8375335]`, i.e. the
  42 s and 3600 s offsets landing on the caller - and the four child clocks with the join.

The second of those is why the file has a claim the first counterfactual could not reach, and it
is the one worth keeping: the seven "this process's own clock never saw the offset" claims pass
for a *second* reason on a one-slot kernel (a fresh namespace's offsets are zero, and the offsets
were written to the other slot), so on their own they would be vacuous. `t-ns-time` therefore
takes this process's readings *across* the unshare and the write, which is where a one-slot
kernel shows itself, and reads the offsets back from the file afterwards as the interlock that
says the two slots really are two.

**One measurement that does not fit, recorded rather than reconciled.** Of the three probes that
forked a child after setting the offsets, two show the child offset by exactly what was written
(`+100000`/`+200000`, and `+7`/`+200000` after a second write) and one shows a child whose
`timens_offsets` file *reads* `monotonic 7` while its `CLOCK_MONOTONIC` is not offset at all. The
minimal shape (`probe-timens6.c`: unshare, one write, fork, both read their clocks) is
reproducible and is the one the table above comes from; the odd one out adds a pipe, a `usleep`
and a `readlink` of the two `ns/time` links before it reads its clocks, and neither removing the
second write nor a pause changed its answer. What is pinned here is the minimal shape; the
divergence is named and not explained.

**USER: the sixth rung's shape, measured whole and none of it built (2026-10-16).** Five probes
(`.workbuddy/probe-userns{,2,3,4,5}.c`), each run twice - once as root and once as an unprivileged
caller (`setpriv --reuid=65534 --clear-groups`) - because the two answer differently and the
difference is the kind:

| question | Linux's answer |
|---|---|
| `unshare(CLONE_NEWUSER)` as root | `0`, and the **caller itself moves** (`ns/user` 4026531837 -> 4026533165). The opposite of PID's and TIME's for-children rule, and the same shape NET has |
| ...as an **unprivileged** caller | `0` as well - this host allows unprivileged user namespaces, which is the property that makes the kind unlike the other seven |
| the caller's ids the instant it unshares | `getuid()` reads **65534** (the overflow id): the new namespace has an **empty map**, so the caller's own (outer) id has no inside name yet |
| the capability set inside | **full** - `CapEff=0x1ffffffffff`, `CAP_SYS_ADMIN` set - *before* any map is written, and an unprivileged creator gets it too (`CapEff=0` outside, full inside). This is the "root inside" property: making a user namespace is itself the grant |
| writing `uid_map` | `"0 65534 1"` (inside 0 <- the caller's own outer id, one id wide) is accepted **once**; a second write is `EPERM`; a line naming a **foreign** outer id is `EPERM`; a two-id line is `EPERM`. One line, its own id |
| `gid_map` | refused until `/proc/<pid>/setgroups` reads `deny` - the dance `newgidmap` performs; after `deny` the same one-line write is accepted |
| `/proc/<pid>/ns` | lists `user`, and there is **no `user_for_children`** (`ENOENT`) - a directory that grew a second name for PID and TIME grows none here |
| `clone(CLONE_NEWUSER\|SIGCHLD)` | `0` - a real door, because the bit (`0x10000000`) sits outside `CSIGNAL` where `CLONE_NEWTIME`'s does not; a second `unshare` from inside (nesting) is also `0` |
| `setns(fd, CLONE_NEWUSER)` | into the namespace the caller is **already in** answers `EINVAL`, not the no-op `0` every other kind gives; a namespace it lacks `CAP_SYS_ADMIN` in is unreachable - the `open` of `/proc/<pid>/ns/user` is refused `EACCES` first |
| what a mapping *is* | a **translation at the boundary**, not a rename. The kernel keeps the outer id: with `0 <- 65534` written, `/etc/hostname` (outer id 0) reads `uid=65534` inside (unmapped -> overflow), a file the caller owns outside reads `uid=0` inside, and a file made inside reads `uid=0` inside but `uid=65534` from outside |

The last row is the one that decides the implementation's size. An id is stored once, in the parent
namespace's numbering, and every *observation* of it - `getuid`/`geteuid`, `stat` of a file, the
owner reported by a directory read - is mapped through the observing namespace's table. A user
namespace is therefore not one more object beside the mount table and the clock offsets; it is a
translation applied wherever an id crosses a boundary.

**Why this rung was measured before it was built.** The measurement said the work is three pieces -
(a) a mapping object and its one-line write rules, (b) id translation at every reader, and (c) the
capability grant at creation - and (c) alone contradicts this kernel's present model, where
`Credentials::initial()` hands every process the full set, so "full inside a namespace you made" is
not yet distinguishable from "full because you are root". All three landed on 2026-10-16 - the grant
as "Step 6c" below - and this table is what that work started from.

**Step 6, landed: USER, and the two pieces left named.** `t-ns-user` walks the translation end to
end in 23 claims: the machine's map is the identity in a real kernel's exact bytes (the 33-byte line,
`%10u %10u %10u`), an id reads as itself under it so nothing that never unshares changes, there is no
`user_for_children` because the kind moves its caller, `unshare(CLONE_NEWUSER)` really does move it,
the caller then reads **65534** for uid, euid and gid because the new table is empty, writing
`uid_map` **once** with a line naming the id it is *stored* under makes it read 0, a second write is
`EPERM`, `setgroups` goes to `deny` and cannot go back on once the gid map exists, a child born here
reads the same mapped id and names the same namespace, `clone(CLONE_NEWUSER)` is `EOPNOTSUPP`
(the child-side creation Step 6e, below, is what closed that), and
`setns` onto the namespace the caller is already in is `EINVAL` rather than the no-op `0` every other
kind gives.

What landed, across the twelve touch points the ladder has used every rung: a leaf crate
(`crates/userns`) whose host tests carry the parts that need a caller without `CAP_SETUID` (the
one-line rule and the three `EPERM`s - a privilege level no task on this machine has, which is why
they are argued with there rather than in an in-guest pin); `Kind::User` with **no** `_for_children`
entry and **one** task field rather than two; `unshare`/`setns` that move the caller; three procfs
files (`uid_map`, `gid_map`, `setgroups`); and the four `getuid`-family readers, which now translate
the stored id through the task's namespace. `CLONE_NEWUSER` is also the one bit `unshare`'s capability
gate does not cover - measured, an unprivileged caller may make one - which is a change to the
verdict rather than to the handler.

Two counterfactuals, and the first is why the claims are written the way they are:

* **Drop the translation** (`shown_uid` returns the stored id, as this kernel behaved before) →
  `t-ns-user` the one red program, three of its 23 claims red: the caller's own overflow reading, "the
  caller reads the inside id the map gave it", and the child's. The first run of this counterfactual
  went red in **one** place, and the reason is the trap this ladder keeps meeting: this machine stores
  every task under id 0, so a map to 0 reads back 0 whether or not anything translated, and three
  claims were passing vacuously. The fix was to map the inside to **7** - a number no stored id here
  is - which is what the file now does. The three *gid* claims stay green in this run, which is
  evidence of its own: the two readers are wired separately.
* **Give the call the for-children shape** (`unshare_user` leaves the caller where it is, as PID's and
  TIME's rule does) → `t-ns-user` the one red program again, but with a **different** set of eleven
  claims: the machine-namespace claims stay green and everything that depends on the caller having
  moved - the link, the overflow reading, the empty map, the write, the gid pair, the child - goes red
  together.

**Step 6b, landed: the file-owner translation, and the two halves become one.** The
`getuid` family reads the ids a task stores *about itself*; `stat` reads the ids an
*inode* stores, and one rule governs both - an id is stored once, in the parent
namespace's numbering, and every observation of it is mapped through the observing
namespace's table. `.workbuddy/probe-userns6.c` measures the second half on this host,
run twice (root and `setpriv --reuid=1000`, which pick different stored ids for the map
line to name):

| question | Linux's answer |
|---|---|
| a file made *before* the unshare, read under the identity map | `st_uid=1000` - the stored id |
| the **same file**, read from inside a namespace mapped `7 <- 1000` | `st_uid=7 st_gid=7` - the inside id, and nothing about the file changed |
| a file made *inside* the namespace | `st_uid=7 st_gid=7` |
| `/etc/hostname` (stored root, which the unprivileged map `7 <- 1000` does not cover) | `st_uid=65534 st_gid=65534` - the overflow id |
| `statx` on the same file | `stx_uid=7 stx_gid=7`, agreeing with `stat` |
| an **outside** reader (a child that never unshared) | `st_uid=1000` - the stored id, from the same file the inside reads as 7 |

What landed is smaller than the plan predicted. The plan said threading the reader's
namespace into the inode layer made this "a change to `vfs` rather than a line"; the
measurement above says the boundary is the **ABI write**, not the inode. `Inode::owner()`
stays the *stored* id on purpose - every permission check compares it against the
caller's stored identity (`vfs::may_as`, the sticky-bit rule in `may_remove`), so a
`vfs` that translated its own answers would refuse the wrong callers - and the syscall
layer renders that id through `crate::sched::current_user_ns()` at each of the four
`stat`-family doors (`stat`/`fstat`/`fstatat`/`statx`, one helper, `observed_owner`).
The pin is four more claims in `t-ns-user`: a file created *before* the unshare reads
the stored id from the machine map, the same file from inside reads the inside id,
a file made inside reads it too, and `statx` agrees with `stat` - the inside id is
`7`, a number no stored id on this machine is, so the claim cannot pass by accident.
What remains of the rung is therefore the second half alone.

The counterfactual is the measured one this rung has used twice before: with the
translation dropped (the helper returning its argument) the gate runs `pass=174 fail=1`
and the one red program is `t-ns-user`, red in exactly **three** claims - the same file
from inside, the file made inside, and `statx` (`statx agrees (0, 0)`, the stored id) -
while the identity-map claim and the whole `getuid` half stay green. That the identity
claim survives is the point: a task that never unshares is not disturbed, which is what
the machine's identity map is for.

**Step 6c, landed: the capability grant, and "root inside" becomes distinguishable.** The
grant was the rung's other half, and the one the first measurement called expensive: every task
on this image starts with the full set (`Credentials::initial`), so "full inside a namespace you
made" was indistinguishable from "full because you are root" *until the caller drops first*.
`.workbuddy/probe-userns7.c` measures the grant on this host (7.2.9), run as root and
unprivileged, across all four doors into a namespace:

| question | Linux's answer |
|---|---|
| a caller that clears its sets with `capset(0,0,0)`, then `unshare(CLONE_NEWUSER)` | `CapEff=CapPrm=0x1ffffffffff` - full - the instant it is in the namespace; and a `unshare(CLONE_NEWNS)`, which needs `CAP_SYS_ADMIN`, then succeeds, so the grant is functional rather than a reporting artifact |
| a capability dropped from the **bounding** set first | back too - `cap_bset` is full again (`PR_CAPBSET_READ(21)` reads `0` before the unshare and `1` after) |
| an **inheritable** bit set before the unshare | gone after it (`CapInh` reads `0`) |
| `clone(CLONE_NEWUSER)` | the child is born with the same full set |
| `setns(fd, CLONE_NEWUSER)` into a **child** namespace, the joiner holding only `CAP_SYS_ADMIN` | full afterwards - the join grants too |
| the task's identity, `dumpable` and `KEEPCAPS` | untouched - the grant is the capability state and nothing else |

What landed is `Credentials::on_enter_user_ns` (the effective, permitted and bounding sets to
`CAP_FULL`, inheritable to `0`) applied at the two doors this kernel served then - `unshare_user`
and `setns_user`, beside the move so the two cannot drift apart (the third, a child's own birth,
is Step 6e below). The pin is eight more claims in
`t-ns-user` (35 now): a child clears its effective and permitted sets (read back empty) and a
bounding capability, unshares a namespace nested inside this one, and reads the full set and the
bounding capability back; and a second door joins a keeper's child namespace holding only
`CAP_SYS_ADMIN` and reads the full set back. The claim cannot pass by accident: the sets are read
*empty* going in, so "full coming out" is only possible if the unshare granted it.

The counterfactual: with the grant's two call sites removed (the method kept, so stage 1 stays
green) the gate runs `pass=174 fail=1` and the one red program is `t-ns-user`, red in exactly
**three** claims - "entering the namespace hands the maker the full effective and permitted
sets", "and a capability dropped from the bounding set is back", and "and joining hands the
caller the full effective and permitted sets" - while every door and setup claim stays green.
That the setup claims survive is the isolation: removing the grant breaks what the namespace
*hands out*, not whether the doors open.

**What the grant does not model, and the gap it named.** A capability is held *in* a
namespace and its descendants, and Linux's `capable(ns, cap)` asks it of a *target* namespace
that need not be the caller's; every check in this tree asked the caller's own namespace, so one
capability word was the whole of it. That is correct for every operation scoped to the caller's
own namespace and wrong for a check scoped to an *ancestor* or a sibling (a `setns` into a
namespace the caller does not own, a signal across a namespace boundary), and this paragraph
named a scoped `capable_in(ns, cap)` as the gap. **Both halves are closed** - the
*initial*-namespace question by §15.56 (the four gates whose Linux rule is `capable()`, `mknod`
first among them) and the *object's* by §15.60 (`docs/uapi/` §5.1 item 42, 2026-10-18: `vfs::ns`
records whose each namespace is *made* its owner - a user namespace its own, Linux's
`ns_common::user_ns` - and `setns` and `AF_PACKET` ask `userns::Caller::capable_in` about that
owner rather than about their own set). The rung's third named shape - a namespace descriptor that
did **not** pin the namespace it named, so a re-join through one was the no-op a copy would give -
was closed the same day: the descriptor is the namespace-shaped file `docs/uapi/` §5.1 item 44
describes (`vfs::ns::NsFile`; `.workbuddy/probe_ns_fd_stat.c` for the file's own shape,
`.workbuddy/probe_ns_fd_pin.c` for the pin). **The `mknod` device-node gate is the
first caller-visible consequence of it, measured** (2026-10-17, `docs/uapi/` §5.1 item 36): Linux's
`vfs_mknod` asks `capable(CAP_MKNOD)`, which is the *initial* namespace, so a caller that has entered
a user namespace - uid 0 in it, the whole set read back - still cannot make a device node on a
filesystem the parent namespace owns (`.workbuddy/probe_mknod_userns.sh`: `touch` in the same
directory answers `0` while `mknod c 1 3` answers `EPERM`, and the `dev 0` whiteout is served there
as it is outside), where this kernel's gate, asking the caller's own namespace, served it (§15.56
is where that became the initial-namespace question). The
earlier gates' scoping was not measured this way; this one was.

**Step 6d, landed: the socket surface, and the third reader of the same table.** The two surfaces
above read an id a task stores about *itself* (`getuid`) and an id an *inode* stores (`stat`); a
socket is the third - the ids a *peer* or a *message* carries, which `SO_PEERCRED` and
`SCM_CREDENTIALS` hand the reader as a `struct ucred`. `.workbuddy/probe-userns8.c` measures the
shape on this host (7.2.9), run as root and unprivileged, with a reader that reads once *before* it
unshares and again *after*, so capture-time and read-time translation are told apart:

| question | Linux's answer |
|---|---|
| `SO_PEERCRED` on a pair, read in the machine namespace | `uid=gid=<stored>` - the creator's ids, which for a pair are both ends' peer |
| the **same** read, after `unshare(CLONE_NEWUSER)` and a map `7 <- <stored>` | `uid=gid=7` - the stored id mapped through the reader's table |
| `SCM_CREDENTIALS` a read carries, read in the machine namespace | the *sender's* stored ids |
| the **same** message, read inside the namespace | `uid=gid=7` |

Two facts decide the implementation. The translation is at the **read**, not at capture - the
second read of an unchanged pair moves - so the boundary is again the ABI write and not the socket
object, exactly as Step 6b found for the inode. And the *pid* in the same `ucred` is a **pid**
namespace's question, not this one: it is left alone here and named below.

What landed is one helper, `observed_cred`, applied at the two doors a unix socket renders a
`ucred` through - `emit_unix_control`'s `SCM_CREDENTIALS` and the `SO_PEERCRED` `getsockopt` (both
the `UnixSock` and the anonymous `DuplexEnd` pair) - beside the four `stat` doors that use the same
rule. The pin is four more claims in `t-ns-user` (40 then; 46 after Step 6e): the pair reads the stored id under the
machine's identity map, the overflow id with the table still empty, and the inside `7` after the
maps exist, on both `SO_PEERCRED` and `SCM_CREDENTIALS` - the inside id is `7`, a number no stored
id on this machine is, so none of them can pass by accident.

The counterfactual: with the two call sites removed (the helper kept, so stage 1 stays green) the
gate runs `pass=174 fail=1` and the one red program is `t-ns-user`, red in exactly **three** claims
- the empty-map read, and the two after the map - while the identity-map claim stays green. That it
survives is the same isolation Step 6b's counterfactual showed: a task that never unshares is not
disturbed.

**Step 6e, landed: the child-side creation, and the door `clone` was refusing.** `unshare` and
`setns` move a *live* task into a namespace; `clone(CLONE_NEWUSER)` makes a child that is born
already inside one of its own - Linux's `create_user_ns` running for the new task. It was the one
door of this rung Linux serves and this kernel answered `EOPNOTSUPP` to, because a child born in a
fresh namespace is a different construction from a caller that moves. `.workbuddy/probe-userns9.c`
measures the shape on this host (7.2.9), run as root and unprivileged:

| question | Linux's answer |
|---|---|
| a child born by `clone(CLONE_NEWUSER)` | in a **fresh** namespace (`/proc/<child>/ns/user` differs from the parent's), its table **empty** |
| the child's own `getuid`/`getgid` before any map | `65534` - the overflow id, exactly as after `unshare` |
| the child's capability set at birth | the **full** set (`capeff=1ff:ffffffff`), the grant applied to the child too |
| who may write the child's map | the **parent** (in the namespace's parent) or the **child itself**, with the same one-line rule `7 <outer> 1` |

And `.workbuddy/probe-userns10.c` measures what the flag word does to the *other* kinds: a word
carrying `CLONE_NEWUSER` **lifts** the capability gate for the rest of it - `NEWUSER|NEWUTS`,
`|NEWNS`, `|NEWPID`, `|NEWNET`, `|NEWIPC` and `|NEWCGROUP` all succeed unprivileged, while each of
those kinds *alone* is `EPERM`. That is why `clone_namespace_verdict`'s gate is two rules: a word
without `CLONE_NEWUSER` is checked against the caller's own namespace, one with it against the
fresh namespace the child already holds the full set in.

What landed is a namespace minted in `spawn_copy` for the child when the flag is set (mirroring
`child_pid_ns`, and the only place a user namespace is created for a *new* task) and one grant line
- `Credentials::on_enter_user_ns` applied to the credentials the child just inherited, the same
call `unshare`'s and `setns`' doors make, so the three cannot drift. The verdict (`crates/abi`)
gained `CLONE_NEWUSER` in its served set and the two-rule gate above; `t-clone`'s refusal list lost
`USER` (its pin moved to `t-ns-user`, where the child's own view is the claim) and gained `CGROUP`,
so the refusal claim still names what is left. The pin is six more claims in `t-ns-user` (46 now):
the child is born in a fresh namespace, reads the overflow id with the table empty, the parent
writes its uid and gid maps with the one-line rule, the child then reads the inside `7`, and - in a
forked helper whose own sets are emptied first - a child born this way still holds the full set.
The helper exists because every task on this image starts with everything: without a parent that
holds nothing, "full at birth" is indistinguishable from "full because it inherited everything".
The drop cannot live in the main process, because a **root** caller that has emptied its sets may
not write a uid map at all (measured, `.workbuddy/probe-userns13.c` - the child's own write is
`EPERM` there while the unprivileged one succeeds), so the grant is pinned with no map write and
the maps by a caller that still holds what it takes.

The counterfactual: with the two `spawn_copy` sites disabled the gate runs `pass=174 fail=1` and
the one red program is `t-ns-user`, red in exactly **six** claims - the fresh-namespace read, the
overflow-id read, both map writes, the inside-id read, and the helper's full-set claim - while
`clone(CLONE_NEWUSER) creates a child` stays green (the verdict still serves the flag; only the
namespace is wrong) and every other program is untouched.

**Step 6f, landed: the socket's `ucred.pid`, and the pid namespace's own render.** The socket
surface's `uid`/`gid` are the *user* namespace's question (Step 6d); the same `struct ucred`'s
`pid` is the **pid** namespace's, and Linux answers it the same way - `SO_PEERCRED` fills it with
`pid_vnr(u->peer_pid)` and `SCM_CREDENTIALS` with the sender's number, both in the reader's
**active** pid namespace. `.workbuddy/probe-ucredpid.c` measures it on this host (7.2.9), run as
root (a pid namespace needs `CAP_SYS_ADMIN`):

| question | Linux's answer |
|---|---|
| a reader in the machine namespace, peer in the machine namespace | the peer's machine pid |
| a reader *inside* a fresh pid namespace, peer in the same namespace | the peer's number **there** (the namespace's `1`) |
| a reader inside the namespace, peer in the **ancestor** namespace | **0** - `pid_vnr` of a task it cannot see |
| the same, through `SCM_CREDENTIALS` | the sender's number in the reader's namespace |

What landed is one line in `observed_cred` (`net::socket`): the stored pid - a task's own slot,
which is what the machine namespace numbers it by - is rendered with
`crate::sched::pid_number_of`, the same translation `getppid`/`kill`/`/proc` use, and `0` when the
peer is not a member of the reader's namespace (`pid_vnr`'s own answer). Both socket doors go
through it, so `SO_PEERCRED` and `SCM_CREDENTIALS` cannot drift. A task that never unshares is
unaffected: the machine namespace numbers a task by its slot, so the stored number and the rendered
one are the same. The pin is two claims in `t-ns-pid` (the pid namespace's own program, where a
reader inside one already stands): a peer in the ancestor namespace reads `0`, and the child's own
socketpair reads the namespace's `1`.

The counterfactual: with the line reverted to `pid: c.pid` the gate runs `pass=174 fail=1` and
`t-ns-pid` is red in exactly those **two** claims - and nothing else moves, because every other
reader is in the machine namespace, where the two numbers are equal.

**Step 6g, landed: the netlink family's `ucred`, both halves.** Step 6f closed the *socket* family's
`ucred`; the same `struct ucred` is handed to a reader by the **netlink** door (`recvmsg_netlink`,
the device-event socket udev and `nlplug-findfs` listen on), and it renders differently enough to be
its own measurement. Three probes, all run as root (a pid namespace needs `CAP_SYS_ADMIN`, and the
uevent one needs `CAP_NET_ADMIN` to announce a device; `NETLINK_USERSOCK` stands in for the peer
path because the credential handling is the protocol-independent `netlink_sendmsg`/`netlink_recvmsg`
code):

| question | Linux's answer |
|---|---|
| a peer's `uid`/`gid`, read by a reader in a user namespace mapping the stored id to 7 | **7** - rendered through the *reader's* user namespace (`.workbuddy/probe-nlcred.c`) |
| the kernel's own zero `uid`/`gid`, read by that same reader | **7** - the zero is the *stored* id and is rendered too (`.workbuddy/probe-nluevent.c`) |
| a peer's `pid`, read by a reader in a *pid* namespace the sender is not in | the sender's number, **not** `0` - the reader's pid namespace does not move it |
| a sender inside a pid namespace, read by a peer in the same one | the sender's number **there** (`1`) (`.workbuddy/probe-nlpid.c`) |
| a socket bound with `nl_pid == 0` by a sender inside a pid namespace | the port id is that same `1` |

The two halves are stored at *send* and rendered at *read*, and only the ids are the reader's to
render: `scm_set_cred` stores `uid`/`gid` as kernel ids - mapped at the read with
`from_kuid_munged(current_user_ns(), …)` - but the `pid` as `pid_vnr(task_tgid(current))`, the
*sender's* number in the *sender's* active pid namespace, resolved once and never re-rendered. So a
listener that recognises kernel origin by `uid 0` is in the machine namespace, exactly as on Linux:
the zero is the stored id, and a reader whose table does not cover it is told `65534`.

What landed is `observed_event_cred` (`net::socket`): the queued `Cred`'s ids go through the
reader's user namespace, the pid is left alone. And `task_identity` now stores the sender's own
number (`pid_number_of(current_tgid())`, which is what `getpid` answers for it) rather than its
slot, for the `ucred` pid and the port id a zero-`nl_pid` bind prefers alike - Linux uses
`task_tgid_vnr` for both. A machine-namespace sender is unaffected: its slot *is* its number, and
the machine's identity map renders an id to itself.

The pin is sixteen more claims in `t-uevent`: a child in a user namespace mapping `0 -> 7` reads a
peer's ids and the kernel's own zero record as `7` (ten claims, the handshake and the announcement
around the four), and a child in a pid namespace reads its own `getpid`, the port id its bind is
given, and a peer's `ucred` as the same `1` (six).

The counterfactual: with `observed_event_cred` replaced by the raw `Cred` and `task_identity`'s pid
back to the slot, the gate runs `pass=174 fail=1` and `t-uevent` is red in exactly **four** claims -
the two render pairs and the two pid ones - while `getpid() == 1` stays green (that is the pid
namespace's own answer, not `task_identity`'s) and nothing else moves.

**What is named and not built.** Nothing of the `ucred` now: both families' doors render it, and
both halves of the netlink record are the measurement above - the last item the socket surface had
named. What stays named on this rung is the two namespace objects with no entry at all (IPC and
cgroup, which wait on the object they would isolate not existing). (The socket family's own `pid`
field landed on 2026-10-16 - "Step 6f"; the netlink family's, and the user-namespace render, on
2026-10-16 - "Step 6g".)


**And one trap in the measuring, worth its line**: a first draft of the raw-netlink-style probe
numbered `IFLA_NET_NS_PID` 27 where `linux/if_link.h` gives 19, and *every* request answered a
plain `0` - including the ones that should have failed. That is the same silence the netns-move
work was written against, arriving a second time from a different direction.

**Named gaps, each with what it waits on.** A namespace's stack is never freed - the
identity carries no reference count, where Linux frees one with the last `struct net` (and
`HOMES` is cleaned per socket, not per
namespace). **vsock's own stack stays machine-wide**, which is now a *measured* shape rather than a
gap (the paragraph below has the probe), and the unix family is split where Linux splits it - its
**abstract** names are keyed by the socket's namespace, its **path** names are not namespaced at
all, which is what makes the pin's cross-namespace handover possible. And a
`/proc/net` reader sees only its own namespace's sockets - a claim now, not a reading: `t-ns-net`
reads `/proc/net/tcp` inside the child's namespace and finds the very listener it just bound there,
and reads the same file in the machine's namespace and finds no row for that same port (2026-10-15).
The collector walks `HOMES` filtered by `current_net_ns()`, which is Linux's shape - a socket is a
row of one namespace's table and no other's. Measured on the host with the `unshare -Urn` door this
section already established (Python's `os.unshare`, no shell): a listener on `127.0.0.1:0` is a row
of the host namespace's `/proc/net/tcp` and absent from a fresh network namespace's, whose table is
empty -

```
port  = 37845 (0x93D5)
host  namespace has it: True
child namespace (unshare -Urn) has it: False
child namespace table rows (non-header): 0
```

The rest
of NET's design (one opaque `net_ns: Option<u64>` on `Task` rather than an `Arc` object, so that
`sched` does not name `net` and turn the existing `net -> sched` recall edge into a mutual pair; the
machine's own network namespace number minted in `vfs::ns`, which both layers may name; a fresh
namespace's stack holding `lo`, down and unaddressed, and no wire) is unchanged and recorded in
`.workbuddy/netns/PLAN.md` with its file/line inventory.

**The rung's second socket family, landed: a unix name is its namespace's, and only the abstract half of it is.**
The follow-up the gap list named last time ("keying `AF_UNIX` per namespace") came out of a probe
*smaller* than the plan and shaped differently, because the plan's premise was half wrong: Linux's
unix sockets are not a namespaced family. A **path** name is not namespaced at all - a socket bound
at a path answers that path from every namespace, both directions - and only an **abstract** name
belongs to one. The half the plan was wrong about is the half `t-ns-net`'s descriptor handover is
built on: it meets at a pathname *because* a pathname crosses.

**And it was measurable after all, which is the rung's own lesson.** For two steps this section
said no namespace semantic could be observed on this host, quoting `probe_netns.c`'s `EPERM` from
`unshare(CLONE_NEWNET)`. That `EPERM` is about the call *without a user namespace*, and the
correction is one flag - `unshare -Urn <probe>`, which creates the network namespace fine:

```
$ unshare -Urn true ; echo $?
0
```

So the socket-family questions below moved from reasoning to observation. The lesson is the one the
trap table already carries in another shape ("a recorded measurement is about the machine that
made it"): **a refused probe is a measurement of the probe, not of the question** - and a probe
that answered `EPERM` once is exactly the thing nobody re-runs.

`.workbuddy/probe_unix_netns.c` + `probe_unix_netns2.c` (the first leaves for a second namespace
and asks; the second keeps a handle on the first one, walks back, and asks from the other side):

```
namespace A = ino 4026533058
  A bound @mpos-probe-abs-one and /tmp/mpos-probe-p1
namespace B = ino 4026533227
  A's abstract name, from B:      connect(@mpos-probe-abs-one) -> Connection refused
  A's path socket, from B:        connect(/tmp/mpos-probe-p1) -> OK
  bind(@mpos-probe-abs-one) again in B -> OK (the name is per-namespace)
  B-only abstract name, from A:   connect(@mpos-probe-abs-two) -> Connection refused
  B's path socket, from A:        connect(/tmp/mpos-probe-p2) -> OK
```

The same name binding a second time is the strong half of the first line: two tables, not one table
with a filter. And `.workbuddy/probe_unix_inner.c` holds the control the sequence above cannot -
inside the fresh namespace a socket made *there* binds a name and another socket made there connects
and accepts (`OK`, `OK`, `OK`) - so the refusals are a boundary rather than a family that stopped
working there.

`.workbuddy/probe_unix_netns3.c` adds the question none of those can answer: **whose** namespace is
it, the calling task's or the socket's? They differ exactly when a socket outlives an `unshare`,
which is the case this rung has already paid for once (the IP stack's handle map):

```
A: made two sockets
A: a new socket is listening on @mpos-where-dial
B: standing in a second namespace
  bind(@mpos-where-bind) on a socket made in A, from B -> OK
    B: a socket made here connecting @mpos-where-bind -> Connection refused
    a socket made in A connecting @mpos-where-bind (from B) -> OK
  control: a socket made in B connecting @mpos-where-dial -> Connection refused
back in A
    A (back): a socket made here connecting @mpos-where-bind -> OK
```

A bind *from* B landed in A's table (a socket of B cannot see the name, a socket of A can), and a
socket of A dials A's name while standing in B. So it is the socket's, which is Linux's `sk_net`
and not a lookup at the call.

**What landed, then, is one field and one key.** `UnixSock` records the namespace it was **made
in** (`net` normalizes it, so a task that `setns`ed back into the machine's own spells the same
`None` a task that never left does - otherwise a socket made after the return would be in a
namespace nothing else can name), `by_name` is keyed by it, and `by_node` deliberately is not.
`accept` mints the accepted socket in the **listener's** namespace rather than the accepter's
(`alloc_in`, Linux's `unix_create1(sock_net(sk))`): the accepted socket is the server's other end,
and a name of its own belongs in the server's table. Everything else about the registry is
unchanged - the slot table is machine-wide, which is what `by_node` needs to be able to reach a
socket bound in another namespace at all.

**The pin.** `t-ns-net` gains a carried-child section: one `fork` carries two unix sockets (the
machine's) into a fresh namespace, and then - the carried dialer **still reaches** the machine's
abstract name; a socket made inside **cannot**; the very same name is **free to take again** there;
a name planted inside is planted and answered **inside**; the parent's namespace **cannot** see the
child's name; and the control, the machine's own name **still answers here**. The child is held
open on a second pipe while the parent asks, because an exited child answers `ECONNREFUSED` from a
dead socket - the same errno for the wrong reason, which would have made the claim pass without
measuring anything.

**And vsock is the item the measurement retired rather than confirmed.**
`.workbuddy/probe_vsock2.c` asks the same question of the second family:

```
namespace A: bind(cid=ANY,port=12345) = OK
  A (same namespace): connect(cid=1,port=12345) -> No such device
  A (same namespace): connect(cid=2,port=4321) -> No such device
namespace B:
  B: connect(cid=1,port=12345) -> No such device
  bind the same port here = Address already in use
  bind a fresh port here  = OK
```

Two things, and the control leg is what keeps them apart. The port table is **machine-global**: a
second namespace cannot take a port the first holds, while a fresh port binds fine - so
"keying vsock per namespace" would be a *divergence* from Linux rather than a fix, and the roadmap
line that asked for it is corrected rather than served. And the `ENODEV` a connect answers there is
a **host** property, not a namespace one: it is the same answer in the original namespace (the
first two lines), because this host has no vsock transport at all - the trap table's "a device class
needs a *host* backend" in another register. What is genuinely unsettled - whether a fresh
namespace has a transport - cannot be told apart from that here, so MPOS's machine-global vsock
stack is **left as it is, named rather than changed**: a task inside a namespace reaches the
machine's device, and whether Linux's shape would answer `ENODEV` instead needs a host with a
transport to ask.

**The pin's own defect, and the log is what found it.** The first gate run of this step came back
green - `pass=173 fail=0`, exit 0 - and the sweep's serial log had `t-ns-net` printing **every
claim twice**, one copy ending `FAILED`. The cause was the claim the *previous* commit added,
"`clone(CLONE_NEWNET)` while holding a socket is served": a clone child returns 0 and must leave,
and that claim's child did not, so it ran the whole program again. Its copy of each claim
interleaved with the original's - including the two that a process *inside* a fresh namespace gets
differently, which is why the doubled half failed on exactly those - and because the harness waits
for the program's own pid, whose exit status was 0, the doubling read as a pass and the `FAILED`
line was nobody's business until this step's log was read for the new claims. The child exits now
(`t-ns-net`), and both arms' serial logs are free of `FAIL` markers.

Two lessons, and the first one is the general shape: **a pin is evidence only if its body runs
once** - a doubled run doubles the denominator silently, and the half that fails is the half nobody
reads. The second is narrower and worth keeping next to the gate protocol: **a green summary line is
a claim about what the harness read, not about what the program printed**. `=== SUMMARY pass=173
fail=0` was true; "`t-ns-net` passed" was also true, of the copy that exited 0. Reading the log for
the *new* claims is what separated them, which is the same discipline as "never report the gate from
a stale run" pointed at a single program.

**Gate.** x86_64 `=== SUMMARY pass=173 fail=0 (173 built)` with the kernel's own
`mpos-kunit 32 passed, 0 failed` and exit 33; aarch64 `=== SUMMARY pass=172 fail=0`, the same 32
cases, exit 0. Neither program count moved: the new claims are six more inside `t-ns-net`, which
both default sweeps already ran, and each arm's serial log carries them once with no `FAIL` line
anywhere in it. Stage 1: the kernel library's 537 and every leaf crate the gate builds (`mpos-abi`
6, `mpos-pidns` 7, `mpos-xtask` 121 among them), all green. Clippy reports nothing on
`x86_64-unknown-none`, `aarch64-unknown-none` and the host lib; `cargo xtask depgraph` is within the
ratchet, and the change adds no edge to it - `unix.rs` reaches `crate::sched::current_net_ns` along
the recorded `net -> sched`, and the namespace spelling it shares with the IP registry
(`net::normalize`) is inside `net` itself.

**The task a pid namespace is spoken for by, landed: its members cannot end it, and its death is the namespace's.**
The rung's remaining named gap was "a namespace's pid-1 death does not zap the namespace", and the
step that closes it turns out to be two rules about one task - the number 1 - with a third and a
fourth rule about *making* a namespace that the same probes turned up and this kernel does **not**
have yet. All of it measured: `.workbuddy/probe_pid1.c`, `probe_pid1_handler.c`,
`probe_pid1_sigsegv.c`, `probe_pid1_alarm.c`, `probe_pid1_fault.c`, `probe_pid1_zap.c`,
`probe_pid1_nested.c`, `probe_fork_after_init.c`, `probe_pid_ns_again.c`. Two lessons repeat
already-recorded ones: this host *can* create namespaces (`unshare -Ur`, §6.3's own correction), and
a probe that answers a question unexpectedly is worth a second probe - three of these exist only
because the previous one's answer was not the one the source suggested.

**The signal rule, and the four classes it sorts raises into.** A namespace's init ignores the
signals it would take *by default*; exactly three things reach it, and everything else is dropped:

```
probe_pid1_sigsegv.c    a sibling's kill(1, SIGSEGV) -> 0, and the init is still alive (also 7, 15)
probe_pid1_alarm.c      an alarm(1) the init armed itself, SIG_DFL -> discarded, it survives
probe_pid1_handler.c    SIGTERM with a handler installed -> the handler ran, the call answered 0
probe_pid1_fault.c      a null-pointer fault -> signalled=1 sig=11: the init died of it
probe_pid1_blocked.c    a *blocked* SIGTERM, waited for with sigtimedwait -> received
probe_pid1.c            from the ancestor namespace: SIGTERM -> ignored, SIGKILL -> died
```

So: a signal with a **handler** crosses; a **forced** SIGKILL/SIGSTOP from an **ancestor** namespace
crosses (and no other default signal from there - the ancestor's SIGTERM was ignored too); a
**synchronous fault** crosses, and is not judged by the rule at all, because discarding it would
return the task to the faulting instruction for ever; and a **blocked** signal is not judged either,
because Linux checks the blocked masks first ("blocked signals are never ignored") - the last of
those was found by re-reading the source after the first implementation was written, and it is the
clause that keeps `sigwait`/`sigtimedwait`/`signalfd` working for an init that wants what its
members send *as data*. A kernel raise *instead of* an event does **not** cross - the `alarm`
measurement is what separates "the kernel" from "forced", and it is why the implementation has three
raise sites rather than two: `queue_signal` (a kernel raise, not forced), `send_signal_info` (a
task's `kill`, forced only when the sender is in an ancestor namespace), and `force_signal` (the
kernel's own forced kill, added for the zap). And the discarded signal still answers its sender
**success** - `kill(1, SIGKILL)` returns 0 and nothing happens - because Linux's check is on
*delivery*, not on permission: the rule exists so that a member cannot take its init down by
accident, not so that it can tell that it may not.

The kernel paths that end a task already had to be taken out of the rule's reach, and finding that
is what the fourth class is: the **strict-seccomp kill and a filter's `RET_KILL_*`** were queued
signals (`send_signal(current(), SIGKILL)`/`SIGSYS`), which an init would have had discarded - an
init running a strict filter would have gone on with the blocked call it was supposed to die for.
Both now `exit(-(sig))` directly, which is Linux's `do_exit(SIGKILL)`/`do_exit(SIGSYS)` and exactly
what their own comments already said they were matching. The fault path needed nothing: `idt.rs`
already ends the task itself when the disposition is default, which is why the measured fault
behaviour is unchanged by this rule rather than a special case in it. One corner is named rather
than settled: `RET_TRAP`'s SIGSYS is still a *queued* signal, so an init with `SIG_DFL` for SIGSYS
survives it - whether Linux's `force_sig_info` there reaches past the protection is the one thing
these probes cannot answer without a seccomp program running as an init, which nothing writes.

**The zap, and the membership that is not the obvious one.** `probe_pid1.c` and `probe_pid1_nested.c`
together decide the shape, and each corrected a reading of the other:

* a task **in** the namespace dies with its init ✓, and so does a task in a namespace **nested
  beneath it** - the naive test (`task->active_pid_ns == ns`) would have stopped at the first level,
  so the test here is "numbered in it or below it" (`PidNs::covers`);
* the task that **unshared** the namespace - whose *children* go into it, which is the second clause
  `zap_pid_ns_processes` is usually described with - **survives**: `probe_pid1_zap.c` has it reap the
  init and reach the end of `main`. So that clause is deliberately *not* implemented, and the
  obvious "kill everything that could put a task in there" reading is a machine that kills a shell
  for its child's exit.

Fired for the **last thread** of the group (Linux's `group_dead`) and never for the machine's own
init, whose exit is the `INIT_EXITED` halt above it. **Named divergence**: Linux loops until the
namespace is empty, because a task killed in the middle of a `fork` can leave a child behind; this
is one pass over the table.

**The membership is pinned two levels deep, which is what closes the "nested at one level" gap's
hard half.** `t-ns-pid`'s init section raises a namespace, an init in it, and - from that init - a
namespace *nested* under it with an init of its own, then kills the outer init and checks that the
nested one died too. That claim is the one the naive membership test fails (the nested task's own
namespace is not the one whose init died) *and* the one a non-forced kill fails (it is the init of
its namespace, so its own members could never have reached it) - so it pins `covers` and the
forcedness of `force_signal` at once. The section's nine words are: the namespace was made; the
sibling's SIGKILL, SIGTERM and SIGUSR1 all answered 0; the init was alive after them; the signal it
had a handler for ran; the one it had *blocked* came back through `sigtimedwait`; the ancestor's
SIGKILL landed; the namespace's other task died; and the task in the namespace nested beneath it died
too. Two of those are the ones that fail against the implementation as it was first written (the
nested membership, and the blocked clause), which is the counterfactual worth having. The gap was
narrower, and its numbering half is closed: the numbering chain itself (one task numbered at every
level, `/proc` at depth, `NSpid`) was pinned only one level deep here and is two now (2026-10-15,
the chain paragraph above). What stays open is one clause of Linux's `sig_ignored` -
named rather than implemented - a **traced** task does not ignore signals either (`return
!t->ptrace`), so a traced init may receive one where this kernel drops it; the probe for that is not
written, and the pin says nothing about it.

**And the two rules about *making* a namespace, landed on the step that found them.**
`unshare(CLONE_NEWPID)` and `clone(CLONE_NEWPID)` share one precondition, and it is
`copy_pid_ns`'s: the caller's **own** pid namespace must be the one its children would go to. That
holds for a task that never unshared and for one *created in* the namespace it is in - a child of an
`unshare`, whose own numbers and whose children's namespace are the same one - and stops holding the
moment the call succeeds, because then the two come from different namespaces. So a process makes a
pid namespace **once**:

```
probe_pid_ns_again.c     unshare(CLONE_NEWPID) twice in one process -> the second is EINVAL
                         clone(CLONE_NEWPID) after one               -> EINVAL
probe_fork_after_init.c  fork() when the children's namespace's init has exited -> ENOMEM
```

The third face is not a separate rule but the same one seen from the other side, and it needed one
new fact: a pid namespace is **sealed** by its init's death. `PidNs::seal`/`is_sealed` is that fact,
set at the moment the zap empties the namespace - not at the init's *reap*, so no window exists in
which a task could be created in a namespace that is already gone. Every creation path asks it
before it allocates anything (`spawn_copy` for `fork`/`clone`/`vfork`, and `spawn_thread` for a
thread, since a thread is allocated its pid in the same chain), which is Linux's order: the whole
new `nsproxy` is built and swapped in at once there, so a refusal that arrived half-way through
would leave a state no caller asked for. `unshare` asks the same precondition **first** among its
bits, for the same reason - a word carrying `CLONE_NEWPID|CLONE_NEWNS` is refused whole rather than
moving the mount table and then failing.

What it cost in the pin is the honest part of it: `t-ns-pid` had *two* sections that forked after the
first namespace's init had exited - the capability gate's lowered child, and the init section's own
task - and both would now be `ENOMEM`. The capability gate moved above the `unshare` it never
depended on, and the init section was already forked at the top of the program for the same reason.
That is not the test bending to the kernel: a program that respects the rule has to be shaped that
way, so the pin is now a program of that shape - which is what made it worth writing the three
claims into it as well:

```
ok:  a second unshare(CLONE_NEWPID) in one process is EINVAL
ok:  and clone(CLONE_NEWPID) after one is EINVAL too
ok:  while a fork into the namespace whose init has exited is ENOMEM
```

**Gate.** x86_64 `=== SUMMARY pass=173 fail=0 (173 built)` with the kernel's own
`mpos-kunit 32 passed, 0 failed` and exit 33; aarch64 `=== SUMMARY pass=172 fail=0`, the same 32
cases, exit 0. Neither program count moved - the new claims are three more inside `t-ns-pid`, whose
sections were reordered rather than added to. Stage 1: the kernel library's 537 and every leaf crate
the gate builds (`mpos-pidns` is 10 now, up from 9: the seal has its own test), all green; clippy
reports nothing on `x86_64-unknown-none`, `aarch64-unknown-none` and the host lib; `cargo xtask
depgraph` is within the ratchet, and no edge is added - `pid.rs`'s new helper is reached by `sched`'s
own modules and by the syscall layer along edges that already exist.

**`/proc/<pid>/status`'s `NS*` fields, landed: the chain, not the reader's number.** The last
reader-relative answer `/proc` gave, and a measurement that corrects the *shape* rather than a value:

```
one namespace in:  NSpid:	43877	2
two levels in:     NSpid:	43878	3	1
a machine's task:  NSpid:	101
```

The field is the task "in each of the PID namespaces of which it is a member", leftmost "with respect
to the PID namespace of the process that mounted this procfs, followed by the value in successively
nested inner namespaces" - so it is a **chain**, and the same line for every reader. The two answers
coincide only when the reader *is* the machine (which is why a cut that printed the reader's number
looked right), so the case that tells them apart is a reader inside the namespace - and that is what
the pin holds: the namespace's own init reading its own status sees two numbers ending in 1, the
machine reads the same line *for it* with its own number first, and a task of the machine's namespace
shows the one number it answers to.

`PidNs::numbers` is the chain (outermost first; the root level is every task's, so a name always has
at least one entry and nothing has to be invented for a level that never registered it - a property
the crate's own test found by expecting the opposite). `NStgid`/`NSpid`/`NSpgid`/`NSsid` all read it
through `sched::pid_numbers_of`, and the last of those replaces a hard `0`: the session id is carried
in the chain too, and the `session` field a task already had is what feeds it. `Pid:`, `Tgid:` and
`PPid:` are deliberately *not* changed - those are the reader's numbers, which is what the short form
of the same file answers.

**And the gate caught the one mistake in it.** The new helper compiled on bare metal and *not* on the
host: `sched` carries stand-ins for the two translations its other modules need
(`pid_number_of`, `resolve_pid`), and a third one needed the same. That is `ARCHITECTURE.md` §3's "a
module has a host body and a machine body - **both move, or it does not build**" arriving as a *gate
failure* rather than as a paragraph, and the fix is the stand-in beside the other two
(`pid_numbers_of` answers the single number a machine with no namespaces prints).

**Gate.** x86_64 `=== SUMMARY pass=173 fail=0 (173 built)` with the kernel's own
`mpos-kunit 32 passed, 0 failed` and exit 33; aarch64 `=== SUMMARY pass=172 fail=0`, the same 32
cases, exit 0. Neither program count moved: the new claims are three more inside `t-ns-pid`'s init
section, whose report is twelve words now. Stage 1: 537 in the kernel library and every leaf crate
the gate builds, `mpos-pidns` at 11 (the chain has its own test); clippy reports nothing on
`x86_64-unknown-none`, `aarch64-unknown-none` and the host lib; `cargo xtask depgraph` is within the
ratchet, and no edge is added - `vfs` already names `sched`.

**The chain, pinned at the depth it claims.** The paragraphs above pin the *shape* of `NSpid` with a
task **one** namespace in - two numbers - and left "pinned at one level only" in the gap list. The
depth-2 case is the measurement at the top of this section (`NSpid:\t43878\t3\t1` for a task two
namespaces in), and it is closed (2026-10-15): `t-ns-pid`'s init section already stands a task two
namespaces in - the nested init the zap takes with its parent - and that task now reads its **own**
chain there, three numbers with its own namespace's `1` last, while the section reads the same task
back from the machine by the chain's first number and expects the identical line. "One number per
namespace the task is a member of" is what the field means; this is that claim at three levels
rather than two, and the two reads agreeing is the half that rules out a reader's-number cut padded
out to three. No kernel source changed - the chain was already built to depth - so this is the pin
the gap list asked for, not a fix.

**Gate.** x86_64 `=== SUMMARY pass=173 fail=0 (173 built)` with the kernel's own `mpos-kunit 32
passed, 0 failed` and exit 33; aarch64 `=== SUMMARY pass=172 fail=0`, the same 32 cases, exit 0.
Neither program count moved: the new claims are two more inside `t-ns-pid`'s init section, whose
report is seventeen words now. Clippy and `cargo xtask depgraph` are where they were (no `src/`
change), and `cargo fmt --check` is red only on the pre-existing `EOPNOTSUPP` note line.

**`F_GETLK`'s `l_pid`, landed: the number the *reader* can act on.** The last pid-crossing call
that still handed back the machine's slot, and it is the one call whose whole *purpose* is to name a
process - a program asks `F_GETLK` to learn which process is in its way. Linux's
`locks_translate_pid` asks `pid_nr_ns(holder, reader's namespace)` for it, and all three answers were
measured on the host (`.workbuddy/probe_getlk.c`):

```
a task inside the namespace reading the init's lock:  l_pid = 1
a lock held *outside* the namespace, read from inside: l_pid = 0
a reader outside reading the init's lock:              l_pid = 55831 (the init's number out there)
```

The middle line is the one a "translate it, or keep the machine's number" reading gets wrong: a
reader whose namespace has no number for the holder at all is handed **0**, because that is what
`pid_nr_ns` answers for a task it cannot see - so the field is not a fallback but a real answer, and
the implementation is one function in the fcntl handler (`holder_pid_in`) over the task id the lock
table already stored. The pin's init section holds a lock from each side of the namespace and asks
from both, which is three claims: names the holder as that namespace does, names nobody for the lock
held outside, and hands the outside reader the holder's number out there.

One neighbour named rather than fixed: a lock's owner is a task id whose liveness `locks.rs` already
checks, so what the translation can be asked about is always a live task. The other neighbour, the
`F_OFD_*` family, has since been served - its `l_pid` is `-1`, the one value this function must never
be asked to translate - and its measurement and code are `docs/notes/` §3.2.

**Gate.** x86_64 `=== SUMMARY pass=173 fail=0 (173 built)` with the kernel's own
`mpos-kunit 32 passed, 0 failed` and exit 33; aarch64 `=== SUMMARY pass=172 fail=0`, the same 32
cases, exit 0. Neither program count moved - the new claims are three more inside `t-ns-pid`'s init
section, whose report is fifteen words now. Clippy reports nothing on either bare-metal target or the
host lib, and `cargo xtask depgraph` is within the ratchet with no edge added.

**NET: one `Stack`, and the device is the reason the list is short.** `net::NET`
is one interface pair (`eth0` + `lo`), one address table, one route table and one
socket registry. The record is already shaped like something a second copy would
work (`Stack` owns its fabrics, and the two-fabric split landed for the
loopback), so a net namespace is a second instance with the socket registry
keyed to it - but the *wire* is one virtio NIC with one MAC and one queue, and a
namespace cannot take it from the machine. So the first target is a namespace
whose `lo` is its own and which has no wire, which is what a container's private
network is, and what makes a socket in it unreachable from the parent. Moving a
physical device between namespaces is a later question and is refused until
there is a place to move it to.

**`setns`, and how a namespace is named.** Linux reaches a namespace through a
descriptor opened on `/proc/<pid>/ns/<kind>`, and the design below follows from
the table: each namespace object becomes an inode whose open descriptor *is* the
namespace (so two descriptors on one `/proc/<pid>/ns/uts` name one object, with
one `st_ino`), `setns` checks the descriptor's type against `nstype` (a mismatch
is `EINVAL`, Linux's answer), and `NS_GET_USERNS`/`NS_GET_PARENT` remain out of
scope even now that a user namespace exists (2026-10-16, "Step 6" below): the two
requests are a pair of `ioctl`s this kernel has not built, which is a separate
question from the kind existing. **When this paragraph was written the call had no
door at all** - `setns` answered `EINVAL` for an open descriptor and `EBADF` for a
closed number, all it could learn - and the descriptor only became the inode this
paragraph describes on 2026-10-18, when the pinning and the file's own shape landed
(`docs/uapi/` §5.1 item 44; `st_ino` is the namespace's own number, which is that
sentence's "one `st_ino`", measured).

**Two decisions about the call itself, rather than about a namespace.**

* A flags word naming a namespace this kernel does not serve is refused **whole**
  - `EOPNOTSUPP`, and nothing is unshared. Linux's `ksys_unshare` is not atomic:
  by the time a later flag fails, an earlier one's unshare has already happened.
  This is a deliberate divergence, and it is the direction §4.4 prefers - a
  caller that asked for two namespaces and got an error learns that *neither*
  happened, instead of being left halfway with nothing saying so. It also means
  the served set can grow without a caller ever observing a half-built word.
* The flag vocabulary stays Linux's, which is now true in the handler and pinned
  by `t-posix` (see below). When a namespace is served, its own flag stops
  answering `EOPNOTSUPP` and starts answering Linux's `EPERM` for an
  unprivileged caller - the same `CAP_SYS_ADMIN` gate Linux puts on the seven
  privileged kinds.

**Verification, one namespace at a time, before the next one starts.** Each of
the four lands with its own `t-ns-<kind>` program, and the shape is the same for
all four - *the child's view differs and the parent's does not*, because the
parent's half is the assertion a "namespace" that is only a shared global
fails:

| program | the unshared side | the parent's side |
|---|---|---|
| `t-ns-uts` | a child that unshares reports `inside`/`inside.test` through `uname` **and** both sysctls, and a task that unshares keeps the names it had | the parent that did not unshare still reports the old names, its own three doors still agree, and a child forked *before* a refused mixed word keeps sharing the parent's namespace |
| `t-ns-mnt` | `mount` a tmpfs at a directory, then read it back | the parent's `/proc/mounts` does not carry it, and the directory is empty there |

**`t-ns-mnt` has landed, and it says more than the row above planned.** The step it pins is MNT
step 1: `MountTable` became a *value* (the machine's own lives behind `machine_table()`), every
task carries an `Arc` to one by reference (`None` = the machine's, the same shape as the `uts`
slot beside it), fork and threads inherit that reference, and `unshare(CLONE_NEWNS)` moves only
the caller to `MountTable::copy` — Linux's `copy_mnt_ns`, counters re-minted because a mount id
names a mount within one table. The test's claims are the plan's pair plus two the writing found:
the copy keeps what was copied (a child that unshared still resolves its root — an *empty*
namespace would make the call a way to lose the system), and a umount inside peels only the copy.
The refusals pinned too, because they are what keeps the served answer honest: a word mixing MNT
with an unserved kind is refused whole and mounts nothing, and `clone(CLONE_NEWNS)` answers
`EOPNOTSUPP` outright — the thread group holds no proxy of its own yet, so handing a *child* its
own table is the next step of the ladder, named rather than half-served.

Step 2 landed on top of it.** `clone(CLONE_NEWNS)` is served — and the reason it needed no
nsproxy turned out to be structural: the child *is* a new thread group, so the fresh `Arc` at
creation is the whole mechanism (what stayed parked with a shared-group handle was CLONE_FS-style
*separation*, where siblings must split — since landed, "Step 3" below). `clone(CLONE_NEWUTS)` came with it, same shape, Linux's
`copy_utsname` on exactly that flag; `t-clone` gained the pair of claims (the child renames only
its copy; the parent keeps its own name read back through raw `uname`) and the refusal probes for
PID/NET/IPC (`EOPNOTSUPP`, privilege-before-kind in both verdict functions — Linux's order, and
the host-side test expectations moved with the measurement rather than the code). The one thing
that moved house: `clone_namespace_verdict` lives in the **`abi` leaf**, because its reader is
`sched::clone` and a `sched → syscall` reference is precisely the V-* edge the depgraph ratchet
keeps at zero — caught here the first time it was written, fixed by moving the rule down, not by
raising the constant. The resolution plumbing
this needed is one rule in one place: `process_fs_context()` answers `(table, root)` for the
caller (both `None` off any task — boot code, host tests), and every free function in
`src/vfs/mount.rs` routes through it, which is also what makes a chrooted task cross mounts in
its *own* namespace's table instead of the machine's.

**What step 2's first gate run measured, and the four things it cost.** The paragraph above was
written from the design; the run that followed it was red, and every failure was a real gap between
two halves of the same seam rather than a claim that had aged:

* **`clone(CLONE_NEW*)` never reached the verdict.** `validate_clone_flags`' `KNOWN` mask carried no
  namespace bit, so the syscall layer refused every namespace flag with `EINVAL` *before*
  `sched::clone` could ask `clone_namespace_verdict` — which left the verdict as code no word could
  reach, and it had no test of its own either. The shape was wrong besides: `EINVAL` says "not a
  flag" where the truth is "present, cannot do that". Fixed where it belongs — `CLONE_NAMESPACES`
  joins the mask, so the word arrives with its bit in it (`CLONE_NEWTIME` is still refused, by the
  signal-byte rule: `clone` has no such flag, `unshare` does) — and the leaf gained the verdict's own
  pin. `t-clone`'s "unknown bit" claim moved to 2^44, because the bit it had named (`1 << 30`) is
  `CLONE_NEWNET`, which is *known* now and refused by name; measured for that claim's sake on the
  host: Linux has no unknown-bit check at all (2^32, 2^44 and `CLONE_PTRACE` each start a child,
  7.2.9), so mpos's `EINVAL` is a designed divergence and both the program and `t-clone`'s header say
  so. Two claims were wrong for a second, independent reason: `t-clone`'s namespace table and
  `t-ns-mnt`'s clone probe both named `CLONE_NEWPID` as `0x0008_0000`, which is `CLONE_SETTLS` — the
  word started a child with a garbage thread pointer (the guest reported the child's page fault at
  `0xfffffffffffffff8`) instead of refusing one. Both words now come from `libc::CLONE_*`.
* **Two more programs had the served set written down as a constant.** `t-caps` asserted
  `unshare(CLONE_NEWNS) == EOPNOTSUPP` "even lowered" and `t-posix` asserted the same at the top of its
  namespace block; both were true while MNT was unbuilt and false the moment it landed (a served kind
  is *gated*, so a lowered caller now hears Linux's `EPERM`). The refused-not-gated claim moved to a
  kind that is still refused whole — first `CLONE_NEWTIME`, and when TIME landed in turn, `CLONE_NEWPID`
  *once the capability is back* in `t-caps` — and each pins the served answer beside it. `t-posix`'s row
  is a *served* answer now: the same drift this section keeps recording, and the same fix, the
  expectation moving with the measurement rather than the other way round. `t-ns-uts` had the same shape one
  level down: its mixed word was `NEWUTS | NEWNS`, which became a legal pair, so the refusal moved to
  `NEWUTS | NEWNET` — what that file's own header had described all along ("the served namespace with
  an unserved one").
* **`/proc/mounts` and `/proc/self/mountinfo` still rendered the machine's table.** The step moved the
  table onto the task and routed the *resolution* free functions through `process_fs_context`, but the
  two renderers kept `mount::global()` — so every task read one list and `t-ns-mnt`'s "a child's umount
  peels its own copy" looked at the machine's mounts from inside a copy, where the peel cannot be seen.
  They call the caller-routed `mount::mount_records()` now. A sibling of the same oversight was **named rather than fixed** here, and asked for its own pin before its own fix: `linkat`'s descriptor-side `EXDEV` check still asked
  `mount::global().fs_serving_inode` (`src/syscall/linux/fs/file.rs`), so a task
  with its own mounts could be refused a link Linux would have made — an exotic
  path, and the remaining half of "every free function routes through the process
  context". It is fixed below, with the family it turned out to belong to.
* **`uname` answered four empty fields on an armed boot.** §10.5's disk-counter protocol `return`ed
  from the middle of the handler after writing `release` and `version`, leaving sysname, nodename,
  machine and domainname as the caller's zeros — and the sweep arms that protocol whenever it stages
  `t-diskbench`, which the default group always does. `uname` is a door `t-uname`, `t-ns-uts` and
  `t-clone` all read through, so eight claims across three programs went red for one early `return`;
  the protocol now *overrides the two fields it owns* and leaves the rest written.

Gates after the fixes: host 526, `mpos-abi` 6, `mpos-xtask` 116, `fmt --check` clean; the sweep
x86_64 `pass=166 fail=0 (166 built)` and aarch64 `pass=175 fail=0` — both machines, both arms, with
`t-ns-mnt`, `t-clone`, `t-caps`, `t-posix` and `t-ns-uts` green on each.
| `t-ns-pid` | the first child is pid 1 to itself (`getpid`), and `/proc/1` names it | the real number is unchanged, and `/proc/<that number>` still names it |
| `t-ns-net` | a socket bound to a port is reachable from inside | the same port is free in the parent's namespace, and a connect to it is refused |

Each also pins its refusals: an unprivileged caller (`setresuid`-dropped) gets
`EPERM` for a served namespace, an unserved one still answers `EOPNOTSUPP`, and
a flags word mixing the two is refused whole.

**Order, and why it is that one.** UTS, then MNT, then PID, then NET: each step
is independently verifiable (its `t-ns-*` above), and none of them needs the next
one to be true. UTS is first because it is the cheapest complete example of the
shape - a per-task object, an inheritance rule, a copy on `unshare`, three doors
- and because it exercises the "refused whole" rule with nothing at stake. MNT
is second because it is the one containers actually need and because its
prerequisite (a per-process root) is a divergence worth fixing on its own. PID
is third because it is the most expensive and the least optional for anything
that runs an init inside; NET fourth because a namespace with `lo` alone is
already useful and the wire is a separate question; TIME fifth because its whole
shape was measured before any of it was built, which made it the cheapest of the
four left. USER, IPC and CGROUP stay refused, each with the reason in the table
above, and each is worth revisiting only when the thing it would isolate exists
(§8.2's seccomp and §7.4's compositor work are both more urgent).

**Step 1, landed: UTS.** The shape the paragraph above predicted was the shape it
took, which is worth recording because it is the claim this section was written
to test: a `Uts` object holding the two names (`src/uts.rs`, not target-gated,
so the copy rule is host-tested), a `Task::uts: Arc<Uts>` field, `fork`/`clone`/
`spawn_thread`/`try_spawn_user` inheriting the `Arc` by reference, and
`unshare(CLONE_NEWUTS)` replacing the caller's with `Uts::copied()` - Linux's
`copy_utsname`, so the new namespace *starts* from the old one's names and only
then diverges.

**The three doors were the whole of the cost, and one of them was already
broken.** `sethostname`/`setdomainname` wrote a pair of process-wide `String`s
in `vfs::procfs`; `uname`'s `nodename` read them; and `/proc/sys/kernel/hostname`
read them. Making the home *the caller's* was the edit. What the edit found is
that the domain half of the pair had never been wired: `uname` reported the
literal `(none)` and `/proc/sys/kernel/domainname` hardcoded the same string on
its read *and dropped every write*, so `setdomainname(2)` had no observable
effect through any door at all. Both now read the caller's namespace, which is
what makes the three agree about *which* namespace they are answering for - the
property `t-ns-uts` holds with one `Doors::agree()`.

**The capability gate is on all four doors, measured rather than recalled.**
Run as a non-root caller on this host's kernel (7.2.7): `sethostname(NULL, 100)`,
`sethostname(name, -1)` and `sethostname(name, 65)` all answer **`EPERM`** - so
the `CAP_SYS_ADMIN` check comes *before* the length check and before the pointer
check, not beside them - and the same holds for `setdomainname`, for
`unshare(CLONE_NEWUTS)` and for every `CLONE_NEW*` bit. The kernel's test for the
capability is the effective-uid one the entropy ioctls already use. The sysctl
files get their own gate for a different reason: on Linux the *file's mode*
(0644) is what refuses a non-root writer, and procfs here keeps no per-node
modes, so the capability is checked in the write policy and answers `EACCES` -
the errno Linux's own door gives. That the mode is not really there to check is a
named divergence below.

Two measurements decided the flag word's shape, and both are Linux's:

| as a non-root caller | Linux 7.2.7 | this kernel |
|---|---|---|
| `unshare(CLONE_NEWUTS)` | `EPERM` | `EPERM` |
| `unshare(CLONE_NEWUTS\|CLONE_NEWNS)` | `EPERM` | `EOPNOTSUPP`, **nothing unshared** |
| `unshare(CLONE_NEWNS)` | `EPERM` | `EOPNOTSUPP` |
| `unshare(CLONE_NEWUTS\|0x1000)` | `EINVAL` | `EINVAL` |
| `unshare(CLONE_THREAD)` | `0` | `0` |

The two rows that are not Linux's are the deliberate ones: a namespace this
kernel does not *serve* is `EOPNOTSUPP` rather than `EPERM`, because `EPERM`
would send the caller looking for a capability that does not help; and the
unserved bit is decided before any grant, so a mixed word unshares nothing.
Linux's `ksys_unshare` does not work that way - it walks the word and a later
failure leaves an earlier unshare done - and the atomicity is what `t-ns-uts`'s
witness child pins: it is forked *before* the refused call, so a rename the
parent makes afterwards is a rename it sees only if the two are still one
namespace. The whole verdict is a pure function over the word and the caller's
privilege (`abi::unshare_verdict`), which is where the four host tests argue with
it.

**Named gaps, each with what it waits on.**

* **`clone(CLONE_NEWUTS)`** - the *child* getting the copy - is still `EINVAL`:
  `validate_clone_flags` refuses namespace bits wholesale, and it says so. Serving
  it is a separate decision (where in `copy_process`'s order the copy happens,
  and what `CLONE_NEWUTS|CLONE_THREAD` means for a group that shares an address
  space) and it wants its own measurement, so the `EINVAL` stands as "this call
  cannot do that here" rather than as a half-built grant.
* **The six thread-group flags** are Linux's as of "Step 3, landed" below: the
  three a sibling makes impossible answer `EINVAL`, and `CLONE_FS`/`CLONE_FILES`
  split the caller's own slot. What is still not modelled is the *other* half of
  `CLONE_SIGHAND` - a thread's handler table is copied rather than shared, which
  is the divergence §6.4 names.
* **`setns` still cannot name a namespace**, and that is now the interesting
  half: `/proc/<pid>/ns/<kind>` does not exist, so there is no descriptor for the
  call to check a type against, and it answers `EINVAL`/`EBADF` - all it can
  learn. The design above (each namespace object an inode whose open descriptor
  *is* the namespace) is unchanged and still the plan for the second step.
* **`/proc/sys/kernel/{hostname,domainname}` are not mode-policed.** procfs keeps
  no per-node modes, so the capability check in the write policy is what refuses
  a non-root writer, and it answers `EACCES` - the right errno by the right
  argument for the wrong mechanism.

**And the flag table under all of this was wrong until this pass.** A caller does
not send a flag *name*; it sends the number `unshare(2)` documents, and this read
`CLONE_NEWNS` as `0x1_0000` - which is `CLONE_THREAD` - and `CLONE_NEWTIME` as
`0x8_0000`, which is `CLONE_SETTLS` and is not a namespace at all. A program
asking to unshare the mount namespace was told `EINVAL`, as though it had sent
something undefined, while a program passing `CLONE_THREAD` was told
`EOPNOTSUPP`, as though it had asked for a mount namespace. Nothing caught it
because the pin in `t-posix` had been written from the same misremembering, so
the test agreed with the bug - the case rule 1's carve-out is for, and the test
moved with the measurement.

The measurements, one call per child on this host's kernel, and the table the
implementation now holds:

| flags | Linux | why |
|---|---|---|
| `0x2_0000` (`CLONE_NEWNS`) | `EPERM` | a namespace flag, refused for want of `CAP_SYS_ADMIN` |
| `0x80` (`CLONE_NEWTIME`) | `EPERM` | the same; Linux puts this one in the *low* byte |
| `0x8_0000` (`CLONE_SETTLS`) | `EINVAL` | not a namespace and not something `unshare` can do |
| `CLONE_VM`/`FS`/`FILES`/`SIGHAND`/`THREAD`/`SYSVSEM` | `0` | thread-group splits, and a single-threaded caller has nothing to split |
| `CLONE_PIDFD`, `CLONE_IO` | `EINVAL` | real flags, meaningless to `unshare` |
| `CLONE_PIDFD\|CLONE_NEWNS` | `EINVAL` | an invalid bit outranks a namespace beside it |

So the eight namespaces are `0x80`, `0x2_0000`, `0x0200_0000` … `0x4000_0000`;
the six thread-group flags are a genuine no-op for a caller with no sibling to
split from, which is what the measurement above was taken on and what Linux
answers for a single-threaded process; and `EINVAL` is decided before the
namespace refusal, which is Linux's order. All of it is pinned by `t-posix`.

Two of those rows have moved since, and one of them is now an overstatement.
`CLONE_NEWUTS` no longer answers `EOPNOTSUPP` - it is the one namespace this
kernel serves, and the row above is the measurement the *unserved* seven still
give ("Step 1, landed" earlier in this section). And "a process is its own only
thread" stopped being true when threads landed (§6.4): the six thread-group flags
are a no-op for the *unshared* caller the table was measured on, and for a caller
that does have siblings three of them are `EINVAL` - which is this kernel's
answer too now, in "Step 3, landed" below.

**Step 3, landed: the six thread-group flags.** The gap the "Step 1, landed" list
carried - all six answering `0` for every caller, where Linux refuses three of
them once a sibling exists - is closed, and closing it needed the measurement
first, because the rule is not the shape a recalled answer gives.
`.workbuddy/probe_unshare_flags.c` (host 7.2.9, one `fork` per flag, one child per
case, the sibling case making a live `pthread`) measured:

| `unshare(flag)` | with a live sibling | alone |
|---|---|---|
| `CLONE_VM` | **`EINVAL`** | `0` |
| `CLONE_FS` | `0` | `0` |
| `CLONE_FILES` | `0` | `0` |
| `CLONE_SIGHAND` | **`EINVAL`** | `0` |
| `CLONE_THREAD` | **`EINVAL`** | `0` |
| `CLONE_SYSVSEM` | `0` | `0` |

and the `0`s are real *splits* rather than no-ops: a thread that `chdir`s moves the
process's own cwd until it unshares `CLONE_FS` (the creating thread's `getcwd`
follows, then stops following), and a thread that closes a descriptor takes it
from its sibling until it unshares `CLONE_FILES` (the sibling's `/proc/<tid>/fd`
loses it, then keeps it).

So the rule is **asymmetric**, and a version that swept all six together would be
wrong twice: the three Linux cannot split off one caller alone - an address space
or a handler table its siblings are using - are `EINVAL`, and the three it *can*
are served by copying the caller's slot. What landed is that asymmetry.
`abi::unshare_verdict` gained the caller's sibling fact and refuses the trio with
Linux's errno, ahead of all the namespace work exactly as `check_unshare_flags`
is; the handler acts on the word the verdict returns, splitting `CLONE_FS` into
`FsStruct::copy` (Linux's `copy_fs_struct`) and `CLONE_FILES` into
`FdTable::duplicate` (Linux's `copy_files`). The *default* had to move first: a
thread was copying the four fs facts, so a thread's `chdir` moved only itself
where Linux's moves its process - the shared handle landed one commit earlier for
that reason, and `t-unshare-fs` pins the default and the split as one fact seen
from both ends.

**What the pin caught that the design did not.** The first run of `t-unshare-fs`
was red on one claim - "`0` for a caller with no sibling", asked in a forked
child, answered `EINVAL` - while the virgin process at the top of the same
program answered `0`. So the verdict was right and the *sibling fact* was not, and
the cause is a shape this file has already paid for once: a **tgid is a slot
number**. A thread's slot is not given back the moment it exits, so a thread that
runs `unshare(CLONE_VM)`, gets `EINVAL` and exits leaves an *exited* task behind
whose tgid is its leader's slot - and the next process forked into a slot a dead
thread once occupied wears that number as its own group id and finds a sibling
nobody can see. The rule was therefore not to patch the new predicate but to stop
having a second one: `thread_group_has_siblings` asks `process_members`, which is
this file's one definition of "the live tasks of one thread group" and filters
`!exited`. The claim stayed *after* the threads on purpose - that ordering is the
regression pin, and a virgin caller would have passed either way.

Gate for the step: x86_64 `pass=168 fail=0 (168 built)` and aarch64
`pass=177 fail=0`, both with `mpos-kunit 29 passed, 0 failed`; host suites all
green (kernel 533, `mpos-xtask` 116); `fmt --check` clean; `clippy` clean on
`x86_64-unknown-none`, `-linux-gnu` and `aarch64-unknown-none`; `depgraph` within
the ratchet. `t-unshare-fs`'s nine claims pass on both machines.

**Step 4, landed: PID — the numbers a process sees, and the half that keeps them from
being a footgun.**

The third rung of the ladder is built: `CLONE_NEWPID` is served by `unshare`, `setns` and
`clone`, and the number a program reads out of `getpid(2)`, writes into `kill(2)` and walks
`/proc/<n>` by is now *the reader's namespace's* rather than the machine's.

**The object.** `crates/pidns` (a leaf, host-tested, 7 cases) holds what a PID namespace is:
two maps - number→slot and slot→number - and a cursor. Three decisions inside it are the whole
design. **The init namespace is an identity map that stores nothing**, so a machine that never
unshares is byte-for-byte what it was before this crate existed (and it is Linux's own shape:
the init namespace's numbers *are* the global ones). **A task is registered at every level of
its chain** - its own namespace and every ancestor - which is what lets a parent one level out
`wait` for a child that lives in a deeper namespace and find it in `/proc`. And **numbers are
never reused**: Linux's answer to "who is this number" is a refcounted `struct pid`, which this
kernel does not build, so the conservative half is taken instead - a released number is *spent*,
and a caller that forgets to release one leaks a number rather than handing a stale promise to
an unrelated task. That gap is named, not closed (§7.1's promise problem is unmoved by this).

**Two fields on `Task`, because Linux has two**: `pid_ns` (the numbers the task's *own*
`getpid` comes from) and `pid_ns_children` (where its children are created - Linux's
`nsproxy->pid_ns_for_children`). The pair is the whole of "the call renumbers the caller's
children and not the caller": `unshare(CLONE_NEWPID)` and `setns(fd, CLONE_NEWPID)` replace the
second only, so the task that asked keeps answering with the number it had. A first draft of
this step had one field and would have had to special-case the caller's own `getpid` out of a
namespace it had just been moved into; the second field is what makes that unnecessary rather
than merely awkward.

**The half that is not optional.** Renumbering without resolving *arguments* would hand a
correct program a footgun: inside a namespace `getpid()` answers 1, so the very next line a
program writes - `kill(getpid(), ...)`, `waitpid(getpid(), ...)` - would be read as the machine's
slot 1, its init. So the same commit translates every pid the caller writes, in the caller's
namespace, and every pid it is handed back: `kill` (single and group), `tgkill`,
`rt_sigqueueinfo`, `wait4`/`waitid` (argument and answer), `setpgid`/`getpgid`/`getsid`/`setsid`,
`ptrace`, `process_vm_readv`/`writev`, the priority pair, the affinity pair, the robust-list
pair, `capget`/`capset`, `timer_create`'s `SIGEV_THREAD_ID`, and the terminal's `TIOCSPGRP`/
`TIOCGPGRP`. The `pid >= MAX_TASKS` bounds that stood in for "no such task" came out with them:
they were the *init* namespace's spelling of that answer, and a namespace's numbers are its own.
A number that names nothing where the caller stands is `ESRCH` (or `ECHILD` for `wait`) - never
another task.

**`/proc` is one tree with per-reader answers.** The rule is applied at two funnels - which
names a directory has (`process_ids`, `Threads::names`) and which directory a name resolves to
(`Processes::node`, `Threads::node`) - plus at every place the file *prints* an id (`stat`'s own
id, its parent and its group; `status`'s `Pid`/`Tgid`/`PPid`/`NSpgid`), so a directory and the
numbers inside it cannot disagree. `/proc/<pid>/ns/pid` is Linux's `pidns_get`: it names the
namespace the task *is in*, while a `setns` against that descriptor moves the namespace its
*children* go to - the asymmetry is Linux's, and it is why `setns(/proc/self/ns/pid)` is a
working no-op.

**What was reasoned rather than measured, and why that is written down.** This sandbox cannot
create a PID namespace: `CapEff` is 0, so `unshare(CLONE_NEWPID)` answers `EPERM` for root and
`unshare --pid --fork` fails the same way (`.workbuddy/probe_pidns.c`, which still measures what
it can - `/proc/<pid>/status` carries `NSpid`, and `/proc/<pid>/ns` lists only the kinds the
machine actually has). Every semantic above therefore comes from this section's design record
and Linux's rules, not from an observation here. Two consequences are *named*: the
`si_pid` a signal carries is the **sender's own** number (Linux's `task_tgid_vnr(current)`), so
a receiver one level out may read a number from a namespace it cannot see - Linux's own wart,
kept rather than smoothed over; and `CLONE_THREAD | CLONE_NEWPID` is refused `EINVAL`. That last
one is no longer *reasoned*: `clone(2)`'s ERRORS list says it outright - "CLONE_NEWPID and one
(or both) of CLONE_THREAD or CLONE_PARENT were specified" - which is the citation the rule should
have carried from the start, and the same sentence added `CLONE_PARENT` to it (`CLONE_PARENT` is
itself accepted-and-ignored here, which is its own named gap below).

**What the machinery caught, in order.** (1) `xtask`'s host stage names every leaf crate and
fails when one is compiled and never tested - the new crate was left out of `LEAF_CRATES`, and
the gate said so in one line rather than letting `mpos-pidns`'s seven cases sit unrun. (2) Four
programs had the *served set* written down as a constant and went red the moment it grew:
`t-caps` (which asked for `EOPNOTSUPP` from `CLONE_NEWPID`), `t-ns-mnt` (which pinned
`clone(CLONE_NEWPID)` as the refused word), `t-ns-setns` (which used `CLONE_NEWPID` as "a word
that is no namespace bit") and `t-clone` (which listed PID among the refused kinds) - each
moved to an unserved kind, which is the third time this file records that drift (§6.3 counted
two for MNT). (3) The host unit tests of both verdicts named PID as refused and were moved with
the measurement, including the mixed word (`NEWNS | NEWPID` became a legal pair, so the
"refused whole" case moved to `NEWPID | NEWNET`).

**Gate.** x86_64 `=== SUMMARY pass=172 fail=0 (172 built)` with the kernel's own
`mpos-kunit 32 passed, 0 failed` and exit 0; aarch64 `=== SUMMARY pass=171 fail=0`, the same 32
cases, exit 0. The new pin runs in both default sweeps, and each arm's count is one higher than
the commit before this one. Host suites: 537 in the kernel, 7 in `mpos-pidns` (its own leaf
crate, named in `xtask`'s `LEAF_CRATES` - see above), 6 in `mpos-abi`, 121 in `xtask`; clippy
reports 0 errors on `x86_64-unknown-none`, `aarch64-unknown-none`, the host lib and the two
touched leaf crates; `cargo xtask depgraph` is within the ratchet, which is the one thing a
namespace does not move: the edges are the same ones, named from the same places.

**The thread rule, and the wrong shape it closed.** Designing NET turned up a shape that was
already shipping for the *served* kinds: `clone(CLONE_THREAD | CLONE_NEWNS)` was accepted by the
verdict and then **silently dropped**, because `sched::clone` routes a thread-shaped word to
`spawn_thread` before the namespace work and a thread here copies its process's namespaces by
reference. A caller that asked for a thread with a namespace of its own got a thread in its
parent's, with nothing said - §4's rule 4 exactly. Linux *serves* the pair (its
`create_new_namespaces` runs for a thread too), so `EINVAL` would be a lie about the request; the
honest answer for a call this kernel cannot honour is `EOPNOTSUPP`, and that is what
`clone_namespace_verdict` now gives it, for both served kinds (`NEWNS`, `NEWUTS`) as well as PID's
documented `EINVAL`. Both arms are pinned in `t-clone` (the word carries `SIGHAND|VM`, so the flag
validation cannot be what refuses it) and in the leaf's own tests.

Two more shapes the same pass found, **named rather than fixed** because each is its own
question: `CLONE_PARENT` is in the validated flag mask and is not honoured, so a caller asking
for it silently gets the caller-as-parent it would have got anyway; and `clone(CLONE_FS |
CLONE_NEWNS)` is accepted here where `clone(2)`'s ERRORS list calls it `EINVAL`. That list also
names `CLONE_NEWUSER | CLONE_FS` and `CLONE_NEWIPC | CLONE_SYSVSEM`: IPC is unserved here, and
`CLONE_NEWUSER` is a door through `clone` now (Step 6e, above), so both pairs are this kernel's to
answer and are named rather than fixed. The first belongs with `parent:`'s implementation, the
second with `validate_clone_flags`'s documented-pair table.

**Named gaps, each with what it waits on.** A namespace's pid 1 dying does not zap the
namespace (Linux's `zap_pid_ns_processes`, which kills every member) - and because numbers are
never reused here, a namespace whose init died goes on numbering rather than refusing. Nested
namespaces work by construction, and the *numbers* are pinned two levels deep now (2026-10-15; the
chain paragraph above). Two clauses this list used to carry closed with it and are recorded above
rather than here: `/proc/<pid>/status`'s `NSpid` is the whole chain and not the reader's number, and
a record lock's `l_pid` is the *reader's* number. What remains is group *numbering*, which is not
per-namespace:
`kill(-n)` now resolves `n` as a pid in the caller's namespace, but a group is still named by its
leader's id, so a namespace's groups are its leaders' numbers. And a `/proc` mounted inside
another mount namespace still lists one machine's processes - per-*reader* numbering is what this
step built; per-*mount* was already true of the mount namespace and is not what a pid namespace
asks for.

### 6.4 Additional Process Syscalls — **DONE**
All wired to real handlers in `posix.rs`/`proc.rs`: `getpgrp`, `setreuid`,
`setregid`, `getresuid`, `getresgid`, `capget`, `capset`.

| Syscall | Nr | Description |
|---------|-----|-------------|
| `getpgrp` | 111 | Implemented, verify correctness |
| `setreuid` | 113 | Set real/effective UID |
| `setregid` | 114 | Set real/effective GID |
| `getresuid` | 118 | Get real/effective/saved UID |
| `getresgid` | 119 | Get real/effective/saved GID |
| `capget` | 125 | Get capability state |
| `capset` | 126 | Set capability state |

**Threads are real, and this section said otherwise for longer than it should
have.** The paragraph here used to read "one request in this family is
deliberately not answered, and it is the largest hole left in the process ABI:
`clone(CLONE_THREAD)` — thread creation — answers `ENOSYS`". That had stopped
being true at commit `58c6947`, which landed a thread group (`Mm`: the frames
behind the group's mappings, how many threads run on them, the paging root a
thread switches to, and where a leaving thread says so), `spawn_thread` reached
from `sched::clone`, `CLONE_CHILD_CLEARTID` writing zero and waking the futex
there - which is what `pthread_join` sleeps on - `exit_group(2)` ending every
thread of a group while `exit(2)` ends only the caller, and `getpid` answering
the group id while `gettid` answers the task's own. `t-clone` pins it and sits in
the default sweep: a thread's write is visible to its creator, which is the whole
difference between a thread and a fork. The stale paragraph was contradicted by a
green test the gate ran every time, which is the lesson worth keeping from it.

What genuinely remains is narrower, and each item is named rather than implied:

* **`CLONE_SIGHAND` is accepted and the handler table is still copied.** Linux
  shares it under that flag; here a handler installed by one thread is not
  installed for its siblings. Nothing in this system's userland does that yet, so
  a *copy* is the honest version of "not shared" - but it is a divergence and not
  a completion.
* **`CLONE_VM` without `CLONE_THREAD` is answered with a copy.** That is
  `posix_spawn`'s shape met precisely (such a child only ever reads the caller's
  memory before `execve`), and the doc comment at `sched::clone` says why a copy
  beats a share there - but it is not the shared address space the flag names.
* **The rest of the thread ABI** is wired where a caller needs it: the tid
  writes, the TLS base, the signal mask inherited across `clone`/`fork`,
  `get_robust_list`, and the per-thread `/proc/<pid>/task` listing
  (`thread_snapshots`).

---


---

### 6.5 Orphaned children are given to init, because a parent is a slot — **LANDED**

Linux's `forget_original_parent`: a process that exits hands every child that was
still its own to init (or to the nearest subreaper), so no zombie ever keeps a
parent that has ceased to exist. This kernel had that edge missing — `exit` retired
the task and woke its parent, and the children it left behind kept a `parent` field
naming **a slot number** — and a slot number is reused, which is what made it
visible as a *record handover* rather than as a wait bug: `sched::read_sigchld`
(a `signalfd`'s child-exit source) and the wait tables both filter by
`task.parent == Some(me)`, so the next process to occupy the dead process's slot
inherited its unreaped orphans.

**Measured**, once the sweep's program count grew far enough that a test landed in
a slot whose previous occupant had left orphans: `t-signalfd` was handed **four**
records for one death — the documented pair (the queued `SIGCHLD` copy, `ssi_code`
0, then the exit record, `CLD_EXITED`, naming its own child) plus records naming
slots `10` and `8` — and the read *before* its fork answered a record (slot `11`)
where the claim is `EAGAIN`. Its own slot is the tell: one of the strays names it,
because the orphan's recorded parent *is* that number.

The fix is the walk in `src/sched/runtime.rs`'s exit path, before the parent is
woken: every task whose `parent == Some(id)` gets `parent = Some(INIT_PID)`. Init
is the only candidate because the kernel stops the machine rather than let init
exit (the same function's guard above), so there is nothing to fall back to.
**Not modelled, and named here rather than implied**: Linux also hands a dead
child to its *tracer* when one is attached, and signals `SIGHUP`+`SIGCONT` to an
orphaned process group when the group becomes session-less; the first belongs in
this walk, the second is a delivery rule beside it, and neither has a caller in
this image (`strace`'s attach is `t-ptrace-attach`'s subject, which does not depend
on the reparenting direction).

The pins are `t-signalfd`'s two claims — "a read with no child exited answers
`EAGAIN`" before the fork, and "a child's death is reported once" (this kernel's
two records, Linux's one) — which are exactly the claims the strays broke. The
second of those two records is history now: `SIGCHLD_PENDING` and the descriptor's
synthetic `ChildExit` record are deleted, so **one** death is one record on both
sides of the seam and that claim reads `1` on both machines (`docs/uapi/` §5.1
item 29, 2026-10-17). What this row measured — an unreaped orphan's slot being
*reused* by the next process — is untouched by that, and so is the fix it records:
the handover in the exit path is what keeps a dead process's children from naming
a slot that has been handed on.

### 6.6 A namespace a descriptor can name — `setns` + `/proc/<pid>/ns/*` — **LANDED**

The half of MNT the step above left: `unshare` gives a caller a namespace of its
own, `clone` gives one to a child, and neither lets a program *adopt* the
namespace another process already made. That is what the descriptor is for -
Linux's `/proc/<pid>/ns/<kind>` is an openable link whose target is a report, and
`setns` joins the object the descriptor holds rather than copying the caller's -
and it is the shape a container runtime is built out of: a parent that makes a
mount namespace, and the processes that join it afterwards.

**Measured first** (`.workbuddy/probe_setns.c`, host 7.2.9), because almost every
claim here is about Linux's answer rather than this kernel's:

| question | the host's answer |
|---|---|
| what is in `/proc/self/ns` | ten names - `net, uts, ipc, pid, pid_for_children, user, mnt, cgroup, time, time_for_children` - every one a symlink (`lstat` mode 777) |
| what a link reads back as | `kind:[<n>]`, and the numbers come from **one pool**: `net` 4026531833 down to `mnt` 4026531832, consecutive in creation order |
| what `open` on a link gives | a descriptor for the *namespace*: `read` is `EINVAL`, `write` is `EBADF` (opened read-only), `lseek` is `ESPIPE`, and `fstat`'s `st_ino` (148898) is **not** the number the link reports |
| `setns(fd, nstype)` with a sibling kind's bit | `EINVAL` (`-22`), not `EPERM` |
| with **two** namespace bits at once | `EINVAL` for both a mount and a UTS descriptor |
| with a word that is no namespace bit (`0x10000000`) | `EINVAL` |
| with a pipe, a plain file, a directory | `EINVAL` - not a namespace, whatever the descriptor is |
| with a number that names nothing | `EBADF` |
| `/proc/<pid>/ns/mnt` for a pid that does not exist | `ENOENT` |
| the capability's position | **after** all of the above: the probe's unprivileged caller was refused `EPERM` for the *matching* bit and `EINVAL` for the mismatched ones |

What the host would not let the probe reach is a *successful* join (this sandbox
refuses `setns` into one's own namespace with `EPERM`), so the success path is
this kernel's design rather than a Linux measurement, and the test says so.

**What landed.** `vfs::ns` owns the two facts both halves need: the one number
pool (`mint()`, starting where Linux's numbers live) and the link text
(`kind:[<id>]`). Its first home was a module of its own, `crate::ns`, and the
depgraph ratchet refused it in the same run: `ns → vfs` (the mount namespace's
object) and `vfs → ns` (the renderer and the link payload) made a **ninth mutual
pair** where eight are recorded. The rule's own remedy is the one taken - remove
the edge rather than raise the constant - and the concept belongs under `vfs`
anyway, where the mount namespace and the `/proc` renderer already live. `MountTable` and `Uts` each carry an id - lazily for the table,
because `MountTable::new` is `const` and the machine's table is built in a
`static`; passed in for the UTS namespace, because a leaf crate cannot mint from a
pool the kernel owns. `/proc/<pid>/ns` is a `Dynamic` directory beside `fd`,
listing **only** the kinds this kernel has, and each entry is the same *magic
link* the descriptor names already are (`ProcLink` grew a second payload beside
its `fd`): the walk hands the link back instead of splicing it, `readlink`
answers the report, and `as_namespace` answers the object - asked of the *live*
process, so a link opened after its process unshared names the namespace it is in
now. `setns` then asks its four questions in the measured order (descriptor →
flags → capability → join) and joins by **reference**: the caller's `fs_ns`
*becomes* the object, so a mount made by one member of the namespace is seen by
every other member, and the caller's root moves to the namespace's root (Linux's
`mntns_install`) while its cwd stays where it is.

**Two things are named rather than done.** The *cwd* is not re-resolved by path
inside the new namespace, which is what Linux does; an inode stays valid across
the join either way, so what is missing is the re-resolution and not the safety.
And the kinds this kernel has not (ipc, user, cgroup) have **no entry** in
`/proc/<pid>/ns` - which is the surface a real kernel built without a namespace
has (a machine with `CONFIG_TIME_NS` off answers `ENOENT` for
`/proc/self/ns/time`), so a reader is told the names this machine can isolate and
nothing it cannot check, and no descriptor for an absent namespace can exist to
be handed to `setns`.

Two of the pin's own claims had to be corrected by the gate rather than the
kernel, and both are worth naming because the kernel was right: a *second*
`mount` at a mountpoint that already has one **succeeds** here (that is Linux's
own mount stack, and the claim that it would be refused was wrong), and the
"namespace outlives the process" claim was reading the marker *after* the peel
that removes it, so it was measuring the `umount` and not the namespace's life.
What the join does is the claim between them - the marker the child wrote inside
its own mount, readable in the caller's namespace after joining and unreadable
before.

Gates: x86_64 `pass=167 fail=0 (167 built)` and aarch64 `pass=176 fail=0` (176
staged, 2 left out), host 530 + `mpos-uts` 5 + `mpos-xtask` 116, `fmt --check`
clean, `depgraph` within the ratchet (8/8). The commit body had to say the
aarch64 arm was still un-re-run when it landed; this is the line that closes
that, added after the fact because a body is a record of the moment and not a
claim about later.

The pin is `t-ns-setns` (both machines, default sweep): the directory's two names
and the six `ENOENT`s, the link text's shape and the two kinds' different
numbers, the descriptor's `EINVAL`/`ESPIPE`/`EPERM` set, every measured refusal
above, and the claim the whole surface exists for - a child that unshares and
mounts a marker, and a caller that joins *its* namespace through
`/proc/<child>/ns/mnt` and reads that marker, where an `unshare` would have left
it looking at an empty copy. The namespace outliving the process that made it is
the last line, pinned in all three of its halves: after the child is killed, the
caller's link still reports the *number* it joined, `/proc/self/mounts` still
carries the child's mount, and the marker the child wrote is still readable.

**That line was got wrong twice, and the second time is the one worth keeping.**
The first draft read the marker *after* the peel that removes the mount, so it was
measuring the `umount` and not the namespace's life; the fix was to take the
observation first. That fix was incomplete, and the commit that landed `setns`
shipped with the second mistake and a defect named out of it: the test peeled in
**two** places - once early, inside the join block, and once at the end - so the
"does the mount outlive its mounter" reading was still taken after the *early*
peel, and the absence it found was again the test's own act. What that produced
was a named defect ("a mount made inside a namespace does not outlive the process
that mounted it") and a hypothesis about its cause (`vfs::ramfs`'s byte budget
charged per `mm`) that the code never had: `Usage` is per *mount*, held by the
`RamFs` the `Mount` record keeps (`Mount::fs`), which the namespace's own `Arc`
keeps alive - so those bytes go exactly when the namespace does and not a moment
before. With the peel left to the end - the single peel the test's own comment had
described all along - all three halves hold and all three are claims, and the run
that says so is the gate below. The lesson is `AGENTS.md`'s own: a recorded
measurement is about the machine that made it, and this number was real while the
*question* was not the one the code was answering. The roadmap's 6.3 row loses the
defect it had carried, and MNT's remaining work is CLONE_FS-style separation
alone.

Gate for the correction: x86_64 `pass=167 fail=0 (167 built)` and aarch64
`pass=176 fail=0` (176 staged, 2 left out), both with `mpos-kunit 29 passed, 0
failed`; the host suites all green (kernel 530, `mpos-xtask` 116, the leaf
crates' own), `fmt --check` clean, `depgraph` within the ratchet. The three new
claims hold on both machines, and `t-ns-setns` prints no note any more.

**The other half of "every free function routes through the process context" — and
the family it turned out to be.** The sentence above named one site, `linkat`'s
descriptor-side `EXDEV` check. Fixing it found a class: a dozen handlers in
`src/syscall/linux/fs/file.rs` resolved through the *machine's* table, because
they reached for `mount::global()` where this module's own header says nothing
outside it may. What let the bug survive review is that the divergence is sharp
rather than subtle — the *following* walks were caller-routed already, so inside a
namespace with a filesystem mounted `stat("/mnt/f")` saw the mount and
`lstat("/mnt/f")` did not. One path, two trees, and the wrong one for every
`ls -l` in a container.

What landed is the rule rather than the site: two free functions in
`src/vfs/mount.rs` — `resolve_no_follow_at` and `fs_serving_inode_of` — which
compute the caller's table the way every other free function there does, and the
sites now go through them. Each one: `newfstatat`'s no-follow walk, `statx` with
the same flag, `readlink`, `xattr_target`, `utimensat`, `mkdirat`'s existence
probe, `fchownat`, `linkat`'s two (the source's resolution and the identity test),
`fchmodat`, `mknodat`'s probe, `vectored_at_position`'s buffer target, and `open`'s
`O_NOFOLLOW`/`ELOOP` probe. Two of those carried a *second* defect: `mkdirat`'s and
`mknodat`'s probes called `MountTable::resolve_no_follow`, whose walk starts at the
machine's **root** — so a chrooted task was refused a directory it did not have
because the machine's root had one. (Those two probes, and `mknod`'s, moved to
`resolve_entry_at` later — the same caller's-table rule, asked by the door whose
question is the *name* rather than the path: `docs/uapi/` §5.1 item 45.) `path_of`'s render jail (`/proc/<pid>/cwd`
naming an inode from a root) is a *different* question — whose root a path is
*rendered* against, not which tree it resolves in — and is left named rather than
moved with these; it wants its own reading and its own pin.

The pin is `t-caller-paths` (both machines, default sweep), one claim per
observable: `lstat`, `statx` with the same flag, `readlink` and
`open(O_NOFOLLOW)`→`ELOOP` inside a mount namespace, and `mkdir("/tmp")` inside a
jail. **Every claim was checked as a counterfactual**, and with the fix stashed all
five fail. That check also *cut the pin down*: three sites the first draft claimed
— a namespace `mkdir`'s `EEXIST`, `linkat`'s `EXDEV`, and `access` — pass with *and*
without the fix, because the filesystem's own create/`link` refuses in the same
place, so they pin nothing and were removed rather than kept as decoration. Rule
4's criterion is the reason to prefer five claims that fail to twelve that pass.

Gates: x86_64 `pass=169 fail=0 (169 built)` and aarch64 `pass=178 fail=0`, both
`mpos-kunit 29 passed, 0 failed`, the five `t-caller-paths` claims green on each;
host 533 + `mpos-xtask` 116; `fmt --check` clean; `clippy` clean on both kernel
targets; `depgraph` within the ratchet.

### R.6 (moved from ROADMAP's status board) — `/proc/<pid>/maps`: the three sources, and why two of them stay beside the VMA table

The file a crash reporter reads to find a program's base address: every mapping in Linux's six-field format — `start-end perms offset dev inode name`, addresses as lowercase hex, the offset zero-padded, `dev` as `%02x:%02x`, and the name in the fixed column Linux uses (73, measured across address and inode widths on 7.2.7) when there is one — which is what makes the last field addressable whatever the width before it. The perms word's fourth character is the `MAP_SHARED` bit, so it is carried on the mapping rather than guessed from what backs it: a `memfd` mapped `MAP_PRIVATE` aliases the same frames and is still `p`. Three sources feed one file, because the address space is recorded in three places: the VMA table (`mmap`, `mprotect`, `brk`), a list of ranges the *loader* filled, and the program break, which is what gives a range the `[heap]` name — applied by address, since this kernel's heap is an ordinary anonymous mapping. `[stack]` is recorded when the stack is laid out and stretched down as the stack grows, so a program reading its own maps from deep in a call is not told about a stack that stops above it.

**What the work needed, and it is the interesting part:** `execve` maps an image's pages *eagerly* — it fills every page as it loads — so an image leaves **no VMAs at all**, and the same is true of a `MAP_PRIVATE` mapping of an ordinary file. A maps file built from the VMA table alone would therefore omit the one line a crash reporter actually wants: the program's own code. The loader's ranges and the private file mappings are recorded *beside* the table (`mm::vma::MappedRange`) rather than put in it, and the reason is the fault path rather than tidiness: recording them as `VmaKind::Anon` VMAs would make an address that should take a fault — one whose frame a `munmap` or a `MADV_DONTNEED` gave back — silently refill with zeros, which for a program's own code means blank bytes where the file's bytes belong. The cost of that choice is named with it: those ranges are the *load's* record and not the current state, so a `munmap` or `mprotect` over one of them — a dynamic program's `RELRO` pass does the second on every start — is not reflected in the report where a VMA would have been edited. `t-maps` pins the lines, not the mutation, so closing this needs a `forget_ranges`/`reprotect` on the recorded list before it can be pinned.

**Two divergences, pinned as divergences:** an image's and a private file mapping's line reports `dev` `00:00` and inode `0`, because there is no object left to name — and `00:00` is what this kernel's own `stat` reports for every filesystem, so the two files agree rather than disagreeing. And a *shared anonymous* mapping is named `/memfd: (deleted)`, which is Linux's own name for a nameless `memfd` mapping (measured: `memfd_create("", 0)` + `MAP_SHARED` reports exactly that), because that is what the object here is; Linux reaches the same shape through `/dev/zero (deleted)`, which is an artefact of its own implementation. `memfd_create` now *keeps* the name it used to only validate, as `/memfd:<name> (deleted)` — the spelling both of its readers expect, measured on Linux — so a `wl_shm` pool's mapping says which buffer it is instead of leaving the column blank. Not written: `/proc/<pid>/numa_maps` and `/proc/<pid>/smaps_rollup` - the same accounting in two other spellings, whose formats are measured in `docs/uapi/` §5.2. (`/proc/<pid>/smaps` *is* written now, and it is this file's records with a field block under each: item 54, `docs/uapi/` §5.1, §15.73.)

**The permission column is the grant, not the bit this kernel records "present" in** (fixed 2026-10-18). The fourth character was always the `MAP_SHARED` bit; the first three were read straight off the VMA's `prot`, and that field does not mean what Linux's `VM_READ`/`VM_WRITE`/`VM_EXEC` mean here: `mmap` and `mprotect` record `READ` for *any* present range and add `USER` only when the caller asked for a non-empty protection (`syscall::linux::proc`'s two conversions), and `USER` is the bit that makes a page reachable from ring 3 — it is the first thing `Vma::permits` asks. So a `PROT_NONE` reservation and every guard page musl maps reported `r--p`, a word a reader branches on: `pmap`, and a crash reporter deciding whether a range is readable, would try to read a range whose every access faults. Measured on a current host first (`.workbuddy/probe_maps_prot.c`, Linux 7.2.11): a two-page `PROT_NONE` reservation prints `7f9132fd1000-7f9132fd3000 ---p 00000000 00:00 0`, a read-only one `r--p`, and a `PROT_NONE` span whose middle was `mprotect`ed to `rw-p` splits into `---p` / `rw-p` / `---p` — the shape the same file reported as `r--p` / `rw-p` / `r--p` here. The fix is one function at the one place both readers take their permissions from (`procfs::granted`: the four permission bits when `USER` is set, and nothing when it is not), so the host unit test `a_permission_column_is_the_grant_and_not_the_present_bit` pins the rule and `t-maps` pins the line (`a PROT_NONE reservation is reported with no permission at all`). It is also what `smaps`' field block needed: its header *is* this line, and its `VmFlags` words are these bits.


### 6.7 A group's threads outlive their leader, so the slot that owns their address space outlives the exit code — **LANDED**

**The report.** A compositor aborted on a desktop boot. The shell beside it took its
prompt back - its `wait` had a code to report - and a moment later the machine died:

```
KERNEL PANIC: panicked at src/sched/runtime.rs:1714:32:
there is no task 10
```

**What the number was.** `1714` is `Table::task`, the shared slot lookup, so the line
names no caller: it says only that *somebody* asked for task 10, which is the pid the
compositor's own startup printed (`DEBUG ]: Hyprland PID: 10`), after the slot was
gone. The first hypothesis - a corrupted descriptor table - was tested against the
same report from two boots and dropped: the "fd" the dispatcher printed was
`rmdir(2)`'s pathname pointer (§7.4's EOPNOTSUPP note, fixed separately in
`1d088e6`), not a descriptor.

**The mechanism, and the shape it is.** A thread has no address space of its own:
`Table::space_owner` answers the *group leader's* id (`mm.leader()`) and every
question a thread asks about its own memory - a fault, a map, an unmap - is answered
by looking that slot up. `reap` freed a slot when two things were true of it: the
task was `exited`, and its exit code had been collected. Nothing in either question
asks whether *another* task still names the slot as the owner of the space it is
executing on. So the leader of a multithreaded group was reaped the moment its parent
collected it, and dropping the leader's `Task` dropped the address space its threads
were still running on.

**Measured, on both sides.** `t-leader-outlived` makes the shape without a signal:
a group of three whose leader ends its own thread with the bare `exit` (`SYS_exit`),
while two siblings step a counter in a `MAP_SHARED` page the parent reads.

* without the guard: the leader's death is collected, and the two siblings then take
  `user #PF` at the instruction the freed space had owned - `PFDIAG page=0x450000
  NOT MAPPED task=13`, `task=14` - and the counter freezes where the death found it
  (`4 then 4`). A worker that asks for the space *before* it faults on it is the one
  the panic line names.
* with it: the slot stays a zombie - its memory is already released, and what remains
  is the answer to the lookups its threads are still making - and the counter goes on
  moving (`4 then 128`, two threads at a 10 ms step).

Linux keeps a thread group leader until the group's last thread is gone, which is
the same rule from the other direction, and this is that rule here.

**The second half, in the same class.** `Scheduler::retire`'s comment already says it:
"a task left queued after its slot went away is a pid the picker would dereference
into nothing". `retire` is called for the siblings of an `exit_group` and nowhere
else, so every other route to that state - a task that puts itself back in its band
when it yields, a slot the reaper frees between the queue and the pick - lands in
`Table::next_turn`, which now drops an id whose task is gone or marked exited rather
than entering it.

**Named, and not fixed.** Two things this session found and deliberately left:

* **a fatal signal ends one thread, not the group.** Linux ends the process
  (`do_group_exit`); here `exit(-sig)` is called from six places
  (`sleep_ticks`, `block_until_code`, `block_until_on`,
  `block_until_on_interruptible`, `block_until_deadline`, `deliver_pending_signal`).
  Changing all six to `exit_group` compiled and passed every gate, and was reverted
  rather than shipped, because the pin written for it (`t-fatal-group-exit`) failed
  with *and* without the change - a claim that cannot tell two kernels apart pins
  nothing, and it was deleted with the change.
* **a signal a thread raises at itself does not reach it.** The same pin failed in
  both kernels because the group never died at all: `raise` from a spawned thread
  left the leader and its sibling stepping (counter `756 then 831`). A `raise` from
  the *leader* does kill it, so the difference is delivery to a non-leader thread -
  `tgkill`'s - and it is unmeasured.

Gates: x86_64 `pass=171 fail=0`, `mpos-kunit 32 passed, 0 failed`; aarch64 as
recorded by the gate run of the same commit.

### 6.8 A pidfd names a process, not a number — **LANDED** (2026-10-21)

**Why**: a pid is a slot index here, and `Table::alloc_slot` hands the first free slot
to the next tenant the moment a reaped task lets go of it. Any handle that stores a
bare number therefore names *whatever is there later*, which is exactly what
`pidfd_open(2)` exists to prevent. §2.6 anticipated the door: `pidfd_send_signal(2)`
would have been a second way onto the missing permission rule, so the rule landed
first; this is the family it was waiting for. All three doors are one design - a
descriptor that opens, a wait that consumes it, and a signal that names it - and a
descriptor nothing can wait on would be half a door.

**Measured whole on a current host first** (Linux 7.2.11; `.workbuddy/probe_pidfd.c`,
`probe_pidfd2.c`, `probe_pidfd3.c`, `probe_pidfd5.c`, `probe_pidfd6.c`,
`probe_pidfd7.c`, `probe_waitid_pidfd_zombie.c`, `probe_flags.c`,
`probe_selfsig2.c`):

* **open's flags are validated before its pid.** `O_NONBLOCK` is the only accepted
  flag; `O_CLOEXEC` alone is `EINVAL` even though `FD_CLOEXEC` is always set, and an
  unknown bit is `EINVAL` before a bad pid can be `EINVAL` or `ESRCH`. `pid <= 0` is
  `EINVAL`, a number naming nothing is `ESRCH`, and a **non-leader tid** is `ENOENT` -
  a pidfd names a thread group. `pid 1` opens without privilege.
* **descriptor shape.** `F_GETFL` is `O_RDWR` (`0x2`), plus `O_NONBLOCK` (`0x802`)
  when asked; `FD_CLOEXEC` is set without being asked; `fstat` reports mode `0700`
  with **no `S_IF*` bits**, size 0, nlink 1; `/proc/self/fd/<n>` reads back
  `anon_inode:[pidfd]`; `read`/`pread`/`write` are `EINVAL` at every length - a
  zero-length read included - and `lseek` is `ESPIPE`.
* **two readiness facts, not one.** The descriptor is readable when the *process*
  has died (its last thread left, whether the leader did or not), and `POLLHUP`
  joins only after the reap. A zombie is therefore readable and not hung up; after
  the reap the same wait is `ECHILD` and the same send is `ESRCH`.
* **`pidfd_send_signal`'s refusal order** is flags, descriptor (`EBADF` for a free
  number or a file that is not a pidfd), record (`EFAULT`; `si_signo != sig` is
  `EINVAL`; a kernel-made code - `si_code >= 0` or `SI_TKILL` - is `EPERM` unless the
  target is the caller's own group), signal range (`EINVAL`), existence (`ESRCH`),
  authority (`EPERM`).
* **the scopes are a choice, not a bitmask.** `0`, `1` (the named thread), `2` (the
  process) and `4` (the process group the target leads) are each valid alone;
  `1|2`, `2|4`, `1|4` and everything above 4 are `EINVAL`. Measured flags 3, 5, 6, 7
  and 8 all `EINVAL`; 0, 1 and 2 are accepted and 4 reaches the group check
  (`probe_flags.c`).
* **a zombie's scopes.** Under `0`/`1`/`2` a zombie accepts `sig=0` and a real signal
  and the call answers 0 (nobody will read it); scope `4` is `ESRCH` even while a live
  worker keeps its group alive, because a zombie leads nothing.
* **records.** A NULL record becomes `SI_USER` naming the sender. A caller's record
  with `SI_QUEUE` and its own `si_value`/`si_pid`/`si_uid` arrives verbatim; the
  self-targeted `SI_USER`/`SI_TKILL` records pass verbatim too, spoofed ids included.
* **`waitid(P_PIDFD)`.** Options and idtype are read first (`EINVAL`), a negative id
  is `EINVAL` before the table lookup, a free or wrong-kind descriptor is `EBADF`; a
  live non-child is `ECHILD`. An `O_NONBLOCK` descriptor (set at open or by
  `F_SETFL`) answers `EAGAIN` when a live child has nothing, while an explicit
  `WNOHANG` still wins with the cleared zero record. A zombie is reported normally.
* **event classes.** A dead child under `WSTOPPED`/`WCONTINUED` alone is `ECHILD`,
  not "nothing yet" - for `P_PID` as well as `P_PIDFD` (`.workbuddy/probe_waitid_classes.c`).

**The landing** gives the descriptor a generation as well as a slot.
`Table::generations` starts at 1 for every slot and `next_generation` at 2, so the
two tenants written directly into reserved slots (init's and the scheduler's) carry
their first generation without a new edit site, while every `alloc_slot` tenant gets
a fresh number; a freed slot keeps the dead tenant's number until somebody takes it,
so a pidfd that outlives its task reads "gone" either way. `PidFd`
(`src/vfs/pidfd.rs`) records the slot and that generation, and every later use asks
`sched::pidfd_still_names`/`pidfd_group_alive`/`pidfd_task_alive`/`pidfd_live_member`
rather than the raw id. `NodeKind::Anon` is the shape `fstat` reports: `abi::mode_of`
gives it `0700` and no format bits, and `dt_type_of` `DT_UNKNOWN` (a directory entry
it can never be); `File::read` skips its zero-length short-circuit for this kind
because Linux's pidfd refuses even that; `Inode::fd_name` carries
`anon_inode:[pidfd]`, which `File::display_name` consults after a recorded label and
before the tree; and `fsync` answers `EINVAL` with the rest of the anonymous family.

The wait side is one selector with a class. `waitid` resolves `P_PIDFD` through the
caller's descriptor table and asks `pidfd_still_names` before its slot can be reused
(`ECHILD`), reads `O_NONBLOCK` off the file description, and threads `WEXITED`
through `scan_child_event`/`wait_child_event`/`poll_child_event`/
`has_matching_children` - a dead child under the stop-only classes is `ECHILD` now
for every idtype, which was a pre-existing divergence `P_PID` shared. The signal
side reuses §2.6's `may_signal` and adds `send_signal_group_info`, the one group
door that carries a caller-authored record through the fan-out instead of minting an
`SI_USER` per member.

**The first sweep was red, and the failing claims were the rule, not the test.**
Three `pidfd_send_signal` flag claims (`3`, and `5`) failed in the guest while the
host run was green - the host run measures Linux, not this kernel. The handler had
validated `flags & !SCOPES`, which accepts every *combination* of known bits; Linux's
rule is the four values themselves. `probe_flags.c` was written to settle it (3, 5,
6, 7, 8 all `EINVAL`), the handler now matches `0 | 1 | 2 | 4`, and the claims are the
pin. That red is also this item's counterfactual: with the mask restored, the guest is
red and the host stays green, which is why the family is pinned on the machine that
runs this kernel rather than only on the host.

**Pins**: `t-pidfd` (new, default group, both machines) - 100 claims on each, the
`ok:` text identical (the first machine's console writes CRLF) - covering the descriptor
shape and refusals, the non-leader `ENOENT`, pid 1's `ECHILD` for a non-child, the
live/zombie/reaped lifecycle, epoll readiness, the delivery records and all four
scopes, the zombie scopes, non-blocking waits, `F_SETFL`, and the self-targeted
verbatim record. Beside
it, `t-waitid` moved its `P_PIDFD` line from "refused `EINVAL`" to the measured
`EBADF`/negative-`EINVAL` pair, and the host suite gained the type-less `st_mode`
claim in `abi::tests` (where `mode_of` owns it) and three tests in `vfs::pidfd`'s
module.

**Gate**: x86_64 `pass=194 fail=0` (194 built, 6 lanes), aarch64 `193 staged,
pass=193 fail=0` (14 left out by `tests.toml`), `mpos-kunit 32 passed, 0 failed` on
both; host unit suite 589 (was 586), `xtask` 139; `cargo fmt --check` clean; the
depgraph ratchet holds - the new calls are `vfs -> sched` (an edge `vfs/eventfd.rs`
and `vfs/inotify.rs` already had) and `syscall -> vfs`, both recorded.

**Named**: `pidfd_getfd(2)` (438), `clone3`'s `CLONE_PIDFD` and
`process_madvise(2)` (440) are not served and fall to the `ENOSYS` catch-all; this
family does not invent a descriptor where those calls expect one. **The first of
the three was served by §6.9**, below. The older
anonymous descriptors (`eventfd`, `timerfd`, `signalfd`, `epoll`, `inotify`) still
report `S_IFCHR` - a divergence `NodeKind::Anon` leaves in place rather than
silently changing. `st_blksize` stays this kernel's 512 where Linux's pidfd answers
4096 (the existing global choice, not a pidfd-specific one), and `st_dev`/`st_ino`/
uid/gid are machine-specific and deliberately not pinned. `/proc/<pid>/fdinfo/<n>`
for a pidfd (`Pid:`/`NSpid:`) was not measured and is not promised. A pidfd's
`O_NONBLOCK` wait does not distinguish "no event yet" from "the child is running" by
anything other than the `EAGAIN` Linux gives both.

### 6.9 A fetched descriptor is the same open file, not a copy of it — **LANDED** (2026-10-21)

`pidfd_open` names a process; `pidfd_getfd(2)` is the door that reaches through
that name into the process's descriptor table and duplicates one entry. The
object it duplicates is the **open file description**, the thing `dup` shares,
not a snapshot of the descriptor's flags and position: two fetches of the same
target read through one offset, and closing one leaves the other.

**Measured on a current host first** (Linux 7.2.11,
`.workbuddy/probe_pidfd_getfd{,2}.c`), because the refusal order is not the
order the arguments are written in:

* flags → pidfd descriptor → task → authority → target fd. A non-zero flag word
  is `EINVAL` even beside `pidfd = -1`; a number naming nothing, or a valid
  descriptor that is not a pidfd, is `EBADF`; a reaped process, a zombie, and a
  leader that exited under a live worker are all `ESRCH`; a non-dumpable
  same-uid target is `EPERM` **before** the target fd is looked at (a
  non-dumpable target with fd `999999` answers `EPERM`, not `EBADF`); only
  then is a missing target fd `EBADF`. `-1` and `999999` were the two measured
  missing numbers.
* the new number is the lowest free one and `F_GETFD` carries `FD_CLOEXEC`
  unconditionally; `F_GETFL` carries the target's access mode.
* the shared description is visible in the offsets: fetch twice, read three
  bytes through the first, and the second's position is three; a read of two
  through the second then returns bytes four and five.
* a live, same-uid process is enough - no parent relation. A process always
  fetches its own, and a fetched pidfd is still a pidfd (`poll` and
  `pidfd_send_signal` go through it).
* a zombie is gone: both before the reap and after it, `pidfd_getfd` answers
  `ESRCH`.

The first probe's mistake is worth keeping: it used one pipe in both
directions, so the parent's close of the write end ended the child's read and
every fetch answered `ESRCH` against a child that had already exited. The
second probe added a command pipe, and the live child answered normally. The
`ESRCH` was real; the fixture was the thing that was wrong.

**The landing.** `PIDFD_GETFD = 438` sits beside the other two numbers in both
`nr_*` tables. `proc::handle_pidfd_getfd` resolves the pidfd through the
caller's table, checks `sched::pidfd_still_names` before asking
`sched::may_inspect` (so a slot handed to another tenant is "gone", not
"somebody else's process"), and then takes the target's table from the new
`sched::pidfd_fd_table`. That helper is the one place the `ESRCH` rule lives:
it answers `None` for a free slot, a reused generation, or a task that has
exited. The description is `FdTable::clone_at`'s `Arc<File>` - the same
description, which is where the shared offset comes from - and
`current_fds().install` gives it the lowest free number before
`fd_flags_set(..., FD_CLOEXEC)` spends the one flag the door never asks about.

**The guest caught a measurement that did not transfer.** The host run is uid
1000, so the main process could make a target non-dumpable and get `EPERM` from
`pidfd_getfd`. The guest runs the test program as root, and root holds
`CAP_SYS_PTRACE`, which `ptrace_may_access` answers before it reads the
dumpable flag. The two EPERM claims were red on the first sweep while the host
was green. The pin moved to a fixture whose target and caller both attempt to
drop to uid 65534: on the guest the drop succeeds, on an already-unprivileged
host it is a no-op and the same uid is already the shape the rule needs. That
is the measured rule now, on the machine that serves it.

**Pins**: `t-pidfd` 100 → **121 claims** on both machines, the `ok:` text
identical (the first machine writes CRLF). The 21 new claims are the order and
its five errnos, the two fetches' shared offset, `FD_CLOEXEC`, the access mode,
a fetched pidfd, the unprivileged authority pair, a process's own descriptor,
and the zombie's `ESRCH` both before and after the reap. No host test: the
helpers `pidfd_fd_table` and `may_inspect` are behind `target_os = "none"`, and
the ABI's claims are the program's.

**Gate**: x86_64 `pass=194 fail=0` (194 built, 6 lanes), aarch64 `193 staged,
pass=193 fail=0` (14 left out by `tests.toml`), `mpos-kunit 32 passed, 0 failed`
on both; host unit suite unchanged (no host test was added), `xtask` 139;
`cargo fmt --check` clean; depgraph holds - `syscall -> sched` is an edge the
pidfd family already recorded.

**Named**: `process_madvise(2)` (440) is still `ENOSYS`; `clone3`'s
`CLONE_PIDFD` was served by §6.10, below. The authority rule keeps §2.6's
divergence: Linux's
`PTRACE_MODE_ATTACH_REALCREDS` compares group ids as well and consults the
security modules, while `cred::ptrace_may_access` compares the real/effective
uid set; on this single-user image no gate can tell the two apart. A pidfd's
`fdinfo` (`Pid:`/`NSpid:`) remains unmeasured and un-promised, and the older
anonymous descriptors still report `S_IFCHR`.

### 6.10 `clone3` is `clone` with a struct, and its `CLONE_PIDFD` names the child it just made — **LANDED** (2026-10-21)

`clone(2)` is a 32-bit flag word whose low byte is an exit signal, five
positional arguments and no room to grow. `clone3(435)` replaces all of that
with one versioned `struct clone_args`, and the door matters here for one flag:
`CLONE_PIDFD` delivers the child **and a descriptor naming it** in the same
call. Item 92 closed the pid-reuse hole for a number already held by recording
the slot's generation; this closes it for the creation itself, where a caller
would otherwise have to `pidfd_open` a number it was handed and hope nothing
was scheduled in between.

**Measured on a current host first** (Linux 7.2.11,
`.workbuddy/probe_clone3{,b,c,d,e,f,g,h,i}.c`), and every rule below is a call,
not a reading of the man page:

* **the size decides everything else.** Below `CLONE_ARGS_SIZE_VER0` (64) is
  `EINVAL`, and it is asked *before* the pointer: a bogus pointer with
  `size = 8` is `EINVAL`, with `size = 64` it is `EFAULT`. The fields between
  the caller's `size` and `VER2` (88) read as zero, and a `size` past `VER2` is
  allowed only while every byte past it is zero - otherwise `E2BIG`. So
  `size = 89`, `96` and `128` with a zero tail all start a child, and a
  `size = 128` struct with one nonzero byte at 100 is `E2BIG`.
* **`stack` is the base, not the top.** `clone(2)`'s `child_stack` is the stack
  pointer itself; `clone3`'s `stack` is the bottom of a region and the child
  starts at `stack + stack_size`. `base + 0` and `0 + size` are both `EINVAL`,
  and `0 + 0` means the child inherits the caller's stack pointer - the fork
  shape. This is the one rule a probe got wrong first: passing the *top* as
  `stack` put the child's `%rsp` one mapping past the end, and the fault read as
  a kernel fault rather than a fixture mistake.
* **`exit_signal` is a field of its own**, 0 through `SIGRTMAX` (64); 65 is
  `EINVAL`. It is also a real delivery: a child born to report 64 kills its
  parent when it dies, which is how the pin's own first host run ended.
* **the flags are `clone`'s plus the three a 32-bit word cannot carry.**
  `CLONE_SIGHAND` without `CLONE_VM`, and `CLONE_THREAD` without both, are
  `EINVAL` (the chain `clone(2)` enforces). `CLONE_CHILD_SETTID` **is** served
  and needs no pointer; `CLONE_PARENT_SETTID` needs none either and a
  destination the caller cannot write is *skipped*, not refused (Linux drops
  `put_user`'s result). `CLONE_DETACHED` - which `clone(2)` still keeps as a
  historical no-op - is `EINVAL` here. An unknown bit is `EINVAL`, and that is
  *Linux's own* `clone3` check, not this kernel's divergence: unlike `clone(2)`,
  which starts a child for an unknown bit, `clone3` validates its flags.
* **`CLONE_PIDFD`**: `pidfd = 0` is `EFAULT` (the write is what fails);
  `pidfd == parent_tid` beside `CLONE_PARENT_SETTID` is `EINVAL` (two writes to
  one address) while *different* destinations are served - measured three ways,
  because the first draft refused the pair unconditionally.
* **`CLONE_INTO_CGROUP`** is `EBADF`: there is no cgroup tree here for a
  descriptor to name, which is exactly Linux's answer for an fd that is not a
  cgroup.
* **`set_tid`/`set_tid_size`**: one set alone, or a size above one, is `EINVAL`;
  one well-formed entry is `EINVAL` without `CLONE_NEWPID` (Linux's own answer -
  the entry names a number in a namespace the child is not entering) and
  `EOPNOTSUPP` with it, where this kernel has no ordered `set_tid` support.

**Two probe mistakes are worth keeping.** The first draft's rule "clone3 rejects
`CLONE_CHILD_SETTID`" was measured with the wrong constant: the C probe spelled
`CLONE_CHILD_SETTID` as `0x400000`, which is `CLONE_DETACHED`, so every row
measured one flag's answer under another's name. The pin's own host run caught
it - the corrected probe is `probe_clone3h.c`, and `CLONE_DETACHED` is the bit
`clone3` actually refuses. The second: a probe whose child fell out of the raw
syscall and carried on through `main` forked a tree of copies of itself, because
a `clone3` child returns from the call and does **not** enter at a function
pointer the way a libc's `clone` wrapper arranges. The test's own child branch
and the small `global_asm` trampoline (used only for the claim that a child with
a stack of its own really starts at the top of it) are that lesson applied.

**The landing.** `CLONE3 = 435` in both `nr_*` tables; `crates/abi` gains
`CLONE_PIDFD`, `CLONE_CLEAR_SIGHAND` and `CLONE_INTO_CGROUP`; the struct's decode
and every rule above live in `abi::clone3_read_len`/`clone3_tail_is_zero`/
`clone3_verdict`, which are host-compiled and host-tested; `proc::handle_clone3`
reads the struct, asks the verdict, builds the same `CloneRequest` the legacy
door builds and hands it to `sched::clone`; and `pidfd_for_task` - the one place
a pidfd's descriptor is built, now shared by `pidfd_open` and `CLONE_PIDFD` -
mints the descriptor, with a child killed rather than left unnamed when the
descriptor cannot be delivered.

**Pins.** **`t-clone3`**, a new program, **37 claims**, green on a current host
first and the 37 `ok:` lines byte-identical on the two machines. The claims that
are the door: `CLONE_PIDFD`'s descriptor is close-on-exec, carries `O_RDWR`,
names the child while it is alive and answers `ESRCH` after the reap, and is a
*different* number from a later `pidfd_open`'s - so it is the call's own name and
not one fetched afterwards. The size discipline, the exit signal, the flag
refusals and the stack pair are each a claim, and `CLONE_CHILD_SETTID`'s write is
read back **by the child** (`gettid` against the parked value, because an exit
code carries eight bits and a caller cannot compare the numbers itself).
`CLONE_CHILD_SETTID`'s shared-memory shape (`CLONE_VM`) is deliberately *not*
claimed: this kernel copies for a non-thread `clone(CLONE_VM)` rather than
sharing, so only the child can see the write. Host tests:
`clone3_args_are_validated_in_linuxs_order` and three `mpos-abi` constant
assertions. The counterfactual - the dispatch arm removed, so the number falls to
`_ => ENOSYS` - is **36 claims red** (the count of the run it was measured on)
and nothing else in the sweep red.

**Gate**: x86_64 `pass=195 fail=0` (195 built, 6 lanes), aarch64 `194 staged,
pass=194 fail=0` (14 left out by `tests.toml`), `mpos-kunit 32 passed, 0 failed`
on both; host unit suite 590, `mpos-abi` 5, `xtask` 139; `cargo fmt --check`
clean; clippy 0 errors on `x86_64-unknown-none` and `aarch64-unknown-none`;
depgraph holds - `syscall -> sched` was already an edge.

**Named**: `process_madvise(2)` (440) is now the pidfd family's only `ENOSYS`,
and it was probed before it was deferred: its one advice with real work behind it
(`MADV_DONTNEED`) is refused `EINVAL` for another process on this host, and the
advices it does accept (`MADV_WILLNEED`/`COLD`/`PAGEOUT`/`COLLAPSE`) are hints
this kernel answers as no-ops, so a door there would be a shape with nothing
behind it. `CLONE_CLEAR_SIGHAND` is `EOPNOTSUPP` because this kernel's `fork`
already starts every process at `SIG_DFL` whatever the flags said - so accepting
the bit could not be told from ignoring it - and *that* inheritance rule, not
this flag, is the gap to close first. `CLONE_NEWTIME` through this door is
`EOPNOTSUPP` (this kernel has no clock namespace to hand a child through `clone`;
`unshare(CLONE_NEWTIME)` is the door that serves the clocks, §6.3).
`CLONE_PTRACE` and any unknown bit are `EINVAL` (this kernel's designed
unknown-bit rule, shared with `clone(2)`). And `clone3` inherits `clone`'s
`CLONE_VM`-without-`CLONE_THREAD` divergence: the child is a copy, not a sharer,
which `vfork`'s own note states as the deliberate choice.
