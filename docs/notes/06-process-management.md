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
| `CLONE_NEWCGROUP` | the cgroup tree a process sees | **served** (2026-10-25, §6.25): the tree exists now — organizational, controller-less, measured on a current host before it was built (`src/vfs/cgroupfs.rs`'s header carries the probe) |
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

**What the work needed, and it is the interesting part:** `execve` maps an image's pages *eagerly* — it fills every page as it loads — so an image leaves **no VMAs at all**, and the same is true of a `MAP_PRIVATE` mapping of an ordinary file. A maps file built from the VMA table alone would therefore omit the one line a crash reporter actually wants: the program's own code. The loader's ranges and the private file mappings are recorded *beside* the table (`mm::vma::MappedRange`) rather than put in it, and the reason is the fault path rather than tidiness: recording them as `VmaKind::Anon` VMAs would make an address that should take a fault — one whose frame a `munmap` or a `MADV_DONTNEED` gave back — silently refill with zeros, which for a program's own code means blank bytes where the file's bytes belong. The cost of that choice is named with it: those ranges are the *load's* record, so a `munmap` or `mprotect` over one of them — a dynamic program's `RELRO` pass does the second on every start — has to be carried to the list or the report describes mappings the address space no longer has. That is now the list's own job: `forget_ranges` clips the recorded ranges on the way out (`munmap`), `reprot_ranges` splits and re-flags them (`mprotect`), and `move_ranges` carries them on `mremap` — each called from the same place the VMA table is edited, so the two descriptions cannot drift apart.

**And it is pinned now** (`t-maps`, 2026-10-23): a `MAP_PRIVATE` mapping of a three-page `memfd` — the shape whose pages come from the recorded range and not from a VMA — then `munmap` of its middle page and `mprotect` of its first. The file must split around the hole (no line at the hole, one line ending at it, one starting after it) and show `r--p` on the page the call named while the page it did not keeps `rw-p`; measured on the host first, where the same three observations come out of a VMA rather than a recorded range. Until this pin the two calls were wired and *nothing asserted them*, which is the "a complete driver plus green host tests" shape §13.29 warns about.

**Two divergences, pinned as divergences:** an image's and a private file mapping's line reports `dev` `00:00` and inode `0`, because there is no object left to name — and `00:00` is what this kernel's own `stat` reports for every filesystem, so the two files agree rather than disagreeing. And a *shared anonymous* mapping is named `/memfd: (deleted)`, which is Linux's own name for a nameless `memfd` mapping (measured: `memfd_create("", 0)` + `MAP_SHARED` reports exactly that), because that is what the object here is; Linux reaches the same shape through `/dev/zero (deleted)`, which is an artefact of its own implementation. `memfd_create` now *keeps* the name it used to only validate, as `/memfd:<name> (deleted)` — the spelling both of its readers expect, measured on Linux — so a `wl_shm` pool's mapping says which buffer it is instead of leaving the column blank. Not written: `/proc/<pid>/numa_maps` and `/proc/<pid>/smaps_rollup` - the same accounting in two other spellings, whose formats are measured in `docs/uapi/` §5.2. (`/proc/<pid>/smaps` *is* written now, and it is this file's records with a field block under each: item 54, `docs/uapi/` §5.1, §15.73.)

**The offset column is a byte offset, and the line a copy used to hide it** (2026-11-01). `t-maps` now pins the column itself: a `memfd` mapped at byte 4096 reads `00001000` and one at 8192 reads `00002000`, with the memfd's own inode in the inode column — measured on Linux 7.2.11 first (`vm_pgoff << PAGE_SHIFT`, not `vm_pgoff`; a kernel printing pages would be off by 4096 and every tool that parses the column with it). The counterfactual (`line.offset / 4096` in the renderer) reddens exactly those two claims and nothing else, on both mappings, so the column is pinned as the shape rather than as a spelling.

What that extension also uncovered is a defect this note has to name rather than fix in place: extending the same block to a **private file mapping** made the pin red on a pre-existing claim, and a dump showed the same `rw-s` range printed **twice** — an unnamed, inode-`0` line above the named one — so `line_at` (first match) read the line with no object behind it. Both lines are `rw-s`, and `mapping_lines` renders a recorded eager range `rw-p`, so the second line is not an eager range; the VMA table is a `BTreeMap` keyed by start and cannot hold two entries for one range, so the duplicate's origin is not yet established. A dedupe attempt (skip an eager range any VMA overlaps) did not clear it and was reverted; the duplicate's own claim was therefore dropped from this pin rather than left red. **Next instrument**: dump the snapshot (`facts.vmas.iter()` and `facts.eager`) beside the parsed file for the run that fails, and compare start/end/permissions of the two lines — the pin's own address-dependent flakiness (it failed in 2 of 5 runs with the extra mappings present) says the pair is produced by a mapping this pin creates, whose range then collides with a stale record.

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
the three was served by §6.9** and **the third by §6.28**, below (the middle one
is this section's own call's flag and landed with §6.10), and the older anonymous
descriptors' type-less shape by §6.11. `st_blksize` stays this kernel's 512 where Linux's pidfd answers
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

**Named**: `process_madvise(2)` (440) was still `ENOSYS` at this landing and was
**served by §6.28**, below; `clone3`'s `CLONE_PIDFD` was served by §6.10, below.
The authority rule keeps §2.6's divergence: Linux's
`PTRACE_MODE_ATTACH_REALCREDS` compares group ids as well and consults the
security modules, while `cred::ptrace_may_access` compares the real/effective
uid set; on this single-user image no gate can tell the two apart. A pidfd's
`fdinfo` (`Pid:`/`NSpid:`) remains unmeasured and un-promised. The older
anonymous descriptors' `S_IFCHR` divergence landed as §6.11.

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
* **`CLONE_INTO_CGROUP`** was `EBADF` when this landed: there was no cgroup
  tree here for a descriptor to name, which is Linux's own answer for an fd that
  is not a cgroup. **Served since 2026-10-25** with the tree (§6.26) — the fd is
  resolved, `EBADF`/`ENOENT`/`EACCES` answer what it names, and `CLONE_THREAD`
  beside the flag is `EOPNOTSUPP`.
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
behind it. **It was served by §6.28**, below, on that measurement read the other
way: the shape - a byte count for an answer, a fixed refusal order, and the
caller's own range walked and dropped as `madvise` walks it - is the service, and
`t-process-madvise` pins every branch of it. `CLONE_CLEAR_SIGHAND` was
`EOPNOTSUPP` at this landing because a `fork` here
started every process at `SIG_DFL` whatever the flags said - so the flagged and
unflagged child were identical. The inheritance landed as §2.7 and the flag is
served now (item 96): a plain child carries the parent's table and the flagged
one starts at `SIG_DFL`, with the `CLONE_SIGHAND` pair `EINVAL`; `t-clone3`
holds all of it. `CLONE_NEWTIME` through this door is
`EOPNOTSUPP` (this kernel has no clock namespace to hand a child through `clone`;
`unshare(CLONE_NEWTIME)` is the door that serves the clocks, §6.3).
`CLONE_PTRACE` and any unknown bit are `EINVAL` (this kernel's designed
unknown-bit rule, shared with `clone(2)`). And `clone3` inherits `clone`'s
`CLONE_VM`-without-`CLONE_THREAD` divergence: the child is a copy, not a sharer,
which `vfork`'s own note states as the deliberate choice.

### 6.11 The anonymous descriptors are Linux's type-less shape — **LANDED** (2026-10-23)

The pidfd learned Linux's `anon_inode` shape in §6.8; the five older members of
the family did not, and the difference was visible to any `/proc` reader. On
Linux none of `eventfd`, `timerfd`, `signalfd`, `epoll`, `inotify` or `pidfd`
has an `S_IF*` bit in `st_mode` - they are not files of any type, which is what
makes `S_ISCHR` a question a device probe must not answer yes to. This kernel
reported the first five as `S_IFCHR` and named them `anon:[<ino>]` in
`/proc/<pid>/fd`, because only `pidfd` had a `fd_name` of its own.

**Measured on a current host first** (`.workbuddy/probe_anon_fd_stat.c`,
`.workbuddy/probe_anon_read0.c`, `.workbuddy/probe_inotify0{,b}.c`, Linux
7.2.11): every row is type 0; the permission word is `0600` for the five and
`0700` for the pidfd; the links are `anon_inode:[eventfd]`, `[timerfd]`,
`[signalfd]`, `[eventpoll]`, `anon_inode:inotify` (the one without brackets) and
`[pidfd]`. A zero-length read is `EINVAL` for `eventfd`/`timerfd`/`signalfd`/
`epoll`/`pidfd` whatever the queue holds, while inotify consults the queue
first: `EAGAIN` when empty and non-blocking, `EINVAL` once a record is queued,
and a wait followed by `EINVAL` when blocking.

**The landing.** `NodeKind::Anon` now carries the permission word
(`Anon { mode }`, the type is always zero), so one kind expresses both rows
without inventing a mode field on `Metadata`; all six inodes ask for it. Each
of the five implements `fd_name`, which `File::display_name` already consulted
before its fallback. `InotifyInstance::read_at` answers the zero-length case in
Linux's order. One consequence is worth stating because it is the read door's
rule rather than the stat door's: `File::read`'s zero-length short-circuit
exempts `Anon` (the pidfd needed that), so the five now reach their own
`read_at` with an empty buffer - which is exactly where their `EINVAL` lives.

**Pin**: the new `t-anon-fd`, 26 claims (three per descriptor, the seven
zero-length answers, and a path-opened file's link as the control), green on
the host first and with identical `ok:` text in the guest on both machines.

**Gate**: x86_64 199 programs, `pass=199 fail=0`; aarch64 198 staged,
`pass=198 fail=0`; `mpos-kunit 32 passed, 0 failed` on both.

**Named**: `st_blksize` remains this kernel's 512 where Linux answers 4096 (the
global choice, not an anon-inode one); `st_dev`/`st_ino`/uid/gid are
machine-specific and not pinned; a pidfd's `fdinfo` (`Pid:`/`NSpid:`) is still
unmeasured and un-promised.

### 6.12 POSIX message queues — **LANDED** (2026-10-23, the IPC namespace's missing half)

`unshare(CLONE_NEWIPC)` was refused because there was nothing for it to isolate -
"SysV IPC objects and POSIX message queues", and this kernel had neither (§6.3).
The queue half is here now: `mq_open`, `mq_unlink`, `mq_timedsend`,
`mq_timedreceive`, `mq_notify` and `mq_getsetattr`, the objects in
`vfs::mqueue` and a validating face in `syscall/linux/ipc.rs`. `CLONE_NEWIPC`
itself is still refused, and its reason has shrunk to SysV.

**Measured whole on a current host first** (Linux 7.2.11,
`.workbuddy/probe_mqueue{,2}.c`), because almost every rule is a choice between
two plausible answers:

* **the name is bare.** Both libcs strip the leading `/` the caller writes and
  reject a name without one, so the *kernel* receives `name`, not `/name` - and
  the raw probe (`.workbuddy/probe_mqueue_names.c`) measured the kernel's own
  rule: 255 bytes fits and 256 is `ENAMETOOLONG`, **any** slash is `EACCES`, and
  the empty name is `ENOENT`. The first draft of this landing implemented the
  libc's rule instead and every `mq_open` on the image answered `EINVAL`, which
  is how the wrapper/syscall split was found.
* **the attributes**: 10 messages of 8192 bytes by default, `mq_flags` 0,
  `mq_curmsgs` 0; `O_CREAT|O_EXCL` on an existing name is `EEXIST`; `maxmsg` 0 is
  `EINVAL` and 11 (past `msg_max`) is `EINVAL` - not a quiet clamp.
* **the order**: highest priority first, FIFO within a priority (`9, 5a, 5b, 1`
  out of an arrival order of `1, 9, 5a, 5b`); `MQ_PRIO_MAX` is 32768, so 32767
  is accepted and 32768 is `EINVAL`.
* **full and empty**: `EAGAIN` to a non-blocking caller, `ETIMEDOUT` on an
  expired timeout, and the timeout is an **absolute `CLOCK_REALTIME`** instant.
* **the descriptor's mode**: `mq_send` on `O_RDONLY` and `mq_receive` on
  `O_WRONLY` are `EBADF`; `mq_getattr`'s `mq_flags` is the descriptor's
  `O_NONBLOCK` bit, which `mq_setattr` writes and `F_SETFL` writes too.
* **size**: a message wider than `mq_msgsize` is `EMSGSIZE`, and so is a receive
  buffer narrower than `mq_msgsize` whatever the queued message's own length is.
* **one registration per queue**, `EBUSY` on a second; the send that finds the
  queue **empty** fires it (`SI_MESGQ`, code `-3`, the registration's `sigval`)
  and the firing clears it. `SIGEV_NONE` is *not* the unregister call: it takes
  the slot like any other registration (measured twice), and a registration whose
  process exits stops holding it.
* **`unlink` retires the name, not the object**: a reopen is `ENOENT` and a
  second unlink is `ENOENT` while the descriptor that already held the queue
  keeps working.
* **readiness**: `POLLOUT` alone on an empty queue, `POLLIN|POLLOUT` with a
  message, `POLLIN` alone when full, and `epoll_wait` agrees. That one needed a
  second fix: `epoll_ctl(ADD)` refused a queue because it reports `S_IFREG` and
  the watchability gate named only pipes, duplex ends and sockets - so a
  watchable file was refused `EPERM`, the same mistake the socketpair end was
  (measured: Linux reports a full queue's `EPOLLIN`).

**The shape that is not Linux's, and is named**: the descriptor is `S_IFREG`
with mode `0600` (Linux's mqueue filesystem says so too) but `st_size` is 0 where
Linux reports the mqueuefs inode's 80 bytes - there is no mqueuefs here, so
there is no directory entry to count. `read`/`write` on the descriptor are both
`EINVAL` (Linux's `write` is `EINVAL` too; its `read` answers the mqueuefs
inode's bytes, a filesystem quirk with nothing to reproduce). The `maxmsg` and
`msgsize` ceilings are constants, not `/proc/sys/fs/mqueue` tunables.
`RLIMIT_MSGQUEUE` is charged at creation by the queue's maximal size and is the
memory bound (`default_rlimits` now seeds Linux's 819200 rather than infinity,
which §8.0's allocator needs), but the accounting is per-uid and the sysctl that
would move it does not exist.

**Pin**: the new `t-mqueue`, 41 claims, using `libc` for the whole family
because `rustix` binds no `mq_*` call - the first clause of
`user/tests/Cargo.toml`'s rule.

**Gate**: x86_64 `pass=201 fail=0 (214 built)`, `mpos-kunit 32 passed, 0
failed`, exit 0. The second machine compiles the change; its sweep could not
boot on this host (no DRM render node), so no aarch64 figure is claimed.

**Named, not claimed**: the IPC namespace itself is still refused (`SysV IPC
objects` remain), the `/dev/mqueue` view does not exist, and a blocked
`mq_timedsend`/`mq_timedreceive` is pinned only by the timeout side - the
blocking rendezvous (a receiver waking a full-queue sender) is implemented and
unclaimed here because no program pins it yet.

### 6.13 System V shared memory — **LANDED** (2026-10-23, SysV's first family)

`CLONE_NEWIPC`'s reason has now been narrowed twice. It was "SysV IPC objects and
POSIX message queues" (§6.3); §6.12 took the queues; this takes the first of the
three System V families - `shmget`, `shmat`, `shmdt`, `shmctl`, plus the
`/proc/sysvipc/shm` row `ipcs` renders. What is left of the namespace's reason is
the semaphore and message-queue families, and those wait on nothing else.

**The object, and where it lives.** A segment is not a file and not an
address-space range: it is a frame-backed object a number names, which any
process holding the number may attach, and whose pages every attachment aliases.
So it is `mm::sysv_shm` - the segment table and the `MappedObject` implementation
the fault path already asks for - and not `vfs`: nothing opens it by path,
`mmap` is not its door, and the two readers that do name it (`shmat`'s mapping and
`/proc/<pid>/maps`) both speak `MappedObject`. The id is Linux's arithmetic,
factored into `mm::ipc_id` because the other two families will use it verbatim:
`id = slot + generation * 32768`. Measured on a current host
(`.workbuddy/probe_shm_ids.c`): a fresh boot's first segment is `0` with
`shm_perm.seq` 0, and after that slot has been removed and made again the number
is `32768` with `seq` 1 - so the generation belongs to the *slot*, not to the
namespace, which is what makes a retained number answer `EINVAL` instead of
naming a stranger.

**Measured whole on a current host first** (Linux 7.2.11,
`.workbuddy/probe_sysv_shm.c`, `probe_shm_perm.c`, `probe_shm_ids.c`,
`probe_shm_rmid.c`, `probe_shm_order.c`), because almost every rule is a choice
between two plausible answers:

* **the size is what was asked for**: `shm_segsz` reports 100 for a 100-byte
  segment, and the mapping is one whole page - the two numbers differ on purpose;
* **no umask**: `shmget(..., 0666)` under `umask 0777` stores `0666`;
* **the errno order on an existing key** is `EEXIST` (from `IPC_CREAT|IPC_EXCL`),
  then `EINVAL` (a size larger than the segment; a smaller one is accepted), then
  `EACCES` (only when `shmflg` names permission bits at all - `shmget(key, 0, 0)`
  is answered with the id, no check). Measured by refusing all three against one
  `0600` segment owned by another user;
* **attach placement**: `shmat(NULL)` returns a page-aligned address and
  successive ones grow **down** a page at a time; an explicit address is taken
  exactly when free, a **busy one is `EINVAL` and not relocated** (unlike a plain
  `mmap` hint), a misaligned one is `EINVAL` unless `SHM_RND` rounds it down, and
  `SHM_REMAP` replaces what is there. Two attaches of one segment alias pages;
  `SHM_RDONLY` gives a mapping a write `SIGSEGV`s on;
* **`shm_nattch` counts address spaces, not the segment's own bookkeeping**: a
  `fork` copies the mapping and the child reads one higher, and its exit takes
  that back. That is why there is no counter field on the object at all: the
  truth is a fold over every VMA table, and `sched::shm_attachment_count` is
  that fold (counting *runs*, so a split mapping is still one attachment);
* **`IPC_RMID` marks, it does not remove**: while one attachment lives the number
  keeps answering `IPC_STAT`, a further `shmat` succeeds, and the key reads back
  as `IPC_PRIVATE` - and a `shmget` on that key makes a **different** segment.
  Only after the last detach is the number `EINVAL`;
* **three refusals, three errnos**: `shmat` of another user's `0600` segment is
  `EACCES` (the triads, with `CAP_IPC_OWNER` behind them), `IPC_STAT` is `EACCES`
  (the same check), and `IPC_SET`/`IPC_RMID` are `EPERM` (the *creator* or
  `CAP_SYS_ADMIN` - `ipcctl_obtain_check`). `IPC_SET` moves uid/gid/mode and
  leaves the creator alone; the mode update preserves the bits above the triads;
* **`SHM_LOCK`** sets a mode bit read back as `02600`, clears on `SHM_UNLOCK`,
  and on this host asks no capability at all - an unprivileged caller locks a
  segment it does not own (`.workbuddy/probe_shm_lock.c`, both as uid 1000 and as
  root). The bit's value is the measurement's (`0o2000`), not an older header's;
* **the table-wide reports**: `IPC_INFO`/`SHM_INFO` return the highest slot index
  (the number `ipcs` walks), `SHM_STAT(index)` answers the segment's id and
  refuses one the caller may not read with `EACCES`, and `/proc/sysvipc/shm`
  prints the header and column widths measured off the host - the key column is
  **decimal**, 10 wide, which is what a reader that splits the line counts.

**What this kernel's numbers are, and why they are its own.** `SHMMAX` is 64 MiB,
`SHMALL` 32768 pages (128 MiB) and `SHMMNI` 4096; `IPC_INFO` reports exactly
those. The page allowance is charged at **creation** from the size asked for - as
Linux's `shm_tot` is - and the frames themselves arrive on the first touch, so a
process cannot promise itself more memory than exists by creating many large
segments and touching one page of each. A limit is not optional while the
allocator panics on failure (§8.0), and a smaller `SHMMAX` is a *configuration*
difference (containers routinely lower it) rather than a shape difference; what
it costs is that a request Linux would answer `ENOMEM` is `EINVAL` here, and that
is named below rather than hidden.

**Named, not claimed**:

* **the other two SysV families** - semaphores (`semget`/`semop`/`semtimedop`/
  `semctl`) and message queues (`msgget`/`msgsnd`/`msgrcv`/`msgctl`) - are still
  `ENOSYS`, so `CLONE_NEWIPC` is still refused and its reason is now those two;
* **the table is the machine's**, not a namespace's: `CLONE_NEWIPC` would give a
  child its own copy, and until that kind exists there is one table to name;
* **`SHM_LOCK` sets a bit and nothing else** - there is no swap here for a lock to
  hold a page out of, so the honest content of the call is the mode word;
* **`shm_rss`** is the pages that have a frame, not a swapped-out accounting
  (`shm_swp` is always 0 for the same reason);
* **`SHMMAX`/`SHMALL` are constants**, not `/proc/sys/kernel/*` tunables, and this
  kernel's `SHMMAX` is far below Linux's default "the address space".

**Pin**: the new `t-sysv-shm`, 61 claims, `libc` for the family (`rustix` binds no
System V IPC call) and the three `shmctl` records spelled as `#[repr(C)]`
structs because the layout *is* the ABI - the asm-generic one on both machines,
with the offsets the host probe printed. It also walks `/proc/sysvipc/shm` and
the `IPC_INFO`/`SHM_STAT` pair `ipcs` uses.

**Gate**: x86_64 `pass=202 fail=0 (214 built)`, `mpos-kunit 32 passed, 0 failed`,
exit 0. The second machine compiles the change (kernel release + clippy, and the
pin builds for it); its sweep could not boot on this host (no DRM render node), so
no aarch64 figure is claimed.

### 6.14 System V semaphores — **LANDED** (2026-10-23, SysV's second family)

`CLONE_NEWIPC`'s reason is down to one item. It was "SysV IPC objects and POSIX
message queues" (§6.3); §6.12 took the queues, §6.13 the shared-memory family,
and this takes the semaphores - `semget`, `semop`, `semtimedop`, `semctl`, the
`SEM_UNDO` bookkeeping behind them, and the `/proc/sysvipc/sem` row `ipcs`
renders. What is left of the namespace's reason is the SysV **message-queue**
family.

**Where the objects live, and why there.** A set is an array of counters plus the
*wait* an operation that cannot be satisfied asks for, so it is
`sched::sysv_sem`: the waiting is the scheduler's, and so is the `SEM_UNDO`
adjustment, which is per-process state applied at death (ARCHITECTURE §3's V-3).
It sits beside `futex` for the same reason that module does. The number a set is
known by is the segment table's arithmetic reused verbatim (`mm::ipc_id`).

**Measured whole on a current host first** (Linux 7.2.11,
`.workbuddy/probe_sysv_sem.c`, `probe_sem2.c`, `probe_sem3.c`,
`probe_sem_unshare.c`, `probe_ipc_order.c`), because almost every rule is a
choice between two plausible answers:

* **`semtimedop`'s timeout is relative** - a 120 ms request returned after
  120.5 ms - and an expired one is **`EAGAIN`**, not `ETIMEDOUT` the way the
  POSIX message queue's absolute instant is;
* **the operation array is atomic**: `semop([+1 s0, -1 s1(0)], IPC_NOWAIT)` is
  `EAGAIN` and leaves `s0` alone. `sem_num` past the set is `EFBIG` (where
  `semctl`'s `semnum` is `EINVAL` - two calls, two errnos), more than 500
  operations is `E2BIG`, an empty array is `EINVAL`, and `sem_flg` bits the
  kernel does not know are **ignored** (`0x2000` accepted);
* **the range refuses at both ends**: `semop(+1)` at 32767, `SETVAL(32768)` and
  `SETVAL(-1)` are all `ERANGE` - the negative one included, which is why the
  handler reads the argument as a signed `int` rather than a clamp;
* **`GETALL`/`SETALL` are `unsigned short` arrays**, `nsems` long - the one place
  this family's values are 16-bit on the wire;
* **a blocked `semop`** is counted by `GETNCNT`/`GETZCNT`, woken by another
  process's raise (50.6 ms observed), **ended by a handler-bearing signal**
  (`EINTR`, where a merely blocked signal does not end it) and **woken by
  `IPC_RMID` with `EIDRM`** - the one refusal about an object's death rather
  than about the caller;
* **`IPC_RMID` removes the set at once** (unlike a segment, which is *marked*):
  the next `semop`/`semctl` naming the number is `EINVAL`, and only a waiter
  already asleep still holds the object;
* **`SEM_UNDO` is per process**: a `+1` with the flag reads back as 1, a forked
  child that exits applies **nothing** (the list is not inherited), a child's own
  record is applied by its own exit, and `unshare(CLONE_SYSVSEM)` **keeps** the
  records (the value came back to 0). The adjustment is the operation
  **negated** - what the exit pass adds back - which is Linux's `semadj`
  direction and the one sign a careful reading of the man page gets wrong;
* **three refusals, three errnos**: the triads gate `semop` and every
  `GET*`/`SET*` (`EACCES` - and a mode-`0000` set refuses even its owner), the
  **creator** or `CAP_SYS_ADMIN` gates `IPC_SET`/`IPC_RMID` (`EPERM`), and
  `IPC_SET` reads its buffer **before** it asks who is calling (`EFAULT` first -
  measured as the one call in either family whose order is the other way round
  from `IPC_STAT`);
* **`semget`** on an existing key accepts a **smaller** `nsems` (`semget(key, 1,
  0)` on a two-slot set returns it) and refuses a larger one with `EINVAL`; the
  mode is stored with no umask; and a request naming permission bits is checked
  against them (`EACCES` for a key another user owns).

**The record the two machines disagree about.** `struct semid64_ds` is 104 bytes
on x86_64 and 88 on aarch64: x86_64's own `asm/sembuf.h` pads each clock with an
unused word, so `sem_ctime` sits at 64 there and 56 here and `sem_nsems` at 80
against 64. That is the trap `AGENTS.md` names - "a flag or struct constant looks
shared but the machines disagree" - and this landing walked into it: the first
draft used the asm-generic offsets on both machines and its own probe printed
`nsems=1792746339` (a `ctime` read as a count) while the kernel wrote sixteen
bytes past the caller's record. The offsets are now `abi`'s, per port, next to
`STAT_BYTES`' precedent, with the measurement beside them
(`.workbuddy/probe_sem_stat.c`).

**What this kernel chose, and named**: `SEMMNI` 4096, `SEMMSL` 1024, `SEMMNS`
65536 semaphores, `SEMOPM` 500, `SEMVMX`/`SEMAEM` 32767 - all reported through
`IPC_INFO`/`SEM_INFO`, which on this kernel answer the same limits and put the
undo statistics (`semusz`/`semaem`, which Linux re-purposes there) in the last
two fields. Linux's own `SEMMNI`/`SEMMSL` are larger (32000 each); a limit is a
configuration difference and the table's allocation is steerable, so §8.0's
rule applies as it did to the segment table.

**Named, not claimed**:

* the **message-queue family** (`msgget`/`msgsnd`/`msgrcv`/`msgctl`) is still
  `ENOSYS`, so `CLONE_NEWIPC` is still refused - with this one family left as
  its reason;
* the table is the machine's, not a namespace's, exactly as the segment table's
  is;
* one `SEM_UNDO` corner is not pinned: a **thread that calls
  `unshare(CLONE_SYSVSEM)`** keeps its records and gets a list of its own (a
  move, not Linux's fresh empty list). That is indistinguishable from Linux for a
  process with no siblings (measured) and unpinned where a sibling's records
  exist. The `pthread_create` **sharing** itself *is* pinned - a thread's `+1`
  joins the process's list, so its death applies nothing and the value stays at
  2 where a per-thread list would have taken its own `+1` back;
* `semctl`'s `SEM_STAT_ANY` is implemented but not claimed by a test (the pin
  walks `SEM_STAT`).

**Pin**: the new `t-sysv-sem`, 74 claims, plus seven host tests in
`sched::sysv_sem` for the operation arithmetic, the waiter registration and the
undo fold - the host tests are what caught `IPC_NOWAIT` being asked of the call
rather than of the operation, and the sign of the undo adjustment, before the
sweep could.

**Gate**: x86_64 `pass=203 fail=0 (214 built)`, `mpos-kunit 32 passed, 0
failed`, exit 0. The second machine compiles the change (kernel release +
clippy, and the pin builds for it, with its own `semid64_ds` offsets); its sweep
could not boot on this host (no DRM render node), so no aarch64 figure is
claimed.

**The flag value, found by the host run (2026-10-27).** The pin and the kernel
agreed on a wrong `IPC_NOWAIT`: both spelled it `0x4000`, where `ipc.h`'s octal
word is `0o4000` (2048, `0x800`). On the guest that was invisible - the test
wrote `0x4000` and `sysv_sem` looked for `0x4000` - but Linux saw an unknown
flag, ran the `semop` as blocking and parked (`t-sysv-sem` then spent the whole
120-second program budget in `semop(-1)` at zero). The constant is now `0o4000`
on both sides and pinned by `the_semaphore_flags_are_ipc_hs_octal_words`; the
gate's own says are unchanged because the two were wrong together.

### 6.15 System V message queues — **LANDED** (2026-10-23, SysV's third and last family)

The fourth door on the IPC namespace's contents: `msgget` (68/186), `msgsnd`
(69/189), `msgrcv` (70/188) and `msgctl` (71/187) fell through the dispatch to
`ENOSYS`. Measured on a current host first (Linux 7.2.11,
`.workbuddy/probe_sysv_msg.c`, `probe_sysv_msg_qbytes.c`, plus two one-off
probes under `/tmp` recorded here for the `MSG_INFO`/`IPC_INFO` fields and
`MSG_COPY` without `IPC_NOWAIT`), as root and unprivileged.

**The shapes that are Linux's**:

* **selection is the receive's rule, the queue is FIFO**: `type 0` takes the
  front; `type > 0` the first of that exact type; `type < 0` the message of
  the **lowest** type that is `<= -type` — *not* the front-most that fits,
  which is the reading the first draft had: with `{4, 10, 1}` queued a `-4`
  takes the `1`. `MSG_EXCEPT` takes the first message of another type;
* **a receive that cannot fit the message waits**: without `MSG_NOERROR` a
  matching message bigger than the buffer is neither taken nor refused —
  `E2BIG` only with `IPC_NOWAIT`, and a wait for one that fits otherwise.
  This was found by hanging the probe (a 64-byte buffer against an 8192-byte
  message parked forever), which is the honest way to learn it;
* `MSG_COPY` answers **by queue index** without taking, requires
  `IPC_NOWAIT` (`EINVAL` without it, `ENOMSG` past the end);
* the **byte budget** (`msg_qbytes`, `MSGMNB` 16384 at boot) is what "full"
  means: two `MSGMAX`-byte messages (8192) fill a fresh queue and a third
  send is `EAGAIN` under `IPC_NOWAIT` — while a blocking one sleeps until a
  receive frees room. A non-positive `mtype` and a `msgsz` past `MSGMAX` are
  both `EINVAL`, the size one before the queue is even looked at;
* blocking ends three ways: the opposite number's operation (`0`), a
  handler-bearing signal (`EINTR`), and `IPC_RMID` (`EIDRM`, measured for a
  blocked send *and* a blocked receive);
* **`IPC_SET`'s `msg_qbytes` is asymmetric**: above `MSGMNB` it needs
  `CAP_SYS_RESOURCE` (`EPERM` without it, even for the owner), while a
  budget **below the queued bytes — even zero — is accepted and stored**
  (measured twice: the readback is 16 with 8192 queued, then 0). The mode
  half rewrites uid/gid/mode with no umask, and reads its buffer before it
  asks who is calling (`EFAULT` first) — the order the segment family keeps;
* `msqid64_ds` is **120 bytes on both machines** — x86_64's own
  `asm/msgbuf.h` already carries the time64 padding, so unlike `semid64_ds`
  (§6.14's trap) one table of offsets serves both ports: the clocks at
  48/56/64, `cbytes`/`qnum`/`qbytes` at 72/80/88, the pids at 96/100;
* `/proc/sysvipc/msg`'s header and column widths are the kernel's own,
  measured character for character (key 10, msqid 11, perms 6, cbytes 12,
  qnum 11, then sixes and the three 11-wide clocks).

**Two bugs the sweep caught that the host could not** (both are why the gate
is the gate):

* a blocking `msgrcv` on an **empty** queue panicked the kernel: `select`'s
  `typ == 0` arm answered index 0 whether or not there was one, and the
  attempt indexed it. The host tests never hit it (their empty-queue claims
  were all `IPC_NOWAIT`); lane 5 died mid-test at `t-sysv-msg`'s blocked
  -receive claim. The fix decides the empty answer where the index is used,
  and a host test pins it (`a_receive_on_an_empty_queue_waits_or_refuses`);
* `MSG_STAT` read its argument as a raw slot index; Linux reads it through
  `ipcid_to_idx` — the number modulo the 32768 stride — so a caller may hand
  it a slot (what `ipcs` walks) *or* a full id. On the host every queue the
  probe made was generation 0, so slot == id and the claim passed by luck;
  the sweep's guest reuses slots across programs, and the claim went red
  there. The argument is now taken modulo the stride.

**Two receive facts the landing added** — both measured after the first green
run, both about `msgrcv`'s arguments rather than about its queue:

* the size word is gated by its **sign**, not by a cap: `do_msgrcv` opens with
  `(long) msgsz < 0` → `EINVAL`, so `LONG_MAX` is accepted and `LONG_MAX + 1`
  is refused with the message still queued (`.workbuddy/probe_msgrcv_bound2.c`:
  `9223372036854775807` returns the 5-byte message, `9223372036854775808` is
  `EINVAL` and `qnum` stays 1). The handler validated `8 + msgsz` instead,
  which **wraps** for a sign-bit-set `size_t`: a 7-byte range then passes
  `buffer_ok` and a whole message is written through a pointer nobody checked.
  The gate is now Linux's own, before the queue is looked at, and the
  destination range it validates is the message's own length
  (`8 + data.len()`, at most `8 + MSGMAX`) rather than the caller's `msgsz`,
  which is what Linux copies;
* a destination the kernel cannot write is `EFAULT` **after** the message has
  been unlinked, so the failed call loses it (measured: `qnum` 1 → 0 across a
  `msgrcv` to the NULL page, `.workbuddy/probe_msgrcv_huge.c`). The first draft
  validated the destination before taking the message and so left it queued —
  the kinder answer, and the wrong one: a caller that retries after `EFAULT`
  re-reads the same message here where Linux hands it the next one, so the
  retry is a livelock. This kernel has no fault-tolerant copy, so it still
  validates *before* it writes; it drops the message on failure, which is the
  half of Linux's ordering a caller can see.

Both are pinned by four claims in `t-sysv-msg` — `a msgsz whose sign bit is set
is EINVAL, and the message stays queued` (counterfactual: without the gate the
call returns the message instead of `EINVAL`), `msgsz == LONG_MAX is accepted -
the gate is the sign, not a size cap`, `a destination that cannot be written is
EFAULT`, and `and the message it could not write is gone, as Linux loses it`.

**What this kernel chose, and named**: `MSGMNI` 4096 (Linux's 32000) — the
same §8.0 allocation bound the other two families' tables carry; `MSGMAX`
8192 and `MSGMNB` 16384, Linux's own numbers, reported through
`IPC_INFO`/`MSG_INFO` (`IPC_INFO`'s `msgpool` is the pool in KiB and `msgmap`
repeats `msgmnb`; `MSG_INFO` re-purposes the pair as queues and messages in
use — the message family's own "cursed fields", the same shape §6.14 recorded
for the semaphore family's `semusz`/`semaem`).

**Named, not claimed**:

* **`CLONE_NEWIPC` has nothing left to wait on** — all three SysV families
  and the POSIX queue half are served — but the namespace itself is still
  refused: each family's table is the machine's, not a namespace's, and
  threading the tables through the namespace object is the landing that
  follows from this one;
* `MSG_STAT_ANY` **is** claimed now, with the permission split the message family
  measures cleanly (`MSG_STAT` refuses a queue nobody may read with `EACCES`,
  `MSG_STAT_ANY` answers it - what the `_ANY` spelling exists for), and the pin
  asks it from a caller **without** `CAP_IPC_OWNER`, because that capability is
  the other half of the question: an owner that holds it is served by both. **A
  correction, and it cost two wrong notes:** a first round of probes read
  `shmctl` as having the split *reversed* (`SHM_STAT` answering a mode-`0000`
  segment while `SHM_STAT_ANY` refused it) and recorded that as a reproducible
  oddity. A decisive re-probe (`.workbuddy/probe_stat_decisive.c`, which
  `unshare(CLONE_NEWIPC)`s first so the caller's *own* user namespace owns the
  IPC namespace) served all three spellings on a mode-`0000` segment, and named
  the mechanism: every answer in that family is `ipcperms`'s, and `ipcperms`
  yields to `ns_capable(ns->user_ns, CAP_IPC_OWNER)` - so the "reversal" was a
  reading of two runs that differed in *which user namespace owned the tables*,
  not in the command. The earlier paragraphs' claim that it was reproduced is
  withdrawn here rather than left standing; what the pin holds is the one split
  this kernel implements, measured where the capability cannot colour it;
* **the `MSG_STAT` argument form is not settled.** Two probes on the same host
  disagree: a generation-nonzero queue whose slot was handed to `MSG_STAT`
  answered `EINVAL` (`.workbuddy/probe_stat_gen.c`) and, in a second run of the
  same shape, the full id (`.workbuddy/probe_stat_key.c`). Neither explains the
  other, so this kernel keeps the reading `ipcid_to_idx` (a slot *or* a full id
  is accepted, the number is reduced modulo the stride) and the pin asks only
  what both readings agree on; a claim that a slot is `EINVAL` would have been
  a guess dressed as a measurement, which is the shape §13.15 warns about;
* `MSG_COPY`'s index shapes are measured now and the pin holds them: past the
  end, **negative** (`-1`) and `INT_MAX` all answer `ENOMSG` and none of them
  takes a message (`.workbuddy/probe_stat_any.c`);
* `IPC_SET` stores whatever uid/gid the record names; Linux maps the id
  through the caller's user namespace first and answers `EINVAL` for one it
  does not cover (`make_kuid`'s validity check — measured on a host whose own
  namespace maps a single id, where renaming a queue to uid 0 is `EINVAL` and
  the same record's own ids are accepted). The sweep's guest runs in the
  initial namespace, which maps every id, so a pin for this has to build a
  namespace of its own first (`unshare(CLONE_NEWUSER)` and a `uid_map`) — that
  is the claim this line waits on, and until it exists this kernel is more
  permissive than Linux for a caller that holds such a namespace;
* a negative `msgtyp` under `MSG_COPY` answers `ENOMSG` here; Linux's own
  answer for it was not measured (the host probe only walked non-negative
  indices).

**Pin**: the new `t-sysv-msg`, **84 claims** (76 green on an unprivileged host
run plus eight that need root — the six another-user claims and `IPC_SET`'s id
rewrite — each skipped with a line when root is absent, the shape
`t-reboot-libc` set), plus eight host tests in `sched::sysv_msg`.

**Gate**: x86_64 `pass=204 fail=0` over six lanes, stage 1's host suites
`1098 tests, 0 failed`, `mpos-kunit 32 passed, 0 failed`, exit 0 (the sweep's
own QEMU exit is 33). The aarch64 arm **builds** — kernel, loader and the pin
(`cargo xtask build --arch aarch64`, release, and `clippy` on
`aarch64-unknown-none`) — but its sweep could not boot on this host: QEMU
stops at `egl: no drm render node available` before the guest prints a line,
the same host limit §6.14 recorded, so no aarch64 figure is claimed.

### 6.16 `CLONE_NEWIPC` — **LANDED** (2026-10-23, the seventh rung, and the last thing the four IPC families waited on)

`unshare(CLONE_NEWIPC)` and `setns(fd, CLONE_NEWIPC)` were `EOPNOTSUPP` — the
kind was refused with its reason narrowed to "the namespace object itself" as
each of its four contents landed (§6.12–§6.15). This is the object: one number
per namespace, minted from the identity pool every kind shares, and the four
tables keyed by it.

Measured on a current host first (Linux 7.2.11, `.workbuddy/probe_ipcns.c`, run
as root **inside a user namespace** — this host's own capabilities do not reach
`CAP_SYS_ADMIN`, and `unshare(CLONE_NEWIPC)` outside one is `EPERM`, which is
the first thing the probe pins):

* **the number restarts per namespace.** The machine's queues here were
  `65581`/`65584` (`slot + generation * 32768`); a fresh namespace's first two
  were `0` and `1`. So a slot index only means anything inside its own space,
  and the table is a *space per namespace* rather than one table with a tag.
* **an id from the namespace left behind is `EINVAL`** — for a queue, a segment
  and a set alike (`msgctl`/`shmctl`/`semctl` → `-22`), not `ENOENT` and not
  `EIDRM`. A lookup is a lookup *in this namespace*.
* **a key is namespace-scoped too**: `msgget(key, 0)` inside → `ENOENT`, and
  `msgget(key, IPC_CREAT)` makes a new object (numbered `0`). Same for
  `shmget`/`semget`, and a POSIX queue's *name* behaves the same way
  (`mq_open` without `O_CREAT` → `ENOENT`, then the same name creates a second
  queue).
* `fork` keeps the namespace (the child finds the queue), and `setns` back into
  the first namespace makes its objects visible again and the second
  namespace's invisible — the two doors of one kind.
* `setns` into a namespace whose owning user namespace the caller has no
  `CAP_SYS_ADMIN` over is `EPERM` — the same rule §6.3 recorded for MNT, which
  is why a userns-rooted process cannot rejoin the machine's IPC namespace.
* `/proc/sysvipc/*` is the **reader's** view: inside a new namespace the old
  namespace's queue has no row.
* `/proc/self/ns/ipc` reads `ipc:[<number>]`, a new inode per namespace
  (`4026531839` for the initial one).

**The trap this measurement handed over** — and the reason the pin's first
draft failed on the host rather than on this kernel: **a number is not an
identity**. Two namespaces can hold live objects numbered alike (`0` and `0`),
so a pin that compares numbers asserts nothing, and *inside the kernel* a waiter
keyed by a number would wake the wrong namespace's sleeper. Every object now
carries a machine-wide **handle** (`mm::ipc_id::next_handle`) beside its
namespace-numbered id: the SysV message-queue and semaphore waiter tables key
by it, `SEM_UNDO` records hold the `Arc` of the set they were made on (so an
adjustment can never land on a stranger's set after an `unshare`), and the
segment reaper finds a segment by identity for the same reason.

**The shape that landed**: `vfs::ns` gained `Kind::Ipc` (name `ipc`, bit
`CLONE_NEWIPC`, one `/proc/<pid>/ns` entry — no `_for_children` spelling,
because the kind moves its caller like NET and USER) and `machine_ipc_id()`
for a task that never unshared; the task carries `ipc_ns: Option<u64>` beside
`net_ns`, inherited by `fork` and by thread creation; `unshare` mints a fresh
one and `setns` joins the descriptor's; and `current_ipc_ns()` normalises
`Some(machine_ipc_id())` to `None`, so a task that `setns`ed *back into* the
machine's namespace keys the same table space as one that never left (NET's
registry does the same, and a table that missed it would lose every object on
the way back).

The four families took the same change each: a `Space` per namespace holding
the slot vectors (and the per-namespace allowance — `MSGMNI`, `SEMMNI`/`SEMNS`,
`SHMMNI`/`SHMALL` are Linux's per-namespace limits), every table read taking the
key, `/proc/sysvipc/*` rendering from the reader's, and the POSIX registry
recording each queue's namespace beside it while `RLIMIT_MSGQUEUE`'s byte
accounting stays machine-wide (Linux charges a *user*, not a namespace).

**The leak the first draft left, and its fix** (same day): as landed first, the
four tables kept a namespace's slot space for ever — nothing told them when a
namespace lost its last task, so `unshare(CLONE_NEWIPC)` in a loop with a queue
made each time grew the kernel without bound (`MAX_TASKS` caps tasks, not
namespaces), which §8.0's panicking allocator turns into a dead machine. A
*number* cannot answer "is it dead", so the namespace became an **object**: the
task's slot holds `Option<Arc<IpcNs>>` (`None` is the machine's own, never
freed), a `/proc/<pid>/ns/ipc` descriptor holds the same `Arc`, and `IpcNs`'s
`Drop` — the last holder — frees the four tables' spaces for its number. The
freeing is safe by construction rather than by care: an object a task that
*left* the namespace still uses keeps its own `Arc` (a mapped segment, a POSIX
queue an open descriptor names), so dropping the space cannot free memory
somebody is still reading — and a segment that outlives its namespace is marked
destroyed, so its frames come back when the last mapping goes. That is Linux's
`ns_common`/`shm_file` refcount arrangement, arrived at because a membership
count could be got wrong in the direction that *frees a live table*.

Pinned by two claims on the observable half — `setns` into a namespace whose
last member left still finds its queue, because the descriptor kept it alive —
and by a host test, `vfs::ns`'s `the_last_reference_frees_the_ipc_tables`: one
object in each family, `drop(ns)`, every table's space gone and the objects
themselves still alive.

**Named, not hidden**:

* **a namespace's objects are the descriptor's to keep, not the namespace's.**
  The `Drop` above is the whole rule: `setns` after the last member left works
  and finds the objects, and a namespace nothing names is freed. What remains
  named is the *other* end — a segment mapped into an address space that never
  detaches holds its frames until that address space goes, which is Linux's
  answer too;
* **`clone(CLONE_NEWIPC)` is served** (same day, the increment after this
  landing): measured first (`.workbuddy/probe_clone_ipc.c`, as root inside a
  user namespace) — the **child** moves into the fresh namespace and the
  **parent keeps its own**, which is what the pair below the door looks like:
  the parent's key still resolves to the parent's queue while the child finds it
  `ENOENT` and numbers its first queue `0`; `clone3`'s own flag field behaves
  identically. The wiring is one field, because the door already served five
  kinds: `abi::clone_namespace_verdict` gained the bit, `CloneRequest` gained
  `ipc_ns`, and the syscall layer mints it — the same place and the same reason
  `net_ns` is minted there (`sched` must not name the identity layer's tables).
  A thread asking for it is still `EOPNOTSUPP`, the rule every served kind has.
  The claims live in `t-ns-ipc` rather than `t-clone`, because a host run of
  `t-clone` stops at its `CLONE_THREAD|CLONE_NEWNS` claim — Linux *serves* that
  word and the "thread" takes the process with it, which is the failure its own
  comment predicts;
* **`CLONE_NEWCGROUP`** is the last kind with nothing behind it, and it waits on
  the cgroup tree it would hold — named in `docs/ROADMAP.md` as before.

**Also closed, the same day, from this section's own "named" list**: `IPC_SET`
stored whatever `uid`/`gid` its record carried, where Linux maps each through
the **caller's** user namespace and refuses an uncovered one with `EINVAL`
(`ipc_update_perm`'s `make_kuid`/`make_kgid` check). Measured with a one-id map
(`.workbuddy/probe_ipc_set_map.c`, as root inside a user namespace): `uid=0`/
`gid=0` is accepted, `uid=1`, `uid=9` and `uid=65534` are `EINVAL`, **both** ids
are asked, and all three doors answer the same. The check sits after the owner
and `CAP_SYS_RESOURCE` questions - Linux's own order - which this host could not
measure (one mapped id leaves no second identity to ask from), so that half is
recorded as unmeasured rather than claimed. `userns` gained
`uid_is_mapped`/`gid_is_mapped` for it, deliberately *not* `uid_to_stored`: that
one reports `NO_MAP_ID` for both "unmapped" and the real id 65534, and 65534 is
an ordinary id here (measured).

**Pin**: the new `t-ns-ipc`, **39 claims**, every one of them **run against a
real kernel before it was pointed at this one** (the four failures that caught:
a number shared across namespaces, `IPC_STAT` with a null buffer reading a live
object as dead, a link taken before the wrong `unshare`, and a `setns` target
that was the machine's rather than the namespace the objects were in). On a
host run without `CAP_SYS_ADMIN` it makes its one claim — an unprivileged
`unshare(CLONE_NEWIPC)` is `EPERM` — notes that, and exits green, the shape
`t-ns-time` has. Six of them are the `IPC_SET` rule above, asked in a child that makes its own
one-id user namespace (so the whole pin is host-measured). Plus two host tests:
`sched::sysv_msg`'s
`a_namespace_has_its_own_slot_space` (a fresh space numbers from 0, the same key
makes a different object, and two objects sharing a number are told apart by
their handles) and `vfs::ns`'s `the_last_reference_frees_the_ipc_tables` (the
counterfactual for the leak: one object in each family, `drop(ns)`, and every
table's space is gone).

**Also moved with the measurement** — the pins that asserted IPC was *unserved*,
which is the drift §6.3 records over and over: `t-caps`'s unserved witness and
the whole-word refusals in `t-ns-net`, `t-ns-pid`, `t-ns-uts` and `t-ns-mnt`
now name `CLONE_NEWCGROUP` (the one kind left), and `t-ns-setns`'s directory
listing expects the `ipc` name and drops it from its absent list.

**Gate**: x86_64 `pass=205 fail=0` over six lanes, stage 1 `1100 tests, 0
failed`, `mpos-kunit 32 passed, 0 failed`, exit 0. `cargo xtask depgraph` is
within the ratchet (no edge appeared). The aarch64 arm builds (kernel, loader,
`clippy` on `aarch64-unknown-none`) but its sweep cannot boot on this host
(`egl: no drm render node available`), so no aarch64 figure is claimed.

### 6.17 `PR_SET_VMA_ANON_NAME` — a mapping a profiler can name (LANDED 2026-10-23)

The last `prctl` option this kernel had state for: `PR_SET_VMA` with its one
sub-operation names an anonymous mapping, so `/proc/<pid>/maps` prints
`[anon:<name>]`. It is what Mesa and Chrome tag their arenas with, and the
`name` field it writes is one `Vma` already carried for the loader and for
`[heap]`.

Measured on a current host first (`.workbuddy/probe_vma_name.c`,
`probe_vma_name2.c`), every claim then re-run against a real kernel:

* naming a **sub-range splits** the mapping at its ends, exactly as `mprotect`
  does: a four-page mapping with its second page named reads as three lines, the
  middle one named;
* a **null pointer clears**, and the pieces fold back into one line - which falls
  out of the table's own `tidy` because two abutting VMAs agree on the name;
* a name of **80 bytes or more is `EINVAL`** (`strndup_user`'s refusal, not a
  truncation), a **misaligned start** is `EINVAL`, and an unknown sub-operation
  is `EINVAL`;
* `len == 0` is **accepted** and does nothing;
* a range nothing is mapped in is **`ENOMEM`** - not `ENOENT`;
* a range a **regular file** mapping sits in is **`EBADF`**: the one errno a
  reader would not guess, and the one this pin caught a real bug through (below);
* an **empty** name is accepted and prints as `[anon:]`.

**The pin found the divergence the code had**: `MAP_PRIVATE` of a regular file is
recorded as an *eager* range here (its pages are read in at `mmap`, so it leaves
no VMA - §6.10's `MappedRange`), and the naming check looked only at the VMA
table. A file mapping was therefore `ENOMEM` ("nothing is mapped there") where
Linux says `EBADF` ("a file is mapped there"). The fix asks the **recorded** list
first, which is the description that holds those mappings, and the comparison is
the one the pin makes: a name on a file-backed range is a refusal, not a name.

**Named, and stricter than the host**: the host *accepts* naming a `memfd`-backed
mapping (both `MAP_PRIVATE` and `MAP_SHARED` answered 0, measured) while
refusing an ordinary file's; this kernel refuses every object-backed range with
the same `EBADF`, because the distinction it would need - "a memfd is not a file"
- is not one its `Vma` records. The stricter answer is the safe half of the pair
(a program that names its `memfd` arena loses the name and nothing else, and the
call it gets back is a refusal rather than a silent success), and it is named
here rather than approximated.

**Pin**: `t-maps`, grown to **53 claims** - the whole-mapping name and its
`[anon:<name>]` line, the split with only the named page carrying it, the clear
that folds the pieces back, the four refusals and the zero length - measured on
the host first and green on both machines.

**No fuzz target, and why** (rule 2's other arm): the name reader is the one
place this landing touches user bytes, and it is bounded by construction - at
most `ANON_VMA_NAME_MAX_LEN` bytes, each one re-validated at a page crossing,
`EINVAL` when the maximum passes without a NUL and `EFAULT` when a page does not
belong to the caller - so there is no structure to explore beyond "short, exactly
80, unterminated, unreadable", which the four claims hold. Everything the other
three `prctl` landings read is a scalar (`PR_SET_PDEATHSIG`'s signal,
`PR_SET_CHILD_SUBREAPER`'s boolean, `PR_SET_NO_NEW_PRIVS`'s exactly-`(1,0,0,0)`
word); none of them is parsed from memory.

**Gate**: x86_64 `pass=205 fail=0`, stage 1 `1100 tests, 0 failed`,
`mpos-kunit 32 passed, 0 failed`, exit 0. The **fuzz workspace could not be built
in this sandbox** and that is recorded rather than worked around: its registry
cache is read-only here and the crates it needs are not unpacked, so
`cargo build` fails in dependency resolution before any target compiles (with
`--offline` it fails the same way). No target's source was touched by this
session's work, and the paragraph above is the argument that none was due.

### 6.18 `PR_SET_CHILD_SUBREAPER` — an orphan comes to the marked ancestor, not to init (LANDED 2026-10-23)

The option was another `prctl` catch-all refusal, and what it buys is a shell's
whole job: a grandchild whose own parent died is reparented to the nearest
ancestor that asked for it instead of to init, so the shell can reap it and see
its status. Measured on a current host first
(`.workbuddy/probe_subreaper.c`), then re-run against a real kernel:

* the setter takes **any `int`** and stores a boolean - `7` reads back as `1` -
  and `PR_GET_CHILD_SUBREAPER` writes that `int` through its pointer (`EFAULT`
  for a null one);
* it is **not inherited by `fork`** (the child reads 0) and is **kept across
  `exec`** (the man page's claim; this kernel copies it in `load`, and the pin
  holds the fork half, which is the half that was measured);
* the behaviour: with the flag set, a middle process that forks a grandchild and
  exits leaves the grandchild reparented to the marked ancestor - measured by
  reaping it there and reading its `getppid()` - and without the flag the same
  shape answers `ECHILD` because init has it instead.

The substrate is a `bool` on `Task`, held on the **group leader** because the
flag is the process's and `prctl` may be called from any thread, plus one arm in
the walk that already chooses a dying task's successor parent:
`alive_thread_of(dying group)`, then `nearest_subreaper` - the ancestor walk
Linux's `find_new_reaper` performs - then init. The walk is bounded by the task
table, so a cycle in a parent chain (which nothing should produce) ends it rather
than hanging the kernel.

**Pin**: `t-prctl`, grown to **53 claims**: the `EFAULT`, the boolean fold, the
fork non-inheritance, the grandchild's `getppid()` and the reap, and the
`ECHILD` without the flag - the last two being the pair that makes the option a
behaviour rather than a stored number.

**A flake seen while gating it**, recorded because the repo's rule is to re-run
rather than believe: the first run of this commit's gate went red on
`t-posix`'s *"getpriority answers 20"*, a claim this change does not touch, and
the immediate re-run was `pass=205 fail=0`. The mechanism is not established
(the claim reads the caller's own nice through the raw door, and nothing here
touches a priority), so it is written down as an observation with its two
readings rather than as a mechanism - and the re-run is the one quoted.
**Closed 2026-10-28 by §6.31**: the pollution was `t-priority`'s
`setpriority(PRIO_PGRP, …)` left in the runner's process group, and the
program now restores by group.

**Gate**: x86_64 `pass=205 fail=0` (the re-run), stage 1 `1100 tests, 0 failed`,
`mpos-kunit 32 passed, 0 failed`, exit 0.

### 6.19 `PR_SET_NO_NEW_PRIVS` — the promise the seccomp door was missing a half of (LANDED 2026-10-23)

The option matters here for a reason that is not the obvious one. Its headline
effect - `execve` of a setuid binary grants nothing - has nothing to read in this
kernel, which does not honour setuid bits on exec at all. Its **second** effect
does: Linux lets a caller install a seccomp filter with `CAP_SYS_ADMIN` **or**
after setting this flag, and `install_seccomp_filter` refused every caller whose
real uid was not root with `EACCES` - the honest refusal while the escape hatch
did not exist, and a *divergence* the day a program needed the door. So the flag
is modelled for that door, and named as such rather than presented as a
privilege-escalation guard it is not yet.

Measured on a current host first (`.workbuddy/probe_nnp.c`):

* the setter takes **exactly `(1, 0, 0, 0)`**: `2`, a non-zero third/fourth/fifth
  argument and a clearing `0` are all `EINVAL`, so the flag is **one-way**;
* the getter answers the boolean as its return value and refuses a call whose
  unused arguments are not zero - a C caller that wrote `prctl(39)` and let the
  variadic tail be garbage gets `EINVAL`, which is how the first version of this
  probe concluded "unimplemented" for half an hour;
* `fork` **inherits** it (measured); the man page's `exec` preservation is what
  the kernel's `load` copies, and that half is not claimed here;
* the door's two halves: a non-root caller without the promise is `EACCES` and
  with it the same call installs the filter. The **negative** half could not be
  measured on this host - its own runtime sets the promise on every process it
  starts, so a caller *without* it does not exist here - so that claim is
  accepted on a host run (with a note printed) and the pin walks both halves in
  the guest, where the child drops to nobody with the flag still unset.

The substrate is a `bool` on `Task`, inherited by `fork`/`clone` and copied
across `exec`, plus one condition inside `install_seccomp_filter`:
`ruid != 0 && !current_no_new_privs()`.

**Pin**: `t-prctl` grows to **61 claims** (the five argument shapes, the
one-way-ness, the getter's argument check and the fork inheritance) and
`t-seccomp` to **57** (the dropped child's `EACCES`, the promise accepted, and
the filter installed after it) - the second pair being what makes the option a
behaviour rather than a stored flag.

**Gate**: x86_64 `pass=205 fail=0`, stage 1 `1100 tests, 0 failed`,
`mpos-kunit 32 passed, 0 failed`, exit 0.

### 6.20 `PR_GET_TID_ADDRESS` — and the process-shaped `clone` that never recorded it (LANDED 2026-10-23)

A small option with a real read: where `clone(CLONE_CHILD_CLEARTID)`'s word
lives, which is the address a joiner sleeps on. The state existed - the thread
path had recorded it since `CLONE_CHILD_CLEARTID` was served - and the option
reports it.

Measured on a current host first (`.workbuddy/probe_tidaddr.c`): a `clone` child
with `CLONE_CHILD_CLEARTID` reads back **exactly** the address its creator passed
as `child_tid`; a pointer that cannot be written is `EFAULT`; arguments past the
second are ignored. A plain process's value is **not** claimed - a libc may set
one for its own bookkeeping (glibc does, and this host's main thread answered a
nonzero address), so "0 on a fresh process" is a property of the libc rather than
of the kernel.

**The claim found a real gap**, and it is the kind a pin exists for: MPOS recorded
`clear_child_tid` **only in the thread path**. A `clone` that asks for the word
without `CLONE_VM` - the fork-shaped one, which is a perfectly ordinary request -
got no address at all, so `PR_GET_TID_ADDRESS` would have answered 0 for it and
`CLONE_CHILD_CLEARTID`'s word would never have been written for that child. The
record now happens where the process child's address space is installed, and the
thread path keeps its own (each is the one its task's `mm` needs).

Two trips the fix took, both worth a line because they are the same mistake in
different clothes: the first insertion anchored on a line that exists in **both**
paths and went into the thread one (duplicating the block that was already
there), and the gate showed the *same* failure again; the second put it where the
process child's `Task` is built. A patch whose anchor is not unique is a patch
whose effect has to be read off the gate, not off the diff.

Named, not claimed: `CLONE_CHILD_SETTID`'s write - Linux puts the child's own tid
in that word for any `clone` that names it - is still the thread path's alone.
The pin does not ask for it and this entry does not claim it.

**Pin**: `t-prctl`, **64 claims**: the `EFAULT`, the child's read-back (the claim
that caught the gap), and the ignored tail argument.

**Gate**: x86_64 `pass=205 fail=0`, stage 1 `1100 tests, 0 failed`,
`mpos-kunit 32 passed, 0 failed`, exit 0.

### 6.21 Three `prctl` options this kernel leaves refused — and the numbers that decide it (2026-10-23)

`prctl`'s catch-all answers `EINVAL` to every option the kernel does not model, and
§4.2's rule is that an option is accepted only once real state backs it. Three
were re-examined against that rule and **stay refused**, each for a measured
reason. Writing the reasons down is the deliverable: each is the shape a later
session would "fix" into a success that does nothing.

**`PR_SET_TIMERSLACK` / `PR_GET_TIMERSLACK` (29/30)** — measured on a current
host (`.workbuddy/probe_misc_prctl.c`): the getter answers the default
**50000** ns, the setter stores a value and the getter reads it back, and
`SET(0)` **resets to the default** rather than to zero. The value is real state
with a real reader (a timer may fire up to `slack` late), and that reader is what
this kernel cannot honour: its timers are a **tick** granular enough to be larger
than Linux's default slack, so it already delays every wake more than 50 µs -
serving the flag would let a caller set a *smaller*-than-tick slack and get a
kernel that violates the promise it just accepted, and a larger one would change
nothing because the tick dominates. A getter reporting 50000 would be a lie about
enforcement and one reporting a tick would be a lie about the caller's set. So
both stay `EINVAL`, and the gap is this paragraph rather than a stored number.

**`PR_SET_PTRACER` (0x59616d61, Yama's)** — the host accepts any value here
(itself, `1`, `PR_SET_PTRACER_ANY`, `0`) even with `ptrace_scope = 1`, because
Yama is *built in* there. The answer this kernel gives is `EINVAL`, and it is
Linux's own answer for a kernel **built without Yama** - the option exists only
in that LSM's `prctl` hook, and a kernel without it falls through to the same
default arm. So the refusal is not a gap at all: it is "this kernel has no Yama",
and the name for it is here rather than in a handler that stores a value nothing
would read.

**`PR_SET_THP_DISABLE` / `PR_GET_THP_DISABLE` (41/42)** — the host accepts `1`
and its getter answers `1`. The flag's reader is
`transparent_hugepage_enabled()`, and this kernel has **no huge pages** (the
`MAP_HUGETLB` refusal is `ENOMEM` for the same reason, `docs/uapi/` §5.1): a
stored flag whose only reader does not exist is precisely the §4.2 violation, so
it stays `EINVAL`. The day a THP pool lands, the flag becomes honest in the same
commit as its reader.

What the same probe also settled, and needs no decision: `PR_SET_MM_*` (code,
data, stack and brk boundaries) is `EPERM` for an unprivileged caller - the gate
is `CAP_SYS_RESOURCE` *before* any argument check - which is what this kernel's
catch-all already gives a non-root caller for a different reason.

### 6.22 The hang in `t-ns-pid`, made sayable (2026-10-23)

Two lanes died at 900 seconds this session with the same last line -
`t-ns-pid`'s *"while a fork into the namespace whose init has exited is
ENOMEM"* - while the same program passed on the runs between, and in one of the
two cases another lane had already gone red on `t-posix`'s `getpriority` claim.
The log could say only where the program had **started**, because the parent's
last act is a blocking read to end-of-file on the pipe from its forked section:
if that section stops, the read never returns, the lane times out, and the
lane timeout reports the clock rather than the program.

That read is now **bounded**, and the section marks its stages: one byte at each
of six points - its own `unshare(CLONE_NEWPID)`, its pipes, the fork of the init,
the first report line, the deep chain's line, and the verdict itself - so a
section that stops names the stage it stopped in. The claim is
`"the init section ran to its end (last stage N)"`, which is a failed claim with a
number in it rather than fifteen minutes of silence.

Two numbers decided the budget. The section's own children sleep up to fifty
seconds by design (the init and its worker park to be killed), so the read's
patience is **60 s** for the verdict and **30 s** for the shorter report - far
above the section's worst case and far below the lane's 900 s. A bounded read is
not a fix for the hang: it is the instrument that makes the next occurrence
answer the question the last two could not.

What the instrument does **not** say yet, and what to look at first: the section's
parent has three blocking reads (two lines on the init's pipe, one on the nested
task's), each waiting for a specific child, so a stage mark of 3 with no 4 means
the init or its worker never wrote - a forked task that never ran, or a wait that
never woke. That is where the next occurrence will point.

**Gate**: x86_64 `pass=205 fail=0`, `mpos-kunit 32 passed, 0 failed`, and
`t-ns-pid` alone is 45 claims with `last stage 6` in its log.

### 6.23 The aarch64 sweep's own hang, measured (2026-10-24) — OPEN

The first full aarch64 sweep this host has run since 2026-10-21 does not reach
its end. Five runs — two on a working tree that touches no kernel source
(§10.5's `t-diskbench` instrument), one on **`f908786` (HEAD) with the tree
stashed** — all froze the same way, at one of two early programs: `t-clone-exit`
(log frozen at its 30693rd byte, three runs) or `t-busybox-farm` (19840th, two
runs). Each time the guest printed the program's `ok:` claims up to a point and
then said nothing for ever — in `t-clone-exit` between *"a child was forked to
die for the zombie claim"* and *"the dead child's record reads state Z"*, the
section whose parent `fork`s a child, then polls
`/proc/<pid>/stat`'s state field in a `nanosleep`(10 ms) loop until it reads
`Z`. `=== PASS` never arrives; the runner answers *"the aarch64 sweep never
printed a summary"*; the host's `qemu-system-aarch64` sits at ~99 % CPU while
it happens. **The hang is not the working tree's**: the stashed-HEAD control
froze at the same program, and the working tree's own x86_64 arm is green
throughout.

What the machine looked like while frozen, read through the run's own QMP
socket (`human-monitor-command` `info registers`, the VM stopped): the single
CPU at EL1 with interrupts enabled, its PC pinned at one instruction of
`arch::aarch64::timer::since_boot_nanos` (the `mov` after the `CNTPCT_EL0`
read) and its `X30` naming `sched::runtime::task::block_until_deadline` as
instantiated for `syscall::linux::fs::file::poll_wait`'s wait condition — the
`poll`/`ppoll` park. Eight samples spread over resumed execution all landed on
the same instruction.

Two instruments then said the loop is **not** turning. A spin counter in
`block_until_nanos`'s loop (`spins += 1`, rate-limited `println` at 1000,
2000, 4000, 10 000, 100 000, 4 000 000 iterations, printing `cond()`,
`table_has_deliverable`, `now`, `deadline`) printed **nothing** across two full
hang windows — so whatever the frozen PC means, the park loop is not cycling
even a thousand times while the guest sits there, and the ~99 % host CPU is
QEMU's own wedging (or a halted clock whose wake never comes), not guest
instruction throughput at that loop. The reading to carry forward: the CPU
state was captured parked-at/returning-to the poll wait's clock check, with
the machine otherwise silent, and the guest-side loop quiet — which points
*away* from a guest spin and toward a wake source that never fires (the tick)
or a host-side stall, and is why the next session should read the clock, not
the loop: is `CNTPCT_EL0` advancing while frozen, is the timer IRQ firing, is
the scheduler's `now` moving — QMP `xp` on the kernel's tick counter and the
sleep heap answers all three without a guest console.

How to reproduce and what is standing: `cargo xtask test --arch aarch64`,
reaches the freeze in 2–4 minutes of sweep (the boot itself is healthy —
programs before the frozen one pass, `mpos-kunit` included). The instrumented
diff that produced the "not turning" reading is one counter plus one
rate-limited `println` at the top of `block_until_nanos`'s loop
(`src/sched/runtime/task.rs`), reverted before this entry landed; re-apply it
or go straight to QMP. The last full aarch64 sweep figure on record is
2026-10-21's — the 2026-10-23 batch (the wait-deadline rework §10.7's commit,
`PR_SET_PDEATHSIG`'s exit path, `PR_SET_CHILD_SUBREAPER`'s reparent walk, the
IPC namespace) could not sweep on this host at all ("its sweep still cannot
boot: `egl: no drm render node available`", its commits say) and booted again
only today, so the regression window is **everything since 2026-10-21, plus
the host environment itself, which also changed** (the render node came back).
Until this closes, the aarch64 arm of the gate has no summary line to quote,
and every aarch64 claim in a commit message has to say so plainly.

**The bisect named the commit, and the freeze state is now measured (2026-10-24,
same day).** A fixed bisect script — the first attempt was voided by its own
harness: a step whose 600-second timeout killed the gate left the QEMU alive,
the stale guest held vhost-vsock CID 4, and every later step's machine aborted
before starting (`unable to set guest cid`), which the script read as "bad" —
run to a verdict with hang detection instead of a timeout, and with a stale-QEMU
kill before every step: **the first bad commit is `4081466`, "a wait ends at its
deadline, not at the next clock tick"** (§10.7's landing), on good `4c33638`
(its parent) and good `31ae543` (2026-10-21, the last tree with a green aarch64
sweep). The freeze also reproduced byte-for-byte with the sweep's GL display
removed (`-display none` instead of `egl-headless,gl=on`, the same
`t-clone-exit` at the same log offset), so the display path is not in it.

What the machine looks like at the freeze, read through the run's own QMP
socket. Three instruments, three runs:

* the timer's own statics (`arch::arch::aarch64::timer`'s `TICKS`/`NEXT`/
  `PERIOD`/`FREQ`/`BOOT_COUNT`, read with the monitor's `x` against the
  kernel's virtual addresses): one freeze held a live counter at 5.83 s
  against **565 ticks = 2.26 s of served ticks** and a compare (`NEXT`) at
  2.22 s — the compare had been expired for over three seconds and no tick had
  been taken (`TICKS` delta 0 across a three-second window); another freeze
  read the same statics as **all zero**, the pre-`init` state, beside a PC in
  the park loop — the reading that says `since_boot_nanos` was answering from
  the `FREQ == 0` fallback, i.e. a clock pinned at zero, in which no wait can
  ever reach its deadline and no sleep can ever be served;
* `info registers`, sampled twice three seconds apart: one freeze held the PC
  byte-identical across the window (the guest executing nothing while QEMU's
  own threads burned ~99 % of a host core); a later freeze caught the PC
  **moving** between samples — `vfs::pipe::PipeEnd::Inode::poll_ready_client`
  to `timer::since_boot_nanos` — a hot `poll_wait` park loop running against
  the dead clock;
* the PC in every capture lands inside or next to `since_boot_nanos` reached
  from `sched::runtime::task::block_until_deadline` instantiated for
  `syscall::linux::fs::file::poll_wait`'s wait condition — §10.7's own code.

**A fix attempt was made and did not stop the freeze.** The reading "the last
tick re-armed its compare into the past and the re-asserted line wedged
delivery" produced the Linux-shaped change — `tick()` re-arms from the
counter's current reading (`now + PERIOD`) when the served deadline had already
passed, accepting the drift instead of a re-assert — built clean on both
machines, and the sweep froze at the same `t-clone-exit` byte offset with it.
Reverted. What survives is the measurement, not the fix.

What the next session should do, in order. (1) **Re-arm the spin counter in
`block_until_nanos`** (thresholds 1000…4 000 000, printing `cond`,
`table_has_deliverable`, `now`, `deadline`) — it printed nothing across two
whole freeze windows once, which is the strongest single constraint: the park
loop is not the thing spinning, so the burn is elsewhere (a second task's loop,
or QEMU itself). (2) **Decide whether the frozen guest is alive** with the
gdbstub, not with paired `stop` samples: QMP's `gdbserver` command, then
single-step — advancing PCs mean a live guest and a QEMU-side delivery
problem; a blocked step means the vCPU thread itself is wedged. (3) **Capture
the GIC state at the freeze** — whether the timer PPI is pending, in service,
or lost — because "the compare expired and the line was never taken" is
consistent with every reading so far, and the line's state decides between a
kernel fix (an ack/EOI path) and a QEMU report. (4) Treat the harness's own
**restart-on-silence** as part of the instrument: a silence detector that
kills and relaunches the boot leaves a second machine whose early boot
(pre-`timer::init`) reads as exactly the zero-statics state above, so sample
the process tree (`/proc/<pid>/task/*/comm`) alongside every QMP read.

Until this closes: the aarch64 arm has no gate figure, every aarch64 claim in
a commit message says so, and the trap table's row is the short version.

### 6.24 The freeze's mechanism, named and closed (2026-10-24, same day) — CLOSED

The trap table's row above is the short version; this is the mechanism, and it
is not what the bisect's shape suggested. §10.7's wait-deadline landing did not
break the aarch64 machine - it made a **second clock** load-bearing, and the
second machine's sweep found the load.

**The two clocks.** The scheduler's park answers one clock: `block_until_nanos`
compares its deadline against `since_boot_nanos`, the free-running counter
divided by `CNTFRQ_EL0`. The tick *counter* (`timer::TICKS`, one increment per
delivered interrupt) is a different clock that falls behind that one by every
masked window longer than a period - the interrupt is level-triggered, the line
stays asserted, and no tick is *counted* while the CPU cannot take it; the
counter catches up in a burst when the mask lifts. §10.7's landing converted
every deadline at the park's boundary but left the *callers* naming deadlines
against `hal::ticks()` - the tick counter - in a dozen places (`poll`/`select`
floors and expiry checks, the futex and SysV timeouts, tty's `VTIME`, the VT
release deadline). The two units multiplied into one door read like one thing.

**The spin.** `t-busybox-farm` (or `t-clone-exit` - whichever lane hit it first)
calls `poll`/`ppoll` on pipes whose writers have not written. The loop's expiry
check compares `hal::ticks()` with a deadline built from `hal::ticks()`; the
park below compares `since_boot_nanos()` with the *same number reinterpreted as
nanoseconds*. The deadline was in the past for the nanosecond clock the moment
the tick counter ticked past it (the units differ by `ns_per_tick()`, 4 000 000
on this machine), so the park answered `Deadline` at once, the loop re-ran
`check_ready`, found nothing, and parked again. Every pass runs with
interrupts masked - the park is entered under the critical section and the
`Deadline` exit never reaches `idle()`'s unmask - so the timer's PPI, asserted
at the distributor the whole time (`GICD_ISPENDR0` bit 30 set, read through
QMP at the freeze), is never delivered: the tick counter stops, the expiry
check never fires, and the loop sustains itself forever. The PC samples that
§6.23 read as "parked" were a loop turning at `since_boot_nanos`'s u128 divide
- the one function every pass calls - and the "QEMU burning a core" was the
TCG vCPU executing it. Nothing was wedged; the machine was *running* the
wrong loop.

**The fix is one rule: a wait is named on the clock its park answers.**
`block_until_deadline` - the door whose input multiplied ticks into
nanoseconds - is deleted; every caller names its deadline against
`since_boot_nanos` directly, the tick-counter builders (`deadline_in_ticks`,
`futex_deadline`'s now arithmetic, tty's `VTIME` span, the VT release timeout,
the socket/ARP/vsock timeouts) convert once at construction, and the expiry
checks read the same clock the park does. tty's `UntilTick` is `Until` in
nanoseconds and `note_input_tick` is `note_input_at`; `vtmode`'s
`RELEASE_TIMEOUT_TICKS` is `RELEASE_TIMEOUT_NS` and its table's `now` is the
nanosecond clock. The x86_64 machine was one masked window away from the same
spin the whole time - the freeze was the second machine's because its sweep
runs the pipes-heavy programs first under TCG, not because the bug lived there.

**Two fixtures the open gate then exposed** (both landed in the 2026-10-23
batch whose aarch64 figure was never taken, §6.23's window):

* `t-prctl`'s `PR_GET_TID_ADDRESS` child passed `child_tid` in the *x86_64*
  slot of a raw `SYS_clone`. The argument order is the machine's
  (`CLONE_BACKWARDS`), the kernel swaps on aarch64, and the child was born
  with `ctid == 0` - the claim read the swap, not the kernel. The fixture now
  names the slot per `target_arch`, beside the kernel's own note.
* `t-sysv-msg`'s blocked-send child died of its own `SIGUSR1`: the handler
  frame write lands just below the interrupted stack pointer, on a page the
  process has never touched, and `write_user` refused it - where Linux's
  frame write rides on `expand_stack`. The delivery path now takes the same
  resolver the EL0 stack-growth fault takes (anchored on the interrupted
  pointer, `RLIMIT_STACK`-bounded) and retries the write before falling to
  the default action. A frame the stack cannot still back refuses honestly.

**The gate, quoted from both arms' own summaries** (2026-10-24, the commit
that carries this fix): x86_64 `=== SUMMARY pass=205 fail=0 (218 built)`,
`mpos-kunit 32 passed, 0 failed`, exit 33; aarch64
`=== SUMMARY pass=203 fail=0 (203 built)`, `mpos-kunit 32 passed, 0 failed`,
`=== END` on every lane - the first aarch64 sweep figure since 2026-10-21, and
the first one the second machine's arm produced without a summary-less
silence. The whole aarch64 sweep now runs in 40 s of wall clock (six lanes,
TCG) where the freeze window offered none at all; the fix also took the
`block_until_deadline` unit multiply out of every wait, so nothing else pays
it either.

**One instrument casualty, recorded because it fired on the day the machine
started being fast**: `drivers::kunit`'s `what_a_read_costs_the_clock` asserts
a masked read *loses* the ticks its mask predicts, and its guard allowed for
"a tick delivered inside the window" - a negative loss - in its arithmetic
(`lost + 1 < predicted`) while still firing on the negative value, because
`-1 + 1` is `0` and `0 < predicted` for any real mask. Measured firing once
under a loaded host (six sweep lanes beside the boot); the guard now reads
`lost >= 0 && lost + 1 < predicted` - the edge its own comment had named all
along, which the arithmetic never implemented.

**One harness lesson, the same day**: two `cargo xtask test` runs in one tree
report nonsense (`AGENTS.md` §5's own row) - one run's output showed a
summary-less verdict over a log file that had the summaries, because a second,
stale sweep process was writing the same `build/test-serial.log` beside it.
The row exists; this is what violating it looks like from the inside.

### 6.25 The cgroup tree, and `CLONE_NEWCGROUP` — the ladder's last rung (LANDED 2026-10-25)

The ladder §6.3 built rung by rung — UTS, MNT, PID, NET, TIME, USER, IPC — had one
rung left, refused since the beginning with "there is no cgroup tree here to see".
The tree is what landed first, and the namespace came with it in the same commit,
because the namespace's whole state *is* a place in that tree.

**Measured on a current host before anything was built** (`/sys/fs/cgroup`, Linux
7.2.11, cgroup2 with `nsdelegate`, probed in a scratch group under the probe's own
scope — `src/vfs/cgroupfs.rs`'s header carries the whole list): a controller-less
group carries **nine files** (`cgroup.type` absent at the root), the read-only half
is `0444` and the writable half `0644` with every directory `0755`, the empty
controller files answer **zero bytes** (not a bare newline), `cgroup.type` answers
`domain\n` (7 bytes), `cgroup.procs` moves a **process** by any of its threads'
numbers while `cgroup.threads` moves one thread, a pid that names nothing is
`ESRCH` and junk is `EINVAL`, `rmdir` of a group with a child is `ENOTEMPTY` and of
a populated one `EBUSY` (measured by leaving the probe shell inside its own group),
and an unknown controller is `ENOENT` — the same answer a controller-less kernel
gives, because the controller does not exist.

**The tree is organizational and nothing else** — no controllers, so
`cgroup.controllers` is empty and `subtree_control` refuses every name. That is
Linux's own answer for a kernel built without controllers, not a truncation of one
built with them. `mount -t cgroup2` serves it; one hierarchy for the machine, a
second mount binding the same tree. Membership is keyed by task slot **and
generation** (`sched`'s pidfd generations), so a reused pid cannot inherit the dead
task's group; a task is born where its parent is grouped (`copy_process`'s rule,
wired into both child paths); the bounds are `max.depth`/`max.descendants` files
carrying Linux's `max` default, enforced against ceilings of 8 and 256 because
`mkdir` is a user-steered allocation path (§10's rule 4).

**The namespace is the view, and the view is one path.** `unshare(CLONE_NEWCGROUP)`
and `setns(fd, CLONE_NEWCGROUP)` set the task's view-root; `/proc/<pid>/cgroup` —
now served on both the process and thread directories — renders the task's real
group path *from that root* (`0::/` where it read `0::/grp`), `/proc/<pid>/ns/cgroup`
gets its `cgroup:[<number>]` identity, and a `fork` inherits the view. The root is
always the maker's **real** current group — measured on the host (`unshare -C`
twice roots the second at the group the caller is in now, whatever the first rooted
at), and it took one gate run to catch the first cut inheriting the *old view*
instead: the second `unshare` was a no-op and every claim downstream failed. A
`clone(CLONE_NEWCGROUP)` child is born view-rooted where its parent is grouped.

**What the gate run also caught, all in the first pass**: every verdict pin that
named CGROUP "the unserved kind" went red at once — `t-caps`, `t-clone`,
`t-ns-mnt`, `t-ns-uts`, `t-ns-pid`, `t-ns-net`, `t-ns-setns` — which is the drift
§6.3 has recorded once per rung, arriving all together for the last one. The
"mixing with an unserved kind is refused whole" claims are extinct as a class: all
eight kinds are served, so those pins became the birth claims (the word now
*unshares* what it names) or moved to the names that never existed
(`mnt_for_children`). And `t-cgroupfs`'s errno claims failed on the test's own
shared-errno cell — a `remove_dir` probe comparing the *previous write's* errno —
fixed by reading each errno at the call that made it.

**The named divergence**: inside the namespace, the **mount** still shows the
machine's whole hierarchy, where Linux re-roots the mount to the view's subtree.
The rendering, the identity and `setns` are Linux's; the mount view is the
remainder (`docs/uapi/` §1's unshare/setns row names it).

**Pin**: `t-cgroupfs` (the tree: nine files, modes, membership moves, refusals,
removal rules, `/proc/self/cgroup`), `t-ns-cgroup` (the namespace: privilege
probe, re-rooting, real-group writes from inside, the fork, the ns identity,
`setns` back). Host-side, `vfs::cgroupfs`'s own suite pins the file shapes and the
claim machinery against the measured figures.

### 6.26 `CLONE_INTO_CGROUP` — the child is born in the group a descriptor names (LANDED 2026-10-25)

§6.10 left this flag refused with `EBADF` and a true sentence under it: *there is
no cgroup tree here for a descriptor to name*, which is Linux's own answer for a
descriptor that is not a cgroup. §6.25 built the tree, so that sentence expired
and the flag became buildable. It is the last `ENOSYS`-shaped hole in `clone3`'s
struct — the `cgroup` word of `struct clone_args`.

**Measured on a current host before anything was built** (`/sys/fs/cgroup`, Linux
7.2.11, root inside the machine's own hierarchy, probes
`.workbuddy/probe_clone3_cgroup{1,2,3,4,5}.c`, `.workbuddy/probe_clone3_order.c`,
`.workbuddy/probe_cgroup_cap.c`):

* **the child and its own fork** read the target's path in
  `/proc/<pid>/cgroup` (`0::/mposprobe`, `0::/mposprobe/a`), and the caller's own
  line does not move;
* **the descriptor must be a group directory**: a regular file, a directory on
  another filesystem and a cgroup *attribute* file (`cgroup.procs`) are all
  `EBADF`; `O_PATH` is **served** — the descriptor is a name, not a reader;
* **a group removed after the descriptor was opened is `ENOENT`** — a cgroup
  that is gone is not the same answer as a node that never was one;
* **`CLONE_THREAD` beside it is `EOPNOTSUPP`** — into a different group *and*
  into the caller's own group alike: a thread joins its group's cgroup and has
  no target to choose;
* **`CLONE_NEWCGROUP` beside it roots the child's namespace at the child's own
  group** — the target, not the caller's group: the child reads `0::/` while
  still being in `/mposprobe/a`;
* **an unprivileged caller is `EACCES`**, and the question is the *first common
  ancestor's* `cgroup.procs`, not the target's: uid 1000 into a world-writable
  (`0666`) target still answers `EACCES` because the machine root's `0644` file
  is on the path. `CAP_DAC_OVERRIDE` turns it into a success,
  `CAP_DAC_READ_SEARCH` alone does not, and root succeeds with *no* capabilities
  at all — the two triads `may_as` applies, measured one bit at a time;
* **a target outside the caller's cgroup namespace is `ENOENT`**, even with a
  descriptor opened before the unshare: the namespace hides it;
* **`clone(2)` accepts the bit and ignores it** — there is no descriptor
  argument there, and Linux starts an ordinary child rather than answering
  `EINVAL` (`probe_clone3_cgroup.c`'s section 6);
* **the order**: a bad descriptor beside `exit_signal = 99` is `EINVAL` and
  beside `pidfd = 0` is `EFAULT`, so the descriptor is resolved after every
  field rule and after the pidfd destination.

**The landing.** The `clone3` verdict no longer answers for the flag; it carries
the `cgroup` word in `Clone3Plan` and answers the one rule the flag word owns
(`EOPNOTSUPP` beside `CLONE_THREAD`). `proc::handle_clone3` resolves the
descriptor through the caller's table, asks
`vfs::cgroupfs::clone_target` what it names, and hands the group to
`sched::clone` in the request. That function is the tree's whole answer:
`EBADF` for a node that is not a `CgDir`, `ENOENT` for a group whose name was
removed (the path is resolved again and its inode compared, so a
`rmdir`-and-recreate of the same name is not the group the descriptor holds) or
that the caller's view hides, and `EACCES` from `may_attach` — the common
ancestor of the caller's group and the target, and that ancestor's
`cgroup.procs` asked through `may_as` for write **and** read.

Inside the spawn, `join_group` writes the membership in the same critical section
that used to call `inherit_membership` only — the child is on the ready queue
before `clone` returns, so the write cannot come after it. The child's cgroup
**namespace**, when `CLONE_NEWCGROUP` is beside the flag, is now rooted at
whatever the child's own group is (the target when the flag named one, the
parent's group otherwise), which is Linux's rule and the reason the old
`ns_root(parent)` call was right only by coincidence.

**One pre-existing divergence this measurement exposed and closed**: the tree
reported Linux's modes and policed nothing. `CgDir`/`CgFile` now
`keeps_modes()`, so the `0644` `cgroup.procs` is a gate rather than decoration,
as kernfs's is — measured: uid 1000's write to a `0644` file is `EACCES`, and
`stat`'s word is no longer a claim about a permission model the tree does not
have. (procfs/sysfs stay as item 37 left them: *their* word is a report, and
that difference is now recorded in the cgroupfs header.)

**Pins.** `t-cgroupfs` grows the door's tree half, **12 claims** (26 in the file → 38): the child lands
in the target and its own fork stays there, the caller's line does not move, the
caller's own group is a served target, `CLONE_NEWCGROUP` roots the namespace at
the child's group, `EBADF` for a regular file / an attribute file / a
descriptor that names nothing, `O_PATH` served, `ENOENT` for a removed group,
`EOPNOTSUPP` for `CLONE_THREAD`, and `EACCES` from a forked uid-65534 caller.
`t-clone` gains the `clone(2)` half (the bit is accepted and ignored), and
`t-clone3`'s `EBADF` claim is reworded to what it always measured — fd 0 is not
a cgroup. Host-side: four new `vfs::cgroupfs` tests (`the_reported_modes_are_gates`,
`a_descriptor_names_a_group_not_a_file`, `a_removed_group_is_gone_not_a_group`,
`the_view_and_the_common_ancestor_are_path_questions`) and the `clone3` verdict's
new pair.

**What stays named.** The mount view of a cgroup namespace (§6.25's divergence)
is untouched: this change re-roots the *namespace the child carries*, not a
mount. And the tree has no delegation: a caller whose nested user namespace owns
a subtree is refused by the common ancestor's DAC, where Linux would let it
through — there is no per-cgroup ownership here for a namespace to hold, so the
machine root's file is the only authority the tree has.

**Gate**: x86_64 `pass=208 fail=0` (208 built), `mpos-kunit 32 passed, 0 failed`,
exit 33; aarch64 `pass=207 fail=0` (207 built), `mpos-kunit 32 passed, 0 failed`,
the machine reaching `=== END`; host unit suite **1120 tests, 0 failed**;
`t-cgroupfs`'s **38** `ok:` lines byte-identical on both machines, and its first
gate run failed exactly three claims — the `EBADF` ones, expected value written
as `ENOTDIR`'s 20 instead of `EBADF`'s 9, which is the pin doing its job.

### 6.27 The pid a door answers with is the caller's namespace's (LANDED 2026-10-25)

Item 106's own pin found this, and named it there rather than working around it
silently. Inside a pid namespace, every door that hands a pid back answered the
**scheduler's task slot**: `fork`/`vfork`, `clone` and `clone3`'s return value,
`wait4`'s return value and `waitid`'s `si_pid`.

**Why no pin had caught it.** A parent *outside* a pid namespace reads the
child's number in the parent's namespace, and for the init namespace a task's
number **is** its slot - so the two agree and every existing claim
(`t-ns-pid`, `t-wait4`, `t-waitid`) was green. The shape that fails is a parent
**inside** the namespace, which is what a container's own process is: it got a
number no other call accepts, so the child it had just created could not be
killed, waited for, or found in `/proc` by it. Measured in the gate before the
fix: a child whose own `getpid()` was `2` was announced by its parent's `fork`
as `9`, and `waitpid(2)` answered `9` for it.

**Measured on a current host first** (`.workbuddy/probe_pid_numbers.c`, Linux
7.2.11, the probe standing as pid 1 of a fresh pid namespace, so every number is
small enough to read):

```text
fork                       parent 2   child getpid 2   wait4(-1) 2
clone3                     return 5   child getpid 5   wait4(-1) 5
waitid(P_PID, 6)           si_pid 6
clone(PARENT_SETTID)                parent's word 3   child getpid 3
clone(NEWPID|PARENT_SETTID)         parent's word 4   child getpid 1   wait4(-1) 4
clone(NEWPID|CHILD_SETTID)          child's word  1   (its own getpid)
```

So the doors answer the child's number **in the caller's namespace**, and the
two `SETTID` words are deliberately *not* the same number: the parent's word is
the parent's number for the child (4, not the child's own 1), while the child's
own word is its own number (1). A first guess had both as the child's own; the
probe corrected it.

**The landing.** One helper, `pid_answer`, answers every door - they are one
question, and a caller uses one door's answer in another's call (`kill(fork())`
must work, and `wait` must report what `fork` answered). It reads the number
off the live task where there is one, and off the **record** for a death: by the
time a `wait` reads a record the slot is reaped and the pid registration with
it, so `Retired` carries `numbers` - one `(namespace, number)` pair per level of
the child's chain (`crate::pidns::numbered_chain`) - exactly as it already
carries `pgid`, `uid` and the CPU for the same reason. `CLONE_PARENT_SETTID`'s
write is that same answer; `CLONE_CHILD_SETTID`'s is `own_pid_number`, the
child's number in its own namespace.

**Pins.** The new **`t-pid-numbers`**, **4 claims**, green on a real host first
and `ok:` for `ok:` identical on both machines: fork's answer / the child's own
number / the wait's report are one number; `CLONE_PARENT_SETTID` carries the
child's number in the caller's namespace; the asymmetric nested pair (child `1`
to itself, the parent's SETTID word and wait the parent's number, the child's own
SETTID word `1`); and `waitid(P_PID)`'s `si_pid`. And **`t-kill-all` loses its
workaround**, which is the second half of the evidence: item 106's pin had to
carry each child's own `getpid` down a pipe and reap children by "any" to avoid
this bug; it now names them by `fork`'s answer and waits by that number, so the
pin depends on the fixed contract instead of dodging the broken one.

**Gate**: x86_64 `pass=210 fail=0` (210 built), aarch64 `pass=209 fail=0` (209
built), `mpos-kunit 32 passed, 0 failed` on both; host unit suite 1121 tests / 0
failed; `t-pid-numbers`' and `t-kill-all`'s `ok:` lines byte-identical on both
machines.

**Named.** `t-ns-pid`'s parent-side claims are unchanged - they pin the
outside-parent shape, which was always right. `pid_answer` keeps a slot fallback
for a record made before the chains were kept and for a task outside the
caller's chain; no door in the ABI can produce the second.

### 6.28 `process_madvise(2)` is `madvise` at a pidfd, and only four advices cross — **LANDED** (2026-10-27)

The pidfd family's last `ENOSYS`. §6.10 deferred it on a true measurement - its
one advice with real work behind it (`MADV_DONTNEED`) is refused for another
process on this host, and the advices it does accept are hints - but that
measurement read the advice list as the service. The service is the *shape*: a
caller branches on the byte count it gets back, on which refusal comes first when
it passes two bad arguments, and on whether the range it named was walked before
the answer. A program that asks another process to `WILLNEED` a range is right to
expect `4096` back, not `0`; a caller that got `EINVAL` for a cross-process
`DONTNEED` is right to conclude the advice does not cross rather than that its
pidfd was bad. So the door is served on the measured contract, and the two visits
to the same question (§6.10's "probed and deferred", this section's "served")
differ in what was measured, not in what the host said.

**Measured first, on the host** (Linux 7.2.11, uid 1000 with an empty effective
capability set; `.workbuddy/probe_process_madvise.c`, then `probe3.c` through
`probe8.c`, every probe printing `rc`/`errno` per case):

* **The answer is a byte count.** One 4096-byte iovec answers `4096`; a 2000-byte
  one answers `2000` (the range is not rounded to pages); a 0x1800-byte range over
  two mapped pages answers `6144`; two iovecs answer `8192`; a zero-length iovec
  contributes `0` and is not even alignment-checked; `vlen = 0` answers `0`. A
  range longer than `MAX_RW_COUNT` answers **the clamp**: a lazy 1.75 GiB VMA
  advised with `MADV_COLD` answers `1879048192` (`0x70000000`), and one advised
  with `0x80000000`, `0xffffffff` or `0x100000000` answers `2147479552`
  (`0x7ffff000`).
* **The import refuses three shapes, and clamping is not one of them.** The iovec
  is read as Linux's `(void __user *base, ssize_t len)`, so an `iov_len` above
  `SSIZE_MAX` is a *negative* length and answers `EINVAL` (`SIZE_MAX` and
  `0x8000000000000000`); a length above `MAX_RW_COUNT` is clamped; and the clamped
  range is checked against user space with `access_ok`'s arithmetic -
  `len > TASK_SIZE_MAX || base > TASK_SIZE_MAX - len` → `EFAULT`, with
  `TASK_SIZE_MAX = (1<<47) - 4096`. Measured at the edge: base `0x7ffffffff000`
  with 4096 is `EFAULT` while base `0x7fffffffe000` with 4096 is *served*, base
  `0x800000000000` is `EFAULT` even with `len = 0`, and base
  `0xfffffffffffff000` with 4096 or 0x2000 is `EFAULT`. The whole array is
  validated before anything is processed: a good iovec followed by a bad range is
  `EFAULT`, not a partial success.
* **The refusal order is fixed and is not the argument order.** `flags != 0` →
  `EINVAL` (even beside an unreadable iovec, a bogus advice and `pidfd = -1`);
  `vlen > 1024` → `EINVAL` (before the array is read: `vlen = 1025` with a NULL
  and with a `0x1` pointer both answer `EINVAL`); the iovec array → `EFAULT`/
  `EINVAL` (so `pidfd = -1` with an unreadable array answers `EFAULT` - the
  descriptor is never reached, and a *good* pidfd with a `0x1` array is `EFAULT`
  too); the pidfd → `EBADF` (`-1`, a `/dev/null`, a pipe's read end, a closed
  `999`); the task → `ESRCH` (a reaped pid cannot even be opened - `pidfd_open`
  answers `ESRCH` - while a zombie's descriptor answers `ESRCH` because its
  address space is gone); the authority → `EPERM` (pid 1 answers it for advice 3,
  4 and 5 **and with no iovecs at all**); then the vocabulary → `EINVAL`.
* **The vocabulary gate runs with no ranges at all.** `vlen = 0` with advice 5 is
  `EINVAL`, while `vlen = 0` with 23, 24, 25, 100, 101, 102 and 103 answers `0` -
  so the gate the syscall applies up front is the **union** of `madvise`'s known
  vocabulary and the four-advice cross set, and the caller's own process then
  applies `madvise`'s own rules per range.
* **Only four advices cross a process boundary** - `MADV_WILLNEED` (3), `COLD`
  (20), `PAGEOUT` (21), `COLLAPSE` (25); that is Linux's
  `process_madvise_behavior_valid`. Measured against a same-uid child while the
  caller held no capabilities: those four answer `EPERM` (the `CAP_SYS_NICE` gate,
  which this bench could not pass) and **everything else answers `EINVAL`**,
  `MADV_DONTNEED` included - so the vocabulary is judged *before* the capability,
  and `DONTNEED` cannot cross even for a privileged caller. The self case is the
  exception: every advice `madvise` accepts is accepted (`0..=4`, `8`,
  `10..=24`, `102`, `103`), `5`, `6`, `7`, `9` and `25` are `EINVAL` on a range,
  and `100`/`101` are `EPERM` (recognised, privilege missing) - the last pair is
  `madvise`'s own divergence here and is inherited below rather than re-decided.
* **A hole is `ENOMEM`, and what the walk already did stands.** A page belongs to
  a range when its **start** is before the range's end, so 0x1800 bytes over two
  mapped pages is served and 0x2001 bytes over the same two is `ENOMEM`; one
  mapped page and 0x1800 bytes is `ENOMEM`. `MADV_DONTNEED` over "page then a
  hole" answers `ENOMEM` **and the mapped page has already been dropped** (the
  probe's `0x5a` reads back `0x00` afterwards), while "hole then page" leaves the
  second page untouched; a completed iovec followed by a failing one answers the
  bytes served (`4096`), and a failing first iovec stops the loop.
* **The self `DONTNEED` really drops**: after a 4096-byte answer the host's
  `mincore` reads the page absent.
* **An instrument lesson, not a kernel one.** The first probe crashed (`SIGSEGV`
  at `0x308`) because its anonymous pages merged with glibc's own VMAs and a
  `MADV_DONTNEED` over the tail zeroed the thread control block - the failure read
  as the *kernel's* fault when it was the probe's. Every probe from `probe3.c` on
  reserves `MAP_FIXED_NOREPLACE` at `0x500000000000` and unmaps the pages it wants
  to be genuine holes (`crash.c`, `crash2.c`, `crash3.c` carry the diagnosis).

**What was built** (one commit, `src/syscall/linux/`):
`nr::PROCESS_MADVISE = 440` in both number tables (the same number on both
machines); the dispatch arm beside the pidfd family; `handle_process_madvise` in
`proc.rs` deciding in the measured order above; the pure half in `abi.rs` -
`process_madvise_import` (the signed-length check, the clamp, the `access_ok`
arithmetic, host-tested against every probe row) and
`process_madvise_advice_is_served` (the four-advice set) - so the numbers are
reviewable on the host rather than restated in a machine-gated file; and
`crates/cred`'s `CAP_SYS_NICE = 23` (the capability had no constant because
nothing asked it yet; `capable()` is the initial-namespace question, which is
Linux's `capable`). The caller's own ranges reuse `madvise`'s policy (`abi::
madvise_advice_is_known` for the per-range vocabulary,
`sched::madvise_dontneed` for the drop) and ask the mapping table only where a
hole can be: `vma_gaps` names the range's undescribed stretches, `user_mapped`
confirms each page of them (the program image, the stack and the heap have a page
but no VMA, and a long lazy mapping must not cost a per-page walk of 2 GiB).
The iovec array is read **twice** - a validating pass and a processing pass -
rather than copied into a `[Iovec; 1024]` array: the copy would be 16 KiB on a
kernel stack several times smaller, and nothing in this handler yields between the
passes, so the second read sees the bytes the first validated.

**Two alternatives were rejected.**
* A cross-process range walk. `madvise_dontneed` and `user_mapped` both act on
  the *calling* task's address space, and this kernel has no per-target page-query
  API; building one for a hint whose whole measured effect is a byte count would
  be a new `sched` traversal for no answer that can differ. So a cross-process
  hint is answered from the argument shape (alignment and length), which is
  exactly what Linux returns when the range is mapped - and the hole case, which
  would differ, is named in §6.10/uapi §1.2 rather than guessed at.
* Answering `MADV_COLLAPSE` across a process with the same no-op. Linux collapses
  the range into a huge page or answers `EINVAL`/`EAGAIN`; a no-op there would be
  a *success* for something the caller can check afterwards, which is the one
  thing this codebase must not do. It is `EOPNOTSUPP` - the call exists here, this
  kernel cannot do that - and it is named.

**Gate**: x86_64 `pass=216 fail=0` (216 built, 6 lanes, 14 of 230 left out by
`tests.toml`), aarch64 `215 staged, pass=215 fail=0` (15 of 230 left out),
`mpos-kunit 34 passed, 0 failed` on both; host unit suite 642, `xtask` 145;
`cargo fmt --check` clean; clippy 0 errors on `x86_64-unknown-none` and
`aarch64-unknown-none`; the depgraph ratchet holds - `syscall -> sched` and
`syscall -> vfs` are edges the pidfd family already recorded, and the only new
cross-crate name is `cred::CAP_SYS_NICE`. `t-process-madvise` is ungrouped, so it
runs on both machines by default; it is also the same claim set on a host with
`MPOS_ON_HOST=1`, where the four claims that need the guest's root capability
(advice 100, cross-process `WILLNEED`, cross-process `COLLAPSE`, and the
capability-dropping child) are named rather than made.

**Counterfactual**: the dispatch arm removed, so 440 falls to `_ => ENOSYS` -
**33 claims red**, of the 45 the program made in that run (the fixture-setup and
`pidfd_open` claims stay green because they never reach the door), and no other
program in the sweep red.

**Named**: three corners are this kernel's answers rather than Linux's, and each
has its measurement above: a cross-process range's *holes* are not seen (no
per-address-space query); `MADV_COLLAPSE` across a process is `EOPNOTSUPP` (no
huge-page pool); and the call inherits `madvise`'s two divergences - `100`/`101`
answer `EINVAL` here where Linux asks `EPERM`, and a plain `madvise` over an
unmapped range answers `0` here where Linux answers `ENOMEM` (§5.2, `t-madvise`).
A cross-process hint's *effect* is likewise the no-op Linux's hints are on this
kernel: there is no page cache to warm and no reclaim to steer, which §5.2
already records for `madvise`.

### 6.29 `futex_waitv(2)` — several words, one sleep, and the index that woke it (LANDED 2026-10-28)

**Why**: `FUTEX_WAIT` sleeps on one address. A runtime whose park condition is
"whichever of these events happens first" cannot express that with a
single-address wait, so it polls — and `futex_waitv` (449) is the call that
exists for exactly that gap. It was the futex family's last `ENOSYS` that a
real thread runtime reaches for (`docs/uapi/01-syscall-table.md`).

**Measured first**, on a Linux 7.2.11 host, before any kernel code:
`build/scratch/futex-waitv/probe_waitv.c`, `probe_flags.c` and
`probe_shared.c` (all three kept in the tree's scratch dir). The probe's first
shape put `FUTEX_PRIVATE_FLAG` in the call's own `flags` word and every
positive case answered `EINVAL`; a ctypes script and `strace` showed the
argument, and Linux 6.12's `SYSCALL_DEFINE5(futex_waitv, …)` states the rule in
one line: *"This syscall supports no flags for now"*. With the call's flags
zero the whole shape is answerable:

| shape | Linux's answer |
|---|---|
| one waiter `FUTEX_32 \| FUTEX_PRIVATE`, woken by `FUTEX_WAKE` | `0` (the index) |
| two words, second woken | `1`; first woken | `0` |
| word already moved | `EAGAIN` (11) |
| moved word **and** a past deadline | `EAGAIN` — the value check runs first |
| `nr = 0` / `nr = 129` / NULL array | `EINVAL` |
| call-level flags `0x80`, `0x2`, `0x100`, … | `EINVAL` |
| waiter flags: `0x2`, `0x82` accepted; `0x1`, `0x4`, `0x8`, `0x80` alone, `0x84`, `0x1000`, `0x1000000` | `EINVAL` (only `FUTEX_32` and `FUTEX_PRIVATE_FLAG`) |
| `__reserved = 1` | `EINVAL` |
| word at offset 1 | `EINVAL` |
| unreadable array, unreadable word | `EFAULT` |
| 60 ms absolute deadline, monotonic or realtime | `ETIMEDOUT` |
| invalid `clockid` with a timeout | `EINVAL`; with a NULL timeout | ignored (the value's own answer) |
| shared waiter (`0x2` without `0x80`), same process and cross-process | served by Linux |

The per-waiter flag table came from `probe_flags.c` measuring, for a 32-bit
word holding `0xdeadbeef`, which candidate flag words block (`ETIMEDOUT`,
meaning the width's read matched), mismatch (`EAGAIN`) or are refused
(`EINVAL`): exactly `0x2` and `0x82` blocked. `probe_shared.c` also measured
that a *private* waiter is not reached by a shared wake (`0` woken,
`ETIMEDOUT`) — the two tables are genuinely different keys.

**Landed**: `src/sched/futex.rs` — `Wait { address, expected }`,
`register_all`, `unregister_all`, `first_woken`, and
`wait_multiple(waits, deadline) -> isize`: register every place, *then* check
every word (`EAGAIN` on the first that moved, after unregistering all — the
same lost-wake ordering `wait_bitset` rests on), then one
`block_until_nanos(|| first_woken(...).is_some(), deadline)`, then the earliest
marked index, `ETIMEDOUT` or `EINTR`. `src/syscall/linux/proc.rs` —
`handle_futex_waitv` parses the 24-byte waiters (read unaligned, like
`futex_deadline`'s timespec), enforces the table above, and caps the array at
`WAITV_MAX = 128` for the worst-case rule. `nr_x86_64.rs`/`nr_aarch64.rs`: 449
on both (the call landed after the number tables converged).

**The divergence named**: a shared waiter (`FUTEX_PRIVATE_FLAG` absent) is
`EOPNOTSUPP`. The wait table is keyed by *user address*, so two processes'
mappings of one shared word cannot be matched; serving it would park a caller
no waker can reach. Linux serves it (measured above, cross-process too), so the
refusal is a named gap rather than a claim about Linux; `FUTEX_WAIT`'s older arm
keeps its own keying divergence recorded and unchanged.

**Pin**: new `t-futex-waitv`, default group, both machines: 20 claims — the two
index answers, `EAGAIN`, monotonic and realtime deadlines (with the elapsed
time inside 40–400 ms), the whole `EINVAL` list, `EFAULT` for both pointers,
the `EAGAIN`-before-deadline order, and (guest only) the shared refusal. The
host tests `a_multi_wait_holds_a_place_on_every_word_and_names_the_one_that_fired`
and `a_multi_wait_names_the_earliest_word_when_several_fire` pin the table half.
The same binary run on the host with `MPOS_ON_HOST=1` passes every claim against
Linux: 20 of 20, so the test is Linux's answers and not this kernel's
expectations written down.

**Gate**: x86_64 `pass=219 fail=0 (219 built)`, aarch64 `pass=218 fail=0 (218
built)`, `mpos-kunit 34 passed, 0 failed` on both; host suite 649 passed (647 +
the two new table tests). **Counterfactual**: dispatch arm removed →
`pass=218 fail=1 (219 built)`, `t-futex-waitv` red on every claim (each call
falling to `_ => ENOSYS`), no other program red. Restored, green.

**Named**: the shared spelling above; and `FUTEX2_SIZE_U64`/`FUTEX2_NUMA`
shapes do not exist in this kernel's answer set at all — Linux 6.12 itself
accepts neither on this probe (`0x4`/`0x8`/`0x10` all `EINVAL`), so there is no
gap to name there yet.

### 6.30 The kernel keyring — `add_key(2)`, `request_key(2)`, `keyctl(2)` — **LANDED (2026-10-28)**

**Why**: the three calls were a blank `ENOSYS` — `t-enosys` walked `keyctl` and
nothing else — and they are not optional for the programs this kernel exists to
run: `keyutils`, PAM's session keyring, and the TLS/session stores several
stacks keep there. Their shape is small, closed, and (unlike a notification
engine) entirely synchronous, which is what made them a done-in-one-pass item
rather than a subsystem.

**Measured first** (Linux 7.2.11, `build/scratch/fanokey/`; the probes are
`probe_keyctl*.c`, `probe_keyperm.c`, `probe_keyfork.c`, and the caller was an
ordinary user, uid 1000, no `CAP_SYS_ADMIN`):

| Question | Measured answer |
|---|---|
| `add_key` answer | a positive serial; the *second* add of the same type+description in the same ring updates the payload **in place** and answers the **same** serial |
| `keyctl(READ)` copy rule | answers the whole payload length and copies **only** when the buffer can hold it all (5 into a 10-byte key: answered 10, copied nothing; NULL/0: answered 10) |
| a keyring's `READ` | its link list as `u32` little-endian serials, 4 bytes per link, `0` when cleared |
| `DESCRIBE` | `"type;uid;gid;perm%08x;desc"`, NUL-terminated, **length counts the NUL** (30 for a 29-byte string); default perm word `0x3f010000` |
| perm ladder | `VIEW 0x01`, `READ 0x02`, `WRITE 0x04`, `SEARCH 0x08`, `LINK 0x10`, `SETATTR 0x20`; possessor/owner/group/other nibbles |
| `UPDATE` | `0`; on a keyring `EOPNOTSUPP` |
| `SEARCH` | the serial, **recursing into nested keyrings**; `UNLINK` of a key not in the ring `ENOENT` |
| possession | a permission *class*, not a filter: a key unlinked from every ring is still found by serial, `READ` on it is `EACCES`, `DESCRIBE` still works |
| states | `REVOKE` → every operation `EKEYREVOKED` (state checked before permission); `INVALIDATE` → `ENOKEY` everywhere |
| `SETPERM`/`CHOWN` | an ordinary owner is refused `EACCES` even while possessing the key — only privilege rewrites the word |
| `add_key` refusals | unknown type `ENODEV`; 4 096-byte description `EINVAL`; user payload 32 768+ `EINVAL`; NULL payload with a length `EFAULT`; a ring not possessed `ENOKEY`; **zero-length payload `EINVAL`** (user), while `"keyring"` takes exactly zero bytes and `EINVAL`s three |
| quota | per-user 200 keys / 20 000 bytes as an ordinary user, over → `EDQUOT` |
| rings | thread ring absent until asked for; `GET_KEYRING_ID` create=0/1; `JOIN_SESSION_KEYRING(name)` answers a serial that `GET_KEYRING_ID` then reports |
| fork | the child inherits the **session** ring (same serial); its process and thread rings are fresh |
| `request_key` | a miss is `ENOKEY` (with or without callout information); a destination ring the task no longer holds is `EACCES`, not `ENOKEY` |
| unknown `keyctl` command | `999` answers `EOPNOTSUPP` |

**Landed**: `src/sched/keys.rs` is the store — `Key` objects (`type`, description,
owner pair, permission word, `State`, `Payload`), a `BTreeMap<Serial, Arc<Key>>`,
per-task thread rings, per-group process/session rings, per-user user rings, a
usage table for the quota, the possession walk, and the fourteen
`keyctl`-command functions. `src/syscall/linux/keys.rs` is the argument layer —
three handlers plus `payload_arg` (whose rules are the *type's*: `"user"`
requires at least one byte, `"keyring"` exactly none), `copy_out` (answer the
length, copy only when the buffer fits, NUL-terminated for `DESCRIBE`), and the
command table. Numbers 248/249/250 on x86_64 and 217/218/219 on aarch64 are in
the per-arch tables; the dispatch arms sit after `FUTEX_WAITV`. Lifetime lives in
the scheduler where the state does: `hand_down_keyrings` from `fork`/`vfork`/
`clone` (session inherited, process and thread rings fresh) and
`exit_cleanup` from `child_exit::exit` (thread ring at task death, group rings on
the last thread). The four new errnos (`EDQUOT`, `ENOKEY`, `EKEYEXPIRED`,
`EKEYREVOKED`) sit in `crates/err` beside their measured answers.

**Named divergences** (each a refusal or a recorded difference, not a silent
approximation):

1. `request_key` has **no upcall**: a miss is `ENOKEY` even when callout
   information is passed, where Linux forks `/sbin/request-key`. Serving the
   shape without the helper would be the "nearly right" failure §3 ranks worst.
2. Privilege is `uid == 0`, there being no capability model — that is the
   `CAP_SYS_ADMIN` spelling for `SETPERM`/`CHOWN` and the root quota
   (1 000 000 keys / 25 MB) is that spelling too.
3. The possession walk traverses keyring links without consulting the link's
   own `SEARCH` bit; Linux's `keyring_search` asks it at each hop. The permission
   *classes* on the key itself are enforced, so this can only widen possession.
4. Keys are not garbage-collected when the last possessor exits: the serial
   stays findable (with `EACCES` on `READ`), and the quota it took stays charged,
   where Linux runs a GC pass after a delay.
5. `CHOWN` to a *different* owner as an unprivileged caller is `EACCES`; the
   host measured `EINVAL` for that shape (asking for the ids it already had).
   The refusal direction is kept.

**Pins**: `user/tests/src/bin/t-keyctl.rs`, default group, both machines — 43
claims: the ring/join rules, add/read/describe/update copy rules, the whole
refusal list above, links and nested search and clear, both states, possession
after unlink, `SETPERM` split by privilege, unknown command, fork inheritance,
and the destination-ring `EACCES`. Run on the host first: the same binary with
`MPOS_ON_HOST=1` passes **43 of 43** against Linux, so the test asserts Linux's
answers rather than this kernel's expectations. Ten host tests in
`src/sched/keys.rs` pin the table half, including the quota refusal
(`the_unprivileged_byte_quota_answers_edquot`).

**Gate**: x86_64 `pass=220 fail=0 (220 built)`, aarch64 `pass=219 fail=0 (219
built)`, `mpos-kunit 34 passed, 0 failed` both; host suite 659 passed.
**Counterfactual**: the three dispatch arms removed → `pass=219 fail=1 (220
built)`, only `t-keyctl` red (every call falls through to `_ => ENOSYS`), and
`t-enosys` stays green because it no longer probes a served call. Restored,
green.

**Moved with it**: `t-enosys` lost its two `keyctl` claims (the twice-asked
once-per-number rule now rides on `userfaultfd`), `docs/uapi/01-syscall-table.md`
moved the number out of §1.1 into the served table, and `item 113` records the
full write-up.

### 6.31 The `t-posix` "getpriority answers 20" flake is `t-priority`'s group set left in the runner — **diagnosed (2026-10-28)**

`t-posix`'s first priority claim - *"getpriority answers 20 (nice 0, kernel
encoding)"* - reddened runs that touched no priority path: once on 2026-10-23
(recorded at §6.18's close), then repeatedly on aarch64 arms in the days after.
It was written down as a flake with its two readings and re-run, because the
mechanism was not established.

**The mechanism.** `t-priority` sets `PRIO_PGRP, 0, -2`. That is the **whole
process group's** nice, and the runner shell that spawns the program is in that
group, so the shell's nice becomes `-2`. The program's final restore named
`PRIO_PROCESS`, not the group - so the set survived it, the runner carried `-2`
for the rest of the lane, and every later program in that lane inherited it:
Linux's rule is that the nice is inherited across `fork` and kept across `exec`,
which is exactly what a shell does to run the next `t-*` program. `t-posix`
then asks a *fresh* process to read the raw encoding, which is `20 - nice`; with
the inherited `-2` the answer is `22`, and the claim fails. This is the test's
own side effect rather than a kernel bug - Linux would answer the same way - and
the "flake" name was the cost of not following the process group.

**Demonstrated**, not inferred: a temporary carrier program ran
`/bin/t-priority` and then `/bin/t-posix` from one process tree, in one lane.
Before the fix, that lane's log read

```
carry: t-priority Ok(Some(0))
FAIL:  getpriority answers 20 (nice 0, kernel encoding)
carry: t-posix Ok(Some(1))
```

and after the fix the same lines read `carry: t-priority Ok(Some(0))` /
`carry: t-posix Ok(Some(0))` with no `FAIL:`. (The carrier was deleted after the
measurement - it proved the mechanism, and the pin below is what keeps it
closed.)

**Why it looked random**: the sweep deals programs to lanes by cost (LPT), so
whether `t-priority` lands in the same lane *before* `t-posix` depends on the
cost table's current shape. The two arms have their own tables, which is why
one arm reddened and its sibling did not on the same commit.

**The fix**: `t-priority` reads the group's raw encoding first
(`getpriority(PRIO_PGRP, 0)`) and, as its last act, restores it *by group*
(`setpriority(PRIO_PGRP, 0, 20 - encoding)`), with a claim naming the restore:
*"the process group is left at the nice it started with"*. A group set covers
the program's own process too, so the separate `PRIO_PROCESS` restore - which
was the bug - is gone.

**Pin**: that restore claim. Against the old shape it fails (`getpriority
(PRIO_PGRP, 0)` reads `22` after the program's own run); with it, the next
program in the lane reads the fresh `20` that `t-posix` asks for. `t-posix`'s
claim stays as it was: it is the correct expectation and the pollution, not the
expectation, was wrong.

### 6.32 `io_uring(7)` — the ring is a mapping, and a completion carries the blocking call's answer (LANDED 2026-10-28)

**Why**: three numbers (`io_uring_setup` 425, `io_uring_enter` 426,
`io_uring_register` 427) had no dispatch arm, so the honest answer was
`ENOSYS` and a probing application fell back to its synchronous path. That
fallback exists because the *setup* call has no cheap emulation: a program that
sees a ring descriptor then submits SQEs and turns CQEs into results, and a
ring that only accepts the descriptor turns every later promise into a wrong
answer. So the shape that had to be measured first is the ring's own ABI - the
mapping offsets, the SQ/CQ words, and what each opcode puts in `cqe.res`.

**Measured on Linux 7.2.11** (real root: `build/scratch/iouring/probe4.c`,
`probe5.c` - `io_uring_setup` needs no capability here, but the probe was run
in a hand-built ring, no liburing):

| Question | Linux's answer |
|---|---|
| `entries` 0 | `EINVAL` |
| `entries` 1 / 2 / 3 / 5 / 8 / 9 / 1024 / 8192 | `sq=1,cq=2` / `2,4` / `4,8` / `8,16` / `8,16` / `16,32` / `1024,2048` / `8192,16384` |
| `entries` 32768 / 32769 | ok / `EINVAL` |
| published layout, `entries=8` | `sq_off: head 0, tail 4, mask 16, entries 24, flags 36, dropped 32, array 320`; `cq_off: head 8, tail 12, mask 20, entries 28, overflow 44, cqes 64, flags 40`; `features=0x3ffff` |
| the three mmaps | `IORING_OFF_SQ_RING=0`, `CQ_RING=0x8000000`, `SQES=0x10000000` all map; a **zero-length** mmap is `EINVAL` |
| `io_uring_enter` | returns the count of SQEs it consumed (bounded by the SQ entries/CQ room: 20 offered on an 8-entry ring returned 8), `EBADF` for a bad fd, **`EOPNOTSUPP` for a valid fd that is not a ring** |
| `GETEVENTS` with an empty CQ | blocks until a completion arrives; a signal ends it with `EINTR` |
| `NOP` | `res=0`, `user_data` preserved |
| `READ` / `WRITE` / `READV` / `WRITEV` | the byte count (`16` / `2` / `16` / `16` in the probe), scattered across iovecs for the vectored forms |
| `FSYNC` / `CLOSE` | `0` / `0`, and after `CLOSE` the descriptor is really closed (`fstat` -> `EBADF`) |
| an opcode no Linux has (250) / a bad SQE fd | `-EINVAL` / `-EBADF` |
| register `FILES` with two real fds / `FILES NULL` / op 4095 / a non-ring fd | `0` / `EFAULT` / `EINVAL` / `EOPNOTSUPP` |

**Landed shape.** `src/vfs/io_uring_abi.rs` is the host-compiled half: the
layout constants, `sq_entries_for` (round up to a power of two, cap 1024),
`params_words` (the exact 120 bytes Linux writes back), `array_offset`, and
four host tests pinning the measured words. `src/vfs/io_uring.rs` is the ring
object: two frame ranges (the SQ/CQ page range and the SQEs range), allocated,
zeroed and published with Linux's own offsets, mapped through the generic
`frame_backed`/`mmap_frame` path with a per-region `mmap_validate` (the three
`IORING_OFF_*` indices are decoded by index range, and any other offset is
`EINVAL`). `src/syscall/linux/io_uring.rs` is the ABI surface: `setup` builds
the ring, reads the caller's `params.flags` word (2, not an argument - Linux's
call takes exactly two), validates the flag mask and publishes the layout;
`enter` executes each SQE **synchronously through the very handler the
blocking syscall uses** (`handle_read`/`handle_pread64`/`handle_write`/
`handle_pwrite64`/`handle_readv`/`handle_preadv`/`handle_writev`/
`handle_pwritev`/`handle_fsync`/`handle_close`), posts the completion and then
parks once if `GETEVENTS` asked to wait; `register` answers `EOPNOTSUPP` for
every op.

**Named divergences** (each refused rather than faked, and pinned as such):

1. **Every non-zero setup flag is `EOPNOTSUPP`** - `SQPOLL`, `CQE32`, `SQE128`,
   `COOP_TASKRUN`, `CQSIZE`: the measured kernel accepts them, but this ring
   has no poll thread, no 32-byte CQEs and no 32-byte SQEs. A bit outside
   Linux's known mask is `EINVAL`, as measured.
2. **`io_uring_register` is refused for every op** (`EOPNOTSUPP`): the fixed
   buffer/file/index shapes are what registration exists for, and this ring
   resolves its arguments per SQE.
3. **The SQ ring is capped at 1024 entries** (Linux allows 32768) and the CQ is
   always `2 x sq_entries` (`CQSIZE` is refused under 1). The cap is the
   §8.0 memory bound: one ring is up to two frame ranges of four pages each.
4. **An opcode at or beyond `IORING_OP_LAST` (65, the header this was measured
   against) is `-EINVAL`**, matching Linux for a number no Linux has; an
   opcode below it that this kernel does not serve completes `-EOPNOTSUPP`,
   so a ring running a served opcode keeps working while a new Linux's opcode
   is refused per item rather than per ring.

**Pinned** by `t-io-uring` (guest run; the same binary under
`MPOS_ON_HOST=1` asserts Linux's answers and passes 22/22 there, which is the
reference direction): the hand-built ring reads its three mappings, checks the
published words, runs `NOP`/`READ`/`WRITE`/`READV`/`WRITEV`/`FSYNC`/`CLOSE`,
checks `EINVAL` for `entries=0`, `EINVAL` for an unknown bit, `EBADF`/`EOPNOTSUPP`
on `enter`, `EINTR` from the `GETEVENTS` park (via a one-second `SIGALRM`), and
the three divergences above on the guest only.

**Counterfactual**: the three dispatch arms removed (`// TEMP-CF` in
`src/syscall/linux/mod.rs`, backup `build/scratch/mod-io_uring.bak`, restored in
the same job): `CF-EXIT=1`, `pass=221 fail=1 (222 built)`, and only
`t-io-uring` red - it panics at its own `Ring::setup(8)` with
`a ring of eight entries can be built: -38`, because `io_uring_setup` then
falls to the default arm and answers `ENOSYS`. Every other program stays
green, `t-enosys` included (it no longer probes the three numbers). After the
restore, `grep -c 'nr::IO_URING_' src/syscall/linux/mod.rs` is 3.

### 6.33 `userfaultfd(2)` — the fault becomes a message, and the answer is the page — **LANDED (2026-10-28)**

**Why**: a live-migration or lazy-restore program maps memory it has no
contents for yet and wants to be *told* about each page it touches, so it can
supply the bytes itself. That is `userfaultfd`: a descriptor carrying a queue of
32-byte pagefault records, a set of registered ranges, and a fault on a
registered page that parks the faulting task until the program answers. The
call had no dispatch arm at all - a probing libc read `ENOSYS` and took its
fallback - so this is new ABI rather than a gap being closed.

**Measured first, as real root.** `userfaultfd` is `EPERM` for an unprivileged
user on this host (`/proc/sys/vm/unprivileged_userfaultfd` is `0`), and `uid 0`
inside `unshare -Ur` is `EPERM` too, because the call asks `CAP_SYS_ADMIN` in
the **initial** user namespace. So the shape was measured on Linux 7.2.11 as
real root: the host kernel booted in QEMU with an initramfs whose `/init` *is*
the probe (`build/scratch/uffd/probe.c`, `probe3.c`, packed by
`build/scratch/uffd/mkcpio.py`; the recipe and its one trap - a newc cpio's
magic is the six characters `070701`, not a zero-padded hex field - are in the
probe's own directory). A faulting task must be a **thread** of the group that
holds the descriptor: a `fork` gives the child no uffd context without
`UFFD_FEATURE_EVENT_FORK`, so its touch is `SIGBUS`.

| Question | Linux's answer |
|---|---|
| `sizeof` the shapes | `uffd_msg` 32, `uffdio_api` 24, `uffdio_register` 32, `uffdio_copy` 40; `UFFD_API` = `0xaa` |
| open's flags | `O_CLOEXEC` and `O_NONBLOCK` honoured (`F_GETFD` 1 / `F_GETFL` `O_RDWR`); `UFFD_USER_MODE_ONLY` (`0x1`) accepted; `0x2` and `0x40000000` are `EINVAL` |
| `UFFDIO_API` | `api = 0xaa` answers 0 with `features = 0x1ffff`, `ioctls = 0x8000000000000003`; a wrong api number, an unknown feature bit and a **second** handshake are all `EINVAL` |
| `UFFDIO_REGISTER`, `MODE_MISSING`, anon | 0 with `ioctls = 0x13c`; the same span twice is 0; `mode = 0`, an unaligned start, `len = 1`, `len = 0` and an unmapped span are all `EINVAL` |
| a read fault | one 32-byte record: `event = 0x12`, `flags = 0`, `address` page-aligned, `ptid = 0`; a buffer shorter than 32 is `EINVAL`; a non-blocking read with nothing queued is `EAGAIN` |
| `UFFDIO_COPY` | 0 with `copy = 4096` and the page really installed; over a **present** page `EEXIST`; an unmapped source `EFAULT`; an unregistered destination `ENOENT`; `len` of 0 or 1 `EINVAL`; an unknown mode bit `EINVAL`; an **unaligned source is accepted** |
| `UFFDIO_ZEROPAGE` | 0 with `zeropage = 4096`, the page reads zero; over a present page `EEXIST` |
| `UFFDIO_WAKE` / `UFFDIO_UNREGISTER` | 0 both, on registered *and* unregistered spans, and unregistering twice is 0 |
| not served by this measurement | `MODE_WP` (needs the write-protect feature), `MODE_MINOR` on anon, `UFFDIO_CONTINUE`, `UFFDIO_MOVE` — each refused by Linux itself in these shapes |
| two threads, one page | **one** record, and one `UFFDIO_COPY` releases both |
| a fork, no `EVENT_FORK` | the child's touch is `SIGBUS` |

**Served, in that measured shape.** The split is `sysv_msg`'s: the machinery is
process state, the descriptor is the file. `src/sched/userfault.rs` holds the
registered ranges, the queue of unanswered pagefaults, the list of tasks parked
on a page, and the fault-path entry point; `src/vfs/userfaultfd.rs` is the
`Inode` face (one record per `read(2)`, `EAGAIN` when non-blocking, `EBADF` for
`write`); `src/vfs/userfaultfd_abi.rs` is the host-compiled shape (the request
words, the mode/flag vocabulary, `pagefault_record`, six host tests);
`src/syscall/linux/userfaultfd.rs` is the syscall and the six ioctls, routed
from `handle_ioctl` by the `0xaa` type byte every `_UFFDIO_*` word carries.

The fault path's involvement is one line in `resolve_anon_fault`: before it
takes the task table it calls `userfault::deliver(page, write)`, which finds the
descriptor that registered the page, queues a record, wakes whoever is in
`read(2)`, parks the faulting task, and answers "somebody else's to resolve" so
no frame is allocated. It has to be there rather than in `populate_in` for a
mechanical reason: parking switches tasks and `populate_in` runs with the table
borrowed. The observable shape is Linux's - the faulting instruction does not
return until the page is there.

**The one bug the gate found, and why it mattered.** The first version parked
the faulting task on the *readable queue*: `read(2)` removes a record when it
hands it to the program, so by the time the program answered with
`UFFDIO_COPY` there was nothing left to wake, and the faulting thread waited
forever (the gate hung, the lane's heartbeat naming
`name=/bin/t-userfaultfd park 2 syscall 202` - futex - and `syscall 14`).
The fix separates the two lists: `State::pending` is the program's mail,
`State::parked` is who to wake when the page arrives. One `UFFDIO_COPY`
releases every task waiting on that page, which is also the measured
two-threads-one-record shape.

**Named divergences, each visible from userland**:

* `features` answers **0** where Linux answers `0x1ffff`. Every
  `UFFD_FEATURE_*` bit is a promise of machinery this kernel does not have, and
  a program that checked the bit and got it back would believe in faults it
  will never receive.
* `MODE_WP`, `MODE_MINOR`, `UFFDIO_WRITEPROTECT`, `UFFDIO_CONTINUE`,
  `UFFDIO_MOVE` and `UFFDIO_POISON` are `EOPNOTSUPP` - present in Linux, and
  not buildable here without a write-protect fault path this kernel does not
  have.
* `ptid` is always 0, the `THREAD_ID` feature not being offered (Linux reports
  0 for it when it was not asked for).
* **The registry is the machine's, not the task's.** Linux scopes uffd context
  per `mm` and gives a forked child none without `EVENT_FORK`; here a forked
  child's fault on a registered page is delivered to the same descriptor and
  parks the child rather than killing it. A program that forked and then
  relied on `SIGBUS` would be relying on a shape this kernel does not have.
* The queue is bounded at `MAX_PENDING = 4096` records and descriptors at
  `MAX_DESCRIPTORS = 64`: a record is an allocation a user steers and the global
  allocator still panics on failure (`docs/notes/` §8.0). A page whose record
  cannot be queued resolves the ordinary way rather than hanging.

**Pinned** by `t-userfaultfd` (guest run; the same binary under
`MPOS_ON_HOST=1` asserts the one claim a host can make - an unprivileged host
refuses the call with `EPERM` - and passes there): the descriptor's flags, the
API handshake and its refusals, registration's five `EINVAL`s, a read fault's
record and its `UFFDIO_COPY` answer, the zero page, the three `EOPNOTSUPP`
refusals, `read`'s short-buffer and `EAGAIN` rules, `WAKE`/`UNREGISTER`, the
two-threads-one-record case, and `t-enosys`'s move from `userfaultfd` to `rseq`
(the only number still falling through).

Full record: `docs/uapi/` §5.1 item 116.

### 6.34 `futex_wake(2)`/`futex_wait(2)` — the futex2 pair — **DONE** (2026-10-28)

The futex family had three spellings before this: the operation-word
`futex(2)` (`FUTEX_WAIT`/`FUTEX_WAKE`), its bitset pair, and the
multi-word `futex_waitv(2)` (`§6.29`). The two remaining numbers are the
syscall-numbered single-word pair: `futex_wake(2)` (454, x86_64 and
aarch64) and `futex_wait(2)` (455, the same on both) — one thirty-two-bit
word, a **bitset** as the wake's mask, and on the wait side an absolute
deadline plus an explicit `clockid`.

Measured first, on the host (`build/scratch/futex2/probe*.c`, Linux
7.2.11), because the argument words are not what the names suggest:

| question | answer |
|---|---|
| `flags` word | `FUTEX2_SIZE_U32` (`0x02`) **required**, `FUTEX2_PRIVATE` (`0x80`) may be added; `SIZE_U8`/`U16`/`U64` and a missing size bit are `EINVAL` on both calls |
| `mask` width | a bitset over the word: wider than 32 bits is `EINVAL`, zero is `EINVAL` |
| `val` width (wait) | thirty-two bits: above `u32::MAX` is `EINVAL`, not truncated |
| word address | unaligned is `EINVAL` on both; aligned and unreadable is `EFAULT` on both |
| `nr` (wake) | an `int`: `0` and a negative count both answer `0`; the answer is the count actually woken, never more than `nr` |
| value mismatch | `EAGAIN`, before any deadline is considered |
| deadline | absolute; `clockid` read only when a deadline is given (`0` realtime, `1` monotonic, else `EINVAL`); with NULL it is ignored; a past deadline is `ETIMEDOUT` |
| `FUTEX2_NUMA` / `FUTEX2_MPOL` | served by Linux 7.2 |

The measurement's own trap: a `mask` of `~0UL` is the *64-bit* spelling and
is refused, while `~0u` waits — the same 32-bit ceiling on the bitset that
`val` has. A probe that passed `~0UL` read the refusal as "NUMA is not
supported", which is what made the node/policy flags look refused until
the mask was narrowed.

**Served, in that shape.** `handle_futex2_wake` and `handle_futex2_wait`
sit beside `handle_futex_waitv` in `src/syscall/linux/proc.rs`; the two
word-level checks (the flag word and the bitset) are host-compiled facts in
`src/syscall/linux/abi.rs` (`futex2_word_flags`, `futex2_bitset`) with host
tests, so the refusals are pinned off the machine. The wait reuses
`sched::futex::wait_bitset` (the same table `FUTEX_WAIT_BITSET` uses) and
the parse `futex_waitv` uses for its deadline, and the wake reuses
`wake_bitset`, which is where the `nr` cap and the bitset match already
live.

**Two divergences, named.**

* `FUTEX2_NUMA` and `FUTEX2_MPOL` are `EOPNOTSUPP`. Linux 7.2 serves both;
  a NUMA futex is *two* words (the value and a node), and this kernel has
  no node or policy to name — answering from the first word would be a
  half-truth the caller cannot branch on.
* The private bit is **not part of this kernel's key**. One wait table per
  user address, exactly as the operation-word call's `FUTEX_PRIVATE_FLAG`
  already is, so a shared wake reaches a private waiter here where Linux
  keeps two key spaces. The same divergence `futex(2)` carries, not a new
  one; the pin therefore claims the shared spelling round-trips, which it
  does because both spellings land in the one table.

**Pinned** by `t-futex2` (29 claims): every refusal above, the deadline's
own firing time, the bitset's selectivity (a mask of bit 1 does not reach
a waiter that asked for bit 2, and the wake's `0` says so), two waiters
and `nr = 1`, a wake larger than the set capping at the waiters present,
both round trips, and — on this kernel only — the `EOPNOTSUPP` NUMA word
(the host prints that it serves it instead). The same binary under
`MPOS_ON_HOST=1` passes 29/29 against Linux: the reference direction.

**Counterfactual** — the two dispatch arms removed: `CF-EXIT=1`,
`pass=225 fail=1 (226 built)`, `t-futex2` red on every claim through
`_ => ENOSYS`, nothing else.

Full record: `docs/uapi/` §5.1 item 119.

### 6.35 `rseq(2)` — a restartable sequence, and the tick that must not break it — **LANDED (2026-10-28)**

`rseq(2)` (334 on x86_64, 293 on aarch64) is how a libc says "this thread has a
block at this address; write the CPU it is running on into it, and never
interrupt the sequence I arm through it". glibc registers one before `main`
runs, and the table of deliberate refusals (§12.1 A6) had carried the number as
`ENOSYS` for exactly why a user-space program cannot do it for itself: the
kernel is what knows the CPU and what can avoid the interruption.

**Measured as real root** (Linux 7.2.11; `build/scratch/rseq/` — every probe a
fresh `musl-gcc -static` task, because a glibc binary's own startup registers a
33-byte area and every later call in that thread then answers `EINVAL`; the
`mini`/`probe` binaries that directory already held were stale and were not
used). The auxv says `AT_RSEQ_FEATURE_SIZE = 33`, `AT_RSEQ_ALIGN = 64`; `struct
rseq` is `cpu_id_start` at 0, `cpu_id` at 4, `rseq_cs` at 8, `flags` at 16,
`node_id` at 20, `mm_cid` at 24. What the call answers:

| shape | Linux 7.2.11 |
|---|---|
| `rseq(area, 32, 0, sig)`, 32-aligned | `0`; `rseq_cs = 0`; ids = the running cpu |
| the same again | `EBUSY` |
| a different area, or a different length | `EINVAL` |
| the same area, a different `sig` | `EPERM` |
| a misaligned area; `len` 0 or 31 | `EINVAL` |
| `len` 33/34/40/64/65, 64-aligned | `0` (the v2 area; writes `flags = 0x10`) |
| `len > 32` at a 32-aligned area | `EINVAL` |
| `flags` 2 | `0`; `flags` 1, 3, `0x80000000` → `EINVAL` |
| unregister with the registered triple | `0`; `cpu_id_start = 0`, `cpu_id = -1`, `node_id = mm_cid = 0` |
| unregister with a wrong `sig` | `EPERM` |
| unregister with a wrong area or length | `EINVAL` |
| unregister with `flags = 1 \| 2`, or with nothing registered | `EINVAL` |
| `fork` | the child inherits the registration (it can unregister the parent's triple) |
| `CLONE_VM` thread | starts with none (registers its own area) |
| `exec` | clears it |

The critical-section half was measured with a hand-written assembly blob whose
`.long 0x53053053` signature word sits four bytes before its abort label: a
syscall made *inside* the section commits (Linux clears its event flags on the
syscall-exit path), a task **preempted** inside it is rewound in every one of six
cases (`rseq_cs` cleared, resume address = `abort_ip`), and a signature word four
bytes off kills the task with `SIGSEGV` — but only when the rewinding runs.

**Served, in that shape.** `src/sched/rseq.rs` holds the per-task registration
(area, length, signature, and the tick-deferral bound), `src/syscall/linux/proc.rs`'s
`handle_rseq` is the argument door, and `nr::RSEQ` gained a dispatch arm in
`src/syscall/linux/mod.rs` where the "deliberately has no arm" comment used to
be. Registration writes `rseq_cs = 0`, `flags = 0`, both ids = the running cpu
(0 — this machine has one) and `node_id = mm_cid = 0`; unregistration writes the
measured *different* four words (`cpu_id_start = 0`, `cpu_id = -1`); a `fork`
child inherits the registration in `hand_down_rseq` beside the keyring's hook, a
thread does not, and `exec`/exit clear it.

**The promise, kept a different way.** Linux keeps a restartable sequence
unbroken by rewinding the resume address out of the span. This kernel cannot do
that: the trap frame the clock returns to is the machine's own and its user
register file is not reachable from the interrupt path (the syscall gate's frame
is, the clock's is not — measured by reading both machines' entries). So the
scheduler is told *where the tick landed* (`Kernel::preempt_at` on the service
table, filled in by `src/kernel_services.rs`, called from the x86_64 PIT/APIC
handlers and the aarch64 `irq` stub) and a tick that lands inside an armed span
is **spent, not switched**: the task keeps the core until the sequence commits
or leaves its span. A task still inside its span on the following tick is ended
with `SIGSEGV`, because a span is a handful of instructions and never a
millisecond — a broken sequence must not hold the only core.

**The claims on `t-rseq`**, plus the armed-section claim (guest-only: the
host prints that it rewinds instead), the v2 shape (both arms, with one
runtime-dependent expectation: the host's `flags` word is `0x10`, this kernel's
is 0), and the fork/thread inheritance. Host run: **PASS** — the armed
child exits `7`, i.e. Linux's own fixup ran, which is the reference direction.
Gates: x86_64 `pass=225 fail=0`, aarch64 `pass=224 fail=0`, `mpos-kunit 34 passed`
both, host suite 685.

**The v2 area is served, with its facts answered honestly.** Measured on Linux
7.2.11 first (`build/scratch/rseq2/probe.c`): lengths 33, 34, 40, 63, 64, 65 and
96 are accepted on a 64-byte aligned area, each writing `flags = 0x10`
(Linux's read-only feature bits for the time-slice extension; `0x30` when
`RSEQ_FLAG_SLICE_EXT_DEFAULT_ON` is asked), `node_id = 0`, `slice_ctrl = 0`, and
`mm_cid` = 0 for the first registering thread, then 1, 2, 3 for three more. Here
the extra words are real answers rather than copies: `flags` is 0 because the
slice extension is not implemented and advertising it would promise a behaviour
this scheduler does not have; `node_id` is 0 because the machine has one node;
`mm_cid` is 0 because only one thread of a process runs at a time on one CPU,
and the concurrency-id contract only asks that *concurrently running* threads
have distinct ids; `slice_ctrl` is 0 and never written again.

**The `auxv` pair is advertised**, which is what makes a libc find the v2
shape at all: `AT_RSEQ_FEATURE_SIZE` (27) answers 33 and `AT_RSEQ_ALIGN` (28)
answers 64 (`src/sched/runtime/load.rs`'s `auxv`, both numbers from Linux
7.2.11's own pair; the initial-stack block's word count grew with the two
entries, because a block that is one entry short is an exec writing past its
own stack). glibc reads them and registers 33 bytes here; musl ignores them.
Linux fills the id words on **every return to user** because the cpu id and
the concurrency id can change under a task; on this machine they cannot -
there is one CPU, so `cpu_id` is always 0, and `mm_cid` is 0 for the same
reason - so the registration's write is the last write those words need, and
no return-to-user hook is carried.

**Named divergences**: the v2 `flags` word is 0 rather than Linux's `0x10`/`0x30`
(above); the id words are written at registration rather than refreshed on each
return to user (unobservable here, because neither id can change on one CPU);
the concurrency id is not refreshed after a `fork`; a preempted section is
deferred then killed rather than rewound to `abort_ip`; and an unregistration
over an unmapped area answers `EFAULT` where Linux delivers `SIGSEGV`.

**Counterfactual** — the dispatch arm removed: `CF-EXIT=1`, `pass=224 fail=1
(225 built)`, only `t-rseq` red, every call falling through `_ => ENOSYS`.

Full record: `docs/uapi/` §5.1 item 120.

### 6.36 `futex_requeue(2)` — move sleepers from one word to another — **LANDED (2026-10-28)**

`futex_requeue` is the futex family's last unserved number (456 on both
machines), and its shape is the whole of `FUTEX_WAITV`'s descriptor pair plus
two counts: `futex_requeue(waiters /*two struct futex_waitv*/, flags,
nr_wake, nr_requeue)`. Everything was measured first, on host Linux 7.2.11
(`build/scratch/futex-req/probe.c`, musl static, so no library had registered
a block of its own):

* `flags` must be 0; a NULL descriptor array, either negative count, a second
  descriptor whose flag word differs from the first, `__reserved` set, a
  missing size bit, a `val` wider than 32 bits, or the NUMA/policy bits on one
  descriptor only are all `EINVAL`.
* the source word is checked against `waiters[0].val`: a moved word is
  `EAGAIN`, an unreadable source `EFAULT`. The **destination word is never
  read** — only its key is derived, which is why an unmapped destination with
  waiters to move answers `1` and those sleepers are simply unreachable.
* the return value is `woken + requeued`; `nr_wake = nr_requeue = 0` leaves the
  waiters asleep and answers `0`; `a == b` is accepted and behaves like a wake.
* two sleepers on A(7), requeue(wake 1, requeue 1) → `2`, then `futex_wake(B)`
  → `1`: the moved sleeper answers a wake on the destination even though it
  submitted its wait against A.

**The bug this measurement found, and the fix.** The first version of the pin
hung the x86_64 gate for nine minutes: a task parked in `futex_wait` polls a
condition, and that condition named the address it submitted. A requeue
rewrites the waiter's *place* to the destination, so the sleeper was parked on
a poll that could never come true — `futex_wake(B)` marked the moved place and
nudged the task, the task woke, re-checked a condition about A, and went back
to sleep. The table now keeps both addresses: `Waiter.address` is where the
place sits now (a requeue moves it), `Waiter.asked` is what the caller
submitted, the wait condition is "any place this task holds has been marked
woken" (`is_woken_for`), every wait releases its places task-wide
(`unregister_task`), and `FUTEX_WAITV`'s answer index is recovered by matching
a woken place's `asked` against the caller's array — the array still names the
original words after a requeue.

**Served, in that shape.** `src/sched/futex.rs` gained `requeue_places` (walk
the queue in place order: the first `nr_wake` places marked woken, the next
`nr_requeue` moved to the destination, the rest untouched) and `requeue` (word
check before touching anything, then the moves, then the wakes outside the
critical section); `src/syscall/linux/proc.rs`'s `handle_futex_requeue` is the
argument door (both descriptors validated, both flag words required to match,
the source word required readable); `nr::FUTEX_REQUEUE` is 456 in both tables
and has a dispatch arm after `FUTEX2_WAIT`.

**Named divergences**: the NUMA/policy flag bits and the shared spelling (no
private bit) are `EOPNOTSUPP` here — this kernel has one table keyed by user
address, so a cross-process shared waiter cannot be reached; requeued places
keep their queue order rather than Linux's priority order (this scheduler's
places have no priority sort in the first place, and the measured answer
depends only on which are first); and the source-word check happens before the
places are looked at rather than under the hash-bucket lock — on one core
those two orders produce the same answers for every shape the pin can build.

**Pinned** by `t-futex-requeue`, which runs its whole reference table under
`MPOS_ON_HOST=1` as well: it passes on Linux there (the answers are Linux's),
and only the guest adds the `EOPNOTSUPP` claims for the shapes this kernel
refuses. Host tests
`a_requeue_wakes_the_first_places_and_moves_the_rest_to_its_other_word` and
`a_requeue_with_no_count_on_one_side_leaves_that_half_alone` pin the table
mechanics; the host suite is 687.

Gates: x86_64 all 227 passed (15 not run), aarch64 all 226 passed (16 not run),
`mpos-kunit 34 passed` both, host suite 687.

**Counterfactual** — the dispatch arm removed: `CF-EXIT=1`, `pass=226 fail=1
(227 built)`, only `t-futex-requeue` red through `_ => ENOSYS`.

Full record: `docs/uapi/` §5.1 item 121.

### 6.37 A fatal signal ends the whole thread group — **LANDED (2026-10-29)**

The display work found this one by hanging inside `timeout(1)`: `timeout -s KILL
15 kmscube` sent its `SIGKILL` and then waited, and the wait never finished. The
kernel's own park dump said which half was stuck — the killed process's *leader*
was gone (`released=true retired=false`), while a second thread named
`kmscube:disk$0` sat in `park 2 (block_until_nanos) syscall 202` (a futex wait)
and the probe's shell sat in `park 4 (block_until_uninterruptible) syscall 61`
(a `wait4`). The group could not die because one of its threads was parked where
a pending signal is never looked at: `take_fatal_signal` is what a blocking call
checks, and a thread asleep in a futex is not in a blocking call that runs it.

Linux's answer is `complete_signal`: a signal with **no handler whose default
action terminates** sets `SIGNAL_GROUP_EXIT` on the thread group, wherever in the
group it was taken, and every thread is ended with it. This kernel had that rule
for `exit_group(2)` alone (a caller that leaves ends its siblings) and for the
pid-namespace init's zap, but not for a fatal signal — so `kill(pid, SIGKILL)`
reached exactly the one task the number named and left the rest of the group
running, which is a shape Linux never produces.

**Served.** One door ends a task of a fatal signal now:
`die_of_fatal_signal` (`src/sched/runtime/child_exit.rs`) walks
`end_group_siblings` and then runs the dying task's own `exit`. The helper is the
loop `exit_group` used, extracted rather than copied: a sibling is marked
`exited` and retired, so it is in no queue the picker can reach and its slot is
freed by the release pass, and its children are handed to init. The dying task
then sees `group_dead` and records the process's death — so the parent's `wait`
answers instead of parking. A thread parked in an interruptible wait needs no
special case: being retired is stronger than being woken, and `exit_group`'s own
siblings have been ended this way since it landed.

The door exists because there were two, and the second one was found later
(§6.77b, and §6.77c for the same rule at the arch's own fault handlers). Having
`take_fatal_signal` end the group put the group's fate in the
hands of *where* the signal was taken: a parked task took it inside a blocking
wait and the group died, but a task that killed **itself** took it at that `kill`
call's own syscall-exit gate, whose `Terminate` arm called a bare `exit` — one
thread died, the parked sibling outlived it, `group_dead` was never true and the
parent's `wait4` still parked. `take_fatal_signal` now only takes the number; the
group walk belongs to the death site, so every caller ends through the one door.
Linux reaches the same place from the other end, deciding at `complete_signal`
time with `SIGNAL_GROUP_EXIT`.

One divergence is named rather than papered over: a sibling ended by the group's
fatal signal never runs its own exit path, so its `CLONE_CHILD_CLEARTID` word is
not cleared and its `SEM_UNDO` list is dropped with `Arc` count 1 — exactly the
shape `exit_group`'s siblings already have.

**Pinned** by `t-kill-ends-group` (both machines) with **two** fixtures: the
child is a re-exec of the same binary with one word, so the fixture's parked
worker is a real `FUTEX_WAIT` on a word nobody wakes; the first fixture is killed
from outside (the original shape, signal taken inside a blocking wait) and the
second kills **itself** (`kill(getpid(), SIGKILL)`), which is the shape that
reaches the syscall-exit gate. Each parent waits 700 ms for the park, then polls
`wait4` with `WNOHANG` for 10 s — a hang is not an answer this pin can report.
The reference is Linux: `MPOS_ON_HOST=1` runs 6/6 ok and `PASS` against 7.2.11.
On the machine the whole sweep is `pass=255 fail=0` (x86_64) and `pass=254
fail=0` (aarch64) after the change, and the counterfactual — the door's call
sites reverted to bare `exit`s on the shipped `take_fatal_signal`-ends-the-group
code — reddens exactly the two **self-kill** claims (`a process that kills itself
is reaped though its thread is parked in a futex`, `a self-kill is still the
signal that is reported as the cause`) while both outside-kill claims stay green:
the asymmetry the single-fixture pin could not see (§6.77b).

Full record: `docs/uapi/` §5.1 item 123.

### 6.68 One range, two descriptions: `/proc/<pid>/maps` printed a span twice — **FIXED (2026-11-02)**

`t-maps` was extended to a private mapping of a real file and a pre-existing claim went red: the
maps file had **two lines for one address span**, an unnamed inode-`0` `rw-s` line above the named
one, and `line_at` (first match) read the one with no object behind it. Observed on a guest in 2 of 5
runs with the extra mappings present; not reproducible on demand, and the temporary detector built
for it (print both sources when two lines share `(start,end)`) reproduced nothing in two further
gates — an intermittent report is not a diagnosis.

What turned it into one was a **host test of the table's shape** instead of another gate run. A
seeded sequence of `insert` / `insert_present` / `remove_range` over one `VmaTable`, checking after
each step that each entry's key is its own `start`, that entries stand in address order, and that no
two of them overlap, panicked at step 9: `insert_present` of an anonymous run whose range overlapped
an existing description left **both** in the table. `insert_present` is `place`-without-`split_out`
on purpose — `split_out` is the path that rewrites neighbours and frees their pages (`NOTES.md`
§15.18) — but it was written as if the caller could only ever hand it a hole or abutting run, while
its callers are `mremap`'s `insert_vma`, `brk`'s growth and `claim_anon_gaps`; only the last one
takes its ranges from `gaps()`. A run placed over an existing description therefore described one
span twice, and the maps file printed what the table held.

The fix is the one line the invariant needs: `insert_present` clips the range out of the table
first (`split_out`, whose pieces are dropped, not freed — the caller has already placed the pages
and dealt with the frames of anything it maps over, exactly as `MAP_FIXED` does through `munmap`
before it gets here), then places the run. And the pin is the host test itself, because it is
deterministic where the guest was not: `every_description_is_keyed_by_its_own_start_and_stands_alone`
fails on the old code at a named step and passes after the fix, `a_lift_and_retake_round_trip_keeps_the_cover`
pins the `mremap` shape as well.

**That was a real defect, but it was not the reported one.** With the clipping in and the claims
back in `t-maps` — a private file mapping beside the anonymous shared one, plus "no range is listed
twice" — the guest still printed the shared span twice. The kernel-side detector put with those
claims (print every source when two rendered lines share a range) printed **nothing**, which moved
the question from the table to the *file*: the text `/proc/<pid>/maps` handed back was produced by
two renderings spliced together. `ProcFile::read_at` re-ran its generator on **every** call and
applied the caller's offset to whatever that generation answered, so a reader whose buffer grows -
std's `read_to_end` starts at 32 bytes and doubles, and growing the buffer maps pages - got its
second chunk's offset applied to a longer file than its first chunk came from. The bytes past the
first chunk's end are the new snapshot's tail; a line appears twice, and only sometimes, because it
depends on whether the process grew between two chunk reads. That is the whole intermittency.

Linux materialises a seq_file once per open for this reason. The fix keeps the rendered text on the
`ProcFile` instance: a read that starts at offset zero re-renders (a fresh open sees the process as
it is), and every chunk after it continues that same snapshot. Measured after: the same pin on both
arms is green, with `a_chunked_read_of_a_generated_file_keeps_one_snapshot` as the deterministic pin
(a generator that grows between calls; the second chunk must end where the first did, and a read from
zero must see the new text).

The lesson, twice over: an intermittent report is diagnosed by pinning the invariant the reader
assumes, and a file whose content changes under the reader must be a snapshot, not a live view.

Full record: `docs/uapi/` §5.1 item 182.

### 6.69 A regular file's descriptor kept no path, so its mapping had no name — **FIXED (2026-11-02)**

The private-file claims of §6.68 left one column unchecked. `t-maps` grew "and its line names the
file, as Linux's does" and it went red on a guest while the same claim passed on Linux: the mapping
of `/t-maps-dup.bin` was listed with the right inode and offset and a **blank name column**.

The label a descriptor records is the path it was opened by, and `open(2)` set it only for a
directory (so `fchdir` then `getcwd` answers) and a character device (`ttyname` reads
`/proc/self/fd/<n>`). An ordinary file kept none, and every door that needs the path fell back to
`File::display_name`, which searches the filesystem tree for the inode - the right answer only while
the name is still there, and never the answer Linux gives, since Linux prints the path the descriptor
was opened by (with ` (deleted)` appended once it is unlinked). `mmap` takes the name column from
`File::label()` for exactly this reason, so a private file mapping read as unnamed.

The fix keeps the label for `NodeKind::File` as well, which also makes `/proc/self/fd/<n>` on a
regular file answer the opened path instead of a tree search - Linux's shape for that door. Measured:
the claim above passes on both arms, and nothing else in the pin (which reads `fd` links, `t-fd`'s
own claims and `t-maps`' remaining 67) moved.

Full record: `docs/uapi/` §5.1 item 183.

### 6.70 An unlinked descriptor's name lacks " (deleted)" — **a named divergence (2026-11-02)**

Linux appends ` (deleted)` to a name once the path it was opened by is gone, and it does so on both
doors (`build/scratch/del/del.c`, Linux 7.2.11):

```
/proc/self/fd/3 -> /tmp/delprobe.5WhpiH
7f8441add000-7f8441adf000 rw-s 00000000 00:34 65483  /tmp/delprobe.5WhpiH
/proc/self/fd/3 -> /tmp/delprobe.5WhpiH (deleted)
7f8441add000-7f8441adf000 rw-s 00000000 00:34 65483  /tmp/delprobe.5WhpiH (deleted)
```

Neither door does here, and the reason is a missing answer rather than a missing print: the suffix
needs "this inode has no names left", and `Inode::nlink()` answers **0 for "not tracked"** - every
node whose filesystem keeps no count reports 0 and `stat` turns that into the kind's convention - so
`nlink() == 0` cannot tell an unlinked file from an inode that never counted. Ramfs does keep a real
count (`RamFile`/`RamDir`'s `links` cell, `set_nlink` on link/unlink), so a per-inode "the name is
gone" answer is available for the memory filesystems and absent for the ones that do not count.

Named, not stubbed: the doors print the path the descriptor was opened by (item 183) and stop there.
The prerequisite for the suffix is a trait-level answer that distinguishes the two zeros - a
`name_is_gone()` alongside `nlink()`, implemented by ramfs and by ext4's real count - and once it
exists both doors print Linux's spelling. Nothing returns a made-up name in the meantime.

Full record: `docs/uapi/` §5.1 item 184.
### 6.71 A mapping that outlives its name says so — **FIXED, pinned host-side (2026-11-02)**

§6.70 named the divergence: Linux appends ` (deleted)` to a descriptor's path and to a mapping's
name column once every name that reached the file is gone, and this kernel could not, because
`nlink()`'s `0` means "no names" *or* "this filesystem keeps no count". §6.70's prerequisite was the
missing answer, and this section adds it.

**Measured first** (`build/scratch/del/del.c`, Linux 7.2.11): opening `/tmp/delprobe.XXXXXX`, mapping
it shared, then unlinking it gives

```
/proc/self/fd/3 -> /tmp/delprobe.5WhpiH
… rw-s 00000000 00:34 65483   /tmp/delprobe.5WhpiH
        (after unlink)
/proc/self/fd/3 -> /tmp/delprobe.5WhpiH (deleted)
… rw-s 00000000 00:34 65483   /tmp/delprobe.5WhpiH (deleted)
```

Both doors, the same suffix, appended *after* the path.

**The answer, as a door of its own**: `Inode::name_is_gone()` — default `false`, because a
filesystem that keeps no count does not know and "does not know" must never become "deleted". It is
implemented where the count is real: ramfs (`links == 0`) and ext4 (`i_links_count == 0`). The
memory-mapping side asks the same question through a new
`mm::vma::MappedObject::name_is_gone()` (default `false`; `MappedInode` and the file-fault object
`FilePages` both forward their inode's answer), so `/proc/<pid>/maps` can append the suffix without
the address space ever naming the VFS.

**Wired**: `File::display_name` (the descriptor's own path, whichever door supplied it) and
`procfs::mapping_lines` (a `Frames` mapping's name column) both append ` (deleted)` when the answer is
true.

**Why the pin is a host test, and what the guest refused to pin.** The first attempt pinned the two
claims in `t-maps` on the guest and produced a *partly* green, partly red pair that differed by arm
(`x86_64`: the mapping's suffix green, the descriptor's red; `aarch64`: both red). The cause is the
second divergence that measurement uncovered: this ext4 frees an inode's table entry at `unlink`
(`ext4::unlink` calls `free_inode` when the count reaches zero, with no orphan list), so "is the name
gone" is answered by re-reading an inode slot that may already name something else — the answer races
with reuse, and a claim that depends on that race is a flaky claim, not a contract. Linux keeps the
inode alive until the last reference closes (the orphan list), which is what makes its answer stable.

So the landed pin is the host test `vfs::file::tests::a_descriptor_outliving_its_name_says_so`:
a ramfs file with a label reads as its path, `unlink` makes `name_is_gone()` true, and the same
descriptor reads as `<path> (deleted)`. Counterfactual: dropping the suffix in `display_name` makes
exactly that test fail (measured). The guest-side behaviour is a named remainder, with its own
prerequisite — an ext4 orphan list — recorded as the next item.

Two further facts from the guest probe (`build/scratch/orphan/aa-orphan.start`, on an image built
before this change): reading an unlinked-but-open ext4 file **still answers its bytes** ("hello
world" after `unlink`), so the missing orphan list does not lose data today; and that kernel printed
the descriptor as `anon:[12939]`, which is the pre-§6.69 behaviour the label work replaced.

#### 6.71a The orphan list is not what is missing — the next open of the same name is

The plan this section ended with was an ext4 orphan list (Linux's answer: keep an unlinked-but-open
inode's table entry and its blocks until the last reference goes). Two measurements say that plan
addresses the wrong thing, and both were made before anything was written.

**1. The bytes survive today, even after the space is reused.** `build/scratch/orph2/orph.c` writes
256 KiB with a known pattern, `unlink`s the file while keeping the descriptor open, then creates 64
fillers of the same size to force block reuse, and reads the descriptor back. On this kernel (built
from `5a8fb24`, run in the guest as `/t-orph` through a `musl-gcc -static` binary) and on the host's
ext4: `fillers=64 read=262144 first_mismatch=-1` — **INTACT**. The bytes are not lost, because
`ext4::unlink` frees an inode's *blocks* and its table entry, but the open description never asks for
them again: it reads through the same `Inode` handle it was opened with, and that handle's block
pointers are still the numbers the file was written at.

**2. What is actually wrong is the next caller's answer about a *different, new* file.** Because the
inode number goes straight back into the inode bitmap, the next `create` of the same name gets the
same number. The open description's `Inode` handle is a `(fs, ino)` pair, so it answers with whatever
the slot holds now. Two doors show it:

- `/proc/self/fd/<n>`: after `unlink`, the link reads `/t-a.bin` — not `/t-a.bin (deleted)`, because
  `name_is_gone()` asks the slot, and the slot no longer says zero. A pin asserted that and was red
  on both arms in the same run where the mapping's suffix claim was green on x86_64 and red on
  aarch64: the answer depends on whether a filler happened to land on the number first, which is a
  race, not a contract.
- The mapping's name column has the same exposure for the same reason.

So the honest order is: **an `Inode` handle cannot be `(fs, ino)` alone if the number can be
recycled while a handle is alive.** Linux's identity for that is the inode's incarnation — the table
entry stays allocated (orphan list) or the handle carries what it read — and here `Ext4Ino` carries
nothing, so every question it answers after a recycle is a question about somebody else's file.

That is a change to `Ext4Ino`'s identity, not to `unlink`: it needs the handle to keep the
`InodeData` it was created with (or a generation/incarnation number checked on every read, which is
the same idea with more device traffic) and every method to answer from that, refusing once the
filesystem says the number belongs to a newer incarnation. `read_inode` already goes through one
door, so the check has one place to live.

Named remainder, and the next item: **`Ext4Ino` has no incarnation — a recycled inode number makes a
live handle answer about the new file.** The orphan list makes the rename smaller (it keeps the
number from being recycled at all while the description is open, which is Linux's cheaper answer),
but the identity hole is the defect; the orphan list is one way to hide it for the unlinked case and
does not help a handle that outlives `rename` over its name either.

### 6.72 A handle names an incarnation, not a number — **FIXED (2026-11-02)**

§6.71a left the defect named and this section closes it: `Ext4Ino` was `(fs, ino)`, so once `unlink`
put the number back into the bitmap, the next `create` of the same name inherited every handle still
open on the old file. `/proc/self/fd/<n>` read `/t-a.bin` instead of `/t-a.bin (deleted)`, and
whether it did depended on allocation order - which is why the same claim came back green on one
arm and red on the other in one gate.

**Where the answer belongs.** Not at `unlink`. A descriptor that outlives its name still reads its
bytes (measured, §6.71a: 256 KiB intact through 64 fillers that force block reuse), and it can only
do that while the handle is still the current incarnation of its number. So the two events are
separated:

- **`unlink` writes the zero-link header** (`write_inode(child, &data)` before `free_inode`), which is
  what makes `name_is_gone()` true for every door, without taking the block pointers away;
- **`alloc_inode` bumps the incarnation** when the number comes back out of the bitmap, which is the
  moment an old handle must stop answering - and the moment a live descriptor's bytes stop being
  reachable, which is Linux's own rule too (its orphan list keeps the number from being recycled
  until the last reference goes; without one, the reuse is what ends access).

**The generation is the mount's memory.** `Core` gained `generations: Mutex<RefCell<BTreeMap<u32,
u32>>>`; `handle(ino)` captures it, `Ext4Ino::data()` refuses with `ESTALE` when the number has moved
on, and every `self.ino` read goes through that door (26 sites; the four that cannot carry an error -
`metadata`, `mode`, `owner`, `atime`/`mtime`/`ctime`, `nlink` - answer `InodeData::EMPTY`, zeroes,
rather than another file's numbers). `name_is_gone()` now answers **generation moved on, or the slot
says zero links**: the first means "this handle's file lost its last name whatever the slot holds",
which is the fact the door was always trying to ask. The inode cache is keyed by `(ino,
generation)`, so a new incarnation never resolves to a stale handle.

The on-disk `i_generation` (offset 100) is deliberately **not** used: it is ext4's checksum/rename
cookie, written back unchanged by this driver, and it says nothing about how many times a number has
been reused in this mount.

**Pins** (host tests, in `vfs::ext4::tests`, where the fixture's `MemDev` can be driven):

- `an_unlinked_file_keeps_its_bytes_while_the_number_is_its_own` - read after `unlink` still answers
  the bytes, so `unlink` did not take the mapping away;
- `a_recycled_inode_number_refuses_the_old_handle` - the same number is handed to a new `create`,
  the old handle's `read_at` answers `ESTALE`, its infallible getters answer zero, and the new handle
  is unaffected.

Both would be green-on-red without the change: with `free_inode`'s immediate release the second test
reads the new file's bytes through the old handle (that is the defect, reproduced).

**Guest pins restored**: `t-maps` regained "a mapping of an unlinked file is listed", "and its name
carries the suffix Linux prints" and "and the descriptor's own path says so too"; the host reference
passes on Linux 7.2.11, and both arms of the gate are green.
passes on Linux 7.2.11, and both arms of the gate are green.

## §6.73 A userland fault's report names the program, as Linux's does — MEASUREMENT + FIX

The machine already printed a fault. It printed the wrong identity: `user #PF at 0x10
code=PageFaultErrorCode(USER_MODE) rip=0x7e276355cc53`, `PFDIAG page=0x0 NOT MAPPED task=10`, and
`user #GP/#SS/#NP at rip 0x7e27ca342986 cs 0x23`. `task=10` is a **slot in the task table**, not
something a person or a log reader can act on: the number is reused the moment the task dies, and a
boot's slot 10 was `/sbin/udevd` in one run and `/usr/bin/weston` in the next.

### What Linux files (7.2.11, `build/scratch/segv/`, `dmesg -C` before each run)

Six fault shapes, one line each, quoted from the boot's own dmesg:

```
m[37799]: segfault at 7fad640ec000 ip 000055d747093210 sp 00007fff76b1ae50 error 4 in m[1210,55d747093000+1000] likely on CPU 8 (core 0, socket 0)
m[37799]: segfault at 7fad640ec000 ip 000055d747093210 sp 00007fff76b1ae50 error 6 in m[1210,55d747093000+1000] likely on CPU 8 (core 0, socket 0)
m[37801]: segfault at 123456789000 ip 000055d747093210 sp 00007ffc48f6b9d8 error 4
m[37803]: segfault at 123456789000 ip 00000000123456789000 sp ... error 14
m[37804]: segfault at 55d747093000 ip 000055d747093000 sp ... error 15 in m[1210,55d747093000+1000]
traps: m[37806] general protection fault ip:55d747093000 sp:7ffd1e2c9a40 error:0
```

Read off that table: the first word is `comm[tid]`; `at` is the **faulting address**; `ip`/`sp` are
the frame; `error` is the architecture's page-fault code (bit 0 present, bit 1 write, bit 2 user,
bit 4 instruction fetch — `4` = a user read of an absent page, `6` = a user write, `14` = a user
instruction fetch, `15` = a fetch into a present but non-executable page); and the `in
<module>[<offset>,<base>+<len>]` clause names the module holding the **ip**, present only when the
ip has a named mapping (absent for an anonymous page and for the absent-address cases). A trap
(`hlt` in ring 3, an undefined instruction) has no faulting address at all and takes the second
shape.

### What this kernel printed, and what it prints now

| field | before | now |
|---|---|---|
| identity | `task=10` (a slot number) | `tfaultchild[10]` (`comm` + thread id) |
| address | `at 0x10` | `at 0x10` (unchanged — the address was already right) |
| frame | `rip=…` | `ip … sp …` (both words) |
| fault word | `code=PageFaultErrorCode(USER_MODE)` | `error 4` |
| the module | `mapping_name_at` in the rate-limited `PFDIAG` dump | `in <path>[<offset>,<base>+<len>]` on the same line |
| a trap | `user #GP/#SS/#NP at rip …` | `traps: <comm>[<tid>] general protection fault ip:… sp:… error:0` |

The `in …` clause comes from `sched::mapping_span_at(ip)` (`src/sched/runtime/child_mm.rs`), which
answers from the eager ranges the interpreter recorded (the image and every library it mapped) and
then from the VMA's own name — and answers `None` for an anonymous page, which is why the clause is
absent exactly where Linux's is.

### The defect found on the way: a ring line with no newline

`src/kernel_services.rs` filed each of these lines with `self.log_line(format_args!("{line}"))` and
no `\n`. The kernel log is a **byte stream** — `log_bytes` appends and `dmesg_snapshot` returns the
whole buffer — so the next writer's text was glued onto the fault line, and `syslog(2)` then answered
two faults' worth of words as one entry. The pin caught it as `FAIL: … and it carries no invented
faulting address`, because the glued line contained a stray ` at ` from the *previous* entry. Same
class as the `src/syscall/linux/fs/file.rs` `mpos: poll spin` newline defect. Both sites now carry
`"\n"`.

### The pin (`user/tests/src/bin/t-fault-report.rs`)

Raw `syslog(2)` (this kernel's `dmesg` ring, read with `kind = READ_ALL`), a forked child that sets
`PR_SET_NAME` to `tfaultchild` so the searched name is one the test invented, then:
the child dies of SIGSEGV; the ring names `tfaultchild[<pid>]`; the line has Linux's shape
(`segfault at … ip … sp … error `); a read carries `error 4` and a store `error 6` on x86_64.

Two claims were made per-machine after the second arm reddened them:

1. `error 4`/`error 6` are **x86_64's** words. On aarch64 this kernel passes the fault status code
   (0x06/0x46 → `0`/`1`), so the aarch64 claims require the column to be present and a store's word
   to differ from a read's, not a specific number. The aarch64 vector entry now reports
   `report_user_fault(addr, frame.pc, frame.sp, ((esr >> 6) & 0x3f) as usize)`; that column is that
   machine's vocabulary, and pinning it to the other machine's number is how a report starts lying in
   a shape that reads correct.
2. The `traps:` shape is x86_64's alone — a ring-3 `hlt` is a `#GP` there, while AArch64 has no
   general-protection class at all (an EL0 `brk` is SIGTRAP, an undefined instruction SIGILL). The
   block is `#[cfg(target_arch = "x86_64")]` and the other arm prints a note saying so.

One claim was **withdrawn**: "a handler that swallows a fault files nothing". Measured on Linux, a
returning SIGSEGV handler re-faults forever (the probe hung for 60 s), so Linux files nothing because
the process never dies — not because a caught fault is unfiled. Claiming it would be claiming
something Linux does not do.

### Counterfactual

`fault_identity()` replaced by `(format!("task{}", self.current()), 0)` — the old slot-shaped
identity — reddens exactly four claims: `the kernel log names the program that faulted, and its
thread`, `and files the fault in Linux's shape: at, ip, sp, error`, `and the error column says user,
read, not present (4)`, `and the error column says user, write, not present (6)`, with
`=== FAIL t-fault-report`. Restored byte-exact.

### Gates

x86_64 `verdict pass=253 fail=0`; aarch64 `verdict pass=252 fail=0`, `mpos-kunit 35 passed`.

## §6.74 An `exec` from a worker thread ends the group - MEASUREMENT + FIX

`execve(2)` from a worker ends every other thread of the process on Linux, and the group keeps the
number its **leader** was born with. This kernel ended nothing: it emptied the old image's frame
list out from under the threads still running on it, which is a page handed to a new owner while
somebody is still fetching instructions from it.

### The measured crash (`build/scratch/execthr/s.img`, kernel `89e1fa7`)

Probe `s.c` (`musl-gcc -O1 -static -pthread`): the leader forks once, the child starts a sibling that
fills a private page and reports over the inherited fd 1, and a worker that `execve`s `/bin/sh`. The
child's output:

```
P0 start
P1 sibling ready, execing now
SSSSSSSSSSSSEXECED
Tgid:	17
Pid:	17
Threads:	1
DONE-SLEEP
rc=7
```

and the machine's own serial, now that §6.73 names the program:

```
t-s[17]: segfault at 40715c ip 000000000040715c sp 00007e3a891b3e00 error 14
```

`error 14` is an instruction fetch. The address is inside the **old** image's `.text`, so the sibling
was not "still running" in any useful sense - it was executing a page that had gone back to the
allocator and been handed to somebody else. The same shape is what udevd, `openrc-run.sh` and
`ebegin` died of: two different programs reading a pointer that should have been written and finding
zero (`udevd[10]: segfault at 30 … error 4 in /sbin/udevd`, `at 10 … error 4 in
/lib/ld-musl-x86_64.so.1`), which is what a recycled-and-zeroed page looks like from the inside.

### Linux's identity rule (7.2.11 / 7.2.14, `build/scratch/execthr/`)

- `id.c`: main tid 72987, a **worker** execs → `sh` reports `own pid=72987`, `Tgid 72987`, `Pid
  72987`. The numbers are the leader's.
- `tid.c`, three runs: main tid 73156 / worker 73157 execs → `Tgid: 73156`, `Pid: 73156`, `Threads:
  1`. Never the worker's tid, and never two.
- `y.c`/`z.c`: a sibling parked in a futex when the exec lands never runs again (`MAIN-SURVIVED`
  never printed), and `h.c` shows a sibling's append counter stops at the exec (19 → 19).

### The fix

1. `src/sched/runtime/child_exit.rs`'s `exec_replace` now runs `end_group_siblings(t, id, tgid)`
   **before** touching the address space, then hands the old space and the old frames back only
   `if old_mm.leave()` - the last thread out. A sibling ended here never runs its own exit path,
   which is why the "only the last one frees" rule is enforced at the counter rather than at each
   sibling's exit.
2. `src/sched/runtime/task.rs`'s `reap` census now counts a thread's slot under its `tgid` as well
   as under `mm.leader()`: a thread's slot *is* its group's number, so the leader's slot cannot be
   freed while a member still lives (Linux's `delay_group_leader`). Without it an exec'd process
   named a slot no lookup could open.
3. `src/sched/runtime/procfs.rs`'s `snapshot_of` counts `Threads:` from the **table** (live members
   of the group, floor 1) instead of reading `mm.live`. Counterfactual evidence that this is not
   cosmetic: with the sibling ending fixed but the counter still read, `/proc/self/status` said
   `Threads: 2` for the single-threaded process the exec left, because the ended leader's task
   object still held the old `Mm`. Linux counts what is alive.

### The pin: `user/tests/src/bin/t-exec-threads.rs`

A phase-2 re-exec of `/proc/self/exe` (the environment is inherited and `MPOS_ET_PHASE=2` is added,
because a first draft that handed over only that one word also dropped `MPOS_TEST_DIR` and the host
reference run could not find its own output file). Claims, all green on Linux 7.2.11 first:

- a worker thread's `exec` ran the new image;
- and the group has one thread afterwards, as Linux's `de_thread` leaves it;
- and the group kept the number its leader had, not the execing thread's;
- the sibling was running before the exec (its beat file was growing);
- and the exec ended it, as Linux's `de_thread` does (the file stops growing);
- and it was not left running on a recycled page (no `CORRUPT` in that file);
- and no fault was filed against the sibling the exec ended (from `syslog(2)`, when readable).

### Counterfactual

With `end_group_siblings` removed from the exec path and the frames handed back unconditionally
(`if !true || old_mm.leave()`), `cargo xtask test --arch x86_64` answers `FAIL: and the group has one
thread afterwards` - and **only** that claim. The sibling claims stayed green, which is worth
recording rather than hiding: this kernel's allocator handed that page back without anyone touching
it in the observation window, so "the sibling died quietly" is a claim the pin makes only about the
runs where recycling actually happens. The claim that fails deterministically is the thread count,
which is why the counterfactual is quoted against that one.

### Gates

- x86_64: `verdict pass=254 fail=0` (`build/scratch/gate-execthr-x3.log`), kunit 35.
- aarch64: `verdict pass=253 fail=0` (`build/scratch/gate-execthr-a1.log`), kunit 35.
- `cargo fmt --all -- --check` clean, `cargo xtask depgraph` within the ratchet, `cargo test -p
  mpos-xtask` 146, host lib 726.

### Also fixed in passing

`src/syscall/linux/proc.rs`'s `clone` doc comment still claimed `CLONE_THREAD` was refused with
`ENOSYS` and that these address spaces are "owned by one task". Threads have been served for
several rounds (`spawn_thread` shares the parent's `Mm`); the comment was wrong about its own code.

## §6.75 A `fork` hands a private file mapping over private - MEASUREMENT + FIX

`MAP_PRIVATE` means the child's stores are the child's. This kernel broke that rule for exactly one
kind of mapping: a private mapping of a **file**, the kind item 177 made frame-backed. A child
inherited it writable and unhardened, so parent and child wrote the same physical frames - which is
what a heap pointer that reads back as zero looks like from the outside.

### The defect

`src/sched/runtime/child_exit.rs`'s fork loop collects the ranges that cross the fork as **shared
objects** (the pages a second process watches together, mapped writable with no COW bit):

```rust
let shared: Vec<(usize, usize)> = parent_vmas
    .iter()
    .filter(|vma| matches!(vma.kind, VmaKind::Frames { .. }))
    .map(|vma| (vma.start, vma.end))
    .collect();
```

It matched the **variant** and never looked at the field inside it. `VmaKind::Frames` carries
`shared: bool`, and since item 177 `Frames { shared: false }` is an ordinary `MAP_PRIVATE` mapping
whose pages are a *copy*, not an object two processes share. Every such range therefore entered the
shared list: the child mapped its parent's file pages through `map_shared`, and the parent's pages
were never hardened with `set_cow_bit_quietly`. Both processes then wrote the same frames.

The same commit that introduced it is the one the user's `git bisect` named (`4930158`), and the
symptom it produced was `udevd`'s threads dying inside musl's `mallocng` with `#PF at 0x10` /
`#PF at 0x30` - a metadata word reading back as zero, because a forked child had overwritten the
parent's heap.

### The measurement (`build/scratch/frk/pf.c`, `musl-gcc -static`)

Creates a four-page file, maps it `PROT_READ|PROT_WRITE, MAP_PRIVATE`, fills it with `'A'`, forks,
the child overwrites every page with `'C'` and exits, the parent re-reads.

| | `[0]` | `[8192]` | verdict |
|---|---|---|---|
| Linux 7.2.11 (host) | `A` | `A` | `child's writes visible to the parent: no (correct)` |
| this kernel, `4930158`..`fa7e5cd` (`build/scratch/frk/f.img`) | `C` | `C` | `child's writes visible to the parent: YES (BROKEN)` |

Same binary, opposite answer. The guest boot's `/root/frk.txt` carries `rc=1`.

### The fix

```rust
.filter(|vma| matches!(vma.kind, VmaKind::Frames { shared: true, .. }))
```

One word: whether the mapping is an object two processes watch together. A private file mapping is
not, so it now takes the ordinary COW path every anonymous page takes - the child gets its own copy
on its first store, which is what Linux does.

### The pin (`user/tests/src/bin/t-mmap-file-lazy.rs`, 21 claims)

Three claims added at the end of the file, on a writable private mapping of the same file: it
succeeds, the child overwrites every page and exits, and
`a child's writes to a private file mapping do not reach the parent` checks the first byte and the
middle page are still `'A'`. Host reference run (`MPOS_ON_HOST=1 MPOS_TEST_DIR=…`) is green on
7.2.11; the claim's whole point is that it answers differently on a kernel that gets it wrong.

**Counterfactual**: with the filter back to `matches!(vma.kind, VmaKind::Frames { .. })`, that claim
is red and everything else stays green (recorded with the run below).

### A claim corrected while writing it

The same file's older "past the end of the file" claim asserted a page **wholly** past EOF reads
zero. Measured on Linux 7.2.11 (`build/scratch/frk/tail.c`): a read of a page wholly beyond EOF kills
the reader with `SIGBUS` (`Bus error`), while a byte inside the file's last partial page past its
data reads zero. The claim now grows the file into the page and asserts the second shape, because
that is what Linux answers - the `SIGBUS` shape belongs to a fault the machine has to raise, which is
its own item.

## §6.76 A handled signal ends a parked wait with `EINTR` - MEASUREMENT + PIN

weston was recorded parked after `caught signal 15` (`park 1 block_until_on`, §7.4g), and the first
question about any such stall is whether the signal path can end a park at all. It can, and the
shape is a claim now.

### Linux 7.2.11 (`build/scratch/sigpark/{e,p}.c`, `musl-gcc -static`)

A child parks, the parent sends `SIGTERM` 300 ms later, and the child prints its call's answer:

```
CHILD epoll_wait -> -1 errno=4 (Interrupted system call) handler_ran=1     (3 runs)
CHILD read -> -1 errno=4 (Interrupted system call) handler_ran=1           (2 runs)
```

So an interruptible wait answers `-1`/`EINTR` and the handler has run by the time it returns - for
the `epoll_wait(2)` of an empty set with `-1` and for a `read(2)` of a pipe nobody writes.

### This machine (`build/scratch/wsig/`, one boot)

The same `epoll_wait` probe on the guest prints the identical line, `rc=0`. The same boot also ran
weston and `kill -TERM`ed it 20 s in: weston was **gone** (`alive after TERM: no`, `state=Z`), so
what §7.4g recorded as a shutdown stall did not reproduce in that shape - the log's last line was
still the connector census, i.e. the kill landed while weston was inside its own startup. A lost
interrupt in the park is therefore not what that reading was; the shutdown question stays open as its
own item, with this pin removing the cheapest explanation.

### The pin (`user/tests/src/bin/t-signal-park.rs`, 7 claims)

`epoll_wait` and `read` each in a forked child that parks, with the parent sending `SIGTERM` 300 ms
in; the child reports `(rc, errno, handler_ran)` back through a pipe. A call that never returns is the
failure mode, so the parent waits four seconds, `SIGKILL`s the child and reports `FAIL` under the
claim's name - the bounded-child shape `t-sock-timeout` established. The fourth claim is the other
half of the rule: a `SIG_IGN` disposition is not an interruption, so the child must stay parked and
the parent's deadline must be the one that ends it.

Host reference run (`MPOS_ON_HOST=1`): all seven green on 7.2.11.

### Counterfactual

Swallowing the interrupt inside the epoll door (`WaitEnd::Interrupted => {}` in `file.rs`'s
`epoll_wait`, so the call answers its ready set - none - instead of `EINTR`) reddens exactly the two
claims that describe it, and nothing else:

```
FAIL:  a parked epoll_wait is ended by a handled signal
FAIL:  and epoll_wait's call answers EINTR
ok:    and the handler had run by then
ok:    a parked read of a pipe is ended by a handled signal
ok:    and read's call answers EINTR
ok:    and the handler had run by then
ok:    an ignored signal does not end the park
FAILED
```

That run was taken on a counterfactual kernel booted with the pin staged into the image
(`build/scratch/sigpark/cf2.img`, `/root/sigpark.txt`): the pipe door keeps its `EINTR` because the
counterfactual touched only the epoll arm, which is exactly the discrimination wanted. A coarser counterfactual - removing
the interruption check from `block_until_nanos` entirely - makes *lane 0 of the sweep hang* (its
serial's last line is `=== RUN t-signal-park`, the lane dies of its 900 s bound): the interruption is
load-bearing well beyond this pin, which is worth knowing but is not a named-red run.

### Gates

x86_64 `verdict pass=255 fail=0`, `mpos-kunit 35 passed`; aarch64 `verdict pass=254 fail=0`,
`mpos-kunit 35 passed` (a first aarch64 run died on the documented `EXCEPTION: synchronous` boot
check, `EC 0x25`, and was re-run green). Host reference run of the pin on 7.2.11: seven claims green.

### §6.75a the boot after the fix: no fault lines at all

The baseline this item was opened on was five `udevd[..]` fault lines per boot (three `#PF` at NULL
plus the two `#GP`s at musl's `a_crash()`). A boot of the kernel carrying §6.74 and §6.75, with the
network and `seatd` started and the uevent settle completed (`Starting udev ...` then
`Waiting for uevents to be processed ...` then `Starting seatd ...`), printed **zero** lines matching
`traps:`, `segfault at`, or `general protection` in 3546 serial lines covering 9445 retired block
transfers. The boot was cut at its 780 s bound before the desktop came up, so this measures the phase
the deaths happened in (udev's device processing), not the whole session - the session-length reading
is still the one a full boot gives.

### §6.77 a fatal signal ends only the thread that took it, not the group

The compositor's stalls (§7.4h) left a worker thread parked on a futex after the main thread was gone.
A minimal probe (`build/scratch/fx/p.c`, host build with glibc, guest with `musl-gcc -static -pthread`)
makes the same shape without any display code: fork a child; the child starts a thread that parks on
`futex(FUTEX_WAIT)` on a word nobody will ever change, waits 100 ms, then either `exit_group(0)` or
`sends SIGKILL to its own group`; the parent waits up to 4 s and reports.

Linux 7.2.11 (the same binary):

```
mode 0 (exit_group): reaped, status=0
mode 1 (SIGKILL): reaped, status=0
```

This kernel (image `build/scratch/fx/f.img`, kernel `c39bf49`):

```
mode 0 (exit_group): reaped, status=0
```

and then the program **never finishes**: the parent is `slot 17 pid 17 name=/t-fx park 2
(block_until_nanos) syscall 61` (61 = `wait4`, `parks=109` and climbing), the killed child is `gone
slot 18 pid 18 name=/t-fx parent=17 park 2 syscall 62 released=true retired=false`, and the child's
parked worker is still there: `slot 19 pid 19 name=/t-fx park 2 (block_until_nanos) syscall 202
(futex) parks=2`.

So `exit_group(2)` does end a futex-parked sibling (`end_group_siblings`, item 190's machinery), while
a **fatal signal does not**: the killed thread ends, its sibling stays parked for ever, the group never
retires (`retired=false`), and every `wait4` on it hangs. That is exactly the shape the compositor
reaches - and it also means a `SIGKILL` cannot clean up a process whose thread is parked on a futex.

### §6.77a the signal reaches the leader, and the leader never takes it

An instrument on the delivery door (`send_signal_info`, printing `sig`, target tid, target's recorded
park site, and whether the wake was held) and on `take_fatal_signal` (printing every fatal it takes),
run in a boot of the same probe:

```
mpos-dbg: signal 15 -> tid=9 tgid=9 comm= park=2 held=0
mpos-dbg: signal 15 -> tid=8 tgid=8 comm= park=2 held=0
mpos-dbg: signal 15 -> tid=7 tgid=7 comm= park=2 held=0
mpos-dbg: signal 12 -> tid=12 tgid=12 comm= park=2 held=0
mpos-dbg: signal 11 -> tid=11 tgid=11 comm= park=2 held=0
mpos-dbg: signal 9 -> tid=18 tgid=18 comm= park=2 held=0     <- the probe's child
```

and **no** `take fatal` line for tid 18. The probe's own output is the same as before
(`mode 0 (exit_group): reaped`, then nothing), the child's slot is `gone ... park 2 syscall 62
released=true retired=false`, and its futex-parked sibling is still `slot 19 ... park 2
(block_until_nanos) syscall 202 parks=2`.

So the signal **is** queued to the leader and `t.sched.wake(18)` **is** called (`held=0`), yet the
leader never runs `take_fatal_signal`: the `comm=` column reads empty for every line, which also says
the diagnostic could not read the target's name from the slot it printed (the field is the target's
`mm`, and a slot whose group has already released its address space reports nothing). The next
instrument is therefore narrower: print, inside `block_until_nanos`'s loop, the pending-but-not-blocked
set of the task for every pass (once per task), and print whether `t.sched.wake(id)` found the target in
its sleep queue - one boot then says whether the wake found a task to requeue at all.

Two further readings, one of them a defect in the instrument itself:

```
mpos-dbg: dispatch sig=9 tid=18 sleeping_before=0 sleeping_after=0 pending=0x100
mpos-dbg: nanos tid=17 pending=0x0 blocked=0x0
mpos-dbg: nanos tid=18 pending=0x0 blocked=0x0
mpos-dbg: nanos tid=19 pending=0x0 blocked=0x0
```

- `pending=0x100` is bit 8, signal 9: the SIGKILL **is** in the target's pending set. `sleeping_before=0`
  is not a lost wake - the sender and the target are the same task, which is *running* inside its own
  `kill(2)`, so there is nothing to requeue.
- The `nanos` lines are the first `block_until_nanos` pass of each of the probe's tasks, and they are the
  **pre-kill** passes (`pending=0x0`): the second child, the one that kills itself, never printed, so it
  never reached a park that the instrument could see.
- `comm=` was empty on every delivery line, which is the instrument's own bug
  (`task.mm.comm_name_bytes()` is not the running task's `comm`), and that is also why the `t-fx` gate
  silenced the second child's prints: a gate that reads an empty name matches nothing. The next
  instrument gates on the task id, not on a name, and prints **every** pass whose pending set is not
  empty.

### §6.77b there were two doors, and only one of them ended the group - **FIXED (2026-11-04)**

The next instrument gates on the task id, as §6.77a's own closing line asked. It printed, for the
second child - the one that kills itself:

```
mpos-dbg: handle_kill args=[18, 9]
mpos-dbg: signal 9 -> tid=18 tgid=18 park=2 held=0
mpos-dbg: dispatch sig=9 tid=18 sleeping_before=0 sleeping_after=0 pending=0x100
```

and no `take fatal` line for tid 18, ever. The reason is not a lost wake: `kill(getpid(), SIGKILL)`
is a signal whose sender *is* its target, so the target is running, `sleeping_before=0`, and
`send_signal_info`'s `t.sched.wake(id)` has nothing to requeue. The signal is taken later, in the
*same syscall* - `src/syscall/mod.rs` calls `linux::deliver_signal` on the way out of every syscall,
and its `DefaultAction::Terminate` arm ended the task with a bare `crate::sched::exit(-(sig as
isize))`.

That is the second door. `take_fatal_signal` (`src/sched/runtime/child_signal.rs`) called
`end_group_siblings` before returning; `deliver_signal` did not. So the group's fate depended on
*where* the signal was taken:

- killed from **outside** while parked: `send_signal_info` wakes the target, the blocking wait's loop
  takes the signal through `take_fatal_signal`, the group ends - the shape the old pin covered;
- killed by **itself**: the pending bit is still there when `kill` returns, so the signal is taken at
  that call's own syscall-exit gate, one thread exits, the parked sibling is never retired,
  `group_dead` is false and the parent's `wait4` parks for ever.

The proof is the park dump's own syscall number: `gone slot 18 pid 18 parent=17 park 2 syscall 62
released=true retired=false` - 62 is `kill` on x86_64, not 34 (`pause`), so the leader died at the
`kill` gate and never reached the `pause` after it.

**The fix** is one door and no second reading of the same rule:
`child_exit::die_of_fatal_signal(sig)` walks `end_group_siblings` and then `exit`s with the number.
`take_fatal_signal` now only *takes* the signal, and every fatal-death site routes through the door:
its six callers in `src/sched/runtime/task.rs` (`:2837`, `:2881`, `:2917`, `:2949`, `:3022`, `:3479`),
`deliver_signal`'s `Terminate` arm and its frame-build fallback, and `rt_sigtimedwait`'s poll loop.
Linux decides the same question earlier, at `complete_signal` time (`SIGNAL_GROUP_EXIT`); this kernel
decides it at the death site, which is why there can be only one death site. The seccomp arms
(`src/syscall/linux/mod.rs:106`, `:213`) deliberately stay outside: their own comments say a
kernel-initiated end is not a delivery, so pid-namespace init protection does not apply.

**The pin had to grow a second fixture.** `t-kill-ends-group`'s existing fixture kills its child from
the parent's side after a 700 ms wait - exactly the shape that already worked. The new one,
`self_killed_process`, has the same parked worker and calls `kill(getpid(), SIGKILL)` itself; neither
fixture parks the leader in `pause(2)`, which would take the signal inside a wait and re-test the old
path.

Counterfactual, measured with the fix's call sites reverted to bare `exit`s on the shipped
`take_fatal_signal`-ends-the-group code, one test booted alone (`build/probe/oneshot/serial.log`):

```
=== RUN t-kill-ends-group
ok:    the fixture process starts
ok:    the process is reaped even though a thread was parked in a futex
ok:    the death is the signal that killed it
ok:    the self-killing fixture process starts
FAIL:  a process that kills itself is reaped though its thread is parked in a futex
FAIL:  a self-kill is still the signal that is reported as the cause
=== t-kill-ends-group exit=1
```

and on the fixed kernel the same boot reads six `ok:` and `exit=0`. Both outside-kill claims stay
green in the counterfactual - the asymmetry the single-fixture pin could not see. Full sweep after the
change: x86_64 `pass=255 fail=0 (255 built)`, aarch64 `pass=254 fail=0 (254 built)`, host 1224 tests
0 failed (`build/gate-both5.log`).

**A trap that cost an hour, recorded so the next session does not pay it again.** The UEFI loader
carries the kernel *inside its own image* (`boot/src/lib.rs:176`,
`PACKED_KERNEL = include_bytes!(concat!(env!("OUT_DIR"), "/kernel.deflate"))`). Rebuilding
`-p mpos` alone changes nothing a boot can see: `.workbuddy/one-test-boot.sh` copies
`target/x86_64-unknown-uefi/release/mpos-boot.efi`, which still holds the previous kernel. Two
"counterfactual" boots at 23:48 and 23:55 reported the pin green because they were running the 23:09
loader; the reading was of the *fixed* kernel both times. Every hand-verification of a kernel change
must rebuild both:

```bash
cargo build --target x86_64-unknown-none --release
MPOS_KERNEL_ELF=$PWD/target/x86_64-unknown-none/release/mpos \
    cargo build -p mpos-boot --release --target x86_64-unknown-uefi
```

A marker `println!` in the boot flow is the cheap check that the booted kernel is the one just built.

**The doors this one did not close.** The fix above routed every death through the *signal* machinery.
The arch's own fault handlers do not pass through it: they answer the disposition themselves, so the
same defect lived there in seven sites, one layer down and behind the `hal::kernel` seam. Closed the
same day by carrying the door across that seam - `hal::kernel::die_of_fatal_signal` (§6.77c).

### §6.77c the same two doors, one layer down: the machine's own fault handler - **FIXED (2026-11-04)**

§6.77b closed every *signal* door. It left one family open, and the audit that found the fix's
shape named it: the arch's fault handlers end a task without going through `sched::exit` at all.
There were seven of them, and every one answered `-11` (SIGSEGV) or `-4` (SIGILL) by calling
`crate::hal::kernel::exit(code)` - a door that marks the faulting task exited and nothing else. So a
process whose *leader* faults with `SIG_DFL` left a parked sibling alive for exactly the reason
§6.77b's self-kill did, and its parent's `wait` parked forever:

- x86_64, `arch/src/arch/x86_64/idt.rs` - the page fault's `disposition_is_default(11)` arm and its
  `fault_repeats(rip)` arm, and `user_fault_or_die`'s two arms of the same pair (four sites).
- aarch64, `arch/src/arch/aarch64/vectors.rs` - `el0_sync`'s `Sys64Answer::Segv` arm, its residual
  fault arm, and the unhandled-sysreg arm that answers `-4` (three sites).

The fix is the door carried across the arch seam, because the arch layer cannot reach `sched` (it is
a `no_std`, downward-only crate that talks to the kernel through `hal::kernel`'s `Kernel` trait):

```rust
// arch/src/hal/kernel.rs, on the trait, beside `exit`
fn die_of_fatal_signal(&self, sig: u8) -> !;

// and the free forward every arch module calls
#[inline]
pub fn die_of_fatal_signal(sig: u8) -> ! { registered().die_of_fatal_signal(sig) }
```

`src/kernel_services.rs` is the trait's only implementor and answers with
`sched::die_of_fatal_signal(sig)`, so **the arch's `-11` now means what the signal number means
everywhere else**: the group ends, then the task exits with `-(sig)`. All seven sites call it, and
`grep -rn "kernel::exit(" arch/src/` has no caller left outside the hal's own definition.

Why the fault sites cannot simply call `exit(-11)` and then have `exit` do the walk: `exit` decides
`group_dead` by asking whether any task of the group is still alive, and a parked sibling always is -
that decision is *correct* for a caller that is the last one leaving and wrong for one that is
leaving first. The walk has to happen before the decision, which is why it is a door and not a
branch inside `exit`.

**Pinned** by `t-kill-ends-group`'s third fixture (`user/tests/src/bin/t-kill-ends-group.rs`). It
spawns the same `FUTEX_WAIT` parker, sleeps 300 ms so the worker is parked, then restores `SIG_DFL`
for `SIGSEGV` and stores through an unmapped address:

```rust
unsafe { libc::signal(libc::SIGSEGV, libc::SIG_DFL) };
let bad = std::hint::black_box(0xdead_0000 as *mut u8);
unsafe { std::ptr::write_volatile(bad, 1) };
```

The `signal(SIGSEGV, SIG_DFL)` is load-bearing and was measured the hard way: a Rust `std` program
starts with the runtime's stack-overflow handler installed, so the first version of this fixture took
the `fault_repeats` arm instead of the default-action arm and the kernel printed

```
task 6 died of SIGSEGV it could not be handed: 0xdead0000 (PageFaultErrorCode(CAUSED_BY_WRITE |
USER_MODE)) at 0x445bb9, the same fault 3 times over, and a handler is installed
```

That is a *correct* answer to a question the pin was not asking. `black_box` is load-bearing for the
same class of reason: a store through a constant address can be folded into a trap instruction, and
the pin is about the machine's page fault.

The three claims are "the faulting fixture process starts", "a process whose thread faults is reaped
though its thread is parked in a futex", and "the death is the fault that thread took"
(`killed_by == Some(SIGSEGV)`; `run_fixture` returns `status.signal()` now, so a fixture that died of
the *wrong* signal fails rather than passing as "reaped"). Host Linux is the reference: all nine
claims `ok:` and `PASS` under `MPOS_ON_HOST=1` (three fixtures x three claims).

**Counterfactual, measured.** With the four x86 fault sites reverted to `kernel::exit(-11)` (kernel
*and* loader rebuilt - see §6.77b's trap), the pin reports

```
t-kill-ends-gro[6]: segfault at dead0000 ip 0000000000445bc5 sp 000000003fa3dbb0 error 6
FAIL:  a process whose thread faults is reaped though its thread is parked in a futex
FAIL:  the death is the fault that thread took
=== t-kill-ends-group exit=1
```

with the six signal-door claims still green. Restored, the same boot prints all nine `ok:` and
`=== t-kill-ends-group exit=0`, with a marker `println!` in the boot flow
(`mpos-dbg: marker-B fault door kernel`) proving the booted kernel was the one just built.

**The gate** (`build/gate-faultdoor3.log`, both arms): stage 1 `1224 tests, 0 failed` (15.97 s);
x86_64 `pass=255 fail=0 (255 built)`, `mpos-kunit 35 passed, 0 failed`, exit 33, `total 16.20 s`;
aarch64 `pass=254 fail=0 (254 built)`, `mpos-kunit 35 passed, 0 failed`, every lane `ok` on
`=== END`, `total 49.30 s`. `t-kill-ends-group` is the heaviest program on x86_64 lane 4 (2.18 s) and
on aarch64 lane 5 (2.35 s), which is its three fixtures' sleep budget, not a stall.

Two runs before that one failed on **unrelated, already-known ground**: a stage-1 red whose captured
tail held compiler warnings and no failing test (`build/gate-faultdoor.log` - the standalone rerun of
the gate's own cargo command is 726 + every leaf crate green), and one `FAIL t-ptrace-attach` on
`a detach leaves a stop the user asked for alone` in `build/test-lane-x86_64-5/serial.log`, the
same claim whose 20x10 ms poll was tightened for budget in §15.93. Both re-ran green without a change
to this work; the readings are recorded so the next red is compared against them rather than
re-litigated.

### §6.77d `KILL_PROCESS` and `KILL_THREAD` are not the same word, and this machine had one answer - **FIXED (2026-11-04)**

The audit that produced §6.77b and §6.77c left one naming on its risk list: the seccomp gate's
`_ => crate::sched::exit(-(SIGSYS as isize))` arm served **both** `SECCOMP_RET_KILL_PROCESS` and
`SECCOMP_RET_KILL_THREAD`. Its own comment argued they were the same call. They are not, and the
difference is the whole content of the two words: one is a thread's death, the other a process's.

**Measured on Linux first** (`build/scratch/sckill/p.c`, gcc -O1 -pthread). The probe forks per mode;
the child parks a `pthread_create`d worker in `futex(word, FUTEX_WAIT|FUTEX_PRIVATE_FLAG, 2)` on a
`MAP_SHARED|MAP_ANONYMOUS` word, sleeps 200 ms, installs a filter answering the mode's action for
`SYS_getpid`, then makes that call. The parent polls `waitpid(WNOHANG)` for 6 s:

```
mode 0 (KILL_THREAD):  STILL ALIVE after 6 s
mode 1 (KILL_PROCESS): reaped, signal=31
```

31 is `SIGSYS`. So both actions end with the same signal - and that is what had made them look
alike - while `KILL_THREAD` leaves the group alive behind its parked worker and `KILL_PROCESS` reaps
it. A parent waiting on the `KILL_THREAD` process blocks until that survivor is killed.

**This machine behaved as `KILL_THREAD` for both**, which is the direction that looks green: the
signal was right, the survivor was wrong, and no test in the tree had a second thread in the child to
see it with.

**The fix** (`src/syscall/linux/mod.rs`, the seccomp gate's action match) gives each word its own
arm, and routes the process one through §6.77b's door:

```rust
sc::RET_KILL_PROCESS => crate::sched::die_of_fatal_signal(SIGSYS),
sc::RET_KILL_THREAD => crate::sched::exit(-(SIGSYS as isize)),
_ => crate::sched::die_of_fatal_signal(SIGSYS),
```

An unrecognised action still ends the process, which is deliberate and unchanged - the man page says
Linux treats an unknown action as `KILL_PROCESS`, and a filter returning an invented action was
measured killing the process with `SIGSYS` (§8.2). The two "ended by the exit door, not by queueing a
signal" paragraphs survive as they were: a filter's own decision must not be droppable by a pid
namespace's init protection.

**The pin** is `user/tests/src/bin/t-seccomp.rs`'s `a_child_with_a_parked_thread(action)`: one child
per action, a private futex word, a worker parked on it, the filter installed, the judged call, and a
`WNOHANG` poll for a second - with a `kill(SIGKILL)` and a reap on the survivor's path so a
`KILL_THREAD` survivor does not leak into the rest of the sweep. The old claim
`"KILL_THREAD kills with SIGSYS too (no threads here to tell them apart)"` is now
`"KILL_THREAD kills the caller with SIGSYS"` beside two new ones:

```
ok:    KILL_PROCESS reaps a process whose other thread is parked in a futex
ok:    KILL_THREAD leaves a process whose other thread is parked in a futex alive
```

All seven kill claims pass on **both** machines and on a current Linux host with `MPOS_ON_HOST=1`.

**Counterfactual, measured** (the two arms merged back into one, kernel *and* loader rebuilt -
§6.77b's trap):

```
FAIL:  KILL_PROCESS reaps a process whose other thread is parked in a futex (got (false, None))
ok:    KILL_THREAD leaves a process whose other thread is parked in a futex alive
=== t-seccomp exit=1
```

Only the process claim flips; the thread claim stays green either way, which is exactly the
asymmetry - a one-arm kernel is a correct `KILL_THREAD` and a lying `KILL_PROCESS`.

**Gate** (`build/gate-seccomp.log`, both arms, `TMPDIR=$PWD/build/tmp`): stage 1 `1224 tests,
0 failed` (29.93 s); x86_64 `pass=256 fail=0 (256 built)`, `mpos-kunit 35 passed, 0 failed`, exit 33,
`total 30.12 s`; aarch64 `pass=255 fail=0 (255 built)`, `mpos-kunit 35 passed, 0 failed`, every lane
`ok` on `=== END`, `total 74.99 s`, heaviest `t-oom-kill 15.15 s`.

**Not fixed here, and named**: the *strict* mode's `SIGKILL` (§8.2's first arm) is a different
promise and stays `exit`; and the three arch fault sites of §6.77c are untouched by this section.

### §6.77e a number names a process or a thread, and this machine had one answer for both - **FIXED (2026-11-04)**

Found while pinning §6.77d: the new `t-seccomp` claims reached
`ok: KILL_PROCESS reaps a process whose other thread is parked in a futex` and then the boot hung
with a stall dump whose three lines say everything -

```
mpos:   slot 5 pid 5 name=/bin/t-seccomp   park 2 (block_until_nanos) syscall 61 parks=63
mpos:   gone slot 6 pid 6 name=/bin/t-seccomp   parent=5 park 2 syscall 317 released=true retired=false fds=3
mpos:   slot 7 pid 7 name=/bin/t-seccomp   park 2 (block_until_nanos) syscall 202 parks=2
```

- slot 5 is the test's own blocking `wait4`;
- slot 6 died at its own **`seccomp` call gate** (317) with `retired=false` - the KILL_THREAD
  fixture's leader, ended by `SECCOMP_RET_KILL_THREAD`, which is correct;
- slot 7 is that fixture's parked worker, alive, which is also correct.

The hang was the **cleanup**: the parent's next `kill(kid, SIGKILL)` - aimed at the number of a
process whose leader has exited and whose worker still lives - answered `ESRCH`, and the `waitpid`
after it had no bound. So the pin was not measuring the seccomp gate at all; it was measuring what a
pid number means.

**Measured on Linux first** (`.workbuddy/probe_kill_leaderless.c`, Linux 7.2.11, merged from the
four scratch probes `build/scratch/zombiekill/p{,2,3,4,6,7,8}.c`; a `static` flag is *not* shared
across `fork`, so the worker's handler result is published through a `MAP_SHARED` page - that detail
cost the first probe its reading, `p5.c`):

```
kill(leaderless 9, 0)           = 0 errno=0
tkill(exited leader 9, 0)       = 0 errno=0
tgkill(9, 9, 0)                = 0 errno=0
tkill(exited leader 9, SIGUSR1) = 0 errno=0
  worker handler ran? no
kill(leaderless 9, SIGUSR1)     = 0 errno=0
  worker handler ran? YES
kill(leaderless 9, SIGKILL)     = 0 errno=0
  waitpid=9 status=0x0
kill(zombie 11, 0)              = 0 errno=0
tkill(zombie 11, 0)             = 0 errno=0
kill(reaped 11, 0)              = -1 errno=3
tkill(reaped 11, 0)             = -1 errno=3
tkill(dead worker 13, 0)        = -1 errno=3
kill(dead worker 13, 0)         = -1 errno=3
```

Two rules, and they differ:

- **A `kill` number names a process.** It stays owned while any thread of the group lives, so
  `kill(<dead leader>, 0)` is 0 and a real signal reaches the *surviving worker* (its handler ran).
  A zombie's number is still owned too. Only once the whole group is reaped does the number die
  (`ESRCH`).
- **A `tkill`/`tgkill` number names a thread.** `tkill(<dead leader>, 0)` is 0 and delivers
  **nothing** - the number is the group's, so it outlives the thread - while `tkill(<dead
  worker>, 0)` is `ESRCH` whether that thread has been joined or not. A worker's number dies with
  the thread.

**This machine had one answer for both**, and it was the thread's: `kill`, `tkill`, `tgkill` and
`rt_sigqueueinfo` each resolved a number through `sched::resolve_pid` to a **slot**, and
`send_signal_info` answers `Sent::NoTask` for an exited task, so every door said `ESRCH`. Measured
on the guest (`build/scratch/zombiekill/t-zkall.c`, booted with the kernel and loader both rebuilt):

```
kill(leaderless 6, 0)       = -1 errno=3
tkill(exited leader 6, 0)   = -1 errno=3
tgkill(6, 6, 0)            = -1 errno=3
kill(leaderless 6, SIGKILL) = -1 errno=3
kill(zombie 6, 0)           = -1 errno=3
tkill(zombie 6, 0)          = -1 errno=3
```

Four *wrong* answers in the first five lines (Linux answers 0 to all four), and the same shape meant
a process that killed its own leaderless group could never be reaped - which is what stalled
`timeout(1)`-shaped code and, here, the pin.

**The fix** is the two resolution rules written down where the numbers are resolved
(`src/sched/runtime/child_signal.rs`, beside `process_members`):

```rust
pub enum Named {
    /// A live thread, which is where the copy goes.
    Thread(TaskId),
    /// The number is owned but its thread has exited: nothing is delivered and
    /// the sender still hears success.
    Gone(TaskId),
}

pub fn named_process(number: u32) -> Option<Named>   // kill, rt_sigqueueinfo
pub fn named_thread(number: u32) -> Option<Named>    // tkill, tgkill
pub fn thread_group_of(id: TaskId) -> Option<TaskId> // tgkill's second name
```

`named_process` resolves the number to its group and answers the live **leader** if there is one, a
surviving worker if the leader is gone, and `Gone` when the group has no live thread left (a zombie
whose number is still owned); `named_thread` answers the slot itself when it is alive and `Gone`
only when the dead thread *is* the group's leader. `Gone` is a success to the sender with nothing
queued, `None` is `ESRCH`, and the permission question (`may_signal`) is asked about
`Named::slot()` - a zombie's credentials are still its own.

Every door was rewired to it: `handle_kill`'s positive arm (`src/syscall/linux/proc.rs`),
`tkill` split out into its own `handle_tkill` (it had been dispatching to `handle_kill`, which is
why the thread rule had nowhere to live), `handle_tgkill` (whose group check now compares
`thread_group_of(tid.slot())` against the named group rather than reading `snapshot().pid`), and
`handle_rt_sigqueueinfo`. `resolve_pid` keeps its own users (`ptrace`, `process_vm_*`, the priority
and affinity pairs) - a **slot** is the right answer for those, and this change is about what a
*name* means, not about how a slot is found.

**The pin** is `user/tests/src/bin/t-kill-numbers.rs` (12 claims, four shapes: a leaderless group, a
zombie, a reaped number, and a dead worker's tid published through a pipe). Its two shapes worth
naming: the leader ends itself with a raw `exit(2)` so the group survives the leader deliberately
(`pthread_exit` would be the same shape, less obviously), and every `wait` is a **bounded**
`WNOHANG` poll - the counterfactual run proved why, because a blocking one hung the whole lane.

**Counterfactual, measured.** With the four doors reverted (loader rebuilt too - §6.77b's trap), the
boot reports

```
kill(6,0) = -1 errno=3
tkill(6,0) = -1 errno=3
tgkill(6,6,0) = -1 errno=3
kill(6,SIGKILL) = -1 errno=3
```

four claims red, and then the run **hangs** on the unbounded `waitpid` (stall dump: the parent at
`syscall 61`, the leader `gone ... retired=false`, the worker at `syscall 230`). Restored, the same
boot prints all twelve `ok:` and `=== t-kill-numbers exit=0`, and on a current Linux host
`MPOS_ON_HOST=1` gives the same twelve.

**Gate** (`build/gate-pidnum3.log`, both arms, first run green on all three stages): stage 1
`1224 tests, 0 failed` (27.20 s); x86_64 `pass=256 fail=0 (256 built)`, `mpos-kunit 35 passed,
0 failed`, exit 33, `total 27.39 s`; aarch64 `pass=255 fail=0 (255 built)`, `mpos-kunit 35 passed,
0 failed`, every lane `ok` on `=== END`, `total 70.03 s`. `t-kill-numbers` runs and passes on both.

**What this does not fix**, named rather than implied: a *zombie*'s number being owned is answered
here on the resolution side only - the group's `Retired` record still has to exist for `kill` to
find anything, which is `exit`'s job and unchanged; and `kill(-1)`/`kill(0)`/`kill(-pgid)` were
already group-shaped and are untouched.

### §6.77f the last `-11`: `rseq`'s kill decided the task's fate, not its group's - **FIXED (2026-11-04)**

§6.77c closed the arch's own fault handlers and, in its last line, named two sites it had not
touched: `out_of_memory` (which already walks the group) and `rseq`'s `Tick::Kill`, whose exit code
`-11` claimed a `SIGSEGV` death while ending only the faulting thread. The audit had put it on the
"do not change" list for the wrong reason - its *comment* says `SIGSEGV` is Linux's answer to an
unreadable section, which is true, and the code then delivered one thread's worth of it.

`crate::sched::rseq::tick(ip)` answers `Tick::Kill` for two shapes, both of them the kernel deciding
a task's memory is unusable rather than the mmu taking a fault: a registration whose block
`read_user` can no longer reach, and a task still inside its armed span on a second tick (a span is
instructions, never a millisecond - §6.35). Either way the task must die of `SIGSEGV`, and **a fault
is a process's death on Linux**, so the group goes with it.

The site (`src/sched/runtime/task.rs`, `preempt_at`) now calls the door instead of exiting:

```rust
crate::sched::rseq::Tick::Kill => die_of_fatal_signal(crate::sched::signal::SIGSEGV),
```

`src/sched/signal.rs` gained `pub const SIGSEGV: u8 = 11` for it - this list had never needed the
number, because userspace reached `SIGSEGV` only through the machine's fault handlers, and the
kernel now raises it itself in this one place. Its doc comment says exactly that, so the constant's
presence is recorded rather than looking like an omission somebody filled in.

**The pin could not be the existing one.** `t-rseq` already claimed

```
ok:    a tick inside an armed section ends it: the task dies of SIGSEGV, never resuming mid-span
```

and a bare `exit(-11)` satisfies it - the signal is identical, which is why this site sat unnoticed
through three earlier fixes. What differs is the **group**: with the faulting thread gone and a
sibling parked, `exit` answers `group_dead == false`, records nothing as retired, never wakes the
parent, so its `wait` never returns. So `t-rseq` gained a second fixture,
`armed_child_with_parked_thread()`: a child that maps a private futex word, parks a
`std::thread` on it, arms the rseq descriptor in the same span the existing probe uses, and dies in
it; the parent polls `waitpid(WNOHANG)` 300x10 ms rather than blocking, so the unfixed shape fails a
claim instead of hanging the lane (the lesson §6.77e paid for), and kills and reaps on the survivor's
path so nothing outlives the program.

```
ok:    a tick inside an armed section reaps the whole process, parked sibling and all
```

**Counterfactual, measured** (the site back to `exit(-11)`, kernel *and* loader rebuilt - §6.77b's
trap):

```
ok:    a tick inside an armed section ends it: the task dies of SIGSEGV, never resuming mid-span
FAIL:  a tick inside an armed section reaps the whole process, parked sibling and all
=== t-rseq exit=1
```

The old claim stays green in both worlds. That is the whole reason this defect survived three
sweeps: the signal was right and the survivor was invisible.

**Gate**, taken as two runs because this host's six concurrent TCG aarch64 lanes kept tripping the
EL0 `EXCEPTION: synchronous` flake recorded in `AGENTS.md` §5 (`build/gate-rseq.log` had x86_64 green
with `t-ptrace-attach` and `t-cgroupfs` red on aarch64, `build/gate-rseq2.log` and
`build/gate-rseq4.log` each lost a lane or two to it): the first machine alone,
`build/gate-rseq4.log` - stage 1 `1224 tests, 0 failed` in 15.09 s, `pass=256 fail=0 (256 built)`,
`mpos-kunit 35 passed, 0 failed`, exit 33, `total 15.28 s`; the second alone,
`build/gate-rseq-a.log` - `pass=255 fail=0 (255 built)`, `mpos-kunit 35 passed, 0 failed`,
`no process exit: the lanes ended on === END`, `total 30.04 s`. `=== PASS t-rseq` appears in the
lane logs of both arms; every one of the failures above was re-run to green and each is the
already-recorded flake, not this change's.

**What is left of the family**: `out_of_memory` (`src/sched/runtime/child_exit.rs`) still does its
own `end_group_siblings` + `exit` rather than calling the door. It is *correct* - it walks the group
- and it is duplication rather than a defect, so it is named here and not carried as an open bug.

### §6.78a a `ptrace` claim read a clock where it should have read a state - **FIXED (2026-11-04)**

`t-ptrace-attach`'s "a detach leaves a stop the user asked for alone" went red in gate runs on
2026-11-04, twice on the aarch64 arm and once on the x86_64 arm, each time alone on its lane and each
time re-running green. It was carried as a load-dependent flake, and the poll it failed in **is**
tight: a 20 × 10 ms `waitpid(WNOHANG)` window (200 ms) against a child whose own sleep is 100 ms, after
the program's budgets were shortened from 2000/1000/800 ms to 100/50/100 ms (§15.93). Re-running it
eight times in one boot — the shape that makes a budget claim show itself — was 8/8 green, so the
window was not obviously the whole story.

**The claim was asking a question a window cannot answer.** What a debugger depends on is the state a
detach leaves the tracee in: a process the user stopped is **`T`** afterwards, and one a detach wrongly
handed back running is `R`/`S` (or gone, if it reached its exit inside the window). The old check
observed only "nothing was collected in 200 ms", which a busy machine can produce for a process that
*is* running — and equally for one that is still parked, which is why the reds were load-shaped and
not reproducible.

**The reading, instrumented on the machine** (`build/probe/oneshot/serial.log`):

```
mpos-dbg: ptrace detach state="T" gone=false
ok:    a detach leaves a stop the user asked for alone
```

`T` with nothing collected — the kernel's answer was right in every run measured; the flake was the
*check's* confidence, not the behaviour.

**The fix** makes the claim its own promise: read field 3 of `/proc/<pid>/stat` and assert the state
letter, keeping the window as a second half rather than the whole test.

```rust
let state = stat_field(&format!("/proc/{stopped}/stat"), 3).unwrap_or_default();
r.check(
    &format!("a detach leaves a stop the user asked for alone (state {state:?})"),
    !gone && state == "T",
);
```

`stat_field` is the helper `t-tid-dir` already owns the shape of (field 2 is inside `comm`, which may
contain `)`), copied here rather than shared because the test crate has no room for a third opinion
about a one-field read.

**Measured after the fix.** Eight sequential runs in one boot, all `ok:` with `state "T"`; the host
reference (`MPOS_ON_HOST=1`) `ok:` and `PASS`; the both-arm gate
(`build/gate-ptrace.log`) green on all three stages — stage 1 `1224 tests, 0 failed` (15.05 s),
`verdict pass=256 fail=0` + kunit 35/0 for the first machine, `verdict pass=255 fail=0` + kunit 35/0
for the second.

**Rule for the next claim built on a poll:** if the property is a state the kernel reports, assert the
state and use the poll only to establish that nothing was *collected*. A poll alone cannot distinguish
"still waiting" from "wrong", and the load that makes the two look alike is exactly the load a gate
runs under.

### 6.79 A detach handed back a tracee the user had stopped — observation, not yet a diagnosis (2026-11-04)

`t-ptrace-attach`'s "a detach leaves a stop the user asked for alone" reddened twice in one afternoon on
the **second** machine's six-lane sweep, and the evidence the claim now prints says the two readings are
not the same event:

```
FAIL:  a detach leaves a stop the user asked for alone (state "", collected true)
```

`collected true` is the sharp one: `waitpid(WNOHANG)` collected the child inside the 200 ms window, so
the detach resumed a tracee the user had stopped with `kill -STOP`. `state ""` is the weaker one — the
`/proc/<pid>/stat` read itself failed, which can also happen because the pid was already gone.

What is measured so far:

* **not reproducible alone, on either machine.** Eight sequential runs in one x86_64 boot: 8/8
  `ok:    a detach leaves a stop the user asked for alone (state "T", collected false)`. The same claim
  is green on both arms of every full gate run in the session that was not one of these two.
* **not caused by the signal fix that happened to be in the tree.** A control run with that fix stashed
  (`build/gate-control.log`) was green here and red on the futex claims the fix is meant to flip.
* the shape the kernel reports for a *green* detach of a user-stopped task, from the instrumented serial
  run (`build/gate-dbg-serial.log`): `mpos-dbg: detach id=7 sig=0 stop=true group=Some(true)
  delivering=None pending=0x40000 blocked=0xfffffffffffbfeff` — kept, not cleared, no wake.

The suspect, by reading rather than by measurement: **`PTRACE_ATTACH` injects its stop as the same
`SIGSTOP` bit a user's `kill -STOP` sets** (`src/syscall/linux/proc.rs`'s attach path does
`set_tracer(id, me)` then `send_signal(id, SIGSTOP)`), and a task has **one** stop object. `detach_traced`
(`src/sched/runtime/trace.rs`) drops the attach's stop by clearing the pending `SIGSTOP`/`SIGTSTP`/
`SIGTTIN`/`SIGTTOU` bits and then waking the task when no stop object is left — so if the attach's stop
overwrote a user's group stop, or the user's stop was still only *pending*, the detach clears the user's
stop too and the task runs. The Linux-shaped fix candidate is to make an attach to an already-stopped
task *report that stop to the tracer* instead of queuing a second `SIGSTOP`, so nothing needs clearing.
That is a hypothesis; a failing run has not yet been caught with the instrument in place (the one attempt
hung its lane before the program ran).

**Owed:** catch the failing shape with the detach instrument, then decide between "one stop object per
task" and Linux's separate group-stop accounting.

#### What the stop's own park does with a wake (measured 2026-11-04, still not a diagnosis)

Reading `trace::park` (`src/sched/runtime/trace.rs`) turned up a mechanism that would be enough to explain
the observation and is *not* the same thing as the hypothesis above: **the stop park sleeps once and
returns on the first wake of any kind**, and it does not ask on the way out whether the stop is over.
Instrumented with one print after the sleep, on a child held in a `SIGSTOP` while its own 200 ms
`setitimer(ITIMER_REAL)` kept firing:

```
mpos-dbg: PARKWAKE id=6 stop_now=true
```

so the park does return while `trace.stop` is still set — the interval timer's `check_itimers` wake
(`src/sched/runtime/task.rs`'s `self.sched.wake(TaskId(i))`) is enough to take a stopped task out of the
park and back into the signal-delivery path. A stop that is ended by a wake rather than by a *resume* is
the shape Linux does not have, and it is what a later spurious wake plus a cleared record could turn into
`collected true`.

The two probes were written to make that differential visible and **both shapes hold on this machine as
they do on Linux**, so neither is a pin for it:

* Linux first (`build/scratch/stoptimer/p2.c`, `gcc -O1`): a child asking for 2500 ms of `nanosleep` and
  `SIGSTOP`ped 150 ms in is `T` 2600 ms later — a second past the deadline it had already armed — with
  nothing collected, and `SIGCONT` is what gets it running again. The same shape on the machine's own
  probe (`build/scratch/stoptimer/t-stoptimer.c`, musl static, booted by `.workbuddy/one-test-boot.sh`)
  reads `reported stopped = 1` / `after four of its own ticks inside the stop: state=T collected=0` /
  `collected after SIGCONT = no`.
* Linux's interval-timer shape (`build/scratch/stoptimer/p3.c`): `state=T collected=0` after four of the
  child's own 200 ms ticks fire inside the stop. The machine answers the same, with the `PARKWAKE` line
  above sitting between the stop and the reading.
* The claim written for it (`t-signal-park`'s "a stopped process stays stopped while its own timer
  expires", phase-free: a single 2500 ms sleep, stop at 150 ms, read at 2600 ms) **passed with and
  without a re-park loop in `park`** — so it satisfies Linux's answer and pins nothing about the fix.
  It was therefore not shipped: `AGENTS.md` §3 rule 4 refuses a claim that is green either way, and a
  fix whose pin cannot be made to fail is a fix nobody has measured.

A re-park loop (`while trace.stop.is_some() { sleep again }`, with the check under the same lock as the
sleep so a resumer's clear-then-wake cannot be missed) was written, built for both machines, and reverted
unshipped, for that reason alone. The record of it is here so the next attempt starts from the
measurement instead of the guess.

**Owed now:** catch a failing detach run with the `PARKWAKE` print *and* a print on whichever path clears
`trace.stop`, so the observation's interleaving is read rather than reasoned; the re-park loop is the
candidate fix and its pin has to be the failing interleaving, not a stationary stop.

### 6.80 A one-lane sweep ran out of the 64 task slots — observation, not yet a diagnosis (2026-11-04)

The first full sweep attempted on a single lane (`MPOS_TEST_LANES=1 cargo xtask test --arch aarch64`,
the shape a serial debugging run wants) died in the kernel:

```
KERNEL PANIC: panicked at src/sched/runtime/task.rs:2156:32:
all 64 task slots are in use
  kernel stack: sp 0xffff80008c5ad960 of [0xffff80008c5a9000..0xffff80008c5b1000), 13984 bytes in use, 18784 bytes to spare
```

immediately after `=== RUN t-stdspawn` / `t-stdspawn: a std program is running` /
`ok:   /bin/true ran and exited 0`. The same run also reported `the sweep staged t-netstats but its
NETSTAT line never reached the log` and `no summary line - the run did not get to the end`.

`alloc_slot` (`src/sched/runtime/task.rs:2156`) panicked rather than refusing, so the answer the *caller*
got was a dead machine where Linux answers `EAGAIN` from `clone`/`fork` at its own task limit.

**The panic is closed** (2026-11-04): `alloc_slot` now answers `Option<usize>`, every spawn door returns
`crate::err::EAGAIN` at the limit, and `spawn_user`'s `SpawnError::NoSlot` maps to the same errno — with
`t-fork-limit` asserting Linux's pair (`forked 196 children before it refused: errno=11` / `after reaping
them all, fork answers 208`). `docs/notes/` §6.81 and item 198 carry it.

**The leak itself is measured but not explained.** Counting live slots across a one-lane sweep
(`MPOS_TEST_LANES=1 cargo xtask test --arch aarch64`, six runs) shows the table filling at about one slot
per program until the limit: 53 of the 64 slots at the panic are `comm=busybox`, `ex=0`,
`parent=Some(1)`, parked in aarch64's **`nanosleep` (`nr=101`)** with `parks` counts climbing. One is left
per program by that program's own `busybox timeout -s KILL …` wrapper (the runner's shape in
`xtask/src/selftest.rs`'s `TEST_INIT`); each is forked by the program's slot (`clone child=N parent=7
flags=0x0` or `0x7d0f00`), is reparented to init when its forker exits, and **never execs** — the only
`exec` line for a leaked slot is the runner's own `/usr/bin/tr`. Their `mpos-dbg: long sleep id=N
ms=1000 at=…` timestamps rise by ~1.004 s per iteration, so the `nanosleep` itself returns and it is the
loop around it that never ends. On Linux the same `busybox timeout -s KILL 120 /bin/true` returns in
3 ms, and on this kernel's **x86_64** arm the same three wrapper shapes answer at once
(`build/scratch/to-boot.sh`: `=== A exit=0`, `=== B exit=1` — the program's own argv rule — `=== C
exit=137`), so the loop is aarch64-only and is not the wrapper's normal path.

**Owed:** the aarch64 `busybox timeout` loop's own mechanism — read what the wrapper does after its child
dies on that machine (`build/scratch/` probes and the six `build/gate-slots*.log` runs are the record).

## §6.81 A machine with no room for another task answers `EAGAIN`, not a panic (LANDED 2026-11-04)

§6.80 left a panic and two candidates, and said the answer the *caller* would have got was a dead
machine where Linux answers `EAGAIN`. That half needed no further measurement to act on, so it landed
first and the leak stayed named. Linux's own answer, measured on this host before the fix
(`build/scratch/forklimit/p.c`, `gcc -O1 -o p p.c`: every child parks on a pipe read, the parent counts
until `fork()` fails, then kills and reaps them all and forks once more):

```
forked 196 children before it refused: errno=11 (Resource temporarily unavailable)
after reaping them all, fork answers 208
```

So the limit is a *live* count and its answer is `-1`/`EAGAIN`; nothing is permanent about it.
`alloc_slot` (`src/sched/runtime/task.rs`) had the opposite shape:

```rust
(FIRST_ALLOC_SLOT..MAX_TASKS - 1)
    .find(|&slot| self.tasks[slot].is_none())
    .unwrap_or_else(|| panic!("all {MAX_TASKS} task slots are in use"))
```

**Served.** `alloc_slot` answers `Option<usize>` now, and each of its callers answers the errno a caller
can recover from:

| caller | before | now |
|---|---|---|
| `spawn_copy` (`child_exit.rs`, the fork-shaped `clone`) | panicked | `let Some(child_id) = child else { return crate::err::EAGAIN; }` |
| `spawn_thread` (`child_exit.rs`, `CLONE_THREAD`) | panicked | the same |
| `spawn_user` (`load.rs`) | panicked | `SpawnError::NoSlot`, answered `EAGAIN` by both of `src/syscall/linux/proc.rs`'s matches |
| `create` (`task.rs`, the kernel's own tasks) | panicked | `.expect("the kernel's own task creation has a slot at boot")` — boot-time, and a kernel task at boot has one |

`clone`'s own shape is what makes the negative return safe to thread through: after the spawn function it
calls `hand_down_keyrings` and `hand_down_rseq`, and both start `if child <= 0 { return; }`. So an
`-EAGAIN` flows out of `clone(2)` the way Linux's `-1`/`EAGAIN` does, with no error-cleanup path to unwind.

**Pinned** by `user/tests/src/bin/t-fork-limit.rs`: a pipe whose read end the children park on, then
`fork` until it is refused, then the claims *a fork with no room for another task is refused, not fatal*,
*and the refusal is EAGAIN, the answer Linux gives at its own task limit*, *every child this program made
is collected*, and *a fork after them works again*. `MPOS_ON_HOST=1` answers 4/4 (`300` children on the
host's much larger table, no refusal). On this machine: `(57 children made, answer Some(11))`,
`(57 of 57)` collected, `(answer 6, collected 6)`, `PASS`.

**Counterfactual.** With the panic put back inside `alloc_slot` (and every caller left alone), the same
boot dies before the first claim prints:

```
KERNEL PANIC: panicked at src/sched/runtime/task.rs:2174:32:
all 64 task slots are in use
```

The pin's own first shape hung on the host, and the reason is worth keeping: every child inherited the
pipe's *write* end, so closing the parent's copy left the read end with a writer and no child ever saw
`EOF`. A child must close the write end it inherited before it parks. A fixture that only works because
one side of a pipe happens to die is not a fixture.

**What is still open is §6.80's own question**: which `busybox` loop the lane leaves one of per program,
which is what makes this limit reachable inside a single lane's sweep. Fixing the answer's *shape* does
not fix the leak.

## §6.82 A death was dropped when the store of deaths was full (LANDED 2026-11-04)

The `EAGAIN` fix's pin is what walked into this one. `t-fork-limit` made 57 children, closed their pipe,
and then hung: two claims printed, then nothing, with the parent parked in `wait4` (`parks=11`) while
every child read `released=true retired=true` in the stall dump. The children had died and their records
were gone.

The record store was the cause (`src/sched/runtime/task.rs`):

```rust
pub(crate) const RETIRED_CAP: usize = 32;
…
if retired.len() == RETIRED_CAP { retired.remove(0); }   // in record_retired
```

A record is removed otherwise only by `drop_retired(id)` when its slot is handed to a new tenant - so one
record per slot is the most this machine can ever owe at once, and the slot count is 64. The cap was
half the table, and the eviction dropped the **oldest** uncollected death. Measured with a forty-line
probe (`build/scratch/retired/t-retired.c`: forty children `_exit` while the parent sleeps, then one
`waitpid` each):

```
Linux 7.2.11   heard 40 of 40 deaths
this kernel    heard 32 of 40 deaths        (waitpid answered -1 for the eight)
```

That is worse than a lost exit code, which is why it is not a nit. `wait` for an evicted child answers
`ECHILD` for ever, and a caller **already parked** on it in `block_until_child_event` can never be
released at all: its predicate is `exit_code(id).is_some() || child_event_in(...)`, the record is gone,
and the child is gone. The parked parent in `t-fork-limit` was that case.

**Served.** `RETIRED_CAP` is `MAX_TASKS`, with the reasoning written where the constant is: one death per
slot cannot be exceeded, because the only other way a record leaves is the slot being reused, which calls
`drop_retired` first.

**Pinned** by `user/tests/src/bin/t-wait-many.rs`: forty children are made *one at a time, 25 ms apart*,
each `_exit`ing with its own index, so each frees its slot while its record stays; then the parent
collects all forty and checks each code. Claims: *enough children died at once to fill the old store
(40 of 40)*, *every death is still there to be collected (40 of 40)*, *and each one is the child it
belongs to (40 of 40)*. `MPOS_ON_HOST=1` answers 3/3.

**Counterfactual** (cap back to 32, kernel and loader both rebuilt): `FAIL:  every death is still there
to be collected (32 of 40)` and `FAIL:  and each one is the child it belongs to (25 of 40)`, `FAILED`.

The pin's first shape is the lesson: it forked forty children at once, like the probe, and in a lane
where §6.80's leak already holds most of the table it read `FAIL:  enough children died at once to fill
the old store (9 of 40)` on one arm and `(7 of 40)` on the other - a red that said nothing about the
store. Spacing the forks so each child's slot is recycled made the fixture need two slots instead of
forty, which is the shape a pin has to have: it must be able to run on the machine it will run on.

## §6.83 A test fixture's lifetime is its parent's, not a clock (LANDED 2026-11-04)

`t-proc-access` starts two processes it holds for the length of the program: one whose
`/proc` entry it reads, and one that `setuid(65534)`s itself so the entry names `nobody`.
Both used to `std::thread::sleep(120 s)` and the program's own budget in
`assets/configs/tests.toml` is `timeout = 120`, so the fixture and the budget expired at
the same instant. On a busy lane the machine then answered `FAIL:  and the dropped
process's entry still names it` — the fixture had exited between two of the program's own
steps, which is what the program's comment had already recorded once before
(`docs/notes/07-device-drivers.md` §7.9p raised the fixtures from 20 s to 120 s for exactly
that reason; the budget had caught up with them since).

A duration is a guess about how long the parent will take. The parent knows, so the
fixture waits for the parent to let it go instead: `fn wait_while_the_parent_holds_it()`
reads one byte from fd 0 until the read answers `<= 0`, the two spawns take
`.stdin(Stdio::piped())`, and the program drops both write ends where it used to finish —
so the fixtures cannot outlive the steps they exist for, whatever the lane's load.

The shape took three tries, and the failures are the lesson: a `libc::pipe` +
`Stdio::from_raw_fd` version handed the *same* descriptor to the second spawn, which got
`Os { code: 9, message: "Bad file descriptor" }` because `Stdio` takes ownership, and
`libc::dup`-ing the first read end answered `assertion failed: second_read_end >= 0`.
`Stdio::piped()` is the shape; the second read end comes from the second spawn's own pipe.

The claim now also carries its evidence: the message prints the entry's
`(uid, gid)` as the kernel answers it (`Ok((65534, 65534))` on the machine), so the next
red says *which* way it was wrong instead of only that it was. Under `MPOS_ON_HOST=1`
the fixture cannot drop to `nobody` (a host run is not root), so the claim asserts the
entry exists and names the invoking user's ids — the machine's own form is the one the
gate exercises.

## §6.84 The task table was smaller than the machine's own runner (LANDED 2026-11-04)

§6.80 left one question open - *which* `busybox` was in a one-second loop, and why the lane keeps one per
program - and the answer is: none of the kernel's business. It is `busybox timeout`'s own **watchdog**.

Measured on the first machine with the smallest boot that shows it (`build/scratch/watchdog-init.sh`, one
`busybox timeout -s KILL 5 /bin/true` and three `ls /proc` dumps around it):

```
=== before
TASK 1 cmd=[/bin/sh /init ]
timeout exit=0
=== immediately after
TASK 1 cmd=[/bin/sh /init ]
TASK 7 cmd=[/bin/busybox timeout -s KILL 5 /bin/true ]
=== 3 s later
TASK 1 cmd=[/bin/sh /init ]
TASK 7 cmd=[/bin/busybox timeout -s KILL 5 /bin/true ]
=== 8 s later
TASK 1 cmd=[/bin/sh /init ]
```

Twelve wrappers in a row (`build/scratch/leak-init.sh`) leave twelve of them, each its own session leader
(`stat` reads `pgrp=pid`, `session=pid`) and each reparented to init, and each gone once its own deadline
passed. So `timeout` forks a watcher that sleeps until the budget expires and then kills the program's
group; when the program finishes early - which is the normal case for a test - the watcher still has the
whole budget to serve. With the shipped `tests.toml` budgets that is **120 s per program**, so one slot
per program is held for two minutes after the program it watched is done.

A lane is ~50 programs and `MAX_TASKS` was 64. That is the whole of §6.80's ladder: the lane fills the
table while it is still sweeping, and from then on `busybox timeout` cannot fork at all:

```
=== RUN t-closedfb
timeout: fork: Resource temporarily unavailable
=== FAIL t-closedfb exit=1
```

- which is §6.81's honest `EAGAIN` reaching a caller that can name it, instead of §6.80's panic. The
sweep became visible as red programs rather than a dead machine, and that is what let the ladder be read.

The harness could not kill the watcher it cannot see (it is a grandchild, reparented to init), and a runner
that waits for a 120 s watcher after every program is a runner that has not bounded anything. The table was
the thing that was wrong: `MAX_TASKS` is 256 now (`src/sched/runtime/task.rs`), with the measured chain in
its doc comment. The cost is the stacks - `256 × 32 KiB = 8 MiB` of the machines' 2 GiB - plus the table's
own statics; the design (a hard limit, not a grown collection) does not change. Linux's limit is memory; a
machine whose limit is a table should at least be wider than a normal runner's playground.

**Pinned** by `t-fork-limit`'s new first claim, `the machine has room for more than a hundred tasks at once
(N made)`: it forks until the machine refuses, so it reads 57 at 64 slots and ~249 at 256. The claim fails
on the old width by construction (the counterfactual is the constant, which is why the doc comment carries
the measurement).

**Verified** after the change: `cargo xtask test --arch x86_64` → `pass=258 fail=0 (258 built)`,
`mpos-kunit 35 passed, 0 failed`; `--arch aarch64` → `pass=257 fail=0 (257 built)`, `mpos-kunit 35 passed,
0 failed`, every lane `=== END`, and the boot line reads `scheduler up: 256 slots, running task
TaskId(0)`. Before it, the same two arms were `pass=224 fail=34` and worse, all of it `fork: Resource
temporarily unavailable`.

## §6.85 The seccomp stop's `SETREGS` answered `ESRCH` once — the same park, a cheaper witness

**An observation with a mechanism, not a diagnosis (2026-11-04).** One aarch64 arm run, on the first
`t-seccomp` after the task table widened, reddened on two claims in a row:

```
ok:    the tracer can read the tracee's registers at the seccomp stop
FAIL:  the tracer can rewrite the syscall number at the seccomp stop
FAIL:  the redirected call runs, and the tracee exits normally
ok:    the cancel tracee stops for TRACEME first
…
ok:    the tracer cancels the judged call by writing -1 at the seccomp stop
ok:    the cancelled call does not run, and the tracee exits normally
```

The first claim is a bare `PTRACE_SETREGS` compared against `Ok(0)` (`user/tests/src/bin/t-seccomp.rs:436`),
and the *same* call shape — `PTRACE_SETREGS` writing the number slot at a seccomp stop — passed twice
later in the same program for the cancel tracee. So the answer that failed is
`src/syscall/linux/proc.rs:1166`'s `abi::PTRACE_SETREGS` arm falling to `err::ESRCH`:

```rust
abi::PTRACE_SETREGS => {
    if !buffer_ok(data, abi::uregs::BYTES, false, from_user) { return err::EFAULT; }
    if crate::sched::set_trace_regs(id, &abi::read_user_regs(data)) { 0 } else { err::ESRCH }
}
```

and `set_trace_regs` (`src/sched/runtime/trace.rs:121`) answers `false` for exactly one reason that a
stopped tracee can reach: `task.trace.stop.is_some()` is false. A tracee whose stop is gone is also a
tracee that can run to its exit, which is what the next claim saw.

**That is §6.79's mechanism seen from the tracer's side.** A stop is recorded at the task's own
syscall-exit gate while the task is *running*, so the task is in the ready queue when `trace::park` sleeps;
`policy::sleep` only pushes a heap entry, the picker takes the ready entry, and the park returns at once —
"a park whose sleep happens while the task is still ready is a no-op". Any wake that arrives for another
reason (its own armed timer, an interval timer, a signal being *queued*) takes it out of the stop, and the
tracer's next `SETREGS` meets a task with no stop.

**The pin this suggests, and why it is cheaper than the detach one.** No timer and no race is needed: a
tracer can wake the tracee *itself* between the stop and the `SETREGS`, with a signal the tracee blocks
(`SIGUSR1` behind `sigprocmask`) so nothing is delivered and nothing else changes. Linux answers 0 for
`SETREGS` on a tracee that is still stopped; if this kernel answers `ESRCH` there, the claim fails
without the fix and the fix becomes pinnable — which is exactly what §6.79's re-park loop could not do.
Shape: fork the tracee, install the `TRACE` filter, `PTRACE_ATTACH`, take the seccomp stop, send the
blocked signal, `PTRACE_SETREGS`, assert `Ok(0)`, then `PTRACE_CONT` and read the tracee's exit status.

**Not yet run.** It is written down because the observation is otherwise lost in a lane's serial log, and
because it is the first time the park's no-op sleep has a witness outside `t-ptrace-attach`'s detach.

**A first attempt at that pin, and why it is not in the tree (2026-11-04).** The shape above was written
into `user/tests/src/bin/t-seccomp.rs` as `claim_a_stop_survives_a_queued_signal` and run on this host
first, as the repo requires. It did not reach the question: the parent's second `waitpid` returned with
`status 0xc00` - not a stop at all - so the tracee never showed the tracer its seccomp event, and the claim
would have measured the fixture rather than the kernel. Two neighbouring claims of that program also
reddened on this host (`the most restrictive of several filters decides`, and one before it), which is a
sign the fixture's own setup is what broke rather than the rule: the child raises `SIGSTOP`, then installs
the filter, then makes the judged call, and a `PTRACE_TRACEME` tracee that has already stopped once is
exactly where a `SIGSTOP`-then-filter sequence can go wrong (Linux's own answer to the same sequence is
what the existing three-claim program already uses, and it waits for `TRACEME` first *without* raising).
The claim was reverted rather than shipped red; the next attempt starts from the existing
`seccomp_stop_reaches_a_tracer` shape (which does reach its stop on both machines) and adds only the
blocked signal between the stop and the `SETREGS`.

**A second attempt, with the reference running (2026-11-04).** The pin was rewritten from
`seccomp_stop_reaches_a_tracer` and run on this host first. Measuring the fixture rather than assuming it
paid twice on the way: `seccomp(SECCOMP_SET_MODE_FILTER)` answers `EACCES` unless the caller has already
made the `PR_SET_NO_NEW_PRIVS` promise (`build/scratch/nsig/p.c`: `install without nnp: r=-1 errno=13`,
`nnp: 0`, `install with nnp: r=0 errno=0`), which is what the first attempt's `status 0xc00` had been - the
child's own `_exit(12)`, not a stop. With the promise made, the host run reached the question:

```
ok:    a tracee that blocks a signal stops for TRACEME first
ok:    the tracer of it asks for PTRACE_O_TRACESECCOMP
ok:    and resumes it from its own stop
ok:    the judged call stops the tracee (status 0x7057f: stopped=true event=7)
ok:    a signal it blocks can be queued to a stopped tracee (sent=true)
ok:    and its registers can still be written (answer Some(Ok(0)))
ok:    the tracee then runs to its exit (status 0x0: exited=true)
```

Linux answers `PTRACE_SETREGS` with 0 there, so the claim's target reading exists. On this machine the same
claim failed one step earlier - `FAIL:  the judged call stops the tracee (status 0xe00: stopped=false
event=0)`, the child exiting 14 - and **the three claims of `seccomp_stop_reaches_a_tracer` passed in the
same boot**, including the stop itself (`status 0x7057f: stopped=true event=7`). So the seccomp stop does
hold in that fixture and not in this one, before any signal is queued: the difference between the two is
the fixture, not yet the kernel, and a claim cannot be pointed at the park until it can say which of the
two it is looking at. The `policy::hold` + re-park loop was written and built a second time to test it and
did not change this reading, and both were reverted unshipped with the claim. Next: make the new fixture
report the child's own last step (13/14/15 for the filter's answer and `getppid`'s), so the claim can tell
"the stop never happened" from "the stop was released", then re-ask.

**The third attempt reads the child's own last step, and the answer is `ENOSYS` (2026-11-04).** The child
now reports through its exit code, and on this machine the tracer's second `waitpid` came back
`status 0xf00: stopped=false event=0`, i.e. `FAIL:  the step it reached instead (exit 15)` - the judged
`getppid` answered `ENOSYS`, which is what a `SECCOMP_RET_TRACE` filter's syscall answers when the kernel
delivers it no tracer. So the stop of this fixture is not "released": it **never reached the tracer**,
even though `PTRACE_SETOPTIONS` with `PTRACE_O_TRACESECCOMP` answered 0 in the same boot and the older
fixture's stop (`status 0x7057f: stopped=true event=7`) did arrive. The one remaining difference between
the two fixtures is the action word itself: the old one installs `SECCOMP_RET_TRACE | 0x1234` and this one
bare `SECCOMP_RET_TRACE`. That is the next measurement - and if the bare word is what loses the stop, it is
a finding of its own (Linux answers both the same), not a fixture detail. The claim stays out of the tree
until then.

**The data word is not the difference (2026-11-04).** With the action spelled exactly as the older
fixture spells it (`SECCOMP_RET_TRACE | 0x1234`), the machine still answered `status 0xf00:
stopped=false event=0` / `the step it reached instead (exit 15)`: the judged call came back `ENOSYS` and no
stop reached the tracer. So the action word is ruled out, and the remaining differences between the
fixture that does reach its stop and the one that does not are: which call is judged (`getpid` there,
`getppid` here), the blocked `SIGUSR1`, and the `PR_SET_NO_NEW_PRIVS` promise this one has to make before
its install succeeds. The next measurement is a bisect on those three - judge `getpid` first, since the
older fixture differs in exactly that and nothing else it would notice.

**It is the fixture's position, not its shape (2026-11-04).** Bisecting the three remaining differences
removed all of them: judging `getpid` instead of `getppid` still answered `ENOSYS`, dropping the blocked
signal changed nothing, and dropping the `PR_SET_NO_NEW_PRIVS` promise changed nothing either. The one
thing that does change the reading is **where in the program the claim runs**: moved up to sit immediately
after `seccomp_stop_reaches_a_tracer`, the same fixture gets its stop

```
ok:    BISECT judged getpid: stop reached the tracer (status 0x7057f: stopped=true exit=5)
```

and at the end of `main` the same code gets `status 0xf00` / exit 15. So something the program does between
those two points leaves the machine unable to hand a `RET_TRACE` filter's call to a tracer: the candidates
are the claims that install filters **on top of** a previous one and the claim that makes the
`PR_SET_NO_NEW_PRIVS` promise - both change what a later fork inherits. That is the same class of finding
as the cgroup tree outliving its mounts (§3.28): a claim that leaves process-wide state behind, measured by
a later claim that reads it. The next measurement is a bisect over those claims, and the `SETREGS` claim
goes in only once a fixture can reach its stop at the position it needs to occupy.

**A fixture that reaches its stop, and what it measured (2026-11-04).** The claim is now in the tree as
`t-seccomp`'s `claim_a_stop_survives_a_queued_signal`. Two things had to be fixed for it to get there, and
both are lessons in their own right:

1. **The judged call must be one no earlier claim judges.** The filter-stacking claim in this program
   installs two filters on the **main process** and a `seccomp` filter cannot be removed, so every child
   forked afterwards inherits them: `getpid` answers `EPERM` and `getppid` `ENOSYS` from those filters
   before a later fixture's own filter can speak. That is what all four earlier readings had been - the
   `exit 15` was an inherited filter's `ENOSYS`, not a lost tracer - and it is the same shape as the cgroup
   tree outliving its mounts (§3.28): a claim that leaves process-wide state behind, measured by a later
   claim that reads it. Judging `getuid`, which neither inherited filter touches, makes the question
   askable at the end of `main`.
2. **The install needs `PR_SET_NO_NEW_PRIVS`** on a host (and on any guest that is not root): `EACCES`
   without it, `build/scratch/nsig/p.c`.

Measured on both machines with that shape: the stop arrives (`status 0x7057f: stopped=true event=7`), a
blocked `SIGUSR1` can be queued to the stopped tracee, `PTRACE_SETREGS` answers **`Ok(0)`**, and the tracee
then runs to its exit - Linux's answers. So this shape does **not** reproduce the `ESRCH` that the gate saw
once, which means the loss is a rarer interleaving than "a signal was queued to a stopped tracee". The
claim is worth keeping as the promise it states; the `ESRCH` witness is still owed, and `trace::park`'s
single sleep is still the suspect: a stop recorded at a task's own syscall-exit gate while that task is
still ready is the one shape this fixture cannot force.

## §6.86 A stop is ended by a resume, not by any wake — witness found, fix not shipped (2026-11-04)

`trace::park` slept **once** (`policy::sleep(id, priority, u64::MAX)`) and returned on the first wake of any
kind, so a stopped task came back the moment its own armed timer expired, a signal was queued to it, or its
group got a `SIGCONT`. Linux keeps a stopped task stopped through all of those. The two observables were
`t-ptrace-attach`'s `(state "", collected true)` under load and the gate's one-off `t-seccomp`
`PTRACE_SETREGS` → `ESRCH`.

**The witness** (written into `t-seccomp` as `claim_a_stop_outlives_the_tracees_own_timer`, measured and
then removed with the fix): the tracee arms a 50 ms `ITIMER_REAL` immediately before the judged call, stops
at its seccomp event, the tracer waits 200 ms (four times the timer) and then writes registers. Linux
answers `Ok(0)` (host, measured: `ok:    a stop outlives a timer the tracee armed for itself (answer
Some(Ok(0)))`); this machine answered `FAIL:  a stop outlives a timer the tracee armed for itself (answer
Some(Err(3)))` - `ESRCH`, `set_trace_regs`'s only reachable false, because `trace.stop` was gone.

**The fix that made it green, and why it is not shipped.** `policy::hold` (out of every ready band, then
sleep until somebody ends the stop - `retire` is unusable, a resume must find the sleeper) plus a loop in
`park` re-holding after every wake that did not clear the stop, with the check under the same lock as the
sleep. With it the witness read `ok: … (answer Some(Ok(0)))`. A full both-arm sweep then hung four programs
on their own budgets (`=== FAIL t-waitid timeout=120s`, `=== FAIL t-blk-sleep timeout=120s` on x86_64;
`t-waitid`, `t-exec-threads`, `t-drm-sync`, `t-blk-sleep` on aarch64), so it was reverted (`fc3bf67`,
reverting `9bddb0d`). A task held out of the ready queue that nothing resumes is indistinguishable from a
task lost, and the re-hold loop turns a lost wake into a wedge. What is still needed: the resume path must
find the held task under the same lock that clears the stop, instead of re-holding after the fact - then
this witness and those four timeouts are the check.

**Where the wedge is, as a hypothesis to test (2026-11-04, not measured).** The reverted loop's failure is
consistent with a lost wake, not with a task that is merely held: `park` publishes `trace.stop` and then
sleeps the task, while a resumer clears `trace.stop` and *then* calls `sched.wake(id)` - and `wake` only
finds a task that is already in the sleep queue. Any window in which the clear-and-wake lands before the
sleep does is a wake nothing will repeat, and the task stays out of every ready band forever; the four
timeouts (`t-waitid`, `t-blk-sleep`, `t-exec-threads`, `t-drm-sync`) are all programs that wait on
something, so a task lost this way shows up exactly as a hung wait. The shape to build next is therefore
one critical section shared by both sides: the stop published together with the hold, and the stop cleared
together with the wake, with the resumer checking that the sleeper is there before it drops the lock. The
witness (`answer Some(Err(3))`) and the four timeouts are the two ends of the check.

**A sharper suspect, read out of the policy (2026-11-04, not yet measured).** Both sides of the stop are
already atomic: `resume_traced` clears `trace.stop` and calls `wake` inside one `with_table` closure, and
`park` publishes the stop and sleeps inside one `schedule` closure. So the lost-wake window above is
narrower than the loop suggested - but `sleep` pushes a `Sleeper` **unconditionally** (`policy.rs`: one more
heap entry per call, no dedupe) and `wake` pops **one** entry per call. A loop that re-holds therefore
stacks entries, and a second wake - a timer plus a resume, say - can pop a stale one and push the task into
a ready band *while the task is running*: `pick`'s own doc says "a task in neither queue has been picked and
is the one running", so a ready entry for the running task is the same task runnable twice, which is how a
scheduler wedges rather than merely loses something. The next fix is therefore `hold` being idempotent
(`if !is_sleeping(id) { push }`, and the ready-band prune kept), and the check is the same pair: the witness
must read `Ok(0)` and those four waits must not hang.

**Idempotent `hold` does not save the loop (measured 2026-11-04).** The `hold` above was made idempotent
(`if !is_sleeping(id) { sleep }`) and the re-park loop retried on a full aarch64 arm. It hung three
programs on their budgets instead of four (`=== FAIL t-waitid timeout=120s`, `=== FAIL t-sysv-msg
timeout=120s`, `=== FAIL t-timer-create timeout=120s`, `build/gate-stopwake4.log`), so the duplicate-entry
suspect is wrong too, and the loop shape itself is what wedges: a task that a stray wake has already run
once will be re-held by the loop, and whatever state `park` returns into - a signal-delivery path that
believed it had a stopped task - is not a state the loop can put back. Both halves were reverted again.
**What is left is not a tweak:** the stop has to be owned by one object that both the park and the resume
can see, instead of "a flag plus a queue entry that either side may or may not find".

**The lock is not the problem either (read 2026-11-04).** `with_table` and `schedule` both run inside
`critical_section::with` over the same `TABLE`, so a resumer's clear-and-wake cannot interleave with a
park's publish-and-sleep, and the lost-wake window is ruled out by construction. What remains is the
*return*: `park` hands control back to `deliver_signal`'s `Stop` arm, which returns "the call was not
delivered" - i.e. the task continues the syscall it was stopped in. A loop that keeps a task in `park`
until the stop clears therefore re-enters that same gate whenever a stray wake released it, and a tracee
whose tracer is no longer waiting on a stop is a task stopped with nobody to report it: the three waits that
hang (`t-waitid`, `t-sysv-msg`, `t-timer-create`) are all programs with a tracer-shaped or signal-shaped
wait in them. The next attempt has to answer that return, not the queueing: either `park` re-enters the
delivery path on a stop that is still published (so a spurious stop is reported to the tracer), or the wake
that does not resume is not allowed to return at all - and both are a change to the stop's *interface*, not
to `policy`.

**The next experiment, spelled out so it needs no re-derivation (2026-11-04).** With the loop in place,
the only way a task can be held with nobody to wake it is if the *resumer* never reaches its
clear-and-wake. Both resumers are tasks that park themselves - `wait4` for the tracer, `pause`/`nanosleep`
for the group-stop case - so the first thing to instrument is not `park` but **the resumer's own park**:
print the tracer's task id at entry to and exit from its `wait4` park, and the same for `SIGCONT`'s
delivery, around the loop. If the tracer never leaves its own park while the tracee sits held, the wedge is
a stopped task waiting on a stopped task, and the answer is that a stop must be reported to the tracer
*before* the tracee is held out of the ready queue (the report is the tracer's wake), not after.

**The `SIGCONT` path is not a lost wake either (read 2026-11-04).** The one place a task's state is
changed from outside itself is `send_signal_info`'s `SIGCONT` block: it cancels the four queued stop
signals, clears a group stop (`task.trace.stop = None; task.trace.continued = true;`), and then the
function's own tail computes `let held = stops_by_default(SIGCONT) && task.trace.stop.is_some_and(|s|
s.group); if !held { t.sched.wake(id); }` - and `stops_by_default(SIGCONT)` is false, so the wake is
issued. So the group-stop resumer does wake its tracee, and the wedge in that direction is out too.

What is left, precisely: a stop is *published* by the tracee (`park`'s first closure wakes the tracer and
the parent) and *cleared* by the tracer (`resume_traced`) or by `SIGCONT`; both clears wake. The loop still
wedged three waits, so one of the four clears in the tree (`child_exit.rs:543` tracer death,
`child_signal.rs:396` SIGCONT, `trace.rs:882` resume, `trace.rs:927` detach) must be reached by a path that
does *not* end with a wake, or `park` can be entered a second time for a stop that is already published -
the second `park` would publish a second stop object and sleep with nobody told about it. That is now the
one question to answer with a print, and it is a question about `park` being re-entered, not about the queue.

**Adopting a published stop helps, and is still not it (measured 2026-11-04).** `park` was changed to
skip publishing when a stop was already published for the task (carrying on with the same object), and to
re-hold until that object was cleared; whether the tracer and parent were re-woken on the second entry
made no difference. On a full aarch64 arm this took the loop's hangs from three (`t-waitid`, `t-sysv-msg`,
`t-timer-create`) to **one**: `t-waitid` alone, with the parent in `waitid` (`syscall 95` on this
machine) `parks=278` and a sibling in `syscall 130` `parks=4` (`build/gate-stopwake5.log`,
`build/gate-stopwake6.log`). So two of the three hangs were the second-stop object, exactly as suspected,
and the third is a different shape: a parent woken 278 times in `waitid` and never seeing what it waits
for - a wake that carries no event, which is the wait side of the same "one object, two readers" problem
the group stop has. Reverted; the next experiment is `waitid`'s own event queue with the loop in place.

**Where the last hang is, from the numbers already in hand (2026-11-04).** `t-waitid` says in its own
header that its subject is the raw `SYS_waitid` - "247 here, 95 on the second machine" - and the stall
dump of the surviving hang names the parent in `syscall 95` (waitid) and the *child* in `syscall 130`
(`tgkill`, which is how a task raises `SIGSTOP` on itself). So the child is held inside the syscall gate
it raised its own stop from, and the parent is woken 278 times without receiving the stop event. That
pins the last shape down to one thing to print next: whether the child's stop reached
`child_event_in`/`take_stop_report` before it was held (the group-stop publish wakes the parent, but the
*event* the parent reads is taken from the stop object, and a parent that consumed the first report and
then re-parks needs the second entry to publish a fresh one). With the adopt-and-hold patch restored, that
print is the whole next session's work.

**Re-reporting the adopted stop does not fix the last hang either (measured 2026-11-04).** `park` was
retried with the stop object adopted on a second entry *and* its `reported` flag reset to `false` so the
next waiter would be told again. On a full aarch64 arm only **`t-waitid`** hung
(`build/gate-stopwake7.log`: lane 0 `=== SUMMARY pass=0 fail=1` from `=== FAIL t-waitid timeout=120s`,
lanes 1/2/3/5 `pass=40/53/53/55 fail=0`, lane 4 the recorded EL0 boot flake); the other two waits that
hung with the bare loop (`t-sysv-msg`, `t-timer-create`) did not. So the "second stop object" part of the
diagnosis holds - it fixes two of the three - and `t-waitid`'s wedge is not the report either. What is left
is a `waitid` that watches a group stop raised through the child's own `tgkill` gate: the child is held
where it raised the stop, and the parent is woken but never sees `CLD_STOPPED`. The next print is on
`child_event_in`'s stop branch for a P_ALL/P_PID subject whose child is the one held in `park`, with the
adopt-and-hold patch back in the tree.

**`child_event_in`'s stop branch, read (2026-11-04).** A parent hears a stop only when all of these hold
(`src/sched/runtime/child_exit.rs`, `fn child_event_in`): the child is visible to it by kind
(`kind_visible`), `task.trace.stop` is `Some`, **`!stop.reported`**, and - for a group stop - the caller
asked for stops (`WUNTRACED`/`WSTOPPED`), or for a traced stop that the tracer is the caller. So the one
number that decides the surviving hang is `stop.reported` **at the moment the parked parent re-checks**.
`t-waitid` runs many scenarios in one program, and the report is consumed once (`take_stop_report` sets
`reported = true`); if an earlier claim's `waitid` consumed the report for this same child slot, the later
blocking `waitid` will never see it, and no amount of re-publishing in `park` fixes that - the report must
be re-issued where the *wait* is, not where the stop is. The print to take next is therefore one line in
`child_event_in`'s stop branch (`id`, `me`, `stop.signal`, `stop.reported`, `stops`) plus the same line
from `unreported_stop` for the wildcard door, with the adopt-and-hold patch restored.

**The number, measured (2026-11-04).** With the adopt-and-hold patch in place and a print in
`child_event_in`'s stop branch, the surviving hang produced exactly one line:
`mpos-dbg: CE-REPORTED id=8 me=6 sig=19 traced=false` - a *group* stop (`sig=19`, `SIGSTOP`) belonging to
child 8, asked about by parent 6 with `stops` set, whose `reported` was **already true**. So an earlier
claim in the same program had consumed the report and the blocking `waitid` could never see it.

Allowing a group stop to stay readable to a parent that asks for stops (`(!stop.reported || !stop.traced)`
in the branch) removed a second hang that the same run had shown (`t-sched-getaffinity`, gone:
`pass=256 fail=1` from `pass=255 fail=2`) and left `t-waitid` alone. That points the last step at the
*resume* rather than the report: a `SIGSTOP`ped child is held by `park` until its stop is cleared, and if
nothing sends the `SIGCONT` that clears a group stop, the parent's next `waitid` (for the exit) waits on a
child that is waiting to be resumed - a stopped task waiting on a stopped task, exactly the shape §6.86
predicted. The next measurement is whether `t-waitid` ever sends `SIGCONT` to the child it stopped, and what
Linux does with a self-stopped child that the parent only ever `waitid`s on.

**The shape of the last hang, from the pin itself (read 2026-11-04).** `t-waitid`'s stop section forks a
child that `raise(SIGSTOP)`s itself and learns about it through `fn await_event(kid, options)`, which
loops **400 times of `waitid(P_PID, kid, &mut info, options | WNOHANG)` with a 5 ms sleep between** and
returns the first record whose `si_pid` is the child - for the stop (`WSTOPPED`) and then, after
`kill(kid, SIGCONT)`, for the resume (`WCONTINUED`). The stall dump's `parks=278` on the parent in
`syscall 95` is therefore the *polling loop*: 2 s of five-millisecond polls is ~400 wake-and-park cycles,
which is what `parks` counts, not a blocking wait. The child is in `tgkill` (`syscall 130`), i.e. held
inside the gate it raised its own stop from. So the surviving hang is: the child is held (correct), the
parent polls and never gets a record (not correct, and §6.86's `CE-REPORTED` line says the stop's
`reported` flag was already true), and the program then falls through its `None` arms rather than
blocking - which is why it burns the whole budget. Next: with the adopt-and-hold patch in, print
`child_event_in`'s answer (not just `reported`) for that `P_PID` subject on every poll, and compare with
what `take_stop_report` cleared.

**The combination still hangs, and the dump names the two ends (measured 2026-11-04).** With both halves in
- `park` adopting a published stop and holding until it is cleared, and a group stop staying readable to a
parent that asks for stops - a one-lane aarch64 arm still ended `=== FAIL t-waitid timeout=120s`
(`pass=256 fail=1`, `build/gate-stopwake10.log`). Its stall dump has the parent in `syscall 95` (waitid)
`parks=280` - the 400-poll loop - and the child in `syscall 130` (tgkill) `parks=4`, both in
`park 2 (block_until_nanos)`. So the child *is* held where it raised its stop, the parent *is* polling, and
the poll still gets nothing: the remaining question is not the queue, not the report flag and not the
adoption - it is what `waitid`'s non-blocking path does with a child whose stop is live, which is the one
door neither patch touched (`handle_wait`'s `no_hang` arm and the `WNOHANG` predicate it builds). Reverted;
that arm is the next place to print.

**With instruments on, the stop/resume section passes and the wedge moves later (measured 2026-11-04).**
`park` adopting a published stop and holding until it is cleared, with a print on entry, on each re-hold
and on exit, plus one on the `SIGCONT` clear, gave the whole stop/resume section of `t-waitid` green for the
first time:

```
ok:    a child that stops itself was forked
mpos-dbg: PARKIN id=7 sig=19 group=true publish=true
ok:    a stopped child is reported by WSTOPPED
ok:    si_code says a stop
ok:    si_status is the stopping signal
ok:    si_uid is the stopped child's uid
mpos-dbg: CONT-CLEAR id=7
ok:    a resumed child is reported by WCONTINUED
ok:    with the continuing signal as its status
ok:    si_uid is the resumed child's uid
mpos-dbg: PARKOUT id=7
ok:    and it still exits with its own code
```

- no `PARKHOLD`: the loop never re-held the child, and the `SIGCONT` clear ended its park exactly once. So
  the adopt-and-hold shape is *right for a group stop*, and the earlier "hang" readings were the
  `child_event_in` readable-stop change, not this one (that combination hung at the same place - it made a
  cleared-then-reported group stop readable again to a parent whose `waitid(WEXITED)` then kept answering
  it, which is the opposite of what the blocking wait needs).
- what is left hangs **later** in the same program (`=== FAIL t-waitid timeout=120s` after
  `ok:    and leaves the caller's rusage record untouched`) and a second program reddened that had not
  before (`=== FAIL t-rename timeout=120s`, `pass=255 fail=2`), so this shape still wedges a *blocking*
  wait somewhere after a resume - the next print is `block_until_child_event`'s own predicates for the
  post-resume sections, not the stop.
